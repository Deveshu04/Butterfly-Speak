//! Capitalisation (truecasing) precision/recall/F1, plus sentence-level
//! accuracy for dictation.
//!
//! # What this measures, and what it deliberately excludes
//!
//! [`score`] aligns reference and hypothesis tokens **case-insensitively**
//! — a Levenshtein alignment over lowercased tokens, the same DP shape as
//! `eval::per` and `eval::rawer`, just with a different equality test — and
//! then, for every aligned pair whose lowercase forms match, asks one
//! binary question: is the reference token's original form "not
//! all-lowercase" (the positive class — proper nouns, acronyms,
//! sentence-initial capitals), and did the hypothesis get that right?
//!
//! Token pairs that align to each other but are NOT the same word
//! case-insensitively (a genuine content substitution), plus insertions and
//! deletions, are excluded from precision/recall entirely — there is no
//! casing question to ask about a word with no counterpart on the other
//! side. That is [`crate::eval::rawer::content_wer`]'s job, not this
//! module's: a formatter that swaps "Sarvam" for "great" should show up as
//! a content error, not get scored as a missed capital. See
//! `a_substituted_content_word_is_not_counted_as_a_missed_capital` for the
//! pinned example.
//!
//! # Caseless scripts are excluded, not scored perfect
//!
//! Devanagari, Arabic, Han, Kana, Thai and most other non-bicameral scripts
//! have no case distinction at all. A token written entirely in one of them
//! cannot be miscapitalised, so it is not evidence that a formatter cases
//! well — and it must not be allowed to look like evidence.
//!
//! This is not a theoretical concern for this corpus. **200 of the 1 000
//! committed fixture cases are `lang: "hi"`** (`indic-diarbench.jsonl`), and
//! under the original implementation every caseless token landed in the
//! "reference lowercase, hypothesis lowercase" bucket, driving both
//! denominators to zero and both ratios to a free `1.0`. Averaged across a
//! corpus that is a fifth of the score handed over for nothing.
//!
//! So [`score`] partitions matched pairs by [`is_caseable`] — does either
//! side contain a character with a case distinction at all — and:
//!
//! - caseable pairs go to `true_positives`/`false_positives`/
//!   `false_negatives`/`true_negatives`, the four counts precision and
//!   recall are computed from;
//! - caseless pairs that are still *words* (they have letters, in a script
//!   with no case: Devanagari, Han, Thai …) go to
//!   [`CasingCounts::caseless_tokens`], a separate field that appears in the
//!   output and enters no ratio;
//! - pairs with no letters at all (`2022`, `28%`, `..`) go nowhere. They are
//!   not a casing decision and they are not a caseless script; see
//!   [`has_letter`] for why counting them as the latter overstated the
//!   caseless figure on English text.
//!
//! When a sentence has no caseable tokens at all, both denominators really
//! are zero, and [`CasingScore::precision`] / [`recall`](CasingScore::recall)
//! / [`f1`](CasingScore::f1) are **`None`** — "there was nothing to measure",
//! which is a different claim from `1.0` and cannot be averaged into a
//! corpus figure by accident. `caseless_tokens` says how much text that
//! silence covers.
//!
//! `None` means *only* that. A hypothesis that gets every capital wrong in
//! one direction — inventing capitals the reference does not have, or
//! missing every capital it does — leaves one of precision and recall
//! undefined but has a perfectly well-defined F1 of **0.0**, and
//! [`CasingCounts::f1`] returns it. See that method for why the earlier
//! `precision()? , recall()?` spelling reported those failures as "n/a".
//!
//! Note the caseless rule is *token*-level, not per-language, which is
//! stricter than switching on `Case::lang`: the committed Hindi fixture is
//! not purely Devanagari. Of its 3 573 target tokens, **463 are caseable**
//! — Latin glosses like `आई-क्यू(IQ)` and `इंटरव्यू interview` — and 111 of
//! those carry an uppercase character, spread over 56 of its 200 cases.
//! Those really are casing decisions and they really are scored; the other
//! 352 are caseable too and sit in the negative class. Set aside are the
//! **3 107** genuinely caseless tokens; a further 3 have no letters and are
//! dropped entirely.
//!
//! # Corpus aggregation: MICRO, from the counts
//!
//! A corpus-level F1 is computed by summing [`CasingCounts`] across every
//! pair and taking **one** precision/recall/F1 at the end. That is the
//! *micro* average, and it is the form the published truecasing baselines
//! below are quoted in.
//!
//! Averaging per-sentence F1 (the *macro* average) is a different number,
//! not comparable to those baselines, and it is the same trap as a mean of
//! per-sentence PER — see [`crate::eval::per::PunctCounts`]'s type-level
//! doc, which carries the identical warning. `CasingCounts` implements
//! [`Add`](std::ops::Add), [`AddAssign`](std::ops::AddAssign) and
//! [`Sum`](std::iter::Sum) so the correct formula is also the short one:
//!
//! ```ignore
//! pairs.iter()
//!     .map(|(r, h)| casing::score(r, h).counts)
//!     .sum::<CasingCounts>()
//!     .score()
//! ```
//!
//! `micro_and_macro_f1_are_different_numbers` proves the two formulas land
//! somewhere different on real counts, so this is a live distinction rather
//! than a pedantic one.
//!
//! # Baselines (English, clean prose)
//!
//! Stanford CoreNLP's truecaser reports token-level F1 of 90.89 for its CRF
//! model and 93.19 for LSTM-LARGE. A score below ~90 on clean read speech
//! means the formatter is doing worse than a 2016 CRF baseline. Compare
//! **micro** F1 over English cases only against those figures; a corpus
//! total that folded in the Hindi fixture would not be measuring the same
//! thing they measured.
//!
//! # `sentence_accuracy`
//!
//! Per-token F1 is not what a dictation user experiences — they read whole
//! sentences, and one miscapitalised word in an otherwise-perfect sentence
//! reads as "it got this wrong", not "it got 90% right". `sentence_accuracy`
//! is the fraction of sentences where EVERY token matched exactly (same
//! word, same case) — a strictly harder, more user-facing number than
//! per-token F1, included alongside it rather than instead of it.
//!
//! Sentence boundaries are a property of the **reference only**, so the
//! denominator cannot be moved by the hypothesis: `sentences_total` counts
//! reference sentences, and hypothesis-only tokens (insertions) fail the
//! reference sentence they fall in rather than opening one of their own.
//! See [`sentence_counts`].
//!
//! Boundaries are detected by `SENTENCE_TERMINATORS` — ASCII `.`/`!`/`?`,
//! the CJK full-width equivalents, and the Devanagari danda/double-danda
//! used across several Brahmic scripts this app ships (see `eval::per`'s
//! `HI_MARKS` for the same concern in punctuation scoring) — with an
//! abbreviation exception described on [`ends_sentence`]. A language whose
//! sentence-final punctuation isn't in that list still gets scored correctly
//! at the token level; the whole text just never splits into more than one
//! "sentence", which only ever makes `sentence_accuracy` a stricter number,
//! never a wrong one — one bad token anywhere still fails the single
//! sentence it would have failed as a fragment.

// Exercised only by this file's own `#[cfg(test)]` module as far as the lib
// crate is concerned, same situation as `eval::per` and `eval::rawer` — see
// `rawer` for why `fmtbench` calling into this tree does not lift the allow.
#![allow(dead_code)]

