import { DurableObject } from "cloudflare:workers";
import { carryName, weekEndMs } from "./carry";
import type { Env } from "./index";
import { countWords, PCM_BYTES_PER_MS, weekStart } from "./quota";
import { byteLength, CHAT_BODY_MAX_BYTES, shapeChatBody, shapeClientFrame, shapeRealtimeQuery } from "./shape";

const CHAT_RATE_PER_MIN = 60;
const USAGE_RATE_PER_MIN = 60;
/**
 * Realtime sessions an account may open in a minute. The app opens one per
 * dictation, and at most two more when a connect is retried or its token
 * refreshed; the cap on sessions open at once alone would let a client that
 * sends nothing keep reopening them.
 */
const REALTIME_RATE_PER_MIN = 20;
/** An account is deleted once; ten a minute leaves room to retry after a 502. */
const ACCOUNT_RATE_PER_MIN = 10;
/** The close reason every session of a deleted account gets, with code 1000. */
const ACCOUNT_DELETED = "account deleted";
/** Short of the app's 25 s, so a GoTrue that hangs is still answered with a 502. */
const ADMIN_DELETE_TIMEOUT_MS = 10_000;
/**
 * How long a deleted account's object refuses its tokens. A Supabase access
 * token lives an hour, and nothing in it says the user has since been deleted.
 */
const TOMBSTONE_MS = 2 * 60 * 60_000;
const MAX_SOCKETS = 5;
const CLOSE_QUOTA = 4029;
const CLOSE_SESSION_LIMIT = 4030;
/** Policy violation: a session that stopped streaming audio, or floods frames. */
const CLOSE_POLICY = 1008;
const FLUSH_DEBOUNCE_MS = 10_000;
const DEFAULT_WEEKLY_LIMIT = 2000;
const DEFAULT_WEEKLY_CHAT_LIMIT = 3000;
const DEFAULT_SESSION_MAX_SECONDS = 1800;
const DEFAULT_WEEKLY_AUDIO_SECONDS = 7200;
/**
 * How far a session's audio may fall behind the clock before it is closed as
 * idle. The app streams every 100 ms of microphone audio, silence included,
 * for as long as a dictation records, and after its last audio waits at most
 * six seconds for finals; a minute behind is not the app.
 */
const DEFAULT_SESSION_IDLE_SECONDS = 60;
/**
 * How long a session may stay open without sending any audio at all. The app
 * sends the audio it buffered while connecting as soon as Sarvam has begun
 * the session, so its first chunk arrives within a second or two.
 */
const FIRST_AUDIO_MS = 10_000;
/** A Supabase write from the flush alarm that has not answered in this long is given up on. */
const MIRROR_TIMEOUT_MS = 5_000;
/** How soon a week Supabase did not take is tried again. */
const MIRROR_RETRY_MS = 60 * 60_000;
/** PostgREST's `code` for a row whose user no longer exists. */
const FOREIGN_KEY_VIOLATION = "23503";
/**
 * The same, for the mirror a rollover makes before it can park the outgoing
 * week. The first session of a week reads the counter through that rollover,
 * so this wait sits inside the app's connect, which gives up at 4 s
 * (src-tauri/src/sarvam/ws.rs, `CONNECT_TIMEOUT`).
 */
const ROLLOVER_MIRROR_TIMEOUT_MS = 3_000;
/**
 * The most frames a session may send in any one second, not counting full
 * audio chunks. The app sends one chunk per 100 ms of microphone audio
 * (src-tauri/src/audio.rs, `CHUNK_MS`) and a handful of control frames per
 * dictation. Its chunks are left out because it also sends every chunk it
 * buffered while connecting in one burst when the session begins
 * (sarvam/ws.rs, `pending_audio`): after a retried connect that is a hundred
 * at once. A flood of real chunks is bounded by the weekly audio budget.
 */
const FRAMES_PER_SECOND_MAX = 50;
/** An audio frame at least this long counts as one of the app's chunks. */
const FULL_CHUNK_BYTES = 50 * PCM_BYTES_PER_MS;
/**
 * How far past the weekly limit a session that started under it may run
 * before it is closed: the utterance in flight when the limit is crossed is
 * delivered, and a session cannot run on unbounded.
 */
const WORD_GRACE = 100;
/**
 * A session's audio is written to the week once this much has accumulated,
 * and again when it closes: one storage write per ten seconds of audio, not
 * one per frame.
 */
const AUDIO_SAVE_BYTES = 10_000 * PCM_BYTES_PER_MS;
const CLOSE_REASON_MAX_BYTES = 123;

interface Counter {
  week_start: string;
  words: number;
  /** Chat calls forwarded this week. Absent in counters stored before the cap existed. */
  chat_calls?: number;
  /** Audio streamed to Sarvam this week, in ms. Absent in counters stored before the budget existed. */
  audio_ms?: number;
}

/** One open dictation: the app's socket, Sarvam's, and what the relay keeps for the pair. */
interface Session {
  /** Audio forwarded upstream and not yet written to the week. */
  unsavedAudioBytes: number;
  /** All the audio this session has forwarded. */
  audioBytes: number;
  /** Close the client with `code`/`reason` and Sarvam's socket normally. */
  end(code: number, reason: string): void;
}

/**
 * Thrown inside the counter queue once the account has been deleted: nothing
 * may be counted, and no counter created, under the tombstone.
 */
class AccountDeletedError extends Error {}

/** What every route answers a deleted account's token. */
function accountDeleted(): Response {
  return new Response("account deleted", { status: 401 });
}

