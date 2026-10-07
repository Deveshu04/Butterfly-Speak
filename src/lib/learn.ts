// Learns replacement rules from a dictation the user edited in the Home feed.
//
// The two texts are compared position by position, so only an edit that
// keeps the word count can teach anything. A changed word becomes a rule
// when it looks like the fix of a misheard word: the original is a plain
// word, both sides are long enough, neither is on NEVER_LEARNED, and the
// two are close in spelling. An edit that changes most of the words is a
// rewrite and teaches nothing.
//
// Words typed into other apps are learned from by the field monitor in
// src-tauri/src/learn, with the same thresholds and the same word list. Both
// report the heard word as it was heard and the fix as it was typed; the
// rules they write are applied by the same Rust code.

/**
 * Shortest corrected word learned, in code points of its folded form.
 * Same value as MIN_FIXED_CHARS in src-tauri/src/learn/diff.rs.
 */
const MIN_FIXED_CHARS = 4;

/** Shortest original word learned from, counted the same way. */
const MIN_HEARD_CHARS = 3;

/**
 * How far apart, as distanceRatio, an original and its correction may be.
 * Same value as MAX_DISTANCE in src-tauri/src/learn/diff.rs.
 */
const MAX_DISTANCE = 0.43;

/**
 * Changed words an edit may always have without being a rewrite.
 * Same value as REWRITE_FREE_WORDS in src-tauri/src/learn/diff.rs.
 */
const REWRITE_FREE_WORDS = 2;

/**
 * Beyond that, the share of the words, in percent, an edit may change and
 * still be a fix; exactly this share is still a fix.
 * Same value as REWRITE_PERCENT in src-tauri/src/learn/diff.rs.
 */
const REWRITE_PERCENT = 60;

/**
 * Words no edit teaches, on either side of the pair: English function words,
 * and the words people swap for grammar rather than because the speech was
 * misheard. A rule rewrites its heard word in every later dictation, and
 * which of these is right depends on the sentence ("its" or "it's", "then"
 * or "than"), so one fix must never decide it for every sentence after it.
 * Lowercase, with a straight apostrophe; `neverLearned` looks words up the
 * same way. The field monitor in src-tauri/src/learn/diff.rs rejects the
 * same words.
 */
export const NEVER_LEARNED: ReadonlySet<string> = new Set([
  // Articles, determiners and quantifiers.
  "a", "an", "the", "this", "that", "these", "those", "some", "any", "each",
  "every", "all", "both", "either", "neither", "such", "much", "many", "more",
  "most", "few", "less", "other", "another", "same", "own",
  // Pronouns and possessives.
  "i", "me", "my", "mine", "myself", "you", "your", "yours", "yourself",
  "yourselves", "he", "him", "his", "himself", "she", "her", "hers", "herself",
  "it", "its", "itself", "we", "us", "our", "ours", "ourselves", "they", "them",
  "their", "theirs", "themselves", "who", "whom", "whose", "what", "which",
  "one",
  // Contractions.
  "i'm", "i'd", "i'll", "i've", "you're", "you'd", "you'll", "you've", "he's",
  "he'd", "he'll", "she's", "she'd", "she'll", "it's", "it'd", "it'll",
  "we're", "we'd", "we'll", "we've", "they're", "they'd", "they'll", "they've",
  "that's", "there's", "here's", "who's", "what's", "where's", "let's",
  "isn't", "aren't", "wasn't", "weren't", "don't", "doesn't", "didn't",
  "won't", "wouldn't", "can't", "couldn't", "shouldn't", "hasn't", "haven't",
  "hadn't",
  // Auxiliary and modal verbs.
  "am", "is", "are", "was", "were", "be", "been", "being", "do", "does", "did",
  "have", "has", "had", "will", "would", "shall", "should", "can", "could",
  "may", "might", "must",
  // Prepositions.
  "of", "off", "to", "in", "into", "on", "onto", "at", "by", "for", "from",
  "with", "within", "without", "about", "above", "after", "against", "along",
  "among", "around", "before", "behind", "below", "beside", "between",
  "beyond", "during", "except", "inside", "near", "out", "outside", "over",
  "past", "since", "through", "till", "toward", "towards", "under", "until",
  "up", "upon", "via",
  // Conjunctions.
  "and", "but", "or", "nor", "so", "yet", "if", "as", "because", "although",
  "though", "unless", "whether", "while", "than",
  // Adverbs that work like function words.
  "not", "no", "yes", "then", "there", "here", "where", "when", "why", "how",
  "now", "too", "very", "just", "also", "only",
  // Homophones, fixed for their meaning rather than their sound.
  "two", "four", "won", "hour", "know", "knew", "new", "right", "write",
  "weather", "affect", "effect", "accept", "lose", "loose", "lead", "led",
  "passed", "buy", "bye", "whole", "hole", "piece", "peace", "quiet", "quite",
  "sight", "site", "cite", "threw", "weak", "week", "wait", "weight", "break",
  "brake", "allowed", "aloud", "hear", "wear", "principal", "principle",
  "complement", "compliment", "stationary", "stationery",
  "meet", "meat", "plain", "plane", "role", "roll", "peak", "peek", "steal",
  "steel", "real", "reel", "scene", "seen", "cell", "sell", "waist", "waste",
  "desert", "dessert", "advice", "advise", "device", "devise", "later",
  "latter", "breath", "breathe", "course", "coarse", "board", "bored",
  "great", "grate", "root", "route", "rain", "reign", "stair", "stare",
  "heal", "heel", "dear", "deer", "flour", "flower", "sent", "scent", "cent",
  "patience", "patients", "presence", "presents", "lessen", "lesson",
  "ensure", "insure", "farther", "further", "personal", "personnel",
  "council", "counsel", "practice", "practise", "licence", "license", "feat",
  "feet", "heard", "herd",
]);

