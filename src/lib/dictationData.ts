// The pure half of `stats.svelte.ts` and of Home's "Today" list: no runes, no
// Tauri, no storage, so tests/web/dictationData.test.mjs can run it under
// plain node.
//
// The rule this module exists to keep: the webview's own storage holds counts,
// never what anyone dictated. Dictation text lives in one place on the
// computer, the history database, where the history settings (on/off, keep
// for N days) and the History page's delete and clear all reach it. Home reads
// its list from there (`todayFromHistory`); with history off it shows the
// session's dictations from memory only (`todayOnly`), and nothing is written.
//
// Keep this file free of runtime imports: node runs it straight from source.

/** One row of Home's list. `id` is the history row's id, or `null` for a
 * dictation held in memory only because history is off. */
export interface HomeItem {
  /** Stable across reloads: `db:<id>` or `s:<n>`. */
  key: string;
  id: number | null;
  /** Epoch milliseconds. */
  at: number;
  text: string;
}

/** The fields of a history row this module reads (`HistoryEntry` in api.ts). */
export interface HistoryRowLike {
  id: number;
  text: string;
  /** `datetime('now')`: UTC, "YYYY-MM-DD HH:MM:SS". */
  createdAt: string;
  outcome: string;
}

/** Everything `bs-stats-v1` holds. Counts only; see the note at the top. */
export interface Counters {
  totalWords: number;
  totalDictations: number;
  /** Sum of spoken milliseconds across all dictations (for average WPM). */
  totalSpokenMs: number;
  /** yyyy-mm-dd (local) → words dictated that day. */
  days: Record<string, number>;
  /** yyyy-mm-dd (local) → dictations made that day. */
  dayDictations: Record<string, number>;
  /** app process name → words dictated into it. */
  apps: Record<string, number>;
  /** Dictations started in each two-hour window of the day (index 0 is
   * midnight to 2 am), for Insights' "most active". */
  hours: number[];
  /** Lifetime word-level edits made by cleanup + polish. */
  wordsCorrected: number;
  /** Lifetime dictionary/snippet rule hits. */
  dictFixes: number;
}

export const HOUR_BUCKETS = 12;

export function dayKey(at: number): string {
  const d = new Date(at);
  const m = `${d.getMonth() + 1}`.padStart(2, "0");
  const day = `${d.getDate()}`.padStart(2, "0");
  return `${d.getFullYear()}-${m}-${day}`;
}

export function wordCount(text: string): number {
  return text.split(/\s+/).filter(Boolean).length;
}

function hourBucket(at: number): number {
  return Math.floor(new Date(at).getHours() / 2);
}

export function emptyCounters(): Counters {
  return {
    totalWords: 0,
    totalDictations: 0,
    totalSpokenMs: 0,
    days: {},
    dayDictations: {},
    apps: {},
    hours: new Array(HOUR_BUCKETS).fill(0),
    wordsCorrected: 0,
    dictFixes: 0,
  };
}

function num(v: unknown): number {
  return typeof v === "number" && Number.isFinite(v) && v >= 0 ? v : 0;
}

/** Only the numeric values of a string-keyed record. */
function numbers(v: unknown): Record<string, number> {
  const out: Record<string, number> = {};
  if (v && typeof v === "object" && !Array.isArray(v)) {
    for (const [k, n] of Object.entries(v as Record<string, unknown>)) {
      if (typeof n === "number" && Number.isFinite(n) && n >= 0) out[k] = n;
    }
  }
  return out;
}

const KNOWN_KEYS = new Set([
  "totalWords",
  "totalDictations",
  "totalSpokenMs",
  "days",
  "dayDictations",
  "apps",
  "hours",
  "wordsCorrected",
  "dictFixes",
]);