/** `step`'s result, or null when the account turned out to have been deleted. */
async function unlessDeleted<T>(step: Promise<T>): Promise<T | null> {
  try {
    return await step;
  } catch (err) {
    if (err instanceof AccountDeletedError) return null;
    throw err;
  }
}

/** A realtime upgrade answered, and at once closed with `code`/`reason`. */
function closedAtOnce(code: number, reason: string): Response {
  const pair = new WebSocketPair();
  pair[1].accept();
  mirrorClose(pair[1], code, reason);
  return new Response(null, { status: 101, webSocket: pair[0] });
}

/** A realtime upgrade answered, and at once closed with 4029 "quota". */
function refusedOnQuota(): Response {
  return closedAtOnce(CLOSE_QUOTA, "quota");
}

/**
 * Delete the user from Supabase Auth through GoTrue's admin API. Their
 * `usage_weekly` rows go with them (`on delete cascade`). GoTrue's
 * `user_not_found` is a user already gone, which is what was asked for; any
 * other 404 (a proxy, a wrong path) says nothing about the user.
 */
async function deleteAuthUser(env: Env, userId: string): Promise<boolean> {
  try {
    const res = await fetch(`${env.SUPABASE_URL}/auth/v1/admin/users/${encodeURIComponent(userId)}`, {
      method: "DELETE",
      headers: {
        apikey: env.SUPABASE_SERVICE_ROLE_KEY,
        authorization: `Bearer ${env.SUPABASE_SERVICE_ROLE_KEY}`,
      },
      signal: AbortSignal.timeout(ADMIN_DELETE_TIMEOUT_MS),
    });
    if (res.ok || (res.status === 404 && (await errorCode(res)) === "user_not_found")) return true;
    console.warn(`account delete rejected: ${res.status}`);
    return false;
  } catch {
    console.warn("account delete unreachable");
    return false;
  }
}

/**
 * The `error_code` (GoTrue) or `code` (PostgREST) of a JSON error body, or
 * null for any other body.
 */
async function errorCode(res: Response): Promise<string | null> {
  try {
    const body = (await res.json()) as Record<string, unknown> | null;
    const code = body?.error_code ?? body?.code;
    return typeof code === "string" ? code : null;
  } catch {
    return null;
  }
}

/** A positive number from a var, or the fallback. */
function positiveVar(value: string | undefined, fallback: number): number {
  const n = Number(value);
  return Number.isFinite(n) && n > 0 ? n : fallback;
}

/**
 * A sliding window kept in memory, one minute unless told otherwise. It
 * starts empty again if the object is evicted, which only ever errs towards
 * letting a user in; the weekly counters are the limits that must hold, and
 * they live in storage.
 */
function admit(times: number[], max: number, now: number, windowMs = 60_000): boolean {
  while (times.length > 0 && now - times[0] >= windowMs) times.shift();
  if (times.length >= max) return false;
  times.push(now);
  return true;
}

/**
 * The words a `transcript.final` adds to the week. The app keeps one final per
 * `utterance_idx`, a later final for the same index replacing the earlier
 * (`sarvam/ws.rs`), so an utterance is counted once, for the most words any of
 * its finals held. `counted` belongs to one session: indices start again at 0
 * on the next. A final without a numeric index has nothing to be matched
 * against and counts in full. The partials a session ends with are counted
 * the same way, against the finals of their utterance.
 */
function newWords(counted: Map<number, number>, idx: unknown, words: number): number {
  if (typeof idx !== "number" || !Number.isFinite(idx)) return words;
  const before = counted.get(idx) ?? 0;
  if (words <= before) return 0;
  counted.set(idx, words);
  return words - before;
}

/**
 * The request body, or `null` once it passes `max` bytes. Read chunk by chunk
 * and cancelled at the cap, so a body sent without a length (chunked) is never
 * read further than a declared one would be.
 */
async function readCapped(req: Request, max: number): Promise<Uint8Array | null> {
  if (!req.body) return new Uint8Array(0);
  const reader = req.body.getReader();
  const chunks: Uint8Array[] = [];
  let total = 0;
  for (;;) {
    const { done, value } = await reader.read();
    if (done) break;
    total += value.byteLength;
    if (total > max) {
      await reader.cancel().catch(() => undefined);
      return null;
    }
    chunks.push(value);
  }
  const out = new Uint8Array(total);
  let at = 0;
  for (const chunk of chunks) {
    out.set(chunk, at);
    at += chunk.byteLength;
  }
  return out;
}

/**
 * `fetch()` speaks http(s); the Sarvam realtime URL is written `wss://` (the
 * same constant the app uses), so translate the scheme for the upgrade call.
 */
function upgradeUrl(base: string): string {
  if (base.startsWith("wss://")) return `https://${base.slice(6)}`;
  if (base.startsWith("ws://")) return `http://${base.slice(5)}`;
  return base;
}

/**
 * The codes `close()` will put on the wire. The runtime throws
 * `InvalidAccessError` for the rest -- notably 1005 (the peer sent an empty
 * close frame) and 1006 (the peer dropped the connection), which are precisely
 * the codes a *received* close event reports and which the app produces every
 * time it hangs up after `{"event":"end"}`.
 */
function sendableCloseCode(code: number | undefined): number {
  if (code === 1006) return 1011; // a dropped connection is an abnormal end
  if (code === undefined) return 1000;
  const sendable = (code >= 1000 && code <= 1003) || (code >= 1007 && code <= 1014) || (code >= 3000 && code <= 4999);
  return sendable ? code : 1000;
}

