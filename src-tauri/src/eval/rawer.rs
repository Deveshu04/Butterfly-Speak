//! Word Error Rate with normalisation deliberately left OFF, plus a second
//! WER with it deliberately left ON — two metrics, not one, because a single
//! normalised WER cannot see a formatter at all.
//!
//! # Why two WERs
//!
//! Every standard WER pipeline (Kaldi, jiwer's default, NIST `sclite`)
//! lowercases both sides and strips punctuation before aligning tokens. That
//! is exactly the signal a formatting engine produces: it takes raw ASR
//! output and adds capitalisation and punctuation back in. Score a perfect
//! formatter and a no-op formatter with a normalised WER and they come back
//! identical — the metric was built to be blind to the one thing under
//! test.
//!
//! So this module has two functions with opposite normalisation settings,
//! and they answer two different questions:
//!
//! - [`ra_wer`] ("raw-aware WER"): no normalisation at all. Casing and
//!   punctuation differences count as word errors, same as any other
//!   mismatched token. This is the metric that can actually see formatting
//!   quality, because it is the only one of the two that does not throw the
//!   signal away before scoring.
//! - [`content_wer`]: lowercased, punctuation-stripped — the traditional
//!   normalised WER. On its own it would be uninformative for exactly the
//!   reason above, but it earns its place as a **regression guard**: on read
//!   speech (the reference transcript IS what was said) the formatting
//!   engine must never change a content word, only its casing and
//!   punctuation. `content_wer` strips away casing/punctuation before
//!   scoring, so it isolates word-choice errors specifically. It should sit
//!   at ~0 on a healthy run; a nonzero value means the formatter (or an
//!   upstream stage — disfluency removal, ITN, cleanup) dropped, added, or
//!   changed a word it had no business touching.
//!
//! Neither function tells you whether the *casing* or the *punctuation* was
//! right, only that something about the surface form differs — that
//! diagnosis is [`crate::eval::casing`]'s and [`crate::eval::per`]'s job
//! respectively. `ra_wer` is the smoke alarm; those two are the
//! investigation.
//!
//! # Corpus figures come from [`WerCounts`], never from averaging
//!
//! WER is a ratio of summed edit operations to summed reference length. A
//! corpus WER is therefore `Σ(S + I + D) / Σ reference tokens` — sum the
//! counts, take **one** ratio at the end. Averaging the per-pair ratios is a
//! different number: it gives a four-word utterance the same weight as a
//! forty-word one, and no published WER means that.
//!
//! So each metric comes in two forms. [`ra_wer_counts`] and
//! [`content_wer_counts`] return a [`WerCounts`] (substitutions, insertions,
//! deletions and the reference length they are measured against); it
//! implements [`Add`](std::ops::Add)/[`AddAssign`](std::ops::AddAssign)/
//! [`Sum`](std::iter::Sum), so the correct corpus formula is also the
//! shortest one to write:
//!
//! ```ignore
//! pairs.iter()
//!     .map(|(r, h)| rawer::content_wer_counts(r, h))
//!     .sum::<WerCounts>()
//!     .rate()
//! ```
//!
//! [`ra_wer`] and [`content_wer`] keep returning a bare `f64` for the
//! single-pair case, and are implemented on top of the counts so the two
//! forms cannot drift apart. `micro_and_macro_wer_are_different_numbers`
//! proves the two formulas land somewhere different on real counts.
//!
//! # What `content_wer` still cannot detect
//!
//! A regression guard that structurally cannot fire reads as a pass, which is
//! worse than having no guard at all, so the blind spots are enumerated here
//! rather than left to be discovered. All three are properties of
//! [`normalize_token`]; see its doc for the mechanics.
//!
//! 1. **Casing and edge punctuation — by design.** That is the whole point of
//!    the second metric; `ra_wer` and [`crate::eval::casing`] see those.
//! 2. **Unicode canonical form.** Rust's standard library has no NFC/NFD
//!    normaliser and this crate takes no new dependency for one, so a
//!    reference and a hypothesis that spell the same word with different
//!    canonical forms read as a content substitution. This is measurable, not
//!    hypothetical: **40 of the 200 committed `indic-diarbench.jsonl` targets
//!    are not in NFC**, because they use the precomposed letters U+095B ZA,
//!    U+095C DDDHA and U+095D RHA, whose canonical decompositions
//!    (`ज`+nukta, `ड`+nukta, `ढ`+nukta) are the NFC form — Devanagari nukta
//!    compositions are Unicode composition exclusions. The corpus cannot
//!    trigger it *today*: all 1 000 committed cases spell `input` and
//!    `target` in the same canonical form, so a form-preserving formatter is
//!    safe. A
//!    stage that normalises (an ICU-backed cleanup pass, a model that emits
//!    NFC) would make every such word read as a phantom content error.
//!    `nfc_and_nfd_spellings_of_the_same_word_do_not_compare_equal` pins the
//!    behaviour so it surfaces as a known limit rather than a mystery.
//! 3. **Punctuation outside [`TRIMMABLE_PUNCTUATION`]** survives into the
//!    token and reads as a content error. That is the *safe* direction — the
//!    guard fires when it should not, rather than staying silent when it
//!    should fire — and it is the deliberate trade made when the trimmer
//!    stopped stripping "everything non-alphanumeric"; see
//!    [`normalize_token`].