/**
 * Read whatever `bs-stats-v1` holds, from any version, into counters.
 *
 * Builds the result from the known numeric fields only, so nothing else
 * (above all the old `entries` list of dictation texts) can ride along into
 * the next save. `rewrite` is true whenever what was stored differs from what
 * will be saved: the old text list, an unknown field, a non-number, or a value
 * that would not parse. The caller saves straight away in that case, which is
 * what takes the text out of storage on the first start after an upgrade.
 *
 * A store from before `hours` and `dayDictations` existed gets them from its
 * old `entries` (the last 200 dictations), so Insights keeps its "most
 * active" window and today's count.
 */
export function loadCounters(raw: string | null): { counters: Counters; rewrite: boolean } {
  if (raw === null) return { counters: emptyCounters(), rewrite: false };
  let p: unknown;
  try {
    p = JSON.parse(raw);
  } catch {
    return { counters: emptyCounters(), rewrite: true };
  }
  if (!p || typeof p !== "object" || Array.isArray(p)) {
    return { counters: emptyCounters(), rewrite: true };
  }
  const o = p as Record<string, unknown>;
  const c: Counters = {
    totalWords: num(o.totalWords),
    totalDictations: num(o.totalDictations),
    totalSpokenMs: num(o.totalSpokenMs),
    days: numbers(o.days),
    dayDictations: numbers(o.dayDictations),
    apps: numbers(o.apps),
    hours: new Array(HOUR_BUCKETS).fill(0),
    wordsCorrected: num(o.wordsCorrected),
    dictFixes: num(o.dictFixes),
  };
  if (Array.isArray(o.hours)) {
    for (let i = 0; i < HOUR_BUCKETS; i++) c.hours[i] = num(o.hours[i]);
  }
  if (!Array.isArray(o.hours) && Array.isArray(o.entries)) {
    for (const e of o.entries as unknown[]) {
      const when = e && typeof e === "object" ? (e as Record<string, unknown>).at : undefined;
      if (typeof when !== "number" || !Number.isFinite(when)) continue;
      c.hours[hourBucket(when)] += 1;
      const k = dayKey(when);
      c.dayDictations[k] = (c.dayDictations[k] ?? 0) + 1;
    }
  }
  const rewrite =
    Object.keys(o).some((k) => !KNOWN_KEYS.has(k)) || serializeCounters(c) !== JSON.stringify(sorted(o));
  return { counters: c, rewrite };
}

/** `o` with its known keys in `serializeCounters`' order, for comparison. */
function sorted(o: Record<string, unknown>): Record<string, unknown> {
  const out: Record<string, unknown> = {};
  for (const k of ORDER) if (k in o) out[k] = o[k];
  return out;
}

const ORDER: (keyof Counters)[] = [
  "totalWords",
  "totalDictations",
  "totalSpokenMs",
  "days",
  "dayDictations",
  "apps",
  "hours",
  "wordsCorrected",
  "dictFixes",
];

export function serializeCounters(c: Counters): string {
  const out: Record<string, unknown> = {};
  for (const k of ORDER) out[k] = c[k];
  return JSON.stringify(out);
}

/** Count one dictation. The text is read for its word count and not kept. */
export function recordDictation(
  c: Counters,
  d: {
    text: string;
    at: number;
    durationMs: number;
    app?: string | null;
    wordsCorrected?: number;
    dictFixes?: number;
  },
): Counters {
  const words = wordCount(d.text);
  const key = dayKey(d.at);
  const hours = [...c.hours];
  hours[hourBucket(d.at)] += 1;
  return {
    totalWords: c.totalWords + words,
    totalDictations: c.totalDictations + 1,
    totalSpokenMs: c.totalSpokenMs + num(d.durationMs),
    days: { ...c.days, [key]: (c.days[key] ?? 0) + words },
    dayDictations: { ...c.dayDictations, [key]: (c.dayDictations[key] ?? 0) + 1 },
    apps: d.app ? { ...c.apps, [d.app]: (c.apps[d.app] ?? 0) + words } : c.apps,
    hours,
    wordsCorrected: c.wordsCorrected + num(d.wordsCorrected),
    dictFixes: c.dictFixes + num(d.dictFixes),
  };
}

/** A user's correction on Home: move the word counts by the difference, on
 * the day the dictation was made. The per-app count is left as it was. */