/** A close reason may be at most 123 bytes UTF-8; cut on a code-point boundary. */
function sendableCloseReason(reason: string | undefined): string {
  const r = reason ?? "";
  if (r === "" || byteLength(r) <= CLOSE_REASON_MAX_BYTES) return r;
  let out = "";
  let used = 0;
  for (const ch of r) {
    const n = byteLength(ch);
    if (used + n > CLOSE_REASON_MAX_BYTES) break;
    out += ch;
    used += n;
  }
  return out;
}

/**
 * Mirror one leg's close onto the other. Every close in this file goes through
 * here: a throw from `close()` swallowed by a surrounding catch would leave
 * the other socket open -- an upstream Sarvam socket stranded on a shared
 * concurrency slot, and the object billable, for up to fifteen minutes.
 */
function mirrorClose(target: WebSocket, code: number | undefined, reason: string | undefined): void {
  try {
    target.close(sendableCloseCode(code), sendableCloseReason(reason));
  } catch {
    /* already closed, or closing: nothing left to mirror */
  }
}

export class UserSession extends DurableObject<Env> {
  private sockets = 0;
  private realtimeTimes: number[] = [];
  private chatTimes: number[] = [];
  private usageTimes: number[] = [];
  /** Kept through a delete, unlike the others: clearing it would unlimit the route. */
  private accountTimes: number[] = [];
  /**
   * Raised by every account delete. A session, or an upgrade still waiting
   * on Sarvam, that began before the delete counts no frame that arrives
   * after it.
   */
  private generation = 0;
  /**
   * The delete in progress. A second delete that arrives meanwhile waits for
   * it and gives the same answer: two at once would each carry and erase the
   * week, and the second's erase could remove the first's tombstone and the
   * counter the first's last carry reads.
   */
  private deleting: Promise<204 | 502> | null = null;
  private knownUser: string | null = null;
  /**
   * The keyed hash of the user's address, from the Worker with every request.
   * In memory only; null when carrying is off.
   */
  private carryKey: string | null = null;
  private sessions = new Set<Session>();
  /** The week's `audio_ms` as last read from or written to storage. */
  private audioSavedMs = 0;
  /** Audio taken out of a session and queued for storage, not yet written. */
  private audioInTransitMs = 0;
  /** Audio the tombstone refused to count, for the delete to carry. */
  private refusedAudioMs = 0;
  /** Words the tombstone refused to count, for the delete to carry. */
  private refusedWords = 0;
  /** The week's words as last read from or written to storage. */
  private wordsSeen = 0;
  /** Every read-modify-write of the counter runs here, one after another. */
  private counterQueue: Promise<unknown> = Promise.resolve();
  /** The week whose end this instance has already made sure an alarm is armed by. */
  private armedFor: string | null = null;

  private limit(): number {
    return positiveVar(this.env.WEEKLY_WORD_LIMIT, DEFAULT_WEEKLY_LIMIT);
  }

  private chatLimit(): number {
    return positiveVar(this.env.WEEKLY_CHAT_LIMIT, DEFAULT_WEEKLY_CHAT_LIMIT);
  }

  private sessionMaxMs(): number {
    return positiveVar(this.env.SESSION_MAX_SECONDS, DEFAULT_SESSION_MAX_SECONDS) * 1000;
  }

  private audioBudgetMs(): number {
    return positiveVar(this.env.WEEKLY_AUDIO_SECONDS, DEFAULT_WEEKLY_AUDIO_SECONDS) * 1000;
  }

  private sessionIdleMs(): number {
    return positiveVar(this.env.SESSION_IDLE_SECONDS, DEFAULT_SESSION_IDLE_SECONDS) * 1000;
  }

  /**
   * Run one read-modify-write of the counter after every earlier one. The
   * input gate already keeps two *events* from interleaving their storage
   * steps, but one event can start several updates at once -- a quota cut
   * closes every open session, and each writes its audio -- and those would
   * each read the same counter and keep only the last write. Every read of
   * the counter goes through here too: a rollover writes, and can wait on
   * Supabase between its read and its write.
   */
  private serial<T>(task: () => Promise<T>): Promise<T> {
    const run = this.counterQueue.then(task);
    this.counterQueue = run.catch(() => undefined);
    return run;
  }

  /** The week's counter, rolled over if the week has turned. */
  private readCounter(): Promise<Counter> {
    return this.serial(() => this.counter());
  }

  /**
   * Only ever called inside `serial`. Every read and every spend of the week
   * comes through here, so this is where a deleted account's tokens are
   * stopped, whenever their request got past the check in `fetch`.
   */
  private async counter(): Promise<Counter> {
    if (await this.tombstoned()) throw new AccountDeletedError();
    const ws = weekStart(new Date());
    const stored = await this.ctx.storage.get<Counter>("counter");
    if (stored && stored.week_start === ws) {
      await this.keepArmed(ws);
      return stored;
    }
    // A rollover. The outgoing week can hold up to ten seconds of words that no
    // flush has mirrored yet, and overwriting `counter` would lose them, so park
    // it under a second key and arm the flush. Only one week can be parked: if
    // something is already there it has to reach Supabase first, and if it
    // cannot, it keeps the slot (the older week is the one closer to being lost).
    if (stored && stored.words > 0) {
      const parked = await this.ctx.storage.get<Counter>("counter_prev");
      const free =
        !parked || parked.week_start === stored.week_start || (await this.mirror(parked, ROLLOVER_MIRROR_TIMEOUT_MS));
      if (free) {
        await this.ctx.storage.put("counter_prev", stored);
        await this.scheduleFlush();
      }
    }
    const fresh: Counter = { week_start: ws, words: 0, chat_calls: 0, audio_ms: 0 };
    // A first counter starts from the week a deleted account with the same
    // address had used, so deleting and signing in again resets nothing.
    if (!stored) Object.assign(fresh, await this.carriedWeek(ws));
    await this.ctx.storage.put("counter", fresh);
    await this.keepArmed(ws);
    return fresh;
  }