/// The positive class: a token whose reference form is not all-lowercase.
/// True whenever the token contains at least one Unicode uppercase letter;
/// attached punctuation and digits are never uppercase, so they don't
/// perturb the classification either way.
fn is_cased(token: &str) -> bool {
    token.chars().any(char::is_uppercase)
}

/// True when the token contains at least one character that *has* a case
/// distinction — i.e. it is written in a bicameral script and a
/// capitalisation question can meaningfully be asked about it.
///
/// False for pure Devanagari/Arabic/Han/Kana/Thai text, for digits, and for
/// punctuation. See the module doc for why those are excluded from the
/// precision/recall denominators rather than scored as effortless
/// successes.
///
/// [`char::is_lowercase`] and [`char::is_uppercase`] are the Unicode
/// `Lowercase`/`Uppercase` derived properties, so this is a script-agnostic
/// test rather than a hard-coded block list: Cyrillic, Greek, Armenian and
/// Deseret are caseable by the same rule that admits Latin, and any script
/// Unicode later gives case to becomes caseable without a code change.
fn is_caseable(token: &str) -> bool {
    token.chars().any(|c| c.is_lowercase() || c.is_uppercase())
}

/// True when the token contains at least one letter, in any script.
///
/// This is what separates "written in a caseless script" from "not written
/// in a script at all". `2022`, `28%`, `$1.60` and `..` are not Devanagari,
/// Han or Thai — they are tokens with no letters, and counting them as
/// evidence of caseless-script coverage overstated
/// [`CasingCounts::caseless_tokens`] on English text, where they are the
/// only thing that field ever caught: measured on the committed corpus, 135
/// of the 138 letterless target tokens are in **English** fixtures (`2022`,
/// `30`, `28%`, `11,003`, `1191?` … — 82 in `earnings22`, 36 in
/// `earnings22-subset10`, 17 in `disflqa`, none in `librispeech-pc`) and
/// only 3 are in the Hindi one. A report that printed "N caseless tokens set
/// aside" beside an English score was therefore quoting a number about
/// digits.
///
/// [`char::is_alphabetic`] is the Unicode `Alphabetic` property, which is
/// true for Devanagari consonants and vowels, Han, Kana, Thai and Arabic
/// letters — so every real word in a caseless script still qualifies. It is
/// false for the combining marks
/// [`crate::eval::rawer::normalize_token`] documents at length, but a token
/// made *only* of combining marks is not a word in any script and does not
/// occur in the corpus.
fn has_letter(token: &str) -> bool {
    token.chars().any(char::is_alphabetic)
}

/// Characters that close a "sentence" for
/// [`CasingScore::sentence_accuracy`] purposes. See the module doc for why
/// this list, and why an unlisted terminator only ever makes the metric
/// stricter, not wrong.
const SENTENCE_TERMINATORS: &[char] = &[
    '.', '!', '?', '\u{3002}', // 。 IDEOGRAPHIC FULL STOP
    '\u{FF01}', // ！ FULLWIDTH EXCLAMATION MARK
    '\u{FF1F}', // ？ FULLWIDTH QUESTION MARK
    '\u{0964}', // । DEVANAGARI DANDA
    '\u{0965}', // ॥ DEVANAGARI DOUBLE DANDA
];

/// True when a token's trailing `.` is part of an abbreviation rather than
/// a sentence end, so [`ends_sentence`] does not split there.
///
/// # Why this exists
///
/// Over-splitting makes `sentence_accuracy` **more lenient**, which is the
/// opposite of the metric's purpose. Treat `Dr. Meyer arrived.` as two
/// sentences and a mistake in `Meyer` costs 1 of 2 rather than 1 of 1: the
/// formatter is rewarded for the abbreviation it happened to be near.
///
/// # The rule, and what it deliberately does not do
///
/// Two shapes, both decidable from the token alone with no word list:
///
/// - a single **uppercase** letter plus a period — `J.`, `R.` — i.e. an
///   initial;
/// - a period **between two letters** — `U.S.`, `e.g.`, `a.m.`.
///
/// The second condition is specifically *between*, not merely "contains a
/// period", because a trailing ellipsis written flush against its word is a
/// real sentence end and must keep splitting. That is not hypothetical: **24
/// committed fixture tokens end in two or more dots** (`बहुत...`, `जाता..`,
/// `जॉइंट(joint)...`), zero are abbreviations, and a `contains('.')` test
/// would have silently stopped splitting every one of them.
///
/// # Why letters on both sides, and why uppercase only
///
/// Each condition is as narrow as it is so that two shapes that really do
/// end sentences keep splitting:
///
/// - `1.5m.`, `2.4GHz.`, `$1.5m.` — a decimal followed by a unit. A period
///   between two *alphanumerics* would match `1`·`.`·`5`. No English
///   abbreviation has a digit next to its internal period, so requiring
///   letters on both sides costs nothing. (The bare `11.2%.` shape, which the
///   corpus contains five times, is safe either way: its core holds no
///   letter at all and returns early.)
/// - `a.`, `i.` — a lowercase single letter. An initial is always
///   uppercase, so accepting lowercase would only misfire: "the word starts
///   with the letter a." is a sentence end, never `A. Smith`.
///
/// What is left is genuinely ambiguous from the token alone: `Exhibit A.`
/// ends a sentence, `J. Smith` does not, and both are one uppercase letter
/// plus a period. It is resolved toward **not** splitting, because that is
/// the non-flattering direction — under-splitting merges two sentences and
/// makes `sentence_accuracy` stricter or unchanged, while over-splitting
/// hands out partial credit (see the top of this doc).
///
/// # No word list
///
/// `Mr.`, `Mrs.`, `Dr.`, `etc.` and friends still over-split. That is a
/// known gap, chosen over a list that would be permanently incomplete while
/// reading as coverage. Its cost is measured: scanning all 800 English
/// cases for a mid-sentence token from the usual abbreviation inventory
/// (`Mr. Mrs. Ms. Dr. St. Jr. Sr. Prof. vs. etc. Inc. Ltd. Co. No. U.S.
/// e.g. i.e. a.m. p.m.` …) finds exactly **one**, `Mr.` in
/// `earnings22-subset10-0013` ("… Group Chief Financial Officer Mr. Sean
/// Capazorio."), which splits one reference sentence into two and is the
/// lenient direction. (A second `Mr.` in `earnings22-1819` is the target's
/// final token, where splitting changes nothing.) Both rules below remain
/// no-ops on today's fixtures — **zero** of the 1 000 targets contain a
/// single-letter-plus-period token or a letter·`.`·letter token — and exist
/// for the prose a real user dictates.
fn is_abbreviation_period(token: &str) -> bool {
    let Some(core) = token.strip_suffix('.') else {
        return false;
    };
    // "3.", "11.2%." and "..." are sentence ends, not abbreviations.
    if !core.chars().any(char::is_alphabetic) {
        return false;
    }
    let mut chars = core.chars();
    if chars.next().is_some_and(char::is_uppercase) && chars.next().is_none() {
        return true; // an initial: "J."
    }
    // A period with a LETTER on both sides: "U.S.", "e.g.". Not a trailing
    // ellipsis (whose dots are adjacent to each other) and not a decimal
    // with a unit glued on ("1.5m.").
    let cs: Vec<char> = core.chars().collect();
    cs.windows(3)
        .any(|w| w[1] == '.' && w[0].is_alphabetic() && w[2].is_alphabetic())
}

