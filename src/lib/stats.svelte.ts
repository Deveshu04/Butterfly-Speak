// Lifetime dictation counters, shared across pages and kept in localStorage
// so nothing disappears when you switch sections or restart. Counts only: no
// dictation text is ever written here. The text lives in the history
// database, which Home reads (see dictationData.ts for the rule and the pure
// logic, and tests/web for its tests).
//
// With history off, the dictations of this session are held in `session`, in
// memory only, so Home can still show them; they are gone when the app closes.
// Only today's are held, and at most 200 (`pushSession`).

import { listen } from "@tauri-apps/api/event";
import { TRANSCRIPT_FINAL, type FinalPayload } from "./events";
import {
  applyEdit,
  dayKey,
  emptyCounters,
  loadCounters,
  longestStreak,
  mostActiveWindow,
  pushSession,
  recordDictation,
  serializeCounters,
  type Counters,
  type HomeItem,
} from "./dictationData";
import { settings } from "./stores.svelte";

const STORAGE_KEY = "bs-stats-v1";

class StatsStore {
  #c = $state<Counters>(emptyCounters());
  /** This session's dictations while history is off. Never persisted. */
  session = $state<HomeItem[]>([]);

  #started = false;
  #nextSessionKey = 0;

  get totalWords() {
    return this.#c.totalWords;
  }
  get totalDictations() {
    return this.#c.totalDictations;
  }
  get totalSpokenMs() {
    return this.#c.totalSpokenMs;
  }
  get days() {
    return this.#c.days;
  }
  get apps() {
    return this.#c.apps;
  }
  get wordsCorrected() {
    return this.#c.wordsCorrected;
  }
  get dictFixes() {
    return this.#c.dictFixes;
  }

  /** Idempotent; call once from the app shell. */
  init() {
    if (this.#started) return;
    this.#started = true;
    let raw: string | null = null;
    try {
      raw = localStorage.getItem(STORAGE_KEY);
    } catch {
      /* storage unavailable: start from zero */
    }
    const { counters, rewrite } = loadCounters(raw);
    this.#c = counters;
    // An older store still holds the text of up to 200 dictations; saving
    // now is what takes it out.
    if (rewrite) this.#save();
    listen<FinalPayload>(TRANSCRIPT_FINAL, (e) => {
      const at = Date.now();
      this.#c = recordDictation(this.#c, {
        text: e.payload.text,
        at,
        durationMs: e.payload.durationMs ?? 0,
        app: e.payload.app ?? null,
        wordsCorrected: e.payload.wordsCorrected ?? 0,
        dictFixes: e.payload.dictFixes ?? 0,
      });
      this.#save();
      // History on (or not yet known): the backend filed it in the history
      // database, which is where Home reads it from.
      if (settings.current?.history.enabled === false) {
        this.#nextSessionKey += 1;
        this.session = pushSession(this.session, {
          key: `s:${this.#nextSessionKey}`,
          id: null,
          at,
          text: e.payload.text,
        });
      }
    });
  }

  /** Home's Edit: move the counters by the word difference. The text itself
   * is changed where it lives (the history row, or `editSession`). */
  countEdit(at: number, oldText: string, newText: string) {
    if (oldText === newText) return;
    this.#c = applyEdit(this.#c, at, oldText, newText);
    this.#save();
  }

  /** Home's Edit of a dictation held in memory (history off). */
  editSession(key: string, text: string) {
    this.session = this.session.map((i) => (i.key === key ? { ...i, text } : i));
  }

  /** Clear all history: the user has asked for their dictations to go, so the
   * ones held in memory for Home go too. */
  clearSession() {
    this.session = [];
  }

  get todayWords(): number {
    return this.#c.days[dayKey(Date.now())] ?? 0;
  }

  get todayDictations(): number {
    return this.#c.dayDictations[dayKey(Date.now())] ?? 0;
  }

  /** Start hour (even, 0–22) of the most common 2-hour dictation window. */
  get mostActive(): number | null {
    return mostActiveWindow(this.#c.hours);
  }

  /** Average speaking speed across all dictations, words per minute. */
  get avgWpm(): number {
    if (this.#c.totalSpokenMs < 3000) return 0;
    return Math.round(this.#c.totalWords / (this.#c.totalSpokenMs / 60000));
  }

  /** Consecutive days (ending today or yesterday) with at least one word. */
  get streak(): number {
    const days = this.#c.days;
    let streak = 0;
    const day = new Date();
    // A streak survives until a full day is missed.
    if (!days[dayKey(day.getTime())]) day.setDate(day.getDate() - 1);
    while (days[dayKey(day.getTime())]) {
      streak += 1;
      day.setDate(day.getDate() - 1);
    }
    return streak;
  }

  /** Longest run of consecutive days with at least one word, ever. */
  get longestStreak(): number {
    return longestStreak(this.#c.days);
  }

  /** Last N days as [{key, label, words}] oldest → newest, for bar charts. */
  history(daysBack: number): { key: string; label: string; words: number }[] {
    const out: { key: string; label: string; words: number }[] = [];
    const d = new Date();
    d.setDate(d.getDate() - (daysBack - 1));
    for (let i = 0; i < daysBack; i++) {
      const key = dayKey(d.getTime());
      out.push({
        key,
        label: d.toLocaleDateString([], { weekday: "short" }),
        words: this.#c.days[key] ?? 0,
      });
      d.setDate(d.getDate() + 1);
    }
    return out;
  }

  #save() {
    try {
      localStorage.setItem(STORAGE_KEY, serializeCounters(this.#c));
    } catch {
      /* storage full — stats are best-effort */
    }
  }
}

export const stats = new StatsStore();