  /**
   * Make sure an alarm fires by the end of the week `ws`. Every object that
   * holds a counter keeps one, and it is what erases the object once nobody
   * uses it (`alarm`). One storage read per week per instance.
   */
  private async keepArmed(ws: string): Promise<void> {
    if (this.armedFor === ws) return;
    try {
      await this.armBy(weekEndMs(ws));
      this.armedFor = ws;
    } catch {
      /* tried again on the next counter read */
    }
  }

  /** Make sure an alarm fires at `at` or sooner. */
  private async armBy(at: number): Promise<void> {
    const pending = await this.ctx.storage.getAlarm();
    if (pending === null || pending > at) await this.ctx.storage.setAlarm(at);
  }

  private carryStub(key: string) {
    return this.env.CARRY.get(this.env.CARRY.idFromName(carryName(key)));
  }

  /** What a deleted account with this address used in the week `ws`, or nothing. */
  private async carriedWeek(ws: string): Promise<Partial<Counter>> {
    if (!this.carryKey) return {};
    try {
      const week = await this.carryStub(this.carryKey).read(ws);
      return week ? { words: week.words, chat_calls: week.chat_calls, audio_ms: week.audio_ms } : {};
    } catch {
      // Fails open: a sign-in is not refused because the record is out of reach.
      console.warn("carry lookup failed");
      return {};
    }
  }

  /**
   * Hand this week's counts to the carry object before a delete clears them,
   * with the words and audio the tombstone refused added on top. `false` if
   * it could not take them. Only ever called inside `serial`.
   */
  private async carryOver(key: string, extraWords = 0, extraAudioMs = 0): Promise<boolean> {
    const ws = weekStart(new Date());
    const stored = await this.ctx.storage.get<Counter>("counter");
    const c = stored && stored.week_start === ws ? stored : undefined;
    const words = (c?.words ?? 0) + extraWords;
    const chatCalls = typeof c?.chat_calls === "number" ? c.chat_calls : 0;
    const audioMs = (typeof c?.audio_ms === "number" ? c.audio_ms : 0) + extraAudioMs;
    if (words === 0 && chatCalls === 0 && audioMs === 0) return true;
    try {
      await this.carryStub(key).keep({ week_start: ws, words, chat_calls: chatCalls, audio_ms: audioMs });
      return true;
    } catch {
      console.warn("carry write failed");
      return false;
    }
  }

  /** Add words to the week; resolves to the week's new total. */
  private addWords(n: number): Promise<number> {
    return this.serial(async () => {
      let c: Counter;
      try {
        c = await this.counter();
      } catch (err) {
        // Refused under the tombstone. The client has still been sent these
        // words, so the delete adds them to the week it carries.
        if (err instanceof AccountDeletedError) this.refusedWords += n;
        throw err;
      }
      c.words += n;
      await this.ctx.storage.put("counter", c);
      this.wordsSeen = c.words;
      await this.scheduleFlush();
      return c.words;
    });
  }

  /**
   * Count one chat call against the week, or refuse it. The check and the
   * count are one step of the queue, so two calls arriving together cannot
   * both take the last one. Not mirrored -- the table has no chat column --
   * so no flush.
   */
  private takeChatCall(): Promise<boolean> {
    return this.serial(async () => {
      const c = await this.counter();
      const used = typeof c.chat_calls === "number" ? c.chat_calls : 0;
      if (used >= this.chatLimit()) return false;
      c.chat_calls = used + 1;
      await this.ctx.storage.put("counter", c);
      return true;
    });
  }

  /** The week's audio as far as the object knows it: stored, on its way to storage, and still held by open sessions. */
  private audioUsedMs(): number {
    let unsaved = 0;
    for (const s of this.sessions) unsaved += s.unsavedAudioBytes;
    return this.audioSavedMs + this.audioInTransitMs + unsaved / PCM_BYTES_PER_MS;
  }

  /**
   * Move a session's unsaved audio into the week. Whole milliseconds while the
   * session runs (the remainder stays with it); rounded up at its close.
   */
  private saveAudio(session: Session, closing: boolean): void {
    const exact = session.unsavedAudioBytes / PCM_BYTES_PER_MS;
    const ms = closing ? Math.ceil(exact) : Math.floor(exact);
    if (ms <= 0) return;
    session.unsavedAudioBytes = closing ? 0 : session.unsavedAudioBytes - ms * PCM_BYTES_PER_MS;
    this.audioInTransitMs += ms;
    void this.serial(async () => {
      try {
        const c = await this.counter();
        c.audio_ms = (typeof c.audio_ms === "number" ? c.audio_ms : 0) + ms;
        await this.ctx.storage.put("counter", c);
        this.audioSavedMs = c.audio_ms;
      } catch (err) {
        // Refused under the tombstone. The audio has still reached Sarvam, so
        // the delete adds it to the week it carries.
        if (err instanceof AccountDeletedError) this.refusedAudioMs += ms;
        throw err;
      } finally {
        this.audioInTransitMs -= ms;
      }
    }).catch((err) => {
      // A deleted account counts nothing; that is not a failure.
      if (!(err instanceof AccountDeletedError)) console.warn("audio count failed");
    });
  }