/// True when this reference token closes a sentence: its last character is
/// in [`SENTENCE_TERMINATORS`] and it is not an abbreviation's period (see
/// [`is_abbreviation_period`]).
///
/// A terminator hidden behind a closing quote or bracket (`said."`) is
/// *not* recognised, which under-splits and so only makes the metric
/// stricter. Measured: zero such tokens in all 1 000 committed fixture
/// targets, so nothing on this corpus depends on it.
fn ends_sentence(token: &str) -> bool {
    let Some(last) = token.chars().last() else {
        return false;
    };
    if !SENTENCE_TERMINATORS.contains(&last) {
        return false;
    }
    !(last == '.' && is_abbreviation_period(token))
}

/// One step of a case-insensitive token alignment. `Matched` pairs are the
/// only ones a casing comparison is meaningful for; the other three
/// variants exist so [`sentence_counts`] can still see — and fail — a
/// sentence that a plain precision/recall pass over `Matched` pairs alone
/// would silently skip past.
enum Aligned<'a> {
    /// Lowercase forms equal; casing may still differ.
    Matched(&'a str, &'a str),
    /// Aligned to each other by the DP (nothing cheaper), but different
    /// words even ignoring case — a content error, not a casing one.
    Substituted(&'a str, &'a str),
    /// Present only in the hypothesis.
    Inserted(&'a str),
    /// Present only in the reference.
    Deleted(&'a str),
}

#[derive(Clone, Copy, PartialEq)]
enum Op {
    Match,
    Sub,
    Ins,
    Del,
}

/// Case-insensitive Levenshtein alignment between two token sequences.
/// Same DP-plus-backtrace shape as `eval::per::punctuation_error_rate`,
/// with the match test changed from string equality to
/// lowercase-form equality.
fn align<'a>(reference: &[&'a str], hypothesis: &[&'a str]) -> Vec<Aligned<'a>> {
    let (rn, hn) = (reference.len(), hypothesis.len());
    let ref_lower: Vec<String> = reference.iter().map(|t| t.to_lowercase()).collect();
    let hyp_lower: Vec<String> = hypothesis.iter().map(|t| t.to_lowercase()).collect();

    let mut cost = vec![vec![0usize; hn + 1]; rn + 1];
    let mut back = vec![vec![Op::Match; hn + 1]; rn + 1];
    for i in 1..=rn {
        cost[i][0] = i;
        back[i][0] = Op::Del;
    }
    for j in 1..=hn {
        cost[0][j] = j;
        back[0][j] = Op::Ins;
    }
    for i in 1..=rn {
        for j in 1..=hn {
            if ref_lower[i - 1] == hyp_lower[j - 1] {
                cost[i][j] = cost[i - 1][j - 1];
                back[i][j] = Op::Match;
            } else {
                let sub = cost[i - 1][j - 1] + 1;
                let ins = cost[i][j - 1] + 1;
                let del = cost[i - 1][j] + 1;
                let best = sub.min(ins).min(del);
                cost[i][j] = best;
                back[i][j] = if best == sub {
                    Op::Sub
                } else if best == ins {
                    Op::Ins
                } else {
                    Op::Del
                };
            }
        }
    }

    let mut steps = Vec::with_capacity(rn.max(hn));
    let (mut i, mut j) = (rn, hn);
    while i > 0 || j > 0 {
        match back[i][j] {
            Op::Match => {
                steps.push(Aligned::Matched(reference[i - 1], hypothesis[j - 1]));
                i -= 1;
                j -= 1;
            }
            Op::Sub => {
                steps.push(Aligned::Substituted(reference[i - 1], hypothesis[j - 1]));
                i -= 1;
                j -= 1;
            }
            Op::Ins => {
                steps.push(Aligned::Inserted(hypothesis[j - 1]));
                j -= 1;
            }
            Op::Del => {
                steps.push(Aligned::Deleted(reference[i - 1]));
                i -= 1;
            }
        }
    }
    steps.reverse();
    steps
}

/// `Some(numerator / denominator)`, or `None` when the denominator is zero.
///
/// A zero denominator means *nothing was measured*: no caseable token the
/// hypothesis capitalised (for precision), or none the reference
/// capitalised (for recall). The old implementation returned `1.0` there on
/// the argument that "no wrong guesses were possible" — true, but it made a
/// never-tested sentence indistinguishable from a verified-correct one, and
/// 200 of the 1 000 committed fixture cases are caseless Hindi that
/// collected that free `1.0` on every field.
///
/// `None` cannot be averaged into a corpus figure without the caller
/// deciding what to do about it, which is the whole point. The corpus
/// number that *is* meaningful is micro — sum [`CasingCounts`], then take
/// one ratio — and there a caseless sentence contributes exactly what it
/// should: nothing.
///
/// Contrast `eval::per::PunctCounts::rate`, which returns `0.0` for its own
/// `0/0` and documents that callers must check the counts first. This type
/// makes that check unavoidable instead of advisory.
///
/// **Not used by [`CasingCounts::f1`].** F1 has its own denominator
/// (`2·TP + FP + FN`), which is empty in strictly fewer situations than
/// precision's or recall's; routing it through this helper — or through
/// `precision()?` and `recall()?` — reported real zero scores as "nothing
/// measured". See that method.
fn ratio(numerator: u32, denominator: u32) -> Option<f64> {
    if denominator == 0 {
        None
    } else {
        Some(f64::from(numerator) / f64::from(denominator))
    }
}

/// The raw counts one (reference, hypothesis) pair contributes. **Sum these
/// across a corpus and call [`score`](CasingCounts::score) once** — see the
/// module doc on micro vs macro, and `eval::per::PunctCounts` for the same
/// warning in the punctuation metric.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct CasingCounts {
    /// Caseable token the reference capitalised and the hypothesis also
    /// capitalised.
    pub true_positives: u32,
    /// Caseable token the hypothesis capitalised but the reference did not.
    pub false_positives: u32,
    /// Caseable token the reference capitalised but the hypothesis did not.
    pub false_negatives: u32,
    /// Caseable token both sides left all-lowercase. Enters no ratio (F1 of
    /// a positive class never uses true negatives) but is reported so
    /// "0 positives out of 400 caseable tokens" is distinguishable from
    /// "0 positives out of 3 caseable tokens".
    pub true_negatives: u32,
    /// Aligned token pairs written entirely in a caseless **script** — at
    /// least one letter, none of them cased, so no capitalisation question
    /// can be asked. Held separately and excluded from every denominator;
    /// see the module doc.
    ///
    /// Tokens with no letters at all (`2022`, `28%`, `..`) are **not**
    /// counted here and are not counted anywhere else either: they are not a
    /// caseless script, they are not a casing decision, and booking them
    /// here made this field read as Devanagari coverage on English text. See
    /// [`has_letter`].
    pub caseless_tokens: u32,
    /// Sentences in the **reference**. See [`sentence_counts`].
    pub sentences_total: u32,
    /// Reference sentences the hypothesis reproduced token-for-token,
    /// casing included.
    pub sentences_matched: u32,
}