// Everything public here is exercised only by this file's own
// `#[cfg(test)]` module as far as the *lib crate* is concerned, so a plain
// `cargo build`/`cargo check` sees the whole file as unreachable. Same
// situation, same fix, as `eval::per`: see that file's identical comment for
// the full reasoning.
//
// `src-tauri/src/bin/fmtbench.rs` does call `content_wer` — but it reaches this
// file through a `#[path]` include, compiling it into the *binary's* crate
// (with its own blanket `#[allow(dead_code)]`), because `lib.rs` declares
// `mod eval;` privately and the rlib exposes nothing. So the benchmark does not
// lift this allow. It comes off when something in the shipped app scores text,
// or if `eval` is ever made `pub`.
#![allow(dead_code)]

/// One step of the Levenshtein backtrace. Same shape as
/// `eval::casing`'s `Op`, kept private here rather than shared: that one
/// exists to rebuild *token pairs*, this one to tally *operations*, and a
/// type used for two purposes in two modules is a coupling neither needs.
#[derive(Clone, Copy)]
enum Op {
    Match,
    Sub,
    Ins,
    Del,
}

/// Levenshtein alignment of `hypothesis` against `reference`, returning the
/// operation tally rather than just the distance: `(substitutions,
/// insertions, deletions)`. Their sum IS the edit distance.
///
/// Generic over `T: PartialEq` so the same routine serves both
/// [`ra_wer`]'s raw `&str` tokens and [`content_wer`]'s normalised `String`
/// tokens without duplicating the DP.
///
/// # Ties, and what they do and do not move
///
/// When substitution, insertion and deletion cost the same, the backtrace
/// takes the first in that order, so a *particular* S/I/D split is one of
/// several minimal alignments. Their **sum** is not a choice — it is the
/// edit distance, which is unique — so WER (at any aggregation level) is
/// unaffected by the tie-break. Only the breakdown printed beside it can
/// shift, which is why the breakdown is offered as diagnostic detail and
/// the rate is computed from the total.
fn edit_counts<T: PartialEq>(reference: &[T], hypothesis: &[T]) -> (u32, u32, u32) {
    let (rn, hn) = (reference.len(), hypothesis.len());
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
            if reference[i - 1] == hypothesis[j - 1] {
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

    let (mut substitutions, mut insertions, mut deletions) = (0u32, 0u32, 0u32);
    let (mut i, mut j) = (rn, hn);
    while i > 0 || j > 0 {
        match back[i][j] {
            Op::Match => {
                i -= 1;
                j -= 1;
            }
            Op::Sub => {
                substitutions += 1;
                i -= 1;
                j -= 1;
            }
            Op::Ins => {
                insertions += 1;
                j -= 1;
            }
            Op::Del => {
                deletions += 1;
                i -= 1;
            }
        }
    }
    (substitutions, insertions, deletions)
}

/// The edit tally one (reference, hypothesis) pair contributes, and the
/// reference length it is measured against. **Sum these across a corpus and
/// call [`rate`](WerCounts::rate) once** — see the module doc on why
/// averaging per-pair WERs is a different, non-comparable number.
///
/// The same warning, for the same reason, as [`crate::eval::per::PunctCounts`],
/// [`crate::eval::casing::CasingCounts`] and
/// [`crate::eval::disfluency::RemovalCounts`]. This type was added late: for
/// one round `rawer` was the only metric in the harness without a count
/// type, so the benchmark could only macro-average `ra_wer`/`content_wer`
/// while micro-computing everything else — precisely the trap the count
/// types exist to close.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct WerCounts {
    /// Reference tokens the hypothesis replaced with a different token.
    pub substitutions: u32,
    /// Hypothesis tokens with no reference counterpart.
    pub insertions: u32,
    /// Reference tokens with no hypothesis counterpart.
    pub deletions: u32,
    /// Tokens in the **reference** — the denominator. Summing this across a
    /// corpus is what makes the corpus figure micro.
    pub reference_tokens: u32,
}

impl WerCounts {
    /// `S + I + D`, the edit distance. Unique regardless of how ties in the
    /// alignment were broken; see [`edit_counts`].
    pub fn errors(&self) -> u32 {
        self.substitutions + self.insertions + self.deletions
    }

    /// `(S + I + D) / reference_tokens` — the standard WER formula, taken
    /// **once** over however many pairs were summed into `self`.
    ///
    /// The two degenerate cases are spelled out rather than left to fall out
    /// of `0 / 0` arithmetic:
    ///
    /// - **No reference tokens and no errors**: `None`. There was nothing to
    ///   measure. It is deliberately not `0.0`: a corpus whose references
    ///   are all empty is a broken corpus, and a perfect-looking `0.000` in
    ///   a report is the flattering reading of that. Note this is the one
    ///   place the counts API and the per-pair `f64` API differ on purpose —
    ///   [`ra_wer`]/[`content_wer`] map this `None` back to `0.0`, which is
    ///   the right answer for a single pair where nothing was said and
    ///   nothing was transcribed, and the wrong answer for a corpus total.
    ///   `an_empty_corpus_has_no_wer_but_an_empty_pair_scores_zero` pins
    ///   both halves.
    /// - **No reference tokens but some errors**: [`f64::INFINITY`]. Every
    ///   hypothesis token is an insertion against a reference with no tokens
    ///   to divide by, so the honest answer is "unboundedly bad", not `0.0`
    ///   (which would hide real errors) and not `1.0` (which would
    ///   understate an error count that can exceed the hypothesis length).
    ///   The `inf` propagates through any further arithmetic — the correct
    ///   alarm for a reference that should never have been empty.
    pub fn rate(&self) -> Option<f64> {
        let errors = self.errors();
        if self.reference_tokens == 0 {
            return (errors > 0).then_some(f64::INFINITY);
        }
        Some(f64::from(errors) / f64::from(self.reference_tokens))
    }
}

