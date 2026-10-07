// Harness for src/lib/notes/scratchpadMigration.ts. `pnpm test` builds and
// runs it; by hand:
//
//   node tools/notes-migration/build.mjs && node tools/notes-migration/run-migration-checks.mjs
//
// Exits non-zero when any check fails.
//
// The migration is the one piece of the Notes page whose failure mode is
// losing a user's writing, so it gets driven against a stub `createNote` that
// can fail on demand and a fake `localStorage` that can refuse a named key.

import { migrateScratchpad, scratchpadTitle } from "./bundle.mjs";
import { state, reset } from "./api-stub.js";

const SOURCE = "bs-scratchpad-v1";
const ARCHIVE = "bs-scratchpad-v1.migrated";

let store;
let failWritesTo = null;
globalThis.localStorage = {
  getItem: (k) => (k in store ? store[k] : null),
  setItem: (k, v) => {
    if (failWritesTo === k) throw new Error("quota");
    store[k] = v;
  },
  removeItem: (k) => {
    delete store[k];
  },
};

let failures = 0;
function check(label, cond, extra) {
  if (cond) {
    console.log(`  ok   ${label}`);
  } else {
    failures += 1;
    console.log(`  FAIL ${label}${extra ? ` :: ${extra}` : ""}`);
  }
}

const entries = [
  { id: "c", text: "Third note\nmore body", updatedAt: 3000 },
  { id: "a", text: "First note", updatedAt: 1000 },
  { id: "b", text: "", updatedAt: 2000 },
];
const RAW = JSON.stringify(entries);

// ---------------------------------------------------------------------------
console.log("1. a clean migration");
store = { [SOURCE]: RAW };
reset();
let r = await migrateScratchpad();
check("all three entries migrated", r.migrated === 3 && r.failed === 0, JSON.stringify(r));
check("not blocked", r.blocked === null);
check("source key removed", store[SOURCE] === undefined);
check("archive holds the ORIGINAL raw string, byte for byte", store[ARCHIVE] === RAW);
check(
  "oldest first",
  state.calls.map((c) => c.content).join("|") === "First note||Third note\nmore body",
  JSON.stringify(state.calls.map((c) => c.content)),
);
check(
  "timestamps preserved on both fields",
  state.calls.every((c, i) => c.createdAt === [1000, 2000, 3000][i] && c.updatedAt === c.createdAt),
  JSON.stringify(state.calls.map((c) => [c.createdAt, c.updatedAt])),
);
check(
  "title is the first non-blank line, body untouched",
  state.calls[2].title === "Third note" && state.calls[2].content === "Third note\nmore body",
);
check("empty entry travels rather than being judged", state.calls[1].content === "");

// ---------------------------------------------------------------------------
console.log("2. idempotency - a second call on the same store");
reset();
r = await migrateScratchpad();
check("nothing re-created", r.migrated === 0 && state.calls.length === 0);
check("archive untouched", store[ARCHIVE] === RAW);

// ---------------------------------------------------------------------------
console.log("3. a create fails partway - the tail survives and resumes");
store = { [SOURCE]: RAW };
reset({ failAfter: 2 });
r = await migrateScratchpad();
check("two landed, one reported still waiting", r.migrated === 2 && r.failed === 1, JSON.stringify(r));
check("source key still there, holding ONLY the un-migrated tail", store[SOURCE] !== undefined);
let tail = JSON.parse(store[SOURCE]);
check("the tail is exactly the newest entry", tail.length === 1 && tail[0].id === "c");
check("archive already written, and complete", store[ARCHIVE] === RAW);

console.log("   ...now the retry, with the DB back");
reset();
r = await migrateScratchpad();
check("the retry writes the one that was missing", r.migrated === 1 && r.failed === 0);
check(
  "and no duplicate of the two that landed",
  state.calls.length === 1 && state.calls[0].title === "Third note",
);
check("source key finally gone", store[SOURCE] === undefined);
check("archive STILL the full original, not the shrunken cursor", store[ARCHIVE] === RAW);

// ---------------------------------------------------------------------------
console.log("4. unparseable blob - archived, then cleared");
store = { [SOURCE]: "{not json" };
reset();
r = await migrateScratchpad();
check("nothing to import", r.migrated === 0 && r.failed === 0);
check("the bytes are archived anyway", store[ARCHIVE] === "{not json");
check("source key cleared so it stops being retried", store[SOURCE] === undefined);

// ---------------------------------------------------------------------------
console.log("5. the archive write fails - nothing is created, and it is REPORTED");
store = { [SOURCE]: RAW };
reset();
failWritesTo = ARCHIVE;
r = await migrateScratchpad();
failWritesTo = null;
check("no notes created without an archive behind them", r.migrated === 0 && state.calls.length === 0);
check("the caller is told the migration is stuck, not handed a silent no-op", r.blocked === "archive");
check("and how many are waiting", r.failed === 3, JSON.stringify(r));
check("source key intact", store[SOURCE] === RAW);

// ---------------------------------------------------------------------------
console.log("6. no scratchpad at all");
store = {};
reset();
r = await migrateScratchpad();
check("no-op", r.migrated === 0 && r.failed === 0 && store[ARCHIVE] === undefined);