impl CasingCounts {
    /// Of the caseable tokens the hypothesis capitalised, the fraction the
    /// reference also capitalised. `None` when the hypothesis capitalised
    /// nothing caseable.
    pub fn precision(&self) -> Option<f64> {
        ratio(self.true_positives, self.true_positives + self.false_positives)
    }

    /// Of the caseable tokens the reference capitalised, the fraction the
    /// hypothesis also capitalised. `None` when the reference capitalised
    /// nothing caseable.
    pub fn recall(&self) -> Option<f64> {
        ratio(self.true_positives, self.true_positives + self.false_negatives)
    }

    /// `2·TP / (2·TP + FP + FN)` — the harmonic mean of
    /// [`precision`](Self::precision) and [`recall`](Self::recall), computed
    /// straight from the counts. `None` only when **all three** counts are
    /// zero.
    ///
    /// # Why not `precision()? , recall()?`
    ///
    /// Because that spelling conflates two different facts. F1's denominator
    /// is `2·TP + FP + FN`, which is zero in exactly one situation: nothing
    /// was predicted and nothing was there to find. Every other situation
    /// has a defined F1, *including* the one-sided total failures where one
    /// of precision and recall is undefined on its own:
    ///
    /// - a hypothesis that sprinkles capitals into an all-lowercase
    ///   reference (`TP 0, FP n, FN 0`) has `recall == None` and
    ///   `F1 == Some(0.0)`;
    /// - a hypothesis that misses every capital and invents none
    ///   (`TP 0, FP 0, FN n`) has `precision == None` and `F1 == Some(0.0)`.
    ///
    /// Deriving F1 through `?` reported both as `None`, and `None` renders
    /// as "n/a" in the benchmark report — so a total failure read as *"we
    /// could not measure this"* rather than *"it scored zero"*. That is the
    /// flattering direction, which is the one direction these metrics must
    /// not fail in. `None` here now means only "the denominator is genuinely
    /// empty".
    ///
    /// [`precision`](Self::precision) and [`recall`](Self::recall) are NOT
    /// affected by the same problem and deliberately keep returning `None`:
    /// `TP + FP == 0` really does mean the hypothesis made no positive
    /// prediction to be right or wrong about, and `TP + FN == 0` really does
    /// mean the reference held nothing to find. Those are empty
    /// denominators, not zero scores.
    pub fn f1(&self) -> Option<f64> {
        if self.true_positives == 0 && self.false_positives == 0 && self.false_negatives == 0 {
            return None;
        }
        let tp = f64::from(self.true_positives);
        let denominator = 2.0 * tp + f64::from(self.false_positives) + f64::from(self.false_negatives);
        Some(2.0 * tp / denominator)
    }

    /// Fraction of reference sentences reproduced exactly. `None` when the
    /// reference has no sentences at all (empty reference).
    pub fn sentence_accuracy(&self) -> Option<f64> {
        ratio(self.sentences_matched, self.sentences_total)
    }

    /// Bundles the four derived ratios alongside `self`. Every field of
    /// [`CasingScore`] comes from here, so the ratios and the counts cannot
    /// drift apart.
    pub fn score(self) -> CasingScore {
        CasingScore {
            precision: self.precision(),
            recall: self.recall(),
            f1: self.f1(),
            sentence_accuracy: self.sentence_accuracy(),
            counts: self,
        }
    }
}

impl std::ops::Add for CasingCounts {
    type Output = CasingCounts;

    fn add(self, other: CasingCounts) -> CasingCounts {
        CasingCounts {
            true_positives: self.true_positives + other.true_positives,
            false_positives: self.false_positives + other.false_positives,
            false_negatives: self.false_negatives + other.false_negatives,
            true_negatives: self.true_negatives + other.true_negatives,
            caseless_tokens: self.caseless_tokens + other.caseless_tokens,
            sentences_total: self.sentences_total + other.sentences_total,
            sentences_matched: self.sentences_matched + other.sentences_matched,
        }
    }
}

impl std::ops::AddAssign for CasingCounts {
    fn add_assign(&mut self, other: CasingCounts) {
        *self = *self + other;
    }
}

impl std::iter::Sum for CasingCounts {
    fn sum<I: Iterator<Item = CasingCounts>>(iter: I) -> CasingCounts {
        iter.fold(CasingCounts::default(), std::ops::Add::add)
    }
}

/// Precision, recall, F1 and sentence-level accuracy of capitalisation
/// recovery, plus the [`CasingCounts`] they were derived from.
///
/// Every ratio is [`Option`]: `None` means *that field's own* denominator
/// was zero, i.e. nothing of that kind was measured — **not** that the
/// hypothesis was perfect, and **not** that it scored zero. The three
/// denominators are different, so the fields go `None` at different times:
/// F1 survives one-sided total failures that leave precision or recall
/// undefined. See [`ratio`], [`CasingCounts::f1`] and the module doc.
///
/// For a corpus, do not average these fields. Sum `counts` and call
/// [`CasingCounts::score`] once.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CasingScore {
    /// Of the caseable tokens the hypothesis cased, the fraction whose
    /// reference form was also cased.
    pub precision: Option<f64>,
    /// Of the caseable tokens the reference cased, the fraction the
    /// hypothesis also cased.
    pub recall: Option<f64>,
    /// `2·TP / (2·TP + FP + FN)`; `None` only when all three are zero — see
    /// [`CasingCounts::f1`], which is defined in cases where precision or
    /// recall is not.
    pub f1: Option<f64>,
    /// Fraction of **reference** sentences where every token matched
    /// exactly (same word, same case). See the module doc.
    pub sentence_accuracy: Option<f64>,
    /// The counts every field above was derived from. This is what a corpus
    /// aggregate must be built out of.
    pub counts: CasingCounts,
}

/// Scores `hypothesis`'s capitalisation against `reference`. See the module
/// doc for the alignment rule, the positive-class definition, the caseless
/// exclusion, and what is and is not in scope.
pub fn score(reference: &str, hypothesis: &str) -> CasingScore {
    let r_tokens: Vec<&str> = reference.split_whitespace().collect();
    let h_tokens: Vec<&str> = hypothesis.split_whitespace().collect();
    let steps = align(&r_tokens, &h_tokens);

    let mut counts = CasingCounts::default();
    for step in &steps {
        if let Aligned::Matched(r, h) = step {
            // Either side, not just the reference: if the hypothesis
            // somehow introduced a cased character into a caseless word,
            // that is a casing decision and it should be scored, not filed
            // under "no case distinction exists".
            if !is_caseable(r) && !is_caseable(h) {
                // A caseless *word* is evidence of caseless-script coverage
                // and is reported as such. A token with no letters at all —
                // `2022`, `28%`, `..` — is neither scored nor reported: it
                // is not a capitalisation decision and it is not a caseless
                // script either. See [`has_letter`].
                if has_letter(r) || has_letter(h) {
                    counts.caseless_tokens += 1;
                }
                continue;
            }
            match (is_cased(r), is_cased(h)) {
                (true, true) => counts.true_positives += 1,
                (false, true) => counts.false_positives += 1,
                (true, false) => counts.false_negatives += 1,
                (false, false) => counts.true_negatives += 1,
            }
        }
    }

    let (total, matched) = sentence_counts(&steps);
    counts.sentences_total = total;
    counts.sentences_matched = matched;
    counts.score()
}