export function applyEdit(c: Counters, at: number, oldText: string, newText: string): Counters {
  const delta = wordCount(newText) - wordCount(oldText);
  const key = dayKey(at);
  return {
    ...c,
    totalWords: Math.max(0, c.totalWords + delta),
    days: { ...c.days, [key]: Math.max(0, (c.days[key] ?? 0) + delta) },
  };
}

/** Start hour (even, 0–22) of the busiest two-hour window, or null. */
export function mostActiveWindow(hours: number[]): number | null {
  let best: number | null = null;
  let bestCount = 0;
  for (let i = 0; i < HOUR_BUCKETS; i++) {
    const n = hours[i] ?? 0;
    if (n > bestCount) {
      best = i * 2;
      bestCount = n;
    }
  }
  return best;
}

/** The two-hour window starting at hour `h` (even, 0–22), as "8–10 am", or
 * "10 am–12 pm" when it crosses noon or midnight. */
export function formatWindow(h: number): string {
  const clock = (hour: number) => (hour % 12 === 0 ? 12 : hour % 12);
  const suffix = (hour: number) => (hour < 12 ? "am" : "pm");
  const end = (h + 2) % 24;
  return suffix(h) === suffix(end)
    ? `${clock(h)}–${clock(end)} ${suffix(end)}`
    : `${clock(h)} ${suffix(h)}–${clock(end)} ${suffix(end)}`;
}

/** Longest run of consecutive days with at least one word, from `days`
 * (yyyy-mm-dd → words). Days are stepped on the calendar rather than by
 * 24 hours, which a daylight-saving change would break. */
export function longestStreak(days: Record<string, number>): number {
  const keys = Object.keys(days)
    .filter((k) => days[k] > 0)
    .sort();
  let best = 0;
  let run = 0;
  let prev: string | null = null;
  for (const key of keys) {
    run = prev !== null && nextDayKey(prev) === key ? run + 1 : 1;
    prev = key;
    best = Math.max(best, run);
  }
  return best;
}

/** The day after the local day `key`, at noon so no clock change can move
 * it onto another date. */
function nextDayKey(key: string): string {
  const [y, m, d] = key.split("-").map(Number);
  return dayKey(new Date(y, m - 1, d + 1, 12).getTime());
}

/** A history row's `createdAt` as epoch milliseconds (NaN if unreadable). */
export function parseHistoryTime(createdAt: string): number {
  const m = /^(\d{4})-(\d{2})-(\d{2}) (\d{2}):(\d{2}):(\d{2})$/.exec(createdAt);
  if (!m) return NaN;
  const [, y, mo, d, h, mi, s] = m.map(Number);
  return Date.UTC(y, mo - 1, d, h, mi, s);
}

/** Home's list with history on: today's delivered rows (what was pasted),
 * in the order given (the history list is newest first). */
export function todayFromHistory(rows: HistoryRowLike[], now: number): HomeItem[] {
  const today = dayKey(now);
  const out: HomeItem[] = [];
  for (const r of rows) {
    if (r.outcome !== "done") continue;
    const at = parseHistoryTime(r.createdAt);
    if (Number.isNaN(at) || dayKey(at) !== today) continue;
    out.push({ key: `db:${r.id}`, id: r.id, at, text: r.text });
  }
  return out;
}

/** The items made today, for the in-memory list. */
export function todayOnly(items: HomeItem[], now: number): HomeItem[] {
  const today = dayKey(now);
  return items.filter((i) => dayKey(i.at) === today);
}

/** The most dictations the in-memory list holds, the same as Home asks the
 * history database for. */
export const SESSION_LIMIT = 200;

/** Add a dictation to the in-memory list (history off). Only the new item's
 * day is kept, since Home shows no other, and at most SESSION_LIMIT items,
 * so a tray app left running for days does not hold every earlier day's
 * text in memory. */
export function pushSession(items: HomeItem[], item: HomeItem): HomeItem[] {
  return todayOnly([item, ...items], item.at).slice(0, SESSION_LIMIT);
}