/** Pairs one edit teaches when the caller does not say. */
const DEFAULT_MAX_PAIRS = 3;

/**
 * Everything at either end of a word that is not a letter, a number or a
 * combining mark. Marks count so that Indic vowel signs, nukta and virama
 * stay on their word while a danda or a quotation mark next to it goes. The
 * same test as `belongs_in_word` in src-tauri/src/learn/diff.rs.
 */
const EDGE_NON_WORD = /^[^\p{Alphabetic}\p{N}\p{M}]+|[^\p{Alphabetic}\p{N}\p{M}]+$/gu;

/** An original made only of letters and the combining marks that belong to
 * them, with the zero-width joiners some Indic spellings need between them. */
const LETTERS_ONLY = /^[\p{L}\p{M}\u{200C}\u{200D}]+$/u;

/** Apostrophes a keyboard or an autocorrect may type in a contraction. */
const APOSTROPHES = /[\u{2018}\u{2019}\u{02BC}]/gu;

export interface LearnPair {
  /** The original word as it was heard, edge punctuation aside. */
  from: string;
  /** The word the user typed in its place, as typed. */
  to: string;
}

export interface LearnPairsOptions {
  /** `from` of every rule the user already has. */
  existingReplacementFroms?: Iterable<string>;
  /** The user's vocabulary: a correction to one of these is not learned. */
  dictionary?: Iterable<string>;
  /** The most pairs to return. */
  maxPairs?: number;
}

/** The pairs of words the edit from `original` to `edited` teaches. */
export function learnPairs(
  original: string,
  edited: string,
  options: LearnPairsOptions = {},
): LearnPair[] {
  const before = original.split(/\s+/).filter((w) => w !== "");
  const after = edited.split(/\s+/).filter((w) => w !== "");
  if (before.length === 0 || before.length !== after.length) return [];

  const heard = before.map(trimEdges);
  const typed = after.map(trimEdges);
  const changed = heard.filter((word, i) => fold(word) !== fold(typed[i])).length;
  if (isRewrite(heard.length, changed)) return [];

  const ruled = new Set(Array.from(options.existingReplacementFroms ?? [], fold));
  const known = new Set(Array.from(options.dictionary ?? [], fold));
  const limit = options.maxPairs ?? DEFAULT_MAX_PAIRS;

  const pairs: LearnPair[] = [];
  for (let i = 0; i < heard.length && pairs.length < limit; i++) {
    const from = heard[i];
    const to = typed[i];
    const key = fold(from);
    if (
      to === "" ||
      key === fold(to) ||
      codePoints(from) < MIN_HEARD_CHARS ||
      codePoints(to) < MIN_FIXED_CHARS ||
      !LETTERS_ONLY.test(from) ||
      neverLearned(from) ||
      neverLearned(to) ||
      ruled.has(key) ||
      known.has(fold(to)) ||
      distanceRatio(from, to) > MAX_DISTANCE
    ) {
      continue;
    }
    ruled.add(key);
    pairs.push({ from, to });
  }
  return pairs;
}

function trimEdges(word: string): string {
  return word.replace(EDGE_NON_WORD, "");
}

function neverLearned(word: string): boolean {
  return NEVER_LEARNED.has(fold(word).replace(APOSTROPHES, "'"));
}

/** Canonical composition, then lowercase: one key for one word. */
function fold(word: string): string {
  return word.normalize("NFC").toLowerCase().normalize("NFC");
}

function codePoints(word: string): number {
  return Array.from(fold(word)).length;
}

function isRewrite(words: number, changed: number): boolean {
  return changed > REWRITE_FREE_WORDS && changed * 100 > REWRITE_PERCENT * words;
}

/**
 * Edit distance between two words in code points, scaled by the longer one
 * to 0..1. Both terms use the same folded strings, so a lowercase mapping
 * that changes a word's length cannot push the ratio past 1. Two empty words
 * are 0 apart.
 */
function distanceRatio(a: string, b: string): number {
  const x = Array.from(fold(a));
  const y = Array.from(fold(b));
  const longer = Math.max(x.length, y.length);
  return longer === 0 ? 0 : editDistance(x, y) / longer;
}

function editDistance(a: string[], b: string[]): number {
  const row = Array.from({ length: b.length + 1 }, (_, j) => j);
  for (let i = 0; i < a.length; i++) {
    let diagonal = row[0];
    row[0] = i + 1;
    for (let j = 1; j <= b.length; j++) {
      const above = row[j];
      row[j] = Math.min(diagonal + (a[i] === b[j - 1] ? 0 : 1), above + 1, row[j - 1] + 1);
      diagonal = above;
    }
  }
  return row[b.length];
}