  /**
   * Account for audio a session just forwarded, and close every open session
   * with 4029 once the week's audio reaches the budget. Silence adds no words
   * but is still audio sent to Sarvam, so the word limit alone would not bound
   * what a session can spend.
   */
  private countAudio(session: Session, bytes: number): void {
    session.unsavedAudioBytes += bytes;
    session.audioBytes += bytes;
    if (this.audioUsedMs() >= this.audioBudgetMs()) {
      this.endAll(CLOSE_QUOTA, "quota");
      return;
    }
    if (session.unsavedAudioBytes >= AUDIO_SAVE_BYTES) this.saveAudio(session, false);
  }

  /** Close every open session of this user. */
  private endAll(code: number, reason: string): void {
    for (const session of [...this.sessions]) session.end(code, reason);
  }

  /**
   * Debounced mirror write: one PostgREST upsert every 10 s at most. The
   * week-end alarm every counter keeps is brought forward, and an alarm already
   * due sooner is left alone, so a long dictation still mirrors on the way
   * through instead of waiting for the last word.
   */
  private async scheduleFlush(): Promise<void> {
    try {
      await this.armBy(Date.now() + FLUSH_DEBOUNCE_MS);
    } catch {
      /* the counter of record is this object's storage; the mirror can wait */
    }
  }

  /**
   * One PostgREST upsert of one week. Display only. `true` when the week needs
   * no more mirroring: it landed, or it never can, because Supabase no longer
   * has the user (the row's foreign key is refused) or there is no user id to
   * write it under. `false` just means "not yet".
   */
  private async mirror(c: Counter, timeoutMs = MIRROR_TIMEOUT_MS): Promise<boolean> {
    const userId = this.knownUser ?? (await this.ctx.storage.get<string>("user_id"));
    if (!userId) return true;
    try {
      const res = await fetch(`${this.env.SUPABASE_URL}/rest/v1/usage_weekly?on_conflict=user_id,week_start`, {
        method: "POST",
        headers: {
          apikey: this.env.SUPABASE_SERVICE_ROLE_KEY,
          authorization: `Bearer ${this.env.SUPABASE_SERVICE_ROLE_KEY}`,
          "content-type": "application/json",
          prefer: "resolution=merge-duplicates,return=minimal",
        },
        body: JSON.stringify({ user_id: userId, week_start: c.week_start, words: c.words, updated_at: new Date().toISOString() }),
        // A rollover waits on this inside the counter queue: never for long.
        signal: AbortSignal.timeout(timeoutMs),
      });
      if (res.ok) return true;
      console.warn(`usage mirror rejected: ${res.status}`);
      return res.status === 409 && (await errorCode(res)) === FOREIGN_KEY_VIOLATION;
    } catch {
      console.warn("usage mirror unreachable");
      return false;
    }
  }

  async alarm(): Promise<void> {
    // This alarm has fired; whatever is armed next is decided at the end.
    this.armedFor = null;
    // A deleted account's object holds only its tombstone, until its time.
    const until = await this.ctx.storage.get<number>("deleted_until");
    if (until !== undefined) {
      if (Date.now() >= until) await this.serial(() => this.eraseAll());
      else await this.ctx.storage.setAlarm(until);
      return;
    }
    // A week parked by a rollover goes first: it is the one that can be lost,
    // and nothing will add to it again. It is only dropped once it needs no
    // more mirroring, and only if it is still that week: while the mirror was
    // in flight a rollover may have parked the next week in its place.
    const parked = await this.ctx.storage.get<Counter>("counter_prev");
    if (parked && (await this.mirror(parked))) {
      await this.serial(async () => {
        const now = await this.ctx.storage.get<Counter>("counter_prev");
        if (now && now.week_start === parked.week_start && now.words === parked.words) {
          await this.ctx.storage.delete("counter_prev");
        }
      });
    }
    // Then the live counter, read exactly as stored rather than through
    // `counter()`: a flush armed at Sunday 23:59:55 fires in the new week, and
    // `counter()` would roll it over and mirror `{new week, 0}` over the words
    // the alarm was armed to write. A week that has ended with no words has
    // nothing to show, and is not written.
    const c = await this.ctx.storage.get<Counter>("counter");
    const ended = c !== undefined && c.week_start < weekStart(new Date());
    const settled = c === undefined || (ended && c.words === 0) || (await this.mirror(c));
    await this.serial(() => this.eraseOrRearm(settled));
  }

  /**
   * How every alarm ends. Once nothing the object holds is current -- the
   * counter's week has ended, no parked week is waiting for Supabase, and no
   * session is open or on its way to Sarvam -- it erases itself, so an
   * account nobody uses is gone within about a week of its last use.
   * Otherwise it arms the next alarm: the end of the counter's week when that
   * week is current and has reached Supabase, or else an hour from now, while
   * a week still waits for Supabase or a session keeps an ended week's object.
   * `settled` is whether the counter read before the mirror needs no more
   * mirroring. Only ever called inside `serial`.
   */
  private async eraseOrRearm(settled: boolean): Promise<void> {
    // The account can have been deleted while this alarm waited on Supabase.
    // The tombstone then stays, with its own alarm.
    const until = await this.ctx.storage.get<number>("deleted_until");
    if (until !== undefined) {
      await this.ctx.storage.setAlarm(until);
      return;
    }
    const c = await this.ctx.storage.get<Counter>("counter");
    const parked = await this.ctx.storage.get<Counter>("counter_prev");
    const current = c !== undefined && c.week_start >= weekStart(new Date());
    // `sockets` also counts an upgrade still waiting on Sarvam, which is not
    // in `sessions` yet.
    if (!current && !parked && settled && this.sessions.size === 0 && this.sockets === 0) {
      await this.eraseAll();
      this.forgetAll();
      return;
    }
    if (c && current && !parked && settled) await this.armBy(weekEndMs(c.week_start));
    else await this.armBy(Date.now() + MIRROR_RETRY_MS);
  }