impl std::ops::Add for WerCounts {
    type Output = WerCounts;

    fn add(self, other: WerCounts) -> WerCounts {
        WerCounts {
            substitutions: self.substitutions + other.substitutions,
            insertions: self.insertions + other.insertions,
            deletions: self.deletions + other.deletions,
            reference_tokens: self.reference_tokens + other.reference_tokens,
        }
    }
}

impl std::ops::AddAssign for WerCounts {
    fn add_assign(&mut self, other: WerCounts) {
        *self = *self + other;
    }
}

impl std::iter::Sum for WerCounts {
    fn sum<I: Iterator<Item = WerCounts>>(iter: I) -> WerCounts {
        iter.fold(WerCounts::default(), std::ops::Add::add)
    }
}

/// Aligns two already-tokenised sequences into a [`WerCounts`]. Shared by
/// [`ra_wer_counts`] and [`content_wer_counts`]; the only difference between
/// them is what the tokens look like by the time they get here.
fn counts_of<T: PartialEq>(reference: &[T], hypothesis: &[T]) -> WerCounts {
    let (substitutions, insertions, deletions) = edit_counts(reference, hypothesis);
    WerCounts {
        substitutions,
        insertions,
        deletions,
        reference_tokens: reference.len() as u32,
    }
}

/// [`ra_wer`]'s edit tally, for corpus aggregation. Sum these and call
/// [`WerCounts::rate`] once; see the module doc.
///
/// Tokenisation is `str::split_whitespace`, which splits on
/// [`char::is_whitespace`] — every script this app ships, not just ASCII
/// space.
pub fn ra_wer_counts(reference: &str, hypothesis: &str) -> WerCounts {
    let r: Vec<&str> = reference.split_whitespace().collect();
    let h: Vec<&str> = hypothesis.split_whitespace().collect();
    counts_of(&r, &h)
}

/// WER with **no normalisation**: reference and hypothesis are split on
/// Unicode whitespace and compared token-for-token exactly as written.
/// Casing and punctuation differences are word errors like any other.
///
/// This is deliberate — see the module doc for why a normalised WER cannot
/// see a formatter's output at all. `ra_wer` is what can.
///
/// **One pair only.** A corpus figure is built from [`ra_wer_counts`], not
/// by averaging this. The `unwrap_or(0.0)` below is the single-pair reading
/// of an empty reference *and* an empty hypothesis — nothing was said and
/// nothing was transcribed, so "zero errors" is faithful — and is exactly
/// the reading [`WerCounts::rate`] refuses to make for a corpus total.
pub fn ra_wer(reference: &str, hypothesis: &str) -> f64 {
    ra_wer_counts(reference, hypothesis).rate().unwrap_or(0.0)
}