// ---------------------------------------------------------------------------
console.log("7. title truncation");
const ascii = "x".repeat(200);
check("cut to 80", scratchpadTitle(ascii).length === 80);
check("short line untouched", scratchpadTitle("hi\nthere") === "hi");
check("leading blank lines skipped", scratchpadTitle("\n\n  real line  \nx") === "real line");
const emoji = "\u{1F600}".repeat(100); // 100 code points, 200 UTF-16 units
const cut = scratchpadTitle(emoji);
check(
  "an 80-char cut never splits a surrogate pair",
  Array.from(cut).length === 80 && !/[\uD800-\uDBFF]$/.test(cut),
  JSON.stringify(cut.slice(-4)),
);
const devanagari = "किताब ".repeat(40);
check("devanagari cut stays a valid string", Array.from(scratchpadTitle(devanagari)).length === 80);

// ---------------------------------------------------------------------------
// What the in-flight latch prevents. `+page.svelte` renders the active page
// from a `$derived`, so navigating away from Notes and back mounts the
// component again and re-runs `onMount` -> migrateScratchpad(). Without the
// latch, two concurrent walks advance the same cursor in lockstep and import
// every entry the first has not yet shifted a second time.
console.log("8. re-entrancy - two concurrent calls (the $derived re-mount)");
const six = Array.from({ length: 6 }, (_, i) => ({
  id: `e${i}`,
  text: `note ${i}`,
  updatedAt: 1000 + i,
}));
const SIX_RAW = JSON.stringify(six);

store = { [SOURCE]: SIX_RAW };
reset({ delayMs: 1 });
const [ra, rb] = await Promise.all([migrateScratchpad(), migrateScratchpad()]);
check("exactly six creates for six entries", state.calls.length === 6, `got ${state.calls.length}`);
check(
  "no content imported twice",
  // Distinct-count against the CALL count, not against 6: comparing to 6
  // passes at twelve calls that happen to cover six texts, which is exactly
  // the duplicate this scenario exists to catch.
  new Set(state.calls.map((c) => c.content)).size === state.calls.length,
  JSON.stringify(state.calls.map((c) => c.content)),
);
check("both callers see the same result object", ra === rb || ra.migrated === rb.migrated);
check("source key removed once", store[SOURCE] === undefined);
check("archive is the original six", store[ARCHIVE] === SIX_RAW);

console.log("   ...and the latch releases, so a later navigation can still retry");
store = { [SOURCE]: SIX_RAW };
reset({ delayMs: 0 });
r = await migrateScratchpad();
check("a fresh call after the first settled does real work", r.migrated === 6, JSON.stringify(r));

// ---------------------------------------------------------------------------
// A timestamp that is a `number` but not a usable one, passed straight to
// create_note, would fail the INSERT every time - stranding every LATER entry
// behind it forever, with the page promising a retry that could never
// succeed.
//
// What is worth testing here: `NaN` and `Infinity` cannot reach the
// guard at all. They are not valid JSON, so `JSON.stringify` writes them as
// `null` and `isEntry`'s `typeof === "number"` then rejects the entry before
// any of this runs. The reachable poison is a finite number that is not a safe
// integer — a huge one, or a fractional one — which round-trips through
// `localStorage` intact and lands on `create_note` as-is.
console.log("9. unusable timestamps do not strand the entries behind them");
const poisoned = [
  { id: "p", text: "huge", updatedAt: 1e300 },
  { id: "q", text: "negative huge", updatedAt: -1e300 },
  { id: "r", text: "fractional", updatedAt: 1234.5678 },
  { id: "s", text: "fine", updatedAt: 2000 },
];
store = { [SOURCE]: JSON.stringify(poisoned) };
reset();
r = await migrateScratchpad();
check("every entry lands", r.migrated === 4 && r.failed === 0, JSON.stringify(r));
check(
  "the unusable ones send no override, so the row takes the column default",
  state.calls.filter((c) => c.createdAt === undefined && c.updatedAt === undefined).length === 3,
  JSON.stringify(state.calls.map((c) => c.createdAt)),
);
check(
  "the good one keeps its own timestamp",
  state.calls.some((c) => c.content === "fine" && c.createdAt === 2000),
);
check("source key cleared", store[SOURCE] === undefined);

// ---------------------------------------------------------------------------
// A create that fails for THIS entry every time, not just once.
console.log("10. a permanently failing entry is reported, and the rest still land");
store = { [SOURCE]: JSON.stringify(six) };
reset({ failContent: ["note 2"] });
r = await migrateScratchpad();
check("the two before it landed", r.migrated === 2, JSON.stringify(r));
check("the rest are reported waiting", r.failed === 4);
tail = JSON.parse(store[SOURCE]);
check("the cursor points at the offender", tail.length === 4 && tail[0].text === "note 2");
check("archive intact", store[ARCHIVE] === SIX_RAW);

// ---------------------------------------------------------------------------
// The last entry is migrated but `removeItem` is refused. Without the "[]"
// write the key would still hold that final entry and the next load would
// re-import it.
console.log("11. a refused removeItem does not re-import the last entry");
store = { [SOURCE]: JSON.stringify([six[0]]) };
reset();
const realRemove = globalThis.localStorage.removeItem;
globalThis.localStorage.removeItem = () => {
  throw new Error("refused");
};
r = await migrateScratchpad();
globalThis.localStorage.removeItem = realRemove;
check("the one entry landed", r.migrated === 1 && r.failed === 0);
check("the key survives the refusal but holds an empty list", store[SOURCE] === "[]");
console.log("   ...and the next load finds genuinely no work");
reset();
r = await migrateScratchpad();
check("no re-import", r.migrated === 0 && state.calls.length === 0, JSON.stringify(r));

console.log(failures === 0 ? "\nALL CHECKS PASSED" : `\n${failures} CHECK(S) FAILED`);
process.exit(failures === 0 ? 0 : 1);