  /** Delete everything the object holds. `deleteAll` leaves the alarm set, so it goes first. */
  private async eraseAll(): Promise<void> {
    await this.ctx.storage.deleteAlarm();
    await this.ctx.storage.deleteAll();
  }

  /** Drop what the object remembers of the user in memory. */
  private forgetAll(): void {
    this.carryKey = null;
    this.knownUser = null;
    this.armedFor = null;
    this.realtimeTimes = [];
    this.chatTimes = [];
    this.usageTimes = [];
    this.wordsSeen = 0;
    this.audioSavedMs = 0;
    this.refusedAudioMs = 0;
    this.refusedWords = 0;
  }

  /** Whether this account has been deleted and its object holds only the tombstone. */
  private async tombstoned(): Promise<boolean> {
    return (await this.ctx.storage.get<number>("deleted_until")) !== undefined;
  }

  /**
   * `DELETE /v1/account`: carry the week over, empty this object, then delete
   * the user from Supabase. The object goes first, so a Supabase failure
   * leaves nothing here and a retry empties it again. If the week cannot be
   * carried, nothing is deleted and the answer is 502, so a retry carries it.
   * Once Supabase has deleted the user, the object keeps only a tombstone,
   * which refuses the account's remaining tokens for two hours. One delete
   * runs at a time; one that arrives meanwhile gets the running one's answer.
   */
  private async deleteAccount(userId: string, key: string | null): Promise<Response> {
    if (!admit(this.accountTimes, ACCOUNT_RATE_PER_MIN, Date.now())) return new Response("rate limited", { status: 429 });
    // Checked and set before any await, so two deletes cannot both start.
    if (!this.deleting) {
      const run = this.runDelete(userId, key).finally(() => {
        if (this.deleting === run) this.deleting = null;
      });
      this.deleting = run;
    }
    const status = await this.deleting;
    return status === 204 ? new Response(null, { status: 204 }) : new Response("account not deleted, try again", { status: 502 });
  }

  /** The delete itself, for `deleteAccount`. */
  private async runDelete(userId: string, key: string | null): Promise<204 | 502> {
    // Already deleted: a retry whose first answer was lost on the way.
    if (await this.tombstoned()) return 204;
    this.generation += 1;
    this.endAll(1000, ACCOUNT_DELETED);
    // Behind every counter write already queued, the audio and the partials'
    // words the sessions just closed were holding included, so the week
    // carried is complete and none of those writes lands after the wipe.
    const cleared = await this.serial(async () => {
      if (key && !(await this.carryOver(key))) return false;
      await this.eraseAll();
      return true;
    });
    if (!cleared) return 502;
    // Until GoTrue answers the account exists, and its next request writes
    // the id and arms the alarm again.
    this.knownUser = null;
    this.armedFor = null;
    if (!(await deleteAuthUser(this.env, userId))) return 502;
    // The user is gone. The tombstone goes in before anything that can let
    // another request in, so every request from here on is refused.
    const until = Date.now() + TOMBSTONE_MS;
    await this.writeTombstone(until);
    // What the account's token spent while GoTrue answered is carried too,
    // any session it opened meanwhile is closed, and the object keeps only
    // the tombstone. The audio and the partials' words those sessions still
    // held are refused under the tombstone when they close, as is a final
    // whose count was still queued when the tombstone went in; all of those
    // are queued ahead of the carry, which adds what they were refused. A
    // carry that fails now is logged and passed over: the account cannot be
    // brought back to try again.
    this.generation += 1;
    this.endAll(1000, ACCOUNT_DELETED);
    await this.serial(async () => {
      // Taken inside this step, so a count refused while the tombstone was
      // being written is carried too, and by one carry only.
      const words = this.refusedWords;
      const audioMs = this.refusedAudioMs;
      this.refusedWords = 0;
      this.refusedAudioMs = 0;
      if (key) await this.carryOver(key, words, audioMs);
      await this.eraseAll();
      await this.writeTombstone(until);
    });
    this.forgetAll();
    return 204;
  }

  /** Keep `deleted_until`, with the alarm that erases it at that time. */
  private async writeTombstone(until: number): Promise<void> {
    await this.ctx.storage.put("deleted_until", until);
    await this.ctx.storage.setAlarm(until);
  }