/// Punctuation and symbol characters that [`normalize_token`] strips from a
/// token's edges, beyond the ASCII set [`char::is_ascii_punctuation`]
/// already covers. Drawn from the marks the 23 languages this app ships
/// actually write with — the same concern `eval::per`'s per-language mark
/// sets exist for — plus every non-ASCII **punctuation or symbol** observed
/// at a token edge in the committed fixtures: `…` in `earnings22.jsonl`
/// (4 occurrences), `।` in `indic-diarbench.jsonl` (284), `£` in
/// `earnings22-subset10.jsonl` (1).
///
/// **The list contains no combining mark, and it must not.** That property
/// is the entire point of enumerating punctuation instead of trimming
/// "everything non-alphanumeric"; see [`normalize_token`]. It is also why
/// the previous sentence says *punctuation or symbol* rather than "every
/// non-ASCII character": Devanagari combining marks sit at a token edge
/// **4 300 times** in this corpus across `input` and `target` (U+0940 ी,
/// U+0947 े, U+093E ा, U+0902 ं, U+094B ो, U+0948 ै, U+093F ि, U+0901 ँ,
/// U+0941 ु, U+0942 ू, U+093C ़, U+094C ौ) and every one of them must
/// survive, so "observed at a token edge" is a filter on candidates, never
/// the rule itself.
///
/// # Why currency signs are in a punctuation list
///
/// They are not punctuation, and they are here anyway, because
/// [`is_trimmable_punctuation`] already trims every character
/// [`char::is_ascii_punctuation`] accepts — and that set contains `$`.
/// Leaving `₹` out therefore did not mean "currency is content"; it meant
/// *dollars were content-free and rupees were not*, purely because of which
/// side of the ASCII boundary the glyph fell on. `cleanup::itn` writes
/// `$500`, `₹500` and `50%` from the same `match` arm, so that asymmetry hit
/// this app's own Indian-currency output and nothing else.
///
/// A sign joins this list when there is evidence for it: `₹` because
/// `cleanup/itn.rs` emits it, `£` because it occurs at a token edge in
/// `earnings22-subset10.jsonl` (`£1.60,`, in `input`, `target` and
/// `verbatim` alike). `€`, `¥` and the rest of the `Sc` category are
/// deliberately absent — nothing this app writes and nothing this corpus
/// contains uses them, and a symbol the tests never exercise is a guess. The
/// cost of omitting one is the safe direction (`content_wer` over-fires; see
/// [`normalize_token`]), which is the same trade the rest of this list makes.
const TRIMMABLE_PUNCTUATION: &[char] = &[
    // Typographic quotes and dashes (Latin prose, and what a formatter
    // converts ASCII quotes into).
    '\u{2018}', '\u{2019}', '\u{201A}', '\u{201B}', // ‘ ’ ‚ ‛
    '\u{201C}', '\u{201D}', '\u{201E}', '\u{201F}', // “ ” „ ‟
    '\u{2013}', '\u{2014}', '\u{2015}', '\u{2010}', '\u{2011}', '\u{2012}', // – — ― ‐ ‑ ‒
    '\u{2026}', // …
    '\u{00AB}', '\u{00BB}', '\u{2039}', '\u{203A}', // « » ‹ ›
    '\u{00A1}', '\u{00BF}', // ¡ ¿ (Spanish)
    '\u{00B7}', '\u{2022}', // · •
    // Brahmic sentence marks (Hindi, Marathi, Bengali, Gujarati, Punjabi,
    // Kannada, Malayalam, Tamil, Telugu, Odia — all shipped).
    '\u{0964}', '\u{0965}', // । ॥ danda, double danda
    '\u{0970}', // ॰ Devanagari abbreviation sign
    // Perso-Arabic (Urdu is shipped).
    '\u{060C}', '\u{061B}', '\u{061F}', '\u{06D4}', // ، ؛ ؟ ۔
    // Currency signs, for symmetry with ASCII `$` — see the doc above.
    '\u{20B9}', // ₹ (what `cleanup::itn` writes for "rupees")
    '\u{00A3}', // £ (observed at a token edge in earnings22-subset10)
    // CJK / fullwidth.
    '\u{3001}', '\u{3002}', // 、 。
    '\u{300C}', '\u{300D}', '\u{300E}', '\u{300F}', // 「 」 『 』
    '\u{FF01}', '\u{FF08}', '\u{FF09}', '\u{FF0C}', // ！ （ ） ，
    '\u{FF0E}', '\u{FF1A}', '\u{FF1B}', '\u{FF1F}', // ． ： ； ？
];

/// True for a character [`normalize_token`] will strip from a token edge:
/// ASCII punctuation, or one of [`TRIMMABLE_PUNCTUATION`].
fn is_trimmable_punctuation(c: char) -> bool {
    c.is_ascii_punctuation() || TRIMMABLE_PUNCTUATION.contains(&c)
}

/// Lowercases a token and trims **punctuation** from its **edges only** —
/// internal punctuation (`don't`, `well-known`, `3.14`) survives untouched,
/// matching how a real transcript uses punctuation mid-word rather than as
/// a word boundary. Returns `None` when nothing is left after trimming, so
/// a token that was pure punctuation (`"--"`, `"..."`) disappears from the
/// sequence entirely instead of surviving as an empty-string token that
/// would inflate the edit distance against whatever it happened to align
/// with.
///
/// # Why an explicit punctuation set, and not `!is_alphanumeric`
///
/// This used to trim every character for which [`char::is_alphanumeric`] is
/// false. That predicate is [`char::is_alphabetic`] (the Unicode
/// `Alphabetic` derived property) plus the numeric categories — and
/// `Alphabetic` **excludes** several combining marks that are ordinary
/// letters' spelling in Brahmic scripts. Enumerating the Devanagari block
/// under `rustc` 1.96.1, exactly nine of its 128 code points are
/// non-alphanumeric and so were being trimmed:
///
/// ```text
/// U+093C NUKTA   U+094D VIRAMA   U+0951..U+0954 (Vedic accents)
/// U+0964 DANDA   U+0965 DOUBLE DANDA            U+0970 ABBREVIATION SIGN
/// ```
///
/// Only the last three are punctuation. The first six are orthography, and
/// the virama is the one that mattered: `"सम्"` and `"सम"` both trimmed to
/// `"सम"`, so [`content_wer`] — the *regression guard* for content-word
/// drift — scored a dropped virama as zero errors. Measured on the
/// committed corpus, no `indic-diarbench.jsonl` target token ends in a
/// virama, but **six end in a nukta** (U+093C), which distinguishes ज from
/// ज़ and was being trimmed by the same rule. `U+200C ZWNJ` and `U+200D ZWJ`
/// — orthographically load-bearing across Indic scripts — were trimmed too.
///
/// Trimming a named punctuation set instead makes that whole class of miss
/// structurally impossible: no combining mark is in the set, so no
/// combining mark can be trimmed. The cost is the opposite error — a
/// punctuation mark this app writes but the set omits stays glued to its
/// word and reads as a content error. That is the direction a guard should
/// fail in.
///
/// Lowercasing goes through [`str::to_lowercase`] (full Unicode case
/// folding, not an ASCII-only shift), required by the same 23-language
/// constraint documented in `eval::per`.
///
/// `pub(super)` rather than private: `eval::disfluency` must compare its
/// removed-token multisets on *this exact* normalisation, or the formatter's
/// casing and punctuation mistakes get counted as disfluency-removal errors.
/// Two normalisers that were meant to agree and drifted would be a silent
/// wrong number, so there is one.
pub(super) fn normalize_token(word: &str) -> Option<String> {
    let trimmed = word.trim_matches(is_trimmable_punctuation);
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_lowercase())
    }
}

