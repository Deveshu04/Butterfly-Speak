import { DurableObject } from "cloudflare:workers";
import type { Env } from "./index";

/**
 * One ISO week's usage, carried from a deleted account to the next account
 * with the same e-mail address. Signing in again after a deletion makes a new
 * Supabase user and so a new counter; without this, deleting would reset the
 * weekly allowance.
 */
export interface CarriedWeek {
  week_start: string;
  words: number;
  chat_calls: number;
  audio_ms: number;
}

const WEEK_MS = 7 * 86_400_000;

/** Monday 00:00 UTC after the week that starts on `weekStart` (YYYY-MM-DD). */
export function weekEndMs(weekStart: string): number {
  return Date.parse(`${weekStart}T00:00:00Z`) + WEEK_MS;
}

/** The name of the carry object that holds the week for `key`. */
export function carryName(key: string): string {
  return `carry:${key}`;
}

let hmac: { secret: string; key: Promise<CryptoKey> } | null = null;

/**
 * HMAC-SHA256 of the trimmed, lower-cased address under `secret`, in hex. It
 * cannot be turned back into the address, and without the secret nobody can
 * tell which address a record belongs to.
 */
export async function carryKey(secret: string, email: string): Promise<string> {
  if (!hmac || hmac.secret !== secret) {
    const raw = new TextEncoder().encode(secret);
    hmac = { secret, key: crypto.subtle.importKey("raw", raw, { name: "HMAC", hash: "SHA-256" }, false, ["sign"]) };
  }
  const mac = await crypto.subtle.sign("HMAC", await hmac.key, new TextEncoder().encode(email.trim().toLowerCase()));
  return [...new Uint8Array(mac)].map((b) => b.toString(16).padStart(2, "0")).join("");
}

/** A count as stored: a finite number, never below zero. */
function count(n: unknown): number {
  return typeof n === "number" && Number.isFinite(n) && n > 0 ? n : 0;
}

/**
 * The week one deleted address used, named `carry:<key>`. It holds one record
 * and nothing else, and its alarm erases it when the week ends.
 */
export class Carry extends DurableObject<Env> {
  /**
   * Keep `week`. A second deletion in the same week keeps the larger of each
   * count: the account deleted second started from the first one's numbers.
   */
  async keep(week: CarriedWeek): Promise<void> {
    const held = await this.ctx.storage.get<CarriedWeek>("week");
    const same = held?.week_start === week.week_start;
    const next: CarriedWeek = {
      week_start: week.week_start,
      words: Math.max(count(week.words), same ? count(held.words) : 0),
      chat_calls: Math.max(count(week.chat_calls), same ? count(held.chat_calls) : 0),
      audio_ms: Math.max(count(week.audio_ms), same ? count(held.audio_ms) : 0),
    };
    await this.ctx.storage.put("week", next);
    await this.ctx.storage.setAlarm(weekEndMs(next.week_start));
  }

  /** The week held, if it is the week starting `weekStart`. */
  async read(weekStart: string): Promise<CarriedWeek | null> {
    const held = await this.ctx.storage.get<CarriedWeek>("week");
    return held && held.week_start === weekStart ? held : null;
  }

  async alarm(): Promise<void> {
    const held = await this.ctx.storage.get<CarriedWeek>("week");
    // Re-armed rather than erased if a later week replaced the record after
    // this alarm was set.
    if (held && weekEndMs(held.week_start) > Date.now()) {
      await this.ctx.storage.setAlarm(weekEndMs(held.week_start));
      return;
    }
    await this.ctx.storage.deleteAll();
  }
}