/// `(reference sentences, reference sentences the hypothesis reproduced
/// exactly)`.
///
/// A sentence is reproduced exactly when every alignment step inside it is
/// an [`Aligned::Matched`] pair with byte-identical tokens. Any
/// [`Aligned::Substituted`], [`Aligned::Inserted`] or [`Aligned::Deleted`]
/// step fails the sentence it falls inside, even though it plays no part in
/// precision/recall — a dropped or inserted word is exactly what "every
/// token matched" means to rule out, even though it is not itself a casing
/// error.
///
/// # The denominator is the reference's, not the hypothesis's
///
/// Sentences are delimited by reference tokens ([`ends_sentence`]), and
/// **only** reference tokens create them. Insertions have no reference
/// counterpart, so they are charged to a reference sentence rather than
/// opening one:
///
/// - inside an open sentence → that sentence fails;
/// - after the previous sentence closed (a trailing insertion) → the
///   sentence that just closed fails, because the extra words are what
///   spoiled it;
/// - before any reference token at all → the first sentence fails, once one
///   exists.
///
/// The earlier implementation let a trailing insertion open a sentence of
/// its own, so `sentences_total` moved with the hypothesis: reference
/// `"A. B."` against hypothesis `"A. B. extra"` scored 1 of **3** rather
/// than 0 of 2. Pinned by
/// `a_trailing_insertion_fails_the_last_reference_sentence_it_does_not_add_one`.
///
/// A reference with no tokens has no sentences, so this returns `(0, 0)`
/// and [`CasingCounts::sentence_accuracy`] is `None` — not `1.0`. An empty
/// reference is a broken input, and the metric says "nothing to score"
/// rather than "flawless"; `content_wer` returns `inf` on the same input,
/// which is the loud half of the same signal.
fn sentence_counts(steps: &[Aligned]) -> (u32, u32) {
    // One entry per reference sentence: has anything failed it yet?
    let mut failed: Vec<bool> = Vec::new();
    // Index of the sentence currently accumulating reference tokens, if a
    // reference token has been seen since the last terminator.
    let mut open: Option<usize> = None;
    // An insertion arrived before any reference token existed; charge it to
    // sentence 0 once there is one.
    let mut leading_insertion = false;

    for step in steps {
        let (ref_token, step_matched) = match step {
            Aligned::Matched(r, h) => (Some(*r), r == h),
            Aligned::Substituted(r, _) => (Some(*r), false),
            Aligned::Deleted(r) => (Some(*r), false),
            Aligned::Inserted(_) => (None, false),
        };

        let Some(r) = ref_token else {
            // Insertion: charge it, never create a sentence for it.
            match open {
                Some(idx) => failed[idx] = true,
                None => match failed.last_mut() {
                    Some(previous) => *previous = true,
                    None => leading_insertion = true,
                },
            }
            continue;
        };

        let idx = match open {
            Some(idx) => idx,
            None => {
                failed.push(false);
                failed.len() - 1
            }
        };
        open = Some(idx);
        if !step_matched {
            failed[idx] = true;
        }
        if ends_sentence(r) {
            open = None;
        }
    }

    if leading_insertion {
        if let Some(first) = failed.first_mut() {
            *first = true;
        }
    }

    let total = failed.len() as u32;
    let matched = failed.iter().filter(|f| !**f).count() as u32;
    (total, matched)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Compares every field, so a bug in any ONE field is visible instead of
    /// being hidden by only checking the field a given test happens to be
    /// "about". Ratios are compared with a tolerance (and `None` must match
    /// `None`), counts exactly.
    fn assert_scores_close(actual: CasingScore, expected: CasingScore) {
        let close = |a: Option<f64>, b: Option<f64>| match (a, b) {
            (Some(x), Some(y)) => (x - y).abs() < 1e-9,
            (None, None) => true,
            _ => false,
        };
        assert!(
            close(actual.precision, expected.precision)
                && close(actual.recall, expected.recall)
                && close(actual.f1, expected.f1)
                && close(actual.sentence_accuracy, expected.sentence_accuracy)
                && actual.counts == expected.counts,
            "expected {expected:?}, got {actual:?}"
        );
    }

    /// The `CasingScore` a given set of counts must produce, so a test only
    /// has to spell out the counts it is actually about.
    fn derived(counts: CasingCounts) -> CasingScore {
        counts.score()
    }

    #[test]
    fn perfect_casing_scores_one() {
        let s = score("The Sarvam API is fast.", "The Sarvam API is fast.");
        assert_eq!(s.f1, Some(1.0));
        assert_eq!(s.sentence_accuracy, Some(1.0));
        assert_eq!(
            s.counts,
            CasingCounts {
                true_positives: 3, // The, Sarvam, API
                false_positives: 0,
                false_negatives: 0,
                true_negatives: 2, // is, fast.
                caseless_tokens: 0,
                sentences_total: 1,
                sentences_matched: 1,
            }
        );
    }

    #[test]
    fn a_lowercased_proper_noun_costs_recall() {
        let s = score("The Sarvam API is fast.", "The sarvam API is fast.");
        assert_eq!(s.recall, Some(2.0 / 3.0));
        assert_eq!(s.counts.false_negatives, 1);
        assert_eq!(s.sentence_accuracy, Some(0.0));
    }

    // --- Precision's false-positive path, exercised so deleting it fails. ---

    /// Without this test, deleting the `(false, true) => fp` arm left every
    /// other test green: precision's false-positive path was unchecked.
    ///
    /// Here the hypothesis capitalises "Is", which the reference leaves
    /// lowercase, alongside three genuine capitals. Precision must read
    /// 3/4. Delete the `fp` arm and the token falls into `true_negatives`
    /// instead, `false_positives` drops to 0 and precision reads `Some(1.0)`
    /// — both assertions below fail.
    #[test]
    fn a_spurious_capital_costs_precision() {
        let s = score("The Sarvam API is fast.", "The Sarvam API Is fast.");
        assert_eq!(s.counts.false_positives, 1);
        assert_eq!(s.precision, Some(0.75));
        assert_eq!(s.recall, Some(1.0), "no capital was missed");
    }

    /// The same defect in its starkest form: an all-lowercase reference,
    /// with the hypothesis inventing a capital. There are no true
    /// positives, so precision is a real `0.0` — and `recall` is `None`
    /// because the reference capitalised nothing there was to find.
    ///
    /// The old implementation returned `1.0` for BOTH here (`0/0` read as
    /// "no wrong guesses were possible") and so scored inventing a capital
    /// as flawless.
    ///
    /// F1 is a real `0.0` too: `2·0 / (2·0 + 1 + 0)`. It read `None` for one
    /// round, which the report prints as "n/a" — a total failure dressed as
    /// an unmeasurable. Zero and n/a are the two answers a benchmark must
    /// never swap.
    #[test]
    fn inventing_a_capital_where_the_reference_has_none_is_precision_zero_not_one() {
        let s = score("we should ship it.", "We should ship it.");
        assert_eq!(s.precision, Some(0.0));
        assert_eq!(s.recall, None, "nothing to recall: zero reference capitals");
        assert_eq!(s.f1, Some(0.0), "F1's denominator is 2*tp + fp + fn = 1");
        assert_eq!(s.counts.false_positives, 1);
        assert_eq!(s.counts.true_positives, 0);
        assert_eq!(s.sentence_accuracy, Some(0.0));
    }

    /// The mirror image: the reference capitalises, the hypothesis capitalises
    /// nothing. `precision` is undefined (no positive prediction was made) but
    /// recall is a real `0.0` and so is F1 — `2·0 / (2·0 + 0 + 3)`.
    ///
    /// This is the shape a formatter with its casing stage switched off
    /// produces, i.e. the single most likely real failure, so it must never
    /// read as "not measurable".
    #[test]
    fn missing_every_capital_is_f1_zero_not_unmeasurable() {
        let s = score("The Sarvam API is fast.", "the sarvam api is fast.");
        assert_eq!(s.counts.true_positives, 0);
        assert_eq!(s.counts.false_negatives, 3);
        assert_eq!(s.counts.false_positives, 0);
        assert_eq!(s.precision, None, "the hypothesis predicted no capitals");
        assert_eq!(s.recall, Some(0.0));
        assert_eq!(s.f1, Some(0.0), "a total failure scores zero, not n/a");
    }

    /// `None` still has to mean something, or the fix above would just be
    /// "never say n/a". It survives in exactly one place: all three counts
    /// zero, which is the genuinely empty denominator.
    #[test]
    fn f1_is_none_only_when_there_is_nothing_at_all_to_measure() {
        assert_eq!(CasingCounts::default().f1(), None);
        for counts in [
            CasingCounts {
                false_positives: 1,
                ..Default::default()
            },
            CasingCounts {
                false_negatives: 1,
                ..Default::default()
            },
            CasingCounts {
                true_positives: 1,
                ..Default::default()
            },
        ] {
            assert!(
                counts.f1().is_some(),
                "F1 is defined whenever any of tp/fp/fn is nonzero: {counts:?}"
            );
        }
        // True negatives alone are not evidence about the positive class,
        // so they do not give F1 a denominator.
        assert_eq!(
            CasingCounts {
                true_negatives: 40,
                ..Default::default()
            }
            .f1(),
            None
        );
    }

    /// F1 must still be the harmonic mean wherever the harmonic mean is
    /// defined — the counts formula is a rewrite, not a new metric.
    #[test]
    fn f1_agrees_with_the_harmonic_mean_where_both_ratios_exist() {
        for counts in [
            CasingCounts {
                true_positives: 9,
                false_positives: 1,
                false_negatives: 2,
                ..Default::default()
            },
            CasingCounts {
                true_positives: 1,
                false_positives: 7,
                false_negatives: 3,
                ..Default::default()
            },
            CasingCounts {
                true_positives: 5,
                ..Default::default()
            },
        ] {
            let (p, r) = (counts.precision().unwrap(), counts.recall().unwrap());
            let harmonic = 2.0 * p * r / (p + r);
            let f1 = counts.f1().unwrap();
            assert!((f1 - harmonic).abs() < 1e-12, "{counts:?}: {f1} vs {harmonic}");
        }
    }

    /// "Sarvam" -> "great" is a WORD substitution (`content_wer`'s job),
    /// not a casing question: there is no aligned same-word pair to
    /// compare case on. "API" is a second, genuinely-matched cased token
    /// so the test cannot pass by accident via the all-zero vacuous case —
    /// if the substitution were wrongly folded into a (positive, negative)
    /// casing comparison instead of excluded, recall would read `0.5`, not
    /// `1.0`.
    #[test]
    fn a_substituted_content_word_is_not_counted_as_a_missed_capital() {
        let s = score("Sarvam API is here.", "great API is here.");
        assert_scores_close(
            s,
            derived(CasingCounts {
                true_positives: 1, // API
                false_positives: 0,
                false_negatives: 0,
                true_negatives: 2, // is, here.
                caseless_tokens: 0,
                sentences_total: 1,
                sentences_matched: 0,
            }),
        );
        assert_eq!(s.precision, Some(1.0));
        assert_eq!(s.recall, Some(1.0));
    }

    #[test]
    fn sentence_accuracy_is_the_fraction_of_fully_correct_sentences() {
        let s = score(
            "The Sarvam API is fast. It works well.",
            "The sarvam API is fast. It works well.",
        );
        // First sentence has a miscased token, second is perfect: 1 of 2.
        assert_eq!(s.sentence_accuracy, Some(0.5));
        assert_eq!(s.counts.sentences_total, 2);
    }

    // --- Caseless scripts: excluded, not scored perfect. ---

    /// Devanagari has no case, so a Hindi sentence contains no casing evidence
    /// at all — and 200 of the 1 000 committed fixture cases are exactly that.
    ///
    /// Every ratio must be `None` ("not measured"), never `Some(1.0)`
    /// ("perfect"), and `caseless_tokens` must say how much text the
    /// silence covers. Vocabulary borrowed verbatim from `eval::per`'s
    /// existing Devanagari corpus so nothing here is freshly-typed Hindi.
    #[test]
    fn a_caseless_script_is_not_measured_rather_than_scored_perfect() {
        let text = "नमस्ते\u{0964} दुनिया\u{0964}";
        let s = score(text, text);
        assert_eq!(s.precision, None);
        assert_eq!(s.recall, None);
        assert_eq!(s.f1, None);
        assert_eq!(s.counts.caseless_tokens, 2);
        assert_eq!(s.counts.true_positives, 0);
        assert_eq!(s.counts.true_negatives, 0);
        // The sentence-level number is still real: the danda still splits.
        assert_eq!(s.sentence_accuracy, Some(1.0));
    }

    /// Caseless text must contribute *nothing* to a corpus micro-F1, not a
    /// free perfect score. This is the aggregation-level statement of the
    /// test above, and it is the number that actually gets published.
    #[test]
    fn caseless_sentences_do_not_lift_a_corpus_micro_f1() {
        let english = score("The Sarvam API is fast.", "The sarvam API is fast.").counts;
        let hindi = score("नमस्ते\u{0964} दुनिया\u{0964}", "नमस्ते\u{0964} दुनिया\u{0964}").counts;

        let corpus: CasingCounts = [english, hindi].into_iter().sum();
        assert_eq!(corpus.true_positives, english.true_positives);
        assert_eq!(corpus.false_negatives, english.false_negatives);
        assert_eq!(corpus.caseless_tokens, 2);
        assert_eq!(
            corpus.f1(),
            english.f1(),
            "adding a caseless sentence must not move the micro F1"
        );
    }

    /// The exclusion is per token, not per language: the committed Hindi
    /// fixture writes Latin glosses inside Devanagari sentences — 111 of
    /// its 3 573 target tokens carry an uppercase Latin character — and
    /// those are genuine casing decisions that must still be scored.
    ///
    /// Shape borrowed from `diarbench-hi-0017`'s `आई-क्यू(IQ)`.
    #[test]
    fn a_latin_gloss_inside_devanagari_text_is_still_measured() {
        let reference = "आई-क्यू(IQ) टेस्ट\u{0964}";
        let hypothesis = "आई-क्यू(iq) टेस्ट\u{0964}";
        let s = score(reference, hypothesis);
        assert_eq!(
            s.counts.caseless_tokens, 1,
            "only टेस्ट। is genuinely caseless"
        );
        assert_eq!(s.counts.false_negatives, 1, "IQ -> iq is a missed capital");
        assert_eq!(s.recall, Some(0.0));
    }

    /// A number is not a caseless script. `caseless_tokens` is printed in
    /// the benchmark report as "N caseless tokens set aside", which reads as
    /// Devanagari coverage — so booking English digits and bare punctuation
    /// there inflated it on exactly the text it was not describing. Measured
    /// on the committed corpus, 135 of the 138 letterless target tokens are
    /// in the four English fixtures.
    ///
    /// They are excluded from every count, not moved to another one: a digit
    /// is not a capitalisation decision either.
    #[test]
    fn digits_and_bare_punctuation_are_not_booked_as_caseless_script() {
        // Shape borrowed from earnings22: "2022", "28%" and a stray "--".
        let s = score("Revenue rose 28% in 2022 -- again.", "Revenue rose 28% in 2022 -- again.");
        assert_eq!(
            s.counts.caseless_tokens, 0,
            "no token here is written in a caseless script"
        );
        assert_eq!(s.counts.true_positives, 1, "Revenue");
        assert_eq!(s.counts.true_negatives, 3, "rose, in, again.");

        // ...and a genuinely caseless word alongside them is still counted.
        let mixed = score("\u{0906}\u{0908} 2022 \u{0964}", "\u{0906}\u{0908} 2022 \u{0964}");
        assert_eq!(mixed.counts.caseless_tokens, 1, "only the Devanagari word");
    }

    /// Zero cased tokens on either side but the script IS bicameral: this
    /// is a real measurement of the negative class, not a caseless
    /// exclusion. Precision and recall are still `None` (nothing was
    /// predicted, nothing was there to find) but the four tokens land in
    /// `true_negatives`, not `caseless_tokens` — the two zeros mean
    /// different things and the counts distinguish them.
    #[test]
    fn an_all_lowercase_latin_sentence_is_true_negatives_not_caseless() {
        let s = score("we should ship it.", "we should ship it.");
        assert_scores_close(
            s,
            derived(CasingCounts {
                true_positives: 0,
                false_positives: 0,
                false_negatives: 0,
                true_negatives: 4,
                caseless_tokens: 0,
                sentences_total: 1,
                sentences_matched: 1,
            }),
        );
        assert_eq!(s.precision, None);
        assert_eq!(s.recall, None);
        assert_eq!(s.f1, None);
    }

    // --- Corpus aggregation: micro, and it differs from macro. ---

    #[test]
    fn counts_sum_matches_manual_addition() {
        let a = CasingCounts {
            true_positives: 2,
            ..Default::default()
        };
        let b = CasingCounts {
            false_positives: 1,
            sentences_total: 3,
            ..Default::default()
        };
        let summed: CasingCounts = [a, b].into_iter().sum();
        assert_eq!(summed, a + b);

        let mut acc = CasingCounts::default();
        acc += a;
        acc += b;
        assert_eq!(acc, summed);
        assert_eq!(
            summed,
            CasingCounts {
                true_positives: 2,
                false_positives: 1,
                sentences_total: 3,
                ..Default::default()
            }
        );
    }

    /// The published truecasing baselines (Stanford CRF 90.89, LSTM-LARGE
    /// 93.19) are micro F1 over a corpus. Averaging per-sentence F1 is a
    /// different number and comparing it to those figures is a category
    /// error — the same trap `eval::per` documents for corpus PER. This
    /// proves the two formulas actually disagree on real counts rather than
    /// only in principle.
    #[test]
    fn micro_and_macro_f1_are_different_numbers() {
        // A ten-capital sentence with one miss, and a one-capital sentence
        // that got both directions wrong. Macro gives them equal weight;
        // micro weights each by how much evidence it carries.
        let long = CasingCounts {
            true_positives: 9,
            false_negatives: 1,
            ..Default::default()
        };
        let short = CasingCounts {
            true_positives: 0,
            false_positives: 1,
            false_negatives: 1,
            ..Default::default()
        };

        // long: P = 9/9 = 1, R = 9/10, F1 = 18/19.
        // short: P = 0/1, R = 0/1, F1 = 0.
        let macro_f1 = (long.f1().unwrap() + short.f1().unwrap()) / 2.0;
        assert!((macro_f1 - (18.0 / 19.0) / 2.0).abs() < 1e-9, "{macro_f1}");

        // micro: P = 9/10, R = 9/11, F1 = 2*(81/110)/(189/110) = 6/7.
        let micro_f1 = [long, short].into_iter().sum::<CasingCounts>().f1().unwrap();
        assert!((micro_f1 - 6.0 / 7.0).abs() < 1e-9, "{micro_f1}");

        assert!(
            (macro_f1 - micro_f1).abs() > 0.3,
            "the two formulas must actually disagree, not just be spelled \
             differently: macro {macro_f1}, micro {micro_f1}"
        );
    }

    /// A summed corpus keeps `None` meaning "nothing measured": a corpus of
    /// nothing but caseless text has no F1, rather than a perfect one.
    #[test]
    fn a_wholly_caseless_corpus_has_no_f1_at_all() {
        let hindi = score("नमस्ते\u{0964}", "नमस्ते\u{0964}").counts;
        let corpus: CasingCounts = [hindi, hindi, hindi].into_iter().sum();
        assert_eq!(corpus.f1(), None);
        assert_eq!(corpus.caseless_tokens, 3);
    }

    // --- Sentence boundaries. ---

    /// An inserted word has no reference counterpart, so it cannot be a
    /// `Matched` pair and cannot touch precision/recall/F1 — casing scores
    /// perfect here even though the sentence plainly was not reproduced.
    /// That split is deliberate (see the module doc): `sentence_accuracy`
    /// is what catches it.
    #[test]
    fn an_inserted_word_breaks_sentence_accuracy_but_not_casing_precision() {
        let s = score("We should ship it.", "We should really ship it.");
        assert_eq!(s.precision, Some(1.0));
        assert_eq!(s.recall, Some(1.0));
        assert_eq!(s.f1, Some(1.0));
        assert_eq!(s.sentence_accuracy, Some(0.0));
        assert_eq!(s.counts.sentences_total, 1);
    }

    /// The denominator belongs to the reference. A hypothesis that runs on
    /// past the last reference sentence must FAIL that sentence, not open a
    /// third one it can then be scored 1-of-3 on.
    ///
    /// Before the fix this read `sentences_total: 3`, `sentence_accuracy:
    /// 1/3` — the hypothesis had moved the denominator, and being wrong in
    /// a new way earned partial credit.
    #[test]
    fn a_trailing_insertion_fails_the_last_reference_sentence_it_does_not_add_one() {
        let s = score("It works. We ship.", "It works. We ship. Really.");
        assert_eq!(s.counts.sentences_total, 2, "the reference has two");
        assert_eq!(s.sentence_accuracy, Some(0.5), "the second one is spoiled");
    }

    /// The mirror case: words inserted before any reference token fail the
    /// first reference sentence rather than creating a sentence zero.
    ///
    /// Every aligned pair here matches byte-for-byte, so the ONLY thing that
    /// can fail the sentence is the leading insertion — and there must still
    /// be exactly one sentence for it to fail.
    #[test]
    fn a_leading_insertion_fails_the_first_reference_sentence() {
        let s = score("We ship.", "So We ship.");
        assert_eq!(s.counts.sentences_total, 1);
        assert_eq!(s.sentence_accuracy, Some(0.0));
    }

    /// Sentence boundaries are not ASCII-only: the Devanagari danda
    /// (U+0964, used across several Brahmic scripts this app ships, not
    /// just Hindi) closes a sentence exactly like `.` does. Vocabulary
    /// borrowed verbatim from `eval::per`'s existing Devanagari corpus so
    /// nothing here is freshly-typed Hindi.
    #[test]
    fn devanagari_danda_ends_a_sentence() {
        let text = "नमस्ते\u{0964} दुनिया\u{0964}";

        // Corrupt the second "sentence" only. The first still closes and
        // matches independently of the second, which is only possible if
        // the danda is doing real boundary work rather than being ignored
        // as an ordinary word character.
        let corrupted = "नमस्ते\u{0964} रिपोर्ट\u{0964}";
        let s = score(text, corrupted);
        assert_eq!(s.counts.sentences_total, 2);
        assert_eq!(s.sentence_accuracy, Some(0.5));
    }

    /// Abbreviation periods must not split, because splitting makes the
    /// metric more lenient: without the guard this reference is two
    /// sentences, the mistake lands in one of them, and a broken output
    /// scores 0.5 instead of 0.
    #[test]
    fn an_abbreviation_period_does_not_split_a_sentence() {
        assert!(is_abbreviation_period("U.S."));
        assert!(is_abbreviation_period("e.g."));
        assert!(is_abbreviation_period("J."));
        assert!(!ends_sentence("U.S."));

        let s = score("The U.S. team shipped.", "The U.S. team Shipped.");
        assert_eq!(s.counts.sentences_total, 1, "one sentence, not two");
        assert_eq!(s.sentence_accuracy, Some(0.0));
    }

    /// The counter-case, and the reason the rule is "a period *between*
    /// letters" rather than "contains a period": a trailing ellipsis written
    /// flush against its word really does end a sentence. 24 committed
    /// fixture tokens end in two or more dots (`बहुत...`, `जाता..`,
    /// `जॉइंट(joint)...`) and not one is an abbreviation, so a
    /// `contains('.')` rule would have stopped splitting every one of them.
    #[test]
    fn a_trailing_ellipsis_still_ends_a_sentence() {
        assert!(!is_abbreviation_period("बहुत..."));
        assert!(!is_abbreviation_period("wait..."));
        assert!(!is_abbreviation_period("3."));
        assert!(ends_sentence("बहुत..."));

        let s = score("बहुत... ठीक है\u{0964}", "बहुत... ठीक है\u{0964}");
        assert_eq!(s.counts.sentences_total, 2);
    }

    /// The abbreviation window must require **letters**, not alphanumerics.
    /// A decimal with a unit glued on — "revenue was 1.5m." — is a real
    /// sentence end, and the alphanumeric version matched `1`·`.`·`5` and
    /// swallowed it. No English abbreviation puts a digit next to its
    /// internal period, so tightening this costs none of them.
    #[test]
    fn a_decimal_with_a_unit_still_ends_a_sentence() {
        for token in ["1.5m.", "2.4GHz.", "$1.5m.", "0.5x."] {
            assert!(
                !is_abbreviation_period(token),
                "{token} is a number, not an abbreviation"
            );
            assert!(ends_sentence(token), "{token} must close its sentence");
        }
        // The abbreviations the window exists for are untouched.
        for token in ["U.S.", "e.g.", "i.e.", "a.m."] {
            assert!(is_abbreviation_period(token), "{token} is an abbreviation");
        }
        // A percentage is safe because its core holds no letter, and must
        // stay so: five such tokens are in the committed corpus.
        assert!(!is_abbreviation_period("11.2%."));
        assert!(ends_sentence("11.2%."));

        let s = score("Revenue was 1.5m. Margins held.", "Revenue was 1.5m. margins held.");
        assert_eq!(s.counts.sentences_total, 2, "two sentences, not one");
        assert_eq!(s.sentence_accuracy, Some(0.5));
    }

    /// The initial rule must require an **uppercase** letter. An initial is
    /// always capitalised (`J. Smith`), so the lowercase half of the old
    /// rule could only ever misfire — "the word starts with the letter a."
    /// is a sentence end.
    ///
    /// The uppercase case stays ambiguous from the token alone (`Exhibit A.`
    /// versus `J. Smith`) and is deliberately resolved toward NOT splitting:
    /// under-splitting merges sentences and makes the metric stricter, while
    /// over-splitting hands out partial credit.
    #[test]
    fn only_an_uppercase_single_letter_reads_as_an_initial() {
        assert!(is_abbreviation_period("J."));
        assert!(is_abbreviation_period("R."));
        assert!(!is_abbreviation_period("a."));
        assert!(!is_abbreviation_period("i."));
        assert!(ends_sentence("a."));
        assert!(!ends_sentence("J."));

        let s = score(
            "It starts with the letter a. Then it stops.",
            "It starts with the letter a. then it stops.",
        );
        assert_eq!(s.counts.sentences_total, 2);
        assert_eq!(s.sentence_accuracy, Some(0.5));
    }

    // --- Degenerate inputs. ---

    /// Empty on both sides: zero alignment steps, so there is no sentence,
    /// no caseable token and nothing at all to report. Every field is
    /// `None` — "not measured".
    ///
    /// `1.0` across the board would read as a flawless score for scoring
    /// nothing.
    #[test]
    fn empty_reference_and_hypothesis_measure_nothing() {
        let s = score("", "");
        assert_scores_close(s, derived(CasingCounts::default()));
        assert_eq!(s.precision, None);
        assert_eq!(s.recall, None);
        assert_eq!(s.f1, None);
        assert_eq!(s.sentence_accuracy, None);
    }

    /// Empty reference, non-empty hypothesis: every step is an
    /// [`Aligned::Inserted`], which creates no reference sentence, so
    /// `sentences_total` is 0 and `sentence_accuracy` is `None`.
    ///
    /// It is deliberately not `1.0`: `None` says "no denominator", and the
    /// loud signal for this input lives next door —
    /// `rawer::content_wer("", h)` returns `inf`. A caller reporting either
    /// number must check `sentences_total` first, which is why it is in the
    /// output.
    #[test]
    fn empty_reference_with_nonempty_hypothesis_scores_nothing_rather_than_perfect() {
        let s = score("", "The Sarvam API is fast.");
        assert_eq!(s.counts.sentences_total, 0);
        assert_eq!(s.sentence_accuracy, None);
        assert_eq!(s.f1, None);
        assert_ne!(s.sentence_accuracy, Some(1.0));
    }

    /// The ratios and the counts cannot disagree, because the ratios are
    /// derived from the counts and nowhere else.
    #[test]
    fn a_score_is_exactly_its_counts_rescored() {
        for (r, h) in [
            ("The Sarvam API is fast.", "the sarvam api is Fast."),
            ("नमस्ते\u{0964}", "नमस्ते\u{0964}"),
            ("", "hello"),
            ("We ship. It works.", "we Ship. it works"),
        ] {
            let s = score(r, h);
            assert_eq!(s, s.counts.score(), "{r:?} vs {h:?}");
        }
    }
}