/// WER after lowercasing and stripping edge punctuation — the traditional
/// normalised metric, repurposed here as a regression guard rather than a
/// quality score.
///
/// On read speech the formatting engine must not touch content words, only
/// their casing and punctuation; `content_wer` strips exactly those two
/// things before scoring, so a healthy run should sit at ~0. A nonzero
/// value means a word was added, dropped, or substituted somewhere in the
/// pipeline — by the formatter itself, or an upstream stage such as
/// disfluency removal or ITN reaching further than it should have.
///
/// See the module doc for why this cannot, on its own, tell you anything
/// about formatting quality — a no-op formatter scores the same `~0` as a
/// perfect one here, by design.
///
/// **One pair only**; for a corpus use [`content_wer_counts`]. See
/// [`ra_wer`] for what the `unwrap_or(0.0)` means.
pub fn content_wer(reference: &str, hypothesis: &str) -> f64 {
    content_wer_counts(reference, hypothesis).rate().unwrap_or(0.0)
}

/// [`content_wer`]'s edit tally, for corpus aggregation. Sum these and call
/// [`WerCounts::rate`] once.
///
/// `reference_tokens` counts **normalised** tokens, so a reference of pure
/// punctuation contributes a zero denominator rather than a phantom one —
/// the same reason [`normalize_token`] drops those tokens instead of
/// keeping them as empty strings.
pub fn content_wer_counts(reference: &str, hypothesis: &str) -> WerCounts {
    let r: Vec<String> = reference
        .split_whitespace()
        .filter_map(normalize_token)
        .collect();
    let h: Vec<String> = hypothesis
        .split_whitespace()
        .filter_map(normalize_token)
        .collect();
    counts_of(&r, &h)
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- ra_wer: the whole point is that casing/punctuation count. ---

    #[test]
    fn ra_wer_counts_casing_and_punctuation_as_errors() {
        // This is the entire point: a normalised WER would score these
        // identical, which is why a normalised metric cannot see a
        // formatter at all.
        assert!(ra_wer("Hello, world.", "hello world") > 0.0);
    }

    #[test]
    fn ra_wer_identical_text_scores_zero() {
        assert_eq!(ra_wer("Hello, world.", "Hello, world."), 0.0);
    }

    /// Exact value, not just "> 0": one substitution ("Hello," vs "hello,")
    /// over a two-token reference.
    #[test]
    fn ra_wer_one_substitution_is_one_half() {
        assert!((ra_wer("Hello, world.", "hello, world.") - 0.5).abs() < 1e-9);
    }

    /// Exact value for a pure insertion: reference length 4, one extra
    /// hypothesis token, edit distance 1.
    #[test]
    fn ra_wer_one_insertion_over_four_reference_tokens() {
        assert!((ra_wer("the quick brown fox", "the quick brown fox jumps") - 0.25).abs() < 1e-9);
    }

    /// Exact value for a pure deletion: same shape, missing word instead of
    /// an extra one.
    #[test]
    fn ra_wer_one_deletion_over_four_reference_tokens() {
        assert!((ra_wer("the quick brown fox", "the quick fox") - 0.25).abs() < 1e-9);
    }

    // --- content_wer: casing/punctuation-blind, the regression guard. ---

    #[test]
    fn content_wer_ignores_casing_and_punctuation() {
        assert_eq!(content_wer("Hello, world.", "hello world"), 0.0);
    }

    /// The regression guard: on read speech the formatter must not touch
    /// content.
    #[test]
    fn content_wer_catches_a_dropped_word() {
        assert!((content_wer("send it to sam today", "send it to sam") - 0.2).abs() < 1e-9);
    }

    /// Internal punctuation is part of the word, not a boundary: `don't`
    /// and `well-known` must not be split or otherwise perturbed by
    /// normalisation.
    #[test]
    fn content_wer_keeps_internal_punctuation() {
        assert_eq!(
            content_wer("Well-known, don't you think?", "well-known don't you think"),
            0.0
        );
    }

    /// A token that is pure punctuation must vanish rather than survive as
    /// an empty string: if it didn't, this reference would normalise to
    /// three tokens (`"hello"`, `""`, `"world"`) against the hypothesis's
    /// two, forcing a spurious deletion and a nonzero WER despite the
    /// content being identical.
    #[test]
    fn content_wer_drops_a_punctuation_only_token_entirely() {
        assert_eq!(content_wer("hello -- world", "hello world"), 0.0);
    }

    // --- Degenerate lengths, documented rather than left to panic or NaN. ---

    #[test]
    fn empty_reference_and_hypothesis_score_zero_both_metrics() {
        assert_eq!(ra_wer("", ""), 0.0);
        assert_eq!(content_wer("", ""), 0.0);
    }

    /// An empty reference with a non-empty hypothesis is a genuine
    /// division by zero with a positive numerator — see [`WerCounts::rate`]'s
    /// doc for why that is `inf`, not `0.0` or `1.0`.
    #[test]
    fn empty_reference_with_nonempty_hypothesis_is_infinite() {
        assert!(ra_wer("", "hello world").is_infinite());
        assert!(content_wer("", "hello, world.").is_infinite());
    }

    // --- Counts: the corpus figure has to be micro, so counts must exist. ---

    /// The counts are the implementation, and the `f64` functions are a thin
    /// read of them, so they cannot disagree — but only if the bridge is the
    /// one asserted here. Covers a substitution, an insertion, a deletion
    /// and an exact match in one alignment each.
    #[test]
    fn the_f64_functions_are_exactly_their_counts_rated() {
        for (r, h) in [
            ("the quick brown fox", "the quick brown fox"),
            ("the quick brown fox", "the quick brown Fox"),
            ("the quick brown fox", "the quick brown fox jumps"),
            ("the quick brown fox", "the quick fox"),
            ("Hello, world.", "hello world"),
            ("नमस्ते\u{0964} दुनिया\u{0964}", "नमस्ते दुनिया"),
            ("", "hello world"),
        ] {
            assert_eq!(
                ra_wer_counts(r, h).rate().unwrap_or(0.0),
                ra_wer(r, h),
                "ra_wer disagrees with its counts on {r:?} vs {h:?}"
            );
            assert_eq!(
                content_wer_counts(r, h).rate().unwrap_or(0.0),
                content_wer(r, h),
                "content_wer disagrees with its counts on {r:?} vs {h:?}"
            );
        }
    }

    /// The breakdown must actually classify, not just total: one of each
    /// operation, checked field by field so a bug in any single field is
    /// visible rather than cancelled out inside the sum.
    #[test]
    fn counts_classify_substitutions_insertions_and_deletions() {
        // reference: a b c d   hypothesis: a X c d e   (missing nothing,
        // b -> X substituted, "e" inserted)
        assert_eq!(
            ra_wer_counts("a b c d", "a X c d e"),
            WerCounts {
                substitutions: 1,
                insertions: 1,
                deletions: 0,
                reference_tokens: 4,
            }
        );
        // A pure deletion, and the reference length is the reference's.
        assert_eq!(
            ra_wer_counts("a b c d", "a c d"),
            WerCounts {
                substitutions: 0,
                insertions: 0,
                deletions: 1,
                reference_tokens: 4,
            }
        );
        // Identical text: no operations at all, but a real denominator.
        assert_eq!(
            ra_wer_counts("a b c d", "a b c d"),
            WerCounts {
                substitutions: 0,
                insertions: 0,
                deletions: 0,
                reference_tokens: 4,
            }
        );
    }

    /// `content_wer_counts` counts NORMALISED reference tokens, so a
    /// punctuation-only token is not in the denominator: three whitespace
    /// tokens, two content tokens.
    #[test]
    fn content_counts_use_the_normalised_reference_length() {
        let c = content_wer_counts("hello -- world", "hello world");
        assert_eq!(c.reference_tokens, 2);
        assert_eq!(c.errors(), 0);
    }

    #[test]
    fn counts_sum_matches_manual_addition() {
        let a = WerCounts {
            substitutions: 2,
            reference_tokens: 10,
            ..Default::default()
        };
        let b = WerCounts {
            insertions: 1,
            deletions: 3,
            reference_tokens: 5,
            ..Default::default()
        };
        let summed: WerCounts = [a, b].into_iter().sum();
        assert_eq!(summed, a + b);

        let mut acc = WerCounts::default();
        acc += a;
        acc += b;
        assert_eq!(acc, summed);
        assert_eq!(
            summed,
            WerCounts {
                substitutions: 2,
                insertions: 1,
                deletions: 3,
                reference_tokens: 15,
            }
        );
        assert_eq!(summed.errors(), 6);
        assert_eq!(summed.rate(), Some(6.0 / 15.0));
    }

    /// The reason [`WerCounts`] exists at all: summing counts and dividing
    /// once is a *different number* from averaging the per-pair rates, and a
    /// benchmark that quotes the second while calling it WER is quoting
    /// something no published WER means.
    ///
    /// Here a twenty-token utterance with one error sits beside a two-token
    /// one with one error. Micro weights them by length (2/22); macro gives
    /// the tiny utterance equal say (≈0.275).
    #[test]
    fn micro_and_macro_wer_are_different_numbers() {
        let long_ref = "a b c d e f g h i j k l m n o p q r s t";
        let long_hyp = "a b c d e f g h i j k l m n o p q r s X";
        let short_ref = "yes no";
        let short_hyp = "yes NO";

        let long = ra_wer_counts(long_ref, long_hyp);
        let short = ra_wer_counts(short_ref, short_hyp);
        assert_eq!(long.reference_tokens, 20);
        assert_eq!(short.reference_tokens, 2);

        let macro_wer = (ra_wer(long_ref, long_hyp) + ra_wer(short_ref, short_hyp)) / 2.0;
        let micro_wer = [long, short].into_iter().sum::<WerCounts>().rate().unwrap();

        assert!((macro_wer - (0.05 + 0.5) / 2.0).abs() < 1e-9, "{macro_wer}");
        assert!((micro_wer - 2.0 / 22.0).abs() < 1e-9, "{micro_wer}");
        assert!(
            (macro_wer - micro_wer).abs() > 0.15,
            "the two formulas must actually disagree, not just be spelled \
             differently: macro {macro_wer}, micro {micro_wer}"
        );
    }

    /// The one deliberate divergence between the two APIs. For a single
    /// pair, "" vs "" is faithfully zero errors. For a corpus TOTAL, a zero
    /// denominator is "nothing was measured" and printing `0.000` would read
    /// as a flawless WER over text that was never scored.
    #[test]
    fn an_empty_corpus_has_no_wer_but_an_empty_pair_scores_zero() {
        assert_eq!(WerCounts::default().rate(), None);
        assert_eq!(ra_wer("", ""), 0.0);
        assert_eq!(content_wer("", ""), 0.0);

        // Errors with no reference to divide by stays loud rather than
        // becoming an "unmeasurable".
        let orphan = ra_wer_counts("", "hello world");
        assert_eq!(orphan.reference_tokens, 0);
        assert_eq!(orphan.insertions, 2);
        assert_eq!(orphan.rate(), Some(f64::INFINITY));
    }

    // --- The regression guard must be able to FIRE on Indic content. ---
    //
    // Indic is a regression guard for this project, not a quality target,
    // which makes a guard that structurally cannot fire the worst possible
    // outcome here: it reads as a pass. These pin the class of error the old
    // `!is_alphanumeric` trimmer could not see.

    /// A dropped virama (U+094D) changes the word — `सम्` is not `सम` — and
    /// `content_wer` exists to catch exactly that class of drift.
    ///
    /// This failed before the trimmer stopped stripping every
    /// non-alphanumeric character: `char::is_alphanumeric` is false for the
    /// virama (it is `Mn` and not in Unicode's `Other_Alphabetic`), so both
    /// sides normalised to `सम` and the metric reported a flawless 0.0.
    #[test]
    fn a_dropped_virama_is_a_content_error_not_a_silent_pass() {
        assert_eq!(normalize_token("सम\u{094D}").as_deref(), Some("सम\u{094D}"));
        assert!(content_wer("सम\u{094D}", "सम") > 0.0);
    }

    /// Same mechanism, and this one is present in the committed corpus:
    /// six `indic-diarbench.jsonl` target tokens end in a nukta (U+093C),
    /// which is what distinguishes ज from ज़.
    #[test]
    fn a_dropped_nukta_is_a_content_error_not_a_silent_pass() {
        let with_nukta = "ज\u{093C}";
        assert_eq!(normalize_token(with_nukta).as_deref(), Some(with_nukta));
        assert!(content_wer(with_nukta, "ज") > 0.0);
    }

    /// ZWNJ/ZWJ are orthography in Indic scripts, not punctuation, and the
    /// old trimmer removed them from token edges too.
    #[test]
    fn zero_width_joiners_are_not_trimmed_as_punctuation() {
        assert!(!is_trimmable_punctuation('\u{200C}'));
        assert!(!is_trimmable_punctuation('\u{200D}'));
        assert!(!is_trimmable_punctuation('\u{094D}'));
        assert!(!is_trimmable_punctuation('\u{093C}'));
    }

    /// The other half of the trade: the danda IS punctuation and must still
    /// be trimmed, or every Hindi sentence-final word would read as a
    /// content error against an unpunctuated hypothesis. Regression guard
    /// against "fixing" the virama by simply trimming nothing.
    #[test]
    fn devanagari_punctuation_is_still_trimmed() {
        assert_eq!(content_wer("नमस्ते दुनिया।", "नमस्ते दुनिया"), 0.0);
        assert_eq!(content_wer("नमस्ते, दुनिया॥", "नमस्ते दुनिया"), 0.0);
    }

    /// CJK and Perso-Arabic sentence marks are in the trimmable set for the
    /// same reason the danda is — this app ships those scripts.
    ///
    /// Written as escapes rather than literals: the Urdu pair is RTL, and a
    /// reader cannot tell from the rendered glyphs which end of the string
    /// the question mark is actually on. CJK gets no whitespace, so both
    /// sides here are a single token and the test is about the trailing
    /// mark, not about tokenisation.
    #[test]
    fn non_latin_punctuation_beyond_devanagari_is_trimmed() {
        // 你好，世界。 vs 你好，世界 — U+3002 IDEOGRAPHIC FULL STOP dropped.
        assert_eq!(
            content_wer("\u{4F60}\u{597D}\u{FF0C}\u{4E16}\u{754C}\u{3002}", "\u{4F60}\u{597D}\u{FF0C}\u{4E16}\u{754C}"),
            0.0
        );
        // ٹھیک ہے؟ vs ٹھیک ہے — U+061F ARABIC QUESTION MARK dropped.
        assert_eq!(
            content_wer(
                "\u{0679}\u{06BE}\u{06CC}\u{06A9} \u{06C1}\u{06D2}\u{061F}",
                "\u{0679}\u{06BE}\u{06CC}\u{06A9} \u{06C1}\u{06D2}"
            ),
            0.0
        );
    }

    /// Documented blind spot #2 from the module doc, pinned as behaviour so
    /// it stays a known limit rather than becoming a mystery: `ज़` spelled
    /// precomposed (U+095B, the form 40 of the 200 committed Hindi targets
    /// use) and spelled as its NFC decomposition (`ज` + nukta) are different
    /// token strings, and this crate has no NFC normaliser to reconcile
    /// them.
    ///
    /// If a future stage starts normalising text, this test failing is the
    /// signal that `content_wer` needs a real normaliser rather than that
    /// the formatter broke.
    #[test]
    fn nfc_and_nfd_spellings_of_the_same_word_do_not_compare_equal() {
        let precomposed = "\u{095B}"; // ज़
        let decomposed = "\u{091C}\u{093C}"; // ज + nukta, the NFC form
        assert_ne!(precomposed, decomposed);
        assert!(
            content_wer(precomposed, decomposed) > 0.0,
            "known limit: no NFC normalisation, so canonical-form drift reads \
             as a content error"
        );
    }

    /// The safe-direction cost of an explicit punctuation set, stated as a
    /// test so nobody has to rediscover it: a mark the set omits stays glued
    /// to its word. U+2E2E REVERSED QUESTION MARK is deliberately not in the
    /// set (no shipped language writes it), so it reads as a content error.
    /// A guard that over-fires is recoverable; one that under-fires is not.
    #[test]
    fn punctuation_outside_the_set_reads_as_a_content_error() {
        assert!(!is_trimmable_punctuation('\u{2E2E}'));
        assert!(content_wer("hello\u{2E2E}", "hello") > 0.0);
    }

    // --- Currency: `$` and `₹` must behave the same way. ---

    /// `cleanup::itn` writes `$500`, `₹500` and `50%` from one `match` arm,
    /// but only `₹` fell outside [`char::is_ascii_punctuation`] — so
    /// `content_wer` treated the dollar figure as content-free and the rupee
    /// figure as a content error, on output the same code path produced.
    /// Both sides of the pair are asserted so "fixing" this by making `$`
    /// stop trimming would fail too.
    #[test]
    fn rupee_and_dollar_amounts_normalise_the_same_way() {
        assert!(is_trimmable_punctuation('$'), "ASCII punctuation already");
        assert!(is_trimmable_punctuation('\u{20B9}'), "₹ must match it");
        assert_eq!(normalize_token("$500.").as_deref(), Some("500"));
        assert_eq!(normalize_token("\u{20B9}500.").as_deref(), Some("500"));
        // `cleanup::mod`'s own golden case: "send five hundred rupees" ->
        // "Send ₹500." The content words are `send` and `500` either way.
        assert_eq!(content_wer("Send \u{20B9}500.", "send 500"), 0.0);
        assert_eq!(content_wer("Send $500.", "send 500"), 0.0);
    }

    /// `£1.60,` is in `earnings22-subset10.jsonl` — `input`, `target` and
    /// `verbatim` alike — so this one is not a hypothetical: before the fix
    /// the pound sign stayed glued and the token could never compare equal
    /// to an unadorned amount.
    #[test]
    fn the_pound_sign_the_corpus_actually_contains_is_trimmed() {
        assert!(is_trimmable_punctuation('\u{00A3}'));
        assert_eq!(normalize_token("\u{00A3}1.60,").as_deref(), Some("1.60"));
    }

    /// Adding currency signs must not have smuggled orthography into the
    /// set — the property the whole enumerate-don't-predicate design rests
    /// on. Checked over the WHOLE list rather than the two new entries, so a
    /// future addition is covered without a new test.
    ///
    /// The mark list is not a sample: it is every non-alphanumeric character
    /// the committed corpus actually places at a token edge (4 300
    /// occurrences across `input` and `target`), plus ZWNJ/ZWJ. `rustc` has
    /// no "is a combining mark" predicate, so the property is asserted the
    /// way the bug manifested.
    #[test]
    fn no_trimmable_character_is_orthography() {
        const ORTHOGRAPHY: &[char] = &[
            '\u{093C}', '\u{094D}', '\u{0902}', '\u{0901}', '\u{093E}', '\u{093F}', '\u{0940}',
            '\u{0941}', '\u{0942}', '\u{0947}', '\u{0948}', '\u{094B}', '\u{094C}', '\u{200C}',
            '\u{200D}',
        ];
        for &c in TRIMMABLE_PUNCTUATION {
            assert!(
                !c.is_alphanumeric(),
                "U+{:04X} is alphanumeric and must not be trimmed",
                u32::from(c)
            );
            assert!(
                !ORTHOGRAPHY.contains(&c),
                "U+{:04X} is orthography, not punctuation",
                u32::from(c)
            );
        }
        // ...and the marks themselves must stay untrimmable, which is what
        // the caller actually depends on.
        for &c in ORTHOGRAPHY {
            assert!(
                !is_trimmable_punctuation(c),
                "U+{:04X} must survive normalisation",
                u32::from(c)
            );
        }
    }
}
