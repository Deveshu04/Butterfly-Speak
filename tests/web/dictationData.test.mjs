// Unit tests for src/lib/dictationData.ts, the pure half of the stats store
// and Home's list. Run with `pnpm test` (node's own runner; node strips the
// TypeScript types itself, so nothing is compiled first).

import { test } from "node:test";
import assert from "node:assert/strict";
import {
  HOUR_BUCKETS,
  applyEdit,
  dayKey,
  emptyCounters,
  formatWindow,
  loadCounters,
  longestStreak,
  mostActiveWindow,
  parseHistoryTime,
  pushSession,
  SESSION_LIMIT,
  recordDictation,
  serializeCounters,
  todayFromHistory,
  todayOnly,
} from "../../src/lib/dictationData.ts";

const at = (y, mo, d, h = 12, mi = 0) => new Date(y, mo - 1, d, h, mi).getTime();

test("nothing stored starts from zero and needs no rewrite", () => {
  const { counters, rewrite } = loadCounters(null);
  assert.deepEqual(counters, emptyCounters());
  assert.equal(rewrite, false);
  assert.equal(counters.hours.length, HOUR_BUCKETS);
});

test("an old store with dictation text keeps its counters and loses the text", () => {
  const old = {
    entries: [
      { text: "secret words one", at: at(2026, 9, 28, 9, 30), durationMs: 2000 },
      { text: "secret words two", at: at(2026, 9, 28, 9, 50), durationMs: 1000 },
      { text: "secret three", at: at(2026, 9, 27, 21, 5), durationMs: 500 },
    ],
    totalWords: 1234,
    totalDictations: 99,
    totalSpokenMs: 600000,
    days: { "2026-09-27": 2, "2026-09-28": 6 },
    apps: { notepad: 700, code: 534 },
    wordsCorrected: 12,
    dictFixes: 3,
  };
  const { counters, rewrite } = loadCounters(JSON.stringify(old));
  assert.equal(rewrite, true, "the text has to be written out of storage");
  assert.equal(counters.totalWords, 1234);
  assert.equal(counters.totalDictations, 99);
  assert.equal(counters.totalSpokenMs, 600000);
  assert.deepEqual(counters.days, old.days);
  assert.deepEqual(counters.apps, old.apps);
  assert.equal(counters.wordsCorrected, 12);
  assert.equal(counters.dictFixes, 3);
  // Insights' "most active" and "today" survive as counts.
  assert.equal(counters.hours[4], 2, "two dictations between 8 and 10");
  assert.equal(counters.hours[10], 1, "one between 20 and 22");
  assert.deepEqual(counters.dayDictations, { "2026-09-28": 2, "2026-09-27": 1 });
  const saved = serializeCounters(counters);
  assert.ok(!saved.includes("secret"), "no dictation text is written back");
  assert.ok(!("entries" in JSON.parse(saved)));
});

test("a current store round-trips unchanged and needs no rewrite", () => {
  let c = emptyCounters();
  c = recordDictation(c, { text: "hello there", at: at(2026, 9, 28, 15), durationMs: 900, app: "slack" });
  const { counters, rewrite } = loadCounters(serializeCounters(c));
  assert.equal(rewrite, false);
  assert.deepEqual(counters, c);
});

test("unknown fields never survive a load, so nothing but counts is kept", () => {
  const raw = JSON.stringify({
    totalWords: 5,
    hours: new Array(HOUR_BUCKETS).fill(0),
    dayDictations: {},
    lastText: "should not stay",
    days: { "2026-09-28": 5, bogus: "text here" },
    apps: { notepad: "words" },
  });
  const { counters, rewrite } = loadCounters(raw);
  assert.equal(rewrite, true);
  assert.equal(counters.totalWords, 5);
  assert.deepEqual(counters.days, { "2026-09-28": 5 });
  assert.deepEqual(counters.apps, {});
  assert.ok(!serializeCounters(counters).includes("should not stay"));
});

test("a corrupt store is replaced", () => {
  const { counters, rewrite } = loadCounters("{not json");
  assert.equal(rewrite, true);
  assert.deepEqual(counters, emptyCounters());
});

test("recording a dictation counts it and keeps no text", () => {
  const when = at(2026, 9, 28, 23, 59);
  const c = recordDictation(emptyCounters(), {
    text: "  the quick  brown fox ",
    at: when,
    durationMs: 1500,
    app: "winword",
    wordsCorrected: 2,
    dictFixes: 1,
  });
  assert.equal(c.totalWords, 4);
  assert.equal(c.totalDictations, 1);
  assert.equal(c.totalSpokenMs, 1500);
  assert.deepEqual(c.days, { [dayKey(when)]: 4 });
  assert.deepEqual(c.apps, { winword: 4 });
  assert.deepEqual(c.dayDictations, { [dayKey(when)]: 1 });
  assert.equal(c.hours[11], 1);
  assert.equal(c.wordsCorrected, 2);
  assert.equal(c.dictFixes, 1);
  assert.ok(!serializeCounters(c).includes("quick"));
});