  async fetch(req: Request): Promise<Response> {
    const userId = req.headers.get("x-user-id");
    if (!userId) return new Response("forbidden", { status: 403 });
    const url = new URL(req.url);
    const carryKey = req.headers.get("x-carry-key");
    // Before the id is written below: a delete stores nothing.
    if (url.pathname === "/v1/account" && req.method === "DELETE") return this.deleteAccount(userId, carryKey);
    // A deleted account's tokens stay valid for up to an hour. They are
    // refused here, before anything is stored or read.
    if (await this.tombstoned()) return accountDeleted();
    this.carryKey = carryKey;
    // Kept only so the mirror write knows which row to upsert; written once.
    // Whatever is stored gets the alarm that erases it once unused, even when
    // the request goes no further than this.
    if (this.knownUser !== userId) {
      await this.ctx.storage.put("user_id", userId);
      this.knownUser = userId;
      await this.keepArmed(weekStart(new Date()));
    }
    const limit = this.limit();

    if (url.pathname === "/v1/usage") {
      if (!admit(this.usageTimes, USAGE_RATE_PER_MIN, Date.now())) return new Response("rate limited", { status: 429 });
      const c = await unlessDeleted(this.readCounter());
      if (!c) return accountDeleted();
      return Response.json({ week_start: c.week_start, words: c.words, limit });
    }

    if (url.pathname === "/v1/realtime") {
      if (req.headers.get("upgrade")?.toLowerCase() !== "websocket") return new Response("expected websocket", { status: 426 });
      if (!admit(this.realtimeTimes, REALTIME_RATE_PER_MIN, Date.now())) return new Response("rate limited", { status: 429 });
      const generation = this.generation;
      const c = await unlessDeleted(this.readCounter());
      if (!c) return accountDeleted();
      this.audioSavedMs = typeof c.audio_ms === "number" ? c.audio_ms : 0;
      this.wordsSeen = c.words;
      const spent = () => this.wordsSeen >= limit || this.audioUsedMs() >= this.audioBudgetMs();
      if (spent()) return refusedOnQuota();
      const shaped = shapeRealtimeQuery(url);
      if (!shaped) return new Response("bad query", { status: 400 });
      // The slot is taken here, before the upstream fetch, and given back on
      // every way out. Input gates do not hold across an outbound fetch, so a
      // count raised only once Sarvam answered would let every upgrade that
      // arrived in the meantime through the check.
      if (this.sockets >= MAX_SOCKETS) return new Response("too many sessions", { status: 429 });
      this.sockets += 1;
      let held = true;
      const release = () => {
        if (!held) return;
        held = false;
        this.sockets = Math.max(0, this.sockets - 1);
      };
      let upstreamResp: Response;
      try {
        upstreamResp = await fetch(`${upgradeUrl(this.env.SARVAM_REALTIME_URL)}?${shaped}`, {
          headers: { upgrade: "websocket", "api-subscription-key": this.env.SARVAM_API_KEY },
        });
      } catch {
        release();
        console.warn("upstream realtime unreachable");
        return new Response("upstream unavailable", { status: 502 });
      }
      const upstream = upstreamResp.webSocket;
      if (!upstream) {
        release();
        console.warn(`upstream realtime refused: ${upstreamResp.status}`);
        return new Response("upstream unavailable", { status: 502 });
      }
      upstream.accept();
      // The account can have been deleted while Sarvam was answering, after
      // the sessions that were open then had been closed.
      if (this.generation !== generation) {
        release();
        mirrorClose(upstream, 1000, ACCOUNT_DELETED);
        return closedAtOnce(1000, ACCOUNT_DELETED);
      }
      // The week can have been cut while Sarvam was answering: a session that
      // was open then got its 4029, and this one was not open yet to get it.
      if (spent()) {
        release();
        mirrorClose(upstream, 1000, "quota");
        return refusedOnQuota();
      }
      const pair = new WebSocketPair();
      const client = pair[1];
      client.accept();
      // A binary frame arrives as a `Blob` by default, and `send(blob)`
      // stringifies it -- the frame would reach the other side as the text
      // "[object Blob]". `arraybuffer` is what makes "unchanged" true.
      upstream.binaryType = "arraybuffer";
      client.binaryType = "arraybuffer";
      // Words already counted for each utterance of this session.
      const countedByUtterance = new Map<number, number>();
      // For each utterance that has had partials and no final yet, the most
      // words any of them held. A final replaces them; whatever is left when
      // the session ends is counted then, since the client was sent those
      // words whether or not a final ever followed.
      const partialWords = new Map<number | null, number>();
      const count = (words: number) => {
        if (words <= 0) return;
        // `ctx.waitUntil` is a no-op in a Durable Object, and an unawaited
        // promise here would surface a storage failure as an unhandled
        // rejection; the count is best-effort against the frame stream.
        void this.addWords(words)
          .then((total) => {
            if (total >= limit + WORD_GRACE) this.endAll(CLOSE_QUOTA, "quota");
          })
          .catch((err) => {
            if (!(err instanceof AccountDeletedError)) console.warn("count failed");
          });
      };
      const session: Session = {
        unsavedAudioBytes: 0,
        audioBytes: 0,
        end: (code, reason) => {
          mirrorClose(client, code, reason);
          mirrorClose(upstream, 1000, reason);
          finish();
        },
      };
      // Every way a session ends comes through here once: the timers are
      // cleared, the slot given back, and the audio and the partials' words
      // it still holds written. (Only ever called from an event, after both
      // timers below are set.)
      const finish = () => {
        if (!this.sessions.delete(session)) return;
        clearTimeout(timer);
        clearTimeout(idleTimer);
        release();
        this.saveAudio(session, true);
        // A delete that ends the session included: the client has been sent
        // these words, and their count is queued ahead of the delete's carry,
        // which takes them in, or adds them once the tombstone refuses them.
        let words = 0;
        for (const [idx, n] of partialWords) words += newWords(countedByUtterance, idx, n);
        partialWords.clear();
        count(words);
      };
      this.sessions.add(session);
      // However it is used, a session ends after SESSION_MAX_SECONDS.
      const timer = setTimeout(() => session.end(CLOSE_SESSION_LIMIT, "session_limit"), this.sessionMaxMs());
      // And it ends as idle once its audio falls SESSION_IDLE_SECONDS behind
      // the clock, or FIRST_AUDIO_MS if it has sent none yet. Measured in
      // audio time rather than frames, so a trickle of tiny frames, or pings,
      // keep nothing open. Checked when it could first be due, and re-armed
      // from there, not reset on every frame.
      const openedAt = Date.now();
      const idleMs = this.sessionIdleMs();
      const firstAudioMs = Math.min(FIRST_AUDIO_MS, idleMs);
      const checkIdle = () => {
        const behind = Date.now() - openedAt - session.audioBytes / PCM_BYTES_PER_MS;
        const allowed = session.audioBytes === 0 ? firstAudioMs : idleMs;
        if (behind >= allowed) session.end(CLOSE_POLICY, "idle");
        else idleTimer = setTimeout(checkIdle, allowed - behind);
      };
      let idleTimer = setTimeout(checkIdle, firstAudioMs);
      // When this session's recent frames arrived, full audio chunks aside.
      const frameTimes: number[] = [];
      client.addEventListener("message", (e) => {
        // A session that has been ended forwards nothing more.
        if (!this.sessions.has(session)) return;
        // Only the frames the app sends, in the form it sends them.
        const frame = shapeClientFrame(e.data);
        // Every frame counts toward the ceiling, forwarded or dropped, except
        // the app's own audio chunks.
        const fullChunk = frame !== null && frame.audioBytes >= FULL_CHUNK_BYTES;
        if (!fullChunk && !admit(frameTimes, FRAMES_PER_SECOND_MAX, Date.now(), 1_000)) {
          session.end(CLOSE_POLICY, "rate");
          return;
        }
        if (!frame) return;
        try {
          upstream.send(frame.text);
        } catch {
          mirrorClose(client, 1011, "upstream");
          return;
        }
        if (frame.audioBytes > 0) this.countAudio(session, frame.audioBytes);
      });
      upstream.addEventListener("message", (e) => {
        const data = e.data;
        // Delivered first, so the final that takes the week past the limit
        // still reaches the user before the session is closed for it.
        try {
          client.send(data);
        } catch {
          mirrorClose(upstream, 1011, "client");
        }
        // The only look inside Sarvam's frames: count the words of finals,
        // and keep the word count of partials. Never stored. Not once the
        // account is deleted: by then this session's client is closed, by the
        // delete or before it, so the send above threw and the client never
        // had the frame.
        if (this.generation !== generation) return;
        if (typeof data === "string" && data.includes('"transcript.')) {
          try {
            const j = JSON.parse(data);
            // Sarvam's final is {"event":"transcript.final","utterance_idx":n,
            // "text":"…",…}, and a partial the same with "transcript.partial":
            // the shapes the app parses (sarvam/codec.rs).
            if (typeof j?.text === "string") {
              const idx = typeof j.utterance_idx === "number" && Number.isFinite(j.utterance_idx) ? j.utterance_idx : null;
              if (j.event === "transcript.partial") {
                partialWords.set(idx, Math.max(partialWords.get(idx) ?? 0, countWords(j.text)));
              } else if (j.event === "transcript.final") {
                partialWords.delete(idx);
                count(newWords(countedByUtterance, j.utterance_idx, countWords(j.text)));
              }
            }
          } catch {
            /* not JSON: pass through */
          }
        }
      });
      client.addEventListener("close", (e) => {
        finish();
        mirrorClose(upstream, e.code, e.reason);
      });
      upstream.addEventListener("close", (e) => {
        finish();
        mirrorClose(client, e.code, e.reason);
      });
      client.addEventListener("error", () => {
        finish();
        mirrorClose(upstream, 1011, "client error");
      });
      upstream.addEventListener("error", () => {
        finish();
        mirrorClose(client, 1011, "upstream error");
      });
      return new Response(null, { status: 101, webSocket: pair[0] });
    }

    if (url.pathname === "/v1/chat/completions" && req.method === "POST") {
      if (!admit(this.chatTimes, CHAT_RATE_PER_MIN, Date.now())) return new Response("rate limited", { status: 429 });
      // Bytes throughout: a Devanagari body is three bytes a character, so a
      // `String.length` cap would let ~192 KB through. Refuse on the
      // declared length first, so an oversized body is never read at all.
      const declared = Number(req.headers.get("content-length"));
      if (Number.isFinite(declared) && declared > CHAT_BODY_MAX_BYTES) return new Response("body too large", { status: 413 });
      // Read, not buffered whole: a body with no declared length stops at the cap.
      const bytes = await readCapped(req, CHAT_BODY_MAX_BYTES);
      if (!bytes) return new Response("body too large", { status: 413 });
      const raw = new TextDecoder().decode(bytes);
      let parsed: unknown;
      try {
        parsed = JSON.parse(raw);
      } catch {
        return new Response("bad json", { status: 400 });
      }
      const shaped = shapeChatBody(parsed);
      if (!shaped.ok) return new Response(shaped.reason, { status: shaped.status });
      // The per-minute window bounds a burst; this bounds the week. The app
      // makes roughly one polish call per dictation and one background call
      // per ~50 words, so honest use stays far below the cap.
      // Checked in the counter queue, after the body: the account can have
      // been deleted while it was arriving.
      const allowed = await unlessDeleted(this.takeChatCall());
      if (allowed === null) return accountDeleted();
      if (!allowed) return new Response("weekly chat limit", { status: 429 });
      let up: Response;
      try {
        up = await fetch(this.env.SARVAM_CHAT_URL, {
          method: "POST",
          headers: {
            "content-type": "application/json",
            "api-subscription-key": this.env.SARVAM_API_KEY,
            accept: req.headers.get("accept") ?? "application/json",
          },
          body: shaped.body,
        });
      } catch {
        console.warn("upstream chat unreachable");
        return new Response("upstream unavailable", { status: 502 });
      }
      if (!up.ok) console.warn(`upstream chat: ${up.status}`);
      // Stream the body through untouched (SSE included); pass content-type and status.
      return new Response(up.body, {
        status: up.status,
        headers: { "content-type": up.headers.get("content-type") ?? "application/json" },
      });
    }

    return new Response("not found", { status: 404 });
  }
}
