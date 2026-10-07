// Unit tests for src/lib/learn.ts, the learner behind an edit on Home. Run
// with `pnpm test`.

import { test } from "node:test";
import assert from "node:assert/strict";
import { NEVER_LEARNED, learnPairs } from "../../src/lib/learn.ts";

/** The word for pen with its nukta spelled apart, which is how NFC writes
 * it, and with the precomposed letter. */
const PEN_APART = "\u{0915}\u{093C}लम";
const PEN_TOGETHER = "\u{0958}लम";
/** The word for surely, the same two ways. */
const SURELY_APART = "\u{091C}\u{093C}रूर";
const SURELY_TOGETHER = "\u{095B}रूर";

test("a misheard name corrected in place is learned", () => {
  assert.deepEqual(
    learnPairs(
      "Please forward the invoice to Sidharth before lunch",
      "Please forward the invoice to Siddharth before lunch",
    ),
    [{ from: "Sidharth", to: "Siddharth" }],
  );
});

test("a Devanagari word ending in a vowel sign, before a danda, is learned whole", () => {
  assert.deepEqual(learnPairs("मुझे यह किताब बहुत अछी।", "मुझे यह किताब बहुत अच्छी।"), [
    { from: "अछी", to: "अच्छी" },
  ]);
});

test("a Devanagari word with a virama or a nukta is learned", () => {
  assert.deepEqual(learnPairs("यह क्रिपया भेजो", "यह कृपया भेजो"), [
    { from: "क्रिपया", to: "कृपया" },
  ]);
  assert.deepEqual(learnPairs("मुझे कलम चाहिए", `मुझे ${PEN_APART} चाहिए`), [
    { from: "कलम", to: PEN_APART },
  ]);
});

test("punctuation outside ASCII is trimmed from the edges, and marks inside stay", () => {
  assert.deepEqual(learnPairs("Tell “Pria” now", "Tell “Priya” now"), [
    { from: "Pria", to: "Priya" },
  ]);
  assert.deepEqual(learnPairs("वह (सम्) है", "वह (सम्य) है"), [{ from: "सम्", to: "सम्य" }]);
  assert.deepEqual(learnPairs("देखो «Pria»", "देखो «Priya»"), [{ from: "Pria", to: "Priya" }]);
});

test("a word the dictionary holds in another spelling is not learned again", () => {
  const original = "मुझे कलम चाहिए";
  const edited = `मुझे ${PEN_APART} चाहिए`;
  assert.deepEqual(learnPairs(original, edited, { dictionary: [PEN_TOGETHER] }), []);
});

test("a word that already has a rule in another spelling is not learned again", () => {
  const original = `मैं ${SURELY_APART} आऊँगा`;
  const edited = "मैं जरूर आऊँगा";
  assert.deepEqual(learnPairs(original, edited), [{ from: SURELY_APART, to: "जरूर" }]);
  assert.deepEqual(
    learnPairs(original, edited, { existingReplacementFroms: [SURELY_TOGETHER] }),
    [],
  );
});

test("a homophone or function word fix is never learned, on either side", () => {
  const cases = [
    ["The company changed its logo today", "The company changed it's logo today"],
    ["The company changed it's logo today", "The company changed its logo today"],
    ["It is better then ever before", "It is better than ever before"],
    ["Thanks, your welcome to join", "Thanks, you're welcome to join"],
    ["Their going home early tonight", "They're going home early tonight"],
    ["We parked there car outside", "We parked their car outside"],
    ["I have two much work", "I have too much work"],
    ["Were going to the office", "We're going to the office"],
    ["Whose coming to dinner tonight", "Who's coming to dinner tonight"],
    ["I will loose the keys again", "I will lose the keys again"],
    ["The weather was nice either way", "The whether was nice either way"],
    ["Order the steal beams today", "Order the steel beams today"],
    ["Thank you for your patients today", "Thank you for your patience today"],
    ["Renew the license next month", "Renew the licence next month"],
    ["Ask the personal team about it", "Ask the personnel team about it"],
    ["We skipped desert after lunch", "We skipped dessert after lunch"],
  ];
  for (const [original, edited] of cases) {
    assert.deepEqual(learnPairs(original, edited), [], `${original} -> ${edited}`);
  }
});

test("a fix to a word on the list is not learned, though the heard word is not on it", () => {
  // A typo fixed to a homophone: learned, "thier" would become "their" in
  // every later sentence, including the ones that need "there".
  assert.deepEqual(learnPairs("We parked thier car outside", "We parked their car outside"), []);
});

test("a heard word on the list is not learned from, though the fix is not on it", () => {
  // A misheard name: learned, every later "right" would become "Wright".
  assert.deepEqual(learnPairs("Ask right to call me", "Ask Wright to call me"), []);
});

test("a curly apostrophe is the same word as a straight one on the never-learned list", () => {
  assert.deepEqual(learnPairs("They changed its logo", "They changed it\u{2019}s logo"), []);
  assert.deepEqual(learnPairs("Their going home early", "They\u{2019}re going home early"), []);
});

test("the never-learned list holds the pairs people fix for grammar", () => {
  for (const word of [
    "its", "it's", "then", "than", "your", "you're", "there", "their", "they're",
    "to", "too", "two", "whose", "who's", "were", "we're", "where", "lose", "loose",
  ]) {
    assert.ok(NEVER_LEARNED.has(word), word);
  }
  for (const word of NEVER_LEARNED) {
    assert.equal(word, word.toLowerCase().normalize("NFC"), `${word} is not in lookup form`);
  }
});

test("the never-learned list matches the field monitor's", () => {
  // src-tauri/src/learn/diff.rs pins the same length and the same two ends.
  const words = [...NEVER_LEARNED];
  assert.equal(words.length, 326);
  assert.equal(words[0], "a");
  assert.equal(words[words.length - 1], "herd");
  for (const word of ["meat", "dessert", "personnel", "scent", "cent", "practise", "herd"]) {
    assert.ok(NEVER_LEARNED.has(word), word);
  }
});

test("a fix that is not on the list still learns next to one that is", () => {
  assert.deepEqual(
    learnPairs("Ask Pria if its ready", "Ask Priya if it's ready"),
    [{ from: "Pria", to: "Priya" }],
  );
});

test("the heard word keeps the casing it was heard in, as the field monitor's pairs do", () => {
  assert.deepEqual(learnPairs("Recieve the files today", "Receive the files today"), [
    { from: "Recieve", to: "Receive" },
  ]);
  assert.deepEqual(learnPairs("please recieve the files", "please Receive the files"), [
    { from: "recieve", to: "Receive" },
  ]);
});

test("a case-only change, a rewrite and an edit that changes the word count teach nothing", () => {
  assert.deepEqual(learnPairs("ask sidharth now", "ask Sidharth now"), []);
  assert.deepEqual(learnPairs("send it now", "forward this today"), []);
  assert.deepEqual(learnPairs("ask Sid Harth now", "ask Siddharth now"), []);
});