test("an edit moves the word counts of the dictation's own day, never below zero", () => {
  const when = at(2026, 9, 20, 10);
  let c = recordDictation(emptyCounters(), { text: "one two three", at: when, durationMs: 0 });
  c = applyEdit(c, when, "one two three", "one two three four five");
  assert.equal(c.totalWords, 5);
  assert.equal(c.days[dayKey(when)], 5);
  c = applyEdit(c, when, "a b c d e f g h i j", "a");
  assert.equal(c.totalWords, 0);
  assert.equal(c.days[dayKey(when)], 0);
});

test("most active window is the busiest two-hour bucket, or none", () => {
  assert.equal(mostActiveWindow(new Array(HOUR_BUCKETS).fill(0)), null);
  const h = new Array(HOUR_BUCKETS).fill(0);
  h[3] = 4;
  h[7] = 9;
  assert.equal(mostActiveWindow(h), 14);
});

test("history rows carry UTC times", () => {
  assert.equal(parseHistoryTime("2026-09-28 04:05:06"), Date.UTC(2026, 8, 28, 4, 5, 6));
  assert.ok(Number.isNaN(parseHistoryTime("garbage")));
});

test("Home's list from history is today's delivered rows, newest first", () => {
  const now = at(2026, 9, 28, 12);
  const iso = (ms) => new Date(ms).toISOString().slice(0, 19).replace("T", " ");
  const rows = [
    { id: 9, text: "latest", createdAt: iso(now - 1000), outcome: "done" },
    { id: 8, text: "never pasted", createdAt: iso(now - 2000), outcome: "failed" },
    { id: 7, text: "earlier today", createdAt: iso(now - 3000), outcome: "done" },
    { id: 3, text: "last week", createdAt: iso(now - 8 * 86_400_000), outcome: "done" },
  ];
  const items = todayFromHistory(rows, now);
  assert.deepEqual(
    items.map((i) => [i.key, i.id, i.text]),
    [
      ["db:9", 9, "latest"],
      ["db:7", 7, "earlier today"],
    ],
  );
  assert.equal(items[0].at, Math.floor((now - 1000) / 1000) * 1000);
});

test("the in-memory list keeps only today's items", () => {
  const now = at(2026, 9, 28, 12);
  const items = [
    { key: "s:1", id: null, at: at(2026, 9, 28, 11), text: "today" },
    { key: "s:2", id: null, at: at(2026, 9, 27, 11), text: "yesterday" },
  ];
  assert.deepEqual(
    todayOnly(items, now).map((i) => i.text),
    ["today"],
  );
});

test("the in-memory list holds only today's dictations, newest first, and never more than the limit", () => {
  const now = at(2026, 9, 29, 9);
  let items = [{ key: "s:1", id: null, at: at(2026, 9, 28, 23), text: "yesterday" }];
  items = pushSession(items, { key: "s:2", id: null, at: now, text: "today" });
  assert.deepEqual(
    items.map((i) => i.text),
    ["today"],
    "a tray app running past midnight drops the earlier day's text",
  );
  for (let n = 3; n < SESSION_LIMIT + 10; n++) {
    items = pushSession(items, { key: `s:${n}`, id: null, at: now + n, text: `t${n}` });
  }
  assert.equal(items.length, SESSION_LIMIT);
  assert.equal(items[0].text, `t${SESSION_LIMIT + 9}`);
});

test("a two-hour window says am or pm for each end", () => {
  assert.equal(formatWindow(0), "12–2 am");
  assert.equal(formatWindow(8), "8–10 am");
  assert.equal(formatWindow(10), "10 am–12 pm");
  assert.equal(formatWindow(12), "12–2 pm");
  assert.equal(formatWindow(22), "10 pm–12 am");
});

test("the longest streak counts consecutive days", () => {
  assert.equal(longestStreak({}), 0);
  assert.equal(
    longestStreak({ "2026-09-01": 5, "2026-09-02": 3, "2026-09-04": 1, "2026-09-05": 0 }),
    2,
  );
  assert.equal(longestStreak({ "2026-12-31": 1, "2027-01-01": 1, "2027-01-02": 1 }), 3);
});

test("a daylight-saving change does not break a streak", () => {
  const before = process.env.TZ;
  // New York moves its clocks on 8 March and 1 November 2026, so those days
  // are 23 and 25 hours long.
  process.env.TZ = "America/New_York";
  try {
    assert.equal(longestStreak({ "2026-03-07": 1, "2026-03-08": 1, "2026-03-09": 1 }), 3);
    assert.equal(longestStreak({ "2026-10-31": 1, "2026-11-01": 1, "2026-11-02": 1 }), 3);
  } finally {
    if (before === undefined) delete process.env.TZ;
    else process.env.TZ = before;
  }
});