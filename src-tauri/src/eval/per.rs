// Adapted from NVIDIA NeMo punct_er.py (Apache-2.0); see THIRD_PARTY_NOTICES.md.
// Copyright (c) 2023, NVIDIA CORPORATION & AFFILIATES. All rights reserved.
//! Punctuation Error Rate, per LibriSpeech-PC (Meister et al., ICASSP 2024).
//!
//!   PER = (I_P + D_P + S_P) / (I_P + D_P + S_P + C_P)
//!   D_P = N_P,ref − (S_P + C_P)
//!   I_P = N_P,hyp − (S_P + C_P)
//!
//! Note the denominator is punctuation *operations*, not reference length as
//! in WER, so PER is exactly 1 − accuracy over punctuation slots.
//!
//! ## What `tools/per_oracle.py` is, and when it runs
//!
//! The oracle is a second, independent implementation of the same NeMo
//! source. It is **not** wired into `cargo test`'s default run and nothing in
//! the shipped binary calls it. It runs in exactly one place: the
//! `#[ignore]`d tests in the `oracle` module at the bottom of this file,
//! which shell out to `python3` and diff its counts against ours. They are
//! `#[ignore]`d because they need a Python 3 on PATH, which a plain
//! `cargo test` cannot assume. Run them deliberately:
//!
//! ```text
//! cd src-tauri && cargo test --lib eval::per -- --ignored
//! ```
//!
//! Those tests are the only thing that makes any agreement claim in this file
//! checkable. Everything below is a statement about what they assert, and
//! they will fail if it stops being true — do not weaken a claim here
//! without changing the test that pins it.
//!
//! The harness runs the oracle the way a human would, on purpose: `spawn`
//! *clears* `PYTHONUTF8` and `PYTHONIOENCODING` instead of setting them. It
//! used to set both, which meant these tests were the only caller decoding
//! UTF-8 correctly — the manual invocation the oracle's own docstring
//! documents got no such help, mojibaked every non-ASCII mark under cp1252,
//! and scored a Devanagari pair with a dropped danda as flawless. The oracle
//! decodes its own stdin now, and refuses (non-zero exit, not a 0.000)
//! when `--marks` is empty or matches nothing in the input. See
//! `oracle_scores_non_ascii_stdin_instead_of_silently_perfecting_it`,
//! `oracle_refuses_a_mark_set_that_never_occurs_in_the_input` and
//! `oracle_refuses_an_empty_mark_set`.
//!
//! ## Where the two implementations agree — measured, not assumed
//!
//! Agreement is **not** "ASCII/Latin vs. everything else." The real boundary
//! is a Unicode-category one, and it cuts straight through ASCII.
//!
//! The oracle tokenizes with NeMo's regex `[\w']+|[marks]` and `re.findall`,
//! which returns *only* substrings matching one of those two alternatives:
//! any other character is silently **dropped**. Our tokenizer pushes every
//! non-mark, non-whitespace `char` into the current word, so it **glues**
//! that same character in. Verified agreement zone, by cross-producting a
//! 22-string corpus into 484 pairs (`oracle_agrees_on_the_clean_zone`):
//!
//! > When every character of both strings is matched by Python's `\w`, or is
//! > an ASCII apostrophe, whitespace, or one of `marks`, the counts agree
//! > exactly. 484/484 pairs, zero divergence — Latin, Cyrillic and CJK.
//!
//! That one is stronger than a sample: inside that zone the two tokenizers
//! emit *identical* token streams (a maximal `[\w']+` run is exactly what we
//! accumulate between whitespace and marks), and everything downstream is a
//! line-for-line port with the same tie-break order. The 484 pairs confirm
//! it; they are not the whole argument.
//!
//! Outside that zone the two tokenize differently *always*, but the counts
//! only diverge when the extra/missing word tokens shift which marks the DP
//! aligns against each other — which needs the reference and hypothesis to
//! differ in words, not just in punctuation. Measured:
//!
//! - **Latin, 1089 pairs** (33-string corpus of ordinary English with `-`,
//!   `"`, `(`, `$` and the typographic `’ – — …`): 10 diverged; corpus PER
//!   0.5037 here vs 0.5097 for the oracle = **0.60 PER points**
//!   (`oracle_ascii_divergence_is_small_but_real`). The two worst pairs are
//!   *pure ASCII*, which is what kills the old "ASCII/Latin agrees" claim.
//!   Smallest: ref `a-b.` / hyp `a-, b` → here `S=1`, oracle `I=1, D=1`.
//!   Worst: ref `"ok", he said.` / hyp `a-b, c.` → here `C=2` (rate 0.000),
//!   oracle `C=1, I=1, D=1` (rate 0.667). Both pinned in
//!   `oracle_disagrees_outside_the_clean_zone`, and our half again in
//!   `a_non_word_character_is_glued_into_the_word_not_dropped` so the
//!   default, Python-free test run still guards it.
//! - **Devanagari, reference and hypothesis differing only in punctuation**
//!   (12 realistic pairs): 0 diverged, **0.00 PER points** — identical
//!   counts, not merely a close rate
//!   (`oracle_indic_agrees_when_only_punctuation_differs`).
//! - **Devanagari, words differing too** (20-string corpus, 400 pairs): 20
//!   diverged; corpus PER 0.5886 here vs 0.6308 for the oracle = **4.22 PER
//!   points** (`oracle_indic_divergence_is_material`).
//!
//! Those figures are the output of the tests named above, not a one-off
//! measurement written down. The corpus-gap numbers are pinned as bands
//! rather than literals, because the exact value is a property of the
//! corpus; the divergence *counts* are pinned exactly — 10 of 1089 in
//! `oracle_ascii_divergence_is_small_but_real`, 20 of 400 in
//! `oracle_indic_divergence_is_material`, 0 of 12 in
//! `oracle_indic_agrees_when_only_punctuation_differs` (which asserts every
//! pair individually).
//!
//! So: an Indic PER quoted from NeMo can be over 4 points off ours on a
//! corpus with real word errors in it — the size of an entire model
//! generation — while looking identical on a corpus where only punctuation
//! moves. Any comparison against a published NeMo number has to say which.
//!
//! ## The Indic divergence is NeMo's bug, not ours
//!
//! Python's `\w` is Unicode-aware for *base* letters but excludes the
//! combining-mark categories Mn and Mc — exactly what Devanagari vowel signs
//! (matras) and the virama are. Because those sit *inside* words, dropping
//! them splits the word: the oracle turns "रिपोर्ट" (report) into
//! `["र","प","र","ट"]` and "नमस्ते" (namaste) into `["नमस","त"]`. Our
//! tokenizer keeps both intact.
//!
//! **That is the correct behaviour. Do not "fix" this file to reproduce
//! NeMo's Indic tokenization** — see
//! `devanagari_words_are_not_shattered_by_combining_marks`, which pins it
//! against exactly that regression. This app supports 23 languages, several
//! Indic, so the 4.22-point gap above is load-bearing, not academic.
//!
//! It is the Unicode category that matters, not the script, so Latin is not
//! exempt either: decomposed (NFD) text hits the same regex. The oracle
//! tokenizes NFD "naïve" (n a i U+0308 v e) as `["nai","ve"]` and NFD "café"
//! as `["cafe"]` — word-internal accents split, trailing ones just vanish.
//! Probed on five NFC/NFD pairs and the counts still agreed, because neither
//! split moved a mark; Latin is not immune here, only luckier, and that luck
//! is not asserted anywhere.

// Every item below is reachable only from `#[cfg(test)]` right now, so a
// plain `cargo build`/`cargo check` (no test cfg) sees the whole module as
// unreachable. File-wide is the deliberate choice over scattering
// `#[allow(dead_code)]` across each of the seven items here individually:
// with everything dead, per-item annotations would carry zero extra
// information over this one line, for seven more lines of noise. Contrast
// `sarvam::codec::ClientMsg::Ping`, which uses a genuine per-item
// `#[allow(dead_code)]` — there only ONE variant is unreachable while its
// siblings are live, so the annotation actually distinguishes something.
//
// `fmtbench` calling `punctuation_error_rate` does not lift this: it compiles
// this file into its own crate through a `#[path]` include, because `lib.rs`
// declares `mod eval;` privately. The allow comes off when something in the
// shipped app scores text, or if `eval` is ever made `pub`.
#![allow(dead_code)]

/// The four punctuation operation counts a single (reference, hypothesis)
/// pair produces.
///
/// # Corpus PER: sum the counts, then take ONE ratio
///
/// A corpus-level PER is the ratio of *summed* counts. It is **never** the
/// mean of per-sentence [`rate`](PunctCounts::rate)s — those disagree
/// whenever the pairs carry different amounts of punctuation, because
/// averaging rates silently gives a one-mark sentence the same weight as a
/// twenty-mark one. `corpus_per_sums_counts_it_does_not_average_rates` proves
/// the two formulas land on different numbers (0.55 vs 0.18) for the same two
/// pairs.
///
/// [`Sum`](std::iter::Sum) exists so the correct formula is the short one:
///
/// ```ignore
/// pairs.iter()
///     .map(|(r, h)| punctuation_error_rate(r, h, marks))
///     .sum::<PunctCounts>()
///     .rate()
/// ```
///
/// `tools/per_oracle.py` prints the same aggregate for the same reason, and
/// its module docstring carries the same warning on the Python side.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct PunctCounts {
    pub correct: u32,
    pub substitutions: u32,
    pub insertions: u32,
    pub deletions: u32,
}

impl PunctCounts {
    /// `(I + D + S) / (I + D + S + C)` — 1 − accuracy over punctuation slots.
    ///
    /// Aggregate a corpus by summing counts and calling this **once**; see
    /// the type-level docs. `rate()` on one pair, averaged, is a different
    /// and wrong number.
    ///
    /// Returns `0.0` when there were no punctuation operations at all. That
    /// is a genuine 0/0 and it means *"neither side had any punctuation"* —
    /// not *"the hypothesis punctuated perfectly."* A caller that reports
    /// this number to anyone should check
    /// `correct + substitutions + insertions + deletions == 0` first and say
    /// "n/a" instead, or the empty case will read as a flawless score.
    pub fn rate(&self) -> f64 {
        let errors = self.insertions + self.deletions + self.substitutions;
        let total = errors + self.correct;
        if total == 0 {
            return 0.0;
        }
        errors as f64 / total as f64
    }
}

impl std::ops::Add for PunctCounts {
    type Output = PunctCounts;

    fn add(self, other: PunctCounts) -> PunctCounts {
        PunctCounts {
            correct: self.correct + other.correct,
            substitutions: self.substitutions + other.substitutions,
            insertions: self.insertions + other.insertions,
            deletions: self.deletions + other.deletions,
        }
    }
}

impl std::ops::AddAssign for PunctCounts {
    fn add_assign(&mut self, other: PunctCounts) {
        *self = *self + other;
    }
}

impl std::iter::Sum for PunctCounts {
    fn sum<I: Iterator<Item = PunctCounts>>(iter: I) -> PunctCounts {
        iter.fold(PunctCounts::default(), std::ops::Add::add)
    }
}

#[derive(Clone, Debug, PartialEq)]
enum Tok {
    Word(String),
    Mark(char),
}

/// Splits on whitespace and on `marks`. Every other character — hyphen,
/// quote, dash, combining mark — is glued into the current word rather than
/// dropped; that is where we diverge from `per_oracle.py`, deliberately (see
/// the module doc).
fn tokenize(text: &str, marks: &[char]) -> Vec<Tok> {
    let mut out = Vec::new();
    let mut buf = String::new();
    for ch in text.chars() {
        if marks.contains(&ch) {
            if !buf.is_empty() {
                out.push(Tok::Word(std::mem::take(&mut buf)));
            }
            out.push(Tok::Mark(ch));
        } else if ch.is_whitespace() {
            if !buf.is_empty() {
                out.push(Tok::Word(std::mem::take(&mut buf)));
            }
        } else {
            buf.push(ch);
        }
    }
    if !buf.is_empty() {
        out.push(Tok::Word(buf));
    }
    out
}

/// Every mark collapses to one label so punctuation aligns against
/// punctuation instead of being swallowed by a word substitution.
///
/// The label is a magic string, so a word token that literally *is*
/// `"\0PUNCT"` would mask-compare equal to a mark. `per_oracle.py` uses a
/// unique `object()` and cannot collide at all — the one algorithmic (rather
/// than tokenizer) difference between the two. It is unreachable from the
/// agreement zone described in the module doc, since U+0000 is not a `\w`
/// character, so it does not weaken any claim there.
fn mask(t: &Tok) -> &str {
    match t {
        Tok::Word(w) => w.as_str(),
        Tok::Mark(_) => "\u{0}PUNCT",
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Op {
    Cor,
    Sub,
    Ins,
    Del,
}

/// Counts punctuation operations between `reference` and `hypothesis`,
/// scoring only the characters listed in `marks`.
///
/// # Panics
///
/// If `marks` is empty — in **both** debug and release builds, on purpose.
///
/// With no marks, nothing tokenizes to a mark, every count stays zero, and
/// [`PunctCounts::rate`] returns `0.0`: a perfect score, byte-identical to
/// [`PunctCounts::default`]. For a metric whose output leaves this repo,
/// silently reporting a flawless PER because the mark set was misconfigured
/// is the worst failure mode available.
///
/// It is a real `assert!` rather than `debug_assert!` because benchmark
/// numbers get produced by `--release` builds, where `debug_assert!`
/// compiles to nothing and the silent perfect score comes back. A guard that
/// is absent from the profile people actually measure in is not a guard.
///
/// Chosen over the alternatives for reasons specific to this argument:
///
/// - `marks` is static configuration (a language's mark set, a `const` at
///   every call site), not data arriving from a user or the network. An
///   empty one is a programmer error with no recovery branch — there is
///   nothing for a caller to do about it except not do it.
/// - Returning `Result` would be *worse than nothing here*: the ergonomic
///   thing to write inside an aggregation loop is `.unwrap_or_default()`,
///   and `PunctCounts::default()` is precisely the silent perfect score this
///   guard exists to prevent. It offers a one-word way to reintroduce the
///   bug.
/// - A `MarkSet` newtype making the empty set unconstructible is the
///   strongest option and was the close call. Rejected because it still
///   fails at run time (`MarkSet::new(&[]) -> None`), not compile time, so
///   it buys no extra guarantee over this assert; it changes the interface
///   `fmtbench` is written against; and it pushes a constructor
///   onto every call site to re-check a value that is a literal in all of
///   them.
pub fn punctuation_error_rate(reference: &str, hypothesis: &str, marks: &[char]) -> PunctCounts {
    assert!(
        !marks.is_empty(),
        "punctuation_error_rate called with an empty mark set: every count \
         comes back zero, which is indistinguishable from text that \
         legitimately has no punctuation (PunctCounts::default()). Pass at \
         least one mark."
    );
    let r = tokenize(reference, marks);
    let h = tokenize(hypothesis, marks);
    let (rn, hn) = (r.len(), h.len());

    let mut cost = vec![vec![0usize; hn + 1]; rn + 1];
    let mut back = vec![vec![Op::Cor; hn + 1]; rn + 1];
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
            if mask(&r[i - 1]) == mask(&h[j - 1]) {
                cost[i][j] = cost[i - 1][j - 1];
                back[i][j] = Op::Cor;
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

    let mut counts = PunctCounts::default();
    let (mut i, mut j) = (rn, hn);
    while i > 0 || j > 0 {
        match back[i][j] {
            Op::Cor => {
                if matches!(r[i - 1], Tok::Mark(_)) || matches!(h[j - 1], Tok::Mark(_)) {
                    if r[i - 1] == h[j - 1] {
                        counts.correct += 1;
                    } else {
                        counts.substitutions += 1;
                    }
                }
                i -= 1;
                j -= 1;
            }
            Op::Sub => {
                i -= 1;
                j -= 1;
            }
            Op::Ins => j -= 1,
            Op::Del => i -= 1,
        }
    }

    // Deletions and insertions are DERIVED, not read off the backtrace.
    let n_ref = r.iter().filter(|t| matches!(t, Tok::Mark(_))).count() as u32;
    let n_hyp = h.iter().filter(|t| matches!(t, Tok::Mark(_))).count() as u32;
    let matched = counts.substitutions + counts.correct;
    counts.deletions = n_ref.saturating_sub(matched);
    counts.insertions = n_hyp.saturating_sub(matched);
    counts
}

#[cfg(test)]
mod tests {
    use super::*;

    const MARKS: &[char] = &['.', ',', '?'];

    // Every case below asserts the WHOLE `PunctCounts`, not one field: with
    // insertions and deletions derived from the same `matched` total, a bug
    // that inflates `correct` moves three fields at once, and a
    // single-field assertion would sail past it.

    #[test]
    fn identical_text_has_no_punctuation_errors() {
        let c = punctuation_error_rate("Hello, world.", "Hello, world.", MARKS);
        assert_eq!(
            c,
            PunctCounts {
                correct: 2,
                substitutions: 0,
                insertions: 0,
                deletions: 0
            }
        );
        assert_eq!(c.rate(), 0.0);
    }

    #[test]
    fn a_missing_mark_is_a_deletion() {
        let c = punctuation_error_rate("Hello, world.", "Hello world.", MARKS);
        assert_eq!(
            c,
            PunctCounts {
                correct: 1,
                substitutions: 0,
                insertions: 0,
                deletions: 1
            }
        );
        assert!((c.rate() - 0.5).abs() < 1e-9);
    }

    #[test]
    fn a_spurious_mark_is_an_insertion() {
        let c = punctuation_error_rate("Hello world.", "Hello, world.", MARKS);
        assert_eq!(
            c,
            PunctCounts {
                correct: 1,
                substitutions: 0,
                insertions: 1,
                deletions: 0
            }
        );
        assert!((c.rate() - 0.5).abs() < 1e-9);
    }

    /// The mark is in the right place but is the wrong mark.
    #[test]
    fn a_wrong_mark_in_the_right_place_is_a_substitution() {
        let c = punctuation_error_rate("Are you there?", "Are you there.", MARKS);
        assert_eq!(
            c,
            PunctCounts {
                correct: 0,
                substitutions: 1,
                insertions: 0,
                deletions: 0
            }
        );
        assert!((c.rate() - 1.0).abs() < 1e-9);
    }

    /// The masking step is what makes this work: without it, punctuation gets
    /// absorbed into word substitutions and never aligns against punctuation.
    #[test]
    fn punctuation_aligns_even_when_words_differ() {
        let c = punctuation_error_rate("The cat, obviously.", "A dog, obviously.", MARKS);
        assert_eq!(
            c,
            PunctCounts {
                correct: 2,
                substitutions: 0,
                insertions: 0,
                deletions: 0
            }
        );
        assert_eq!(c.rate(), 0.0);
    }

    #[test]
    fn text_without_any_punctuation_scores_zero_over_zero_as_zero() {
        let c = punctuation_error_rate("hello world", "hello world", MARKS);
        // Full struct, not just `rate()`: rate() is 0/0 for ANY all-zero
        // count set, so asserting only the rate would also pass a bug that
        // produced a bogus `correct: 5` alongside zero errors.
        assert_eq!(c, PunctCounts::default());
    }

    // --- Aggregation: corpus PER is summed counts, not averaged rates. ---

    #[test]
    fn punct_counts_sum_matches_manual_addition() {
        let a = PunctCounts {
            correct: 2,
            ..Default::default()
        };
        let b = PunctCounts {
            insertions: 1,
            ..Default::default()
        };
        let c = PunctCounts {
            deletions: 3,
            ..Default::default()
        };
        let summed: PunctCounts = [a, b, c].into_iter().sum();
        assert_eq!(summed, a + b + c);
        assert_eq!(
            summed,
            PunctCounts {
                correct: 2,
                substitutions: 0,
                insertions: 1,
                deletions: 3
            }
        );
    }

    /// `+=` is what an aggregation loop reaches for when it isn't using
    /// `.sum()`; it must land on the same numbers as both other paths, or a
    /// corpus total silently depends on which one the caller picked.
    #[test]
    fn add_assign_accumulates_the_same_totals_as_add_and_sum() {
        let a = PunctCounts {
            correct: 2,
            substitutions: 1,
            insertions: 0,
            deletions: 0,
        };
        let b = PunctCounts {
            correct: 1,
            substitutions: 0,
            insertions: 3,
            deletions: 4,
        };

        let mut acc = PunctCounts::default();
        acc += a;
        acc += b;

        assert_eq!(
            acc,
            PunctCounts {
                correct: 3,
                substitutions: 1,
                insertions: 3,
                deletions: 4
            }
        );
        assert_eq!(acc, a + b);
        assert_eq!(acc, [a, b].into_iter().sum::<PunctCounts>());
    }

    /// The oracle's module docstring warns of exactly this: corpus PER is
    /// `counts.sum().rate()`, never `rates.iter().sum::<f64>() / n`. This
    /// proves the two formulas actually disagree on real numbers, not just
    /// in theory — a short pair with one small error and a long pair with
    /// one small error do NOT deserve equal weight.
    #[test]
    fn corpus_per_sums_counts_it_does_not_average_rates() {
        let short = PunctCounts {
            correct: 9,
            deletions: 1,
            ..Default::default()
        };
        let long = PunctCounts {
            substitutions: 1,
            ..Default::default()
        };

        let averaged = (short.rate() + long.rate()) / 2.0;
        assert!((averaged - 0.55).abs() < 1e-9);

        let corpus: PunctCounts = [short, long].into_iter().sum();
        assert_eq!(
            corpus,
            PunctCounts {
                correct: 9,
                substitutions: 1,
                insertions: 0,
                deletions: 1
            }
        );
        assert!((corpus.rate() - 2.0 / 11.0).abs() < 1e-9);

        assert!(
            (averaged - corpus.rate()).abs() > 0.3,
            "the two formulas must actually disagree, not just be spelled differently"
        );
    }

    // --- An empty mark set is a caller bug, not a legitimate 0/0. ---

    /// Must hold in `--release` too — see the `# Panics` section on
    /// `punctuation_error_rate` for why this is `assert!`, not
    /// `debug_assert!`. Under `debug_assert!` this test passes in debug and
    /// FAILS in release, which is the profile benchmark numbers come from.
    #[test]
    #[should_panic(expected = "empty mark set")]
    fn empty_mark_set_panics_instead_of_silently_scoring_zero() {
        punctuation_error_rate("Hello, world.", "Hello world!", &[]);
    }

    // --- Unicode coverage: the mark set is a parameter for a reason. ---

    /// NeMo's tokenizer regex is `[\w']+|[marks]`, and Python's `\w` excludes
    /// the Unicode combining-mark categories Mn/Mc that Devanagari vowel
    /// signs and virama are made of, so NeMo's own tokenizer shatters every
    /// matra-bearing word: "रिपोर्ट" (report) -> `["र","प","र","ट"]`,
    /// "नमस्ते" (namaste) -> `["नमस","त"]` (verified directly against the
    /// vendored regex in `tools/per_oracle.py`). That is a NeMo bug for
    /// Indic scripts, not a target: this pins OUR tokenizer keeping such
    /// words intact. If a "fix toward NeMo" ever makes this fail by
    /// fragmenting these words, that change is moving away from
    /// correctness — see the module doc at the top of this file.
    #[test]
    fn devanagari_words_are_not_shattered_by_combining_marks() {
        assert_eq!(
            tokenize("रिपोर्ट", &[]),
            vec![Tok::Word("रिपोर्ट".to_string())]
        );
        assert_eq!(tokenize("नमस्ते", &[]), vec![Tok::Word("नमस्ते".to_string())]);
    }

    #[test]
    fn devanagari_danda_is_a_recognized_mark() {
        const HI: &[char] = &['।', ','];
        let c = punctuation_error_rate("नमस्ते, दुनिया।", "नमस्ते दुनिया।", HI);
        // Comma dropped (deletion), danda survives (correct). Both
        // matra-bearing words stay intact through tokenization — see
        // `devanagari_words_are_not_shattered_by_combining_marks` — so this
        // is genuinely exercising mark alignment, not word drift.
        assert_eq!(
            c,
            PunctCounts {
                correct: 1,
                substitutions: 0,
                insertions: 0,
                deletions: 1
            }
        );
    }

    #[test]
    fn fullwidth_cjk_terminator_is_a_deletion_when_dropped() {
        const ZH: &[char] = &['。', '，', '？'];
        let c = punctuation_error_rate("你好，世界。", "你好，世界", ZH);
        assert_eq!(
            c,
            PunctCounts {
                correct: 1,
                substitutions: 0,
                insertions: 0,
                deletions: 1
            }
        );
    }

    #[test]
    fn fullwidth_cjk_wrong_mark_is_a_substitution() {
        const ZH: &[char] = &['。', '，', '？'];
        let c = punctuation_error_rate("你好吗？", "你好吗。", ZH);
        assert_eq!(
            c,
            PunctCounts {
                correct: 0,
                substitutions: 1,
                insertions: 0,
                deletions: 0
            }
        );
    }

    /// Our half of the documented ASCII divergence from `per_oracle.py`,
    /// pinned WITHOUT needing Python so the default test run protects it.
    /// The oracle's half is asserted in
    /// `oracle::oracle_disagrees_outside_the_clean_zone`; if either side
    /// moves, the module doc's numbers are wrong and one of these two tests
    /// says so.
    #[test]
    fn a_non_word_character_is_glued_into_the_word_not_dropped() {
        // `re.findall(r"[\w']+|[.,?]")` drops the hyphen entirely, yielding
        // ["a", "b", ",", "c", "."]. We keep it, yielding one word token.
        assert_eq!(
            tokenize("a-b, c.", MARKS),
            vec![
                Tok::Word("a-b".to_string()),
                Tok::Mark(','),
                Tok::Word("c".to_string()),
                Tok::Mark('.')
            ]
        );

        // Smallest pure-ASCII pair where that changes the counts: one
        // substitution here, one insertion + one deletion for the oracle.
        assert_eq!(
            punctuation_error_rate("a-b.", "a-, b", MARKS),
            PunctCounts {
                correct: 0,
                substitutions: 1,
                insertions: 0,
                deletions: 0
            }
        );

        // Worst observed pure-ASCII gap: rate 0.000 here, 0.667 for the
        // oracle, on the same pair.
        let c = punctuation_error_rate("\"ok\", he said.", "a-b, c.", MARKS);
        assert_eq!(
            c,
            PunctCounts {
                correct: 2,
                substitutions: 0,
                insertions: 0,
                deletions: 0
            }
        );
        assert_eq!(c.rate(), 0.0);
    }
}

/// Cross-check against `tools/per_oracle.py`, the second implementation.
///
/// Every test here that spawns the oracle is `#[ignore]`d: it needs a
/// `python3`, which a plain `cargo test` cannot assume exists. (The one
/// exception, `dirty_ascii_corpus_keeps_its_dirty_and_clean_halves`, only
/// reads a corpus constant and so runs by default.) They are the only
/// executable form of the agreement claims in this file's module doc, so run
/// them whenever either implementation changes:
///
/// ```text
/// cd src-tauri && cargo test --lib eval::per -- --ignored
/// ```
///
/// If Python is missing they FAIL rather than skip — you asked for them
/// explicitly, so a silent no-op would be another overstated green.
#[cfg(test)]
mod oracle {
    use super::*;
    use std::io::Write;
    use std::process::{Command, Stdio};

    const ORACLE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../tools/per_oracle.py");
    const MARKS: &[char] = &['.', ',', '?'];
    const HI_MARKS: &[char] = &['।', ',', '?'];

    fn interpreter() -> &'static str {
        for candidate in ["python3", "python", "py"] {
            let found = Command::new(candidate)
                .arg("--version")
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .map(|s| s.success())
                .unwrap_or(false);
            if found {
                return candidate;
            }
        }
        panic!(
            "no python3/python/py on PATH — these tests are #[ignore]d precisely \
             because they need one; install Python 3 or don't pass --ignored"
        );
    }

    /// Feeds `pairs` to the oracle and returns its raw output — exit status
    /// included, no success assertion. `run` layers that on; the tests that
    /// pin the oracle's *refusals* need the failure itself.
    fn spawn(pairs: &[(&str, &str)], marks: &[char]) -> std::process::Output {
        let mut input = String::new();
        for (r, h) in pairs {
            input.push_str(&serde_json::json!({ "reference": r, "hypothesis": h }).to_string());
            input.push('\n');
        }
        let marks_arg: String = marks.iter().collect();
        let mut child = Command::new(interpreter())
            .arg(ORACLE)
            .arg("--marks")
            .arg(&marks_arg)
            // These used to be SET here (`PYTHONUTF8=1`, `PYTHONIOENCODING=utf-8`)
            // to stop Python decoding stdin with the locale encoding. That
            // made the harness the only caller getting UTF-8 right: the
            // manual invocation the oracle documents gets no such help, and
            // under cp1252 it mojibaked every danda, matched no mark, and
            // scored a missing-danda pair a flawless 0.000. The oracle now
            // decodes `sys.stdin.buffer` itself, so we clear the variables
            // instead of setting them — a developer whose shell exports them
            // must not be the reason these tests pass.
            .env_remove("PYTHONUTF8")
            .env_remove("PYTHONIOENCODING")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn per_oracle.py");
        let mut stdin = child.stdin.take().expect("oracle stdin");
        // Write from a second thread: with hundreds of pairs the child's
        // stdout pipe fills before we finish writing, and a single-threaded
        // write-then-read would deadlock.
        let writer = std::thread::spawn(move || stdin.write_all(input.as_bytes()));
        let out = child.wait_with_output().expect("oracle output");
        if let Err(e) = writer.join().expect("stdin writer thread panicked") {
            // An oracle that dies mid-input breaks this pipe, so `write_all`
            // comes back with "the pipe has been ended" — a symptom. This
            // used to be `.expect("write stdin")` sequenced BEFORE the status
            // check, so a dead oracle produced that symptom as the test
            // failure and its stderr, holding the traceback, was dropped on
            // the floor. Only a write error next to a *cleanly exited* oracle
            // is unexplained and worth panicking over; otherwise let the
            // caller report `out.stderr`, which is the cause.
            if out.status.success() {
                panic!("writing oracle stdin failed beside a clean exit: {e}");
            }
        }
        out
    }

    /// Runs the oracle over `pairs` and returns its counts, one per pair.
    fn run(pairs: &[(&str, &str)], marks: &[char]) -> Vec<PunctCounts> {
        let out = spawn(pairs, marks);
        assert!(
            out.status.success(),
            "oracle exited {}: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr)
        );

        // Safe despite `spawn` clearing PYTHONIOENCODING, which leaves the
        // child's stdout on the locale encoding: `json.dumps` defaults to
        // `ensure_ascii=True`, so the oracle only ever writes ASCII here, and
        // ASCII is UTF-8. Keep it that way — a non-ASCII `print` on the
        // oracle's stdout path would land as cp1252 bytes and trip this.
        let text = String::from_utf8(out.stdout).expect("oracle stdout is utf-8");
        let mut counts = Vec::new();
        for line in text.lines().map(str::trim).filter(|l| !l.is_empty()) {
            let v: serde_json::Value = serde_json::from_str(line).expect("oracle emits JSON");
            // The oracle appends an aggregate line when given >1 pair.
            if v.get("total").is_some() {
                continue;
            }
            counts.push(PunctCounts {
                correct: v["correct"].as_u64().expect("correct") as u32,
                substitutions: v["substitutions"].as_u64().expect("substitutions") as u32,
                insertions: v["insertions"].as_u64().expect("insertions") as u32,
                deletions: v["deletions"].as_u64().expect("deletions") as u32,
            });
        }
        assert_eq!(counts.len(), pairs.len(), "one count line per input pair");
        counts
    }

    fn cross_product<'a>(corpus: &[&'a str]) -> Vec<(&'a str, &'a str)> {
        let mut pairs = Vec::with_capacity(corpus.len() * corpus.len());
        for r in corpus {
            for h in corpus {
                pairs.push((*r, *h));
            }
        }
        pairs
    }

    /// Every character here is `\w`-matchable, an ASCII apostrophe,
    /// whitespace, or one of `MARKS` — Latin, Cyrillic and CJK.
    const CLEAN: &[&str] = &[
        "hello world",
        "hello world.",
        "hello, world.",
        "hello world, ok.",
        "is it, though?",
        "is it though?",
        "one two three.",
        "one, two, three.",
        "don't stop.",
        "don't, stop.",
        "cafe au lait, please.",
        "naive resume, ok.",
        "privet mir.",
        "привет, мир.",
        "привет мир.",
        "你好世界.",
        "你好, 世界.",
        "abc123 def, ok.",
        "abc123 def ok.",
        "x y z?",
        "x y z.",
        "x, y z?",
    ];

    /// The module doc's agreement claim, executed. 22 strings crossed into
    /// 484 pairs; zero divergence is the whole claim.
    #[test]
    #[ignore = "spawns python3; run with: cargo test --lib eval::per -- --ignored"]
    fn oracle_agrees_on_the_clean_zone() {
        let pairs = cross_product(CLEAN);
        assert_eq!(pairs.len(), 484);
        let theirs = run(&pairs, MARKS);
        let mut disagreements = Vec::new();
        for (idx, (r, h)) in pairs.iter().enumerate() {
            let mine = punctuation_error_rate(r, h, MARKS);
            if mine != theirs[idx] {
                disagreements.push(format!("{r:?} vs {h:?}: {mine:?} != {:?}", theirs[idx]));
            }
        }
        assert!(
            disagreements.is_empty(),
            "the module doc claims exact agreement inside the clean zone; \
             {} of {} pairs disagree:\n{}",
            disagreements.len(),
            pairs.len(),
            disagreements.join("\n")
        );
    }

    /// Ordinary English, 33 strings, mostly arranged as dirty/clean twins:
    /// `a-b` beside `a b`, `don’t` beside `dont`, `cat—dog` beside
    /// `cat dog`, `hello (world), ok.` beside `hello world, ok.`
    ///
    /// "Dirty" means the string carries a character that is neither `\w`, an
    /// ASCII apostrophe, whitespace, nor a mark — here `-`, `"`, `(`, `)`,
    /// `$`, `=`, and the typographic `’ – — …` — so the oracle drops it and
    /// we glue it in. Eighteen strings are dirty; the other fifteen are
    /// entirely inside the clean zone.
    ///
    /// The clean fifteen are controls, not filler, and they carry a result:
    /// **all ten divergences are dirty-against-dirty, and not one pair
    /// involving a clean string diverges.** So a dropped character is
    /// necessary but nowhere near sufficient — it has to be dropped on
    /// *both* sides before the token shift reaches a mark. Delete the clean
    /// half and the corpus stops being able to say that.
    /// `oracle_ascii_divergence_is_small_but_real` asserts it, rather than
    /// leaving it as prose.
    ///
    /// The name is historical and only half right: `’ – — …` are not ASCII.
    /// What earns the corpus its place is that its two *worst* pairs are pure
    /// ASCII, which is what refutes "ASCII agrees, the rest doesn't" — see
    /// `oracle_disagrees_outside_the_clean_zone`.
    const DIRTY_ASCII: &[&str] = &[
        "a b",
        "a b.",
        "a b, c.",
        "a-b",
        "a-b.",
        "a-b, c.",
        "a -b, c.",
        "a- b, c.",
        "a-, b",
        "well-known cat, ok.",
        "well known cat, ok.",
        "don't stop.",
        "dont stop.",
        "don’t stop.",
        "he said \"ok\".",
        "he said ok.",
        "\"ok\", he said.",
        "cat—dog, x.",
        "cat — dog, x.",
        "cat dog, x.",
        "cat–dog, x.",
        "a$b, c.",
        "ab, c.",
        "5 x 3 = 15.",
        "5 x 3 is 15.",
        "e.g. cats, dogs.",
        "eg cats, dogs.",
        "hello (world), ok.",
        "hello world, ok.",
        "wait… what?",
        "wait what?",
        "re-do it, now?",
        "redo it, now?",
    ];

    /// True for a character inside the agreement zone the module doc
    /// describes: `\w`-matchable, an ASCII apostrophe, whitespace, or one of
    /// `MARKS`. Anything else is dropped by the oracle and glued into a word
    /// by us, which is the sole source of divergence.
    ///
    /// Python's `\w` is `str.isalnum()` plus `_`, and `char::is_alphanumeric`
    /// agrees with that over the Latin corpora here — this is a test helper
    /// for classifying `DIRTY_ASCII`, not a second tokenizer.
    fn char_in_clean_zone(c: char) -> bool {
        c.is_alphanumeric() || c == '_' || c == '\'' || c.is_whitespace() || MARKS.contains(&c)
    }

    /// [`char_in_clean_zone`] for every character of `s`.
    fn inside_clean_zone(s: &str) -> bool {
        s.chars().all(char_in_clean_zone)
    }

    /// Keeps `DIRTY_ASCII`'s doc comment honest, and needs no Python to do
    /// it: a well-meant tidy-up that made every string dirty would leave the
    /// divergence tests passing on a weaker corpus while the doc described
    /// one that no longer existed.
    ///
    /// The previous doc comment claimed this corpus was "pure ASCII" and that
    /// "every string" carried a non-`\w` character; both were false — hence a
    /// test rather than another unchecked sentence.
    #[test]
    fn dirty_ascii_corpus_keeps_its_dirty_and_clean_halves() {
        assert_eq!(DIRTY_ASCII.len(), 33, "doc comment says 33 strings");
        let clean = DIRTY_ASCII
            .iter()
            .filter(|s| inside_clean_zone(s))
            .count();
        assert_eq!(
            (DIRTY_ASCII.len() - clean, clean),
            (18, 15),
            "doc comment says 18 dirty / 15 clean; the clean fifteen are the \
             controls behind `oracle_ascii_divergence_is_small_but_real`'s \
             claim that no pair with a clean side ever diverges"
        );

        // And the dirty characters really are the ones the doc lists.
        let dirty_chars: std::collections::BTreeSet<char> = DIRTY_ASCII
            .iter()
            .flat_map(|s| s.chars())
            .filter(|c| !char_in_clean_zone(*c))
            .collect();
        assert_eq!(
            dirty_chars.iter().collect::<String>(),
            "\"$()-=–—’…",
            "doc comment lists exactly these"
        );
    }

    /// The module doc's "1089 pairs, 10 diverged, 0.60 PER points", executed.
    ///
    /// Counts are pinned as literals on purpose: they are quoted in the
    /// module doc and in `per_oracle.py`'s docstring, so if either
    /// implementation shifts, the prose is stale and this says so instead of
    /// the prose quietly becoming fiction.
    #[test]
    #[ignore = "spawns python3; run with: cargo test --lib eval::per -- --ignored"]
    fn oracle_ascii_divergence_is_small_but_real() {
        let pairs = cross_product(DIRTY_ASCII);
        assert_eq!(pairs.len(), 1089);
        let theirs = run(&pairs, MARKS);

        let diverged: Vec<(&str, &str)> = pairs
            .iter()
            .enumerate()
            .filter(|(idx, (r, h))| punctuation_error_rate(r, h, MARKS) != theirs[*idx])
            .map(|(_, p)| *p)
            .collect();
        assert_eq!(
            diverged.len(),
            10,
            "module doc quotes 10 of 1089 diverging on this corpus; got {diverged:?}"
        );

        // `DIRTY_ASCII`'s doc comment claims every divergence is
        // dirty-against-dirty — a dropped character on one side alone never
        // reaches a mark. Measured, so assert it: the clean fifteen are
        // controls and this is the result they buy.
        let clean_side: Vec<&(&str, &str)> = diverged
            .iter()
            .filter(|(r, h)| inside_clean_zone(r) || inside_clean_zone(h))
            .collect();
        assert!(
            clean_side.is_empty(),
            "a pair with a clean side diverged, which the DIRTY_ASCII doc \
             says cannot happen: {clean_side:?}"
        );

        let mine_total: PunctCounts = pairs
            .iter()
            .map(|(r, h)| punctuation_error_rate(r, h, MARKS))
            .sum();
        let their_total: PunctCounts = theirs.iter().copied().sum();
        let gap = (mine_total.rate() - their_total.rate()).abs() * 100.0;
        assert!(
            (0.5..0.7).contains(&gap),
            "module doc quotes 0.60 PER points here; measured {gap:.2} \
             (ours {:.4}, theirs {:.4})",
            mine_total.rate(),
            their_total.rate()
        );
    }

    /// The divergences are pinned, not just admitted: these exact counts are
    /// what the module doc quotes, so a change in either implementation
    /// invalidates the doc here rather than silently.
    #[test]
    #[ignore = "spawns python3; run with: cargo test --lib eval::per -- --ignored"]
    fn oracle_disagrees_outside_the_clean_zone() {
        let pairs = [("a-b.", "a-, b"), ("\"ok\", he said.", "a-b, c.")];
        let theirs = run(&pairs, MARKS);

        // Pure ASCII, hyphen only. Ours: one substitution.
        assert_eq!(
            punctuation_error_rate(pairs[0].0, pairs[0].1, MARKS),
            PunctCounts {
                correct: 0,
                substitutions: 1,
                insertions: 0,
                deletions: 0
            }
        );
        // Theirs: the dropped hyphen splits "a-b" into two word tokens,
        // shifting the alignment into an insertion plus a deletion.
        assert_eq!(
            theirs[0],
            PunctCounts {
                correct: 0,
                substitutions: 0,
                insertions: 1,
                deletions: 1
            }
        );

        // Pure ASCII again, and the rates differ by two thirds.
        let mine = punctuation_error_rate(pairs[1].0, pairs[1].1, MARKS);
        assert_eq!(
            mine,
            PunctCounts {
                correct: 2,
                substitutions: 0,
                insertions: 0,
                deletions: 0
            }
        );
        assert_eq!(
            theirs[1],
            PunctCounts {
                correct: 1,
                substitutions: 0,
                insertions: 1,
                deletions: 1
            }
        );
        assert_eq!(mine.rate(), 0.0);
        assert!((theirs[1].rate() - 2.0 / 3.0).abs() < 1e-9);
    }

    /// Devanagari sentences differing in words as well as punctuation —
    /// what a real ASR benchmark corpus looks like.
    const HI_CORPUS: &[&str] = &[
        "मैंने रिपोर्ट भेज दी।",
        "मैंने रिपोर्ट भेज दी",
        "मैंने रिपोर्ट, भेज दी।",
        "नमस्ते, दुनिया।",
        "नमस्ते दुनिया।",
        "नमस्ते दुनिया",
        "क्या आप ठीक हैं?",
        "क्या आप ठीक हैं।",
        "आप ठीक हैं?",
        "पहले चाय, फिर बात।",
        "पहले चाय फिर बात।",
        "चाय, फिर बात।",
        "वह गया, लेकिन देर से।",
        "वह स्कूल गया लेकिन देर से।",
        "शुक्रिया, दोस्त।",
        "धन्यवाद दोस्त।",
        "मुझे पानी चाहिए।",
        "मुझे, पानी चाहिए",
        "रिपोर्ट तैयार है, भेज दूँ?",
        "किताब तैयार है भेज दूँ।",
    ];

    /// The "4.22 PER points" in the module doc, executed. This is the number
    /// that makes "do not fix toward NeMo" weighable: it is the size of a
    /// model generation, not a rounding artifact.
    ///
    /// Pinned as a band, not a literal: the exact figure is a property of
    /// this corpus, and a band still fails loudly if the gap collapses (a
    /// "fix" toward NeMo's tokenizer) or explodes.
    #[test]
    #[ignore = "spawns python3; run with: cargo test --lib eval::per -- --ignored"]
    fn oracle_indic_divergence_is_material() {
        let pairs = cross_product(HI_CORPUS);
        assert_eq!(pairs.len(), 400);
        let theirs = run(&pairs, HI_MARKS);

        // Pinned exactly, like the ASCII sweep's 10-of-1089 — the module doc
        // claims the divergence *counts* are, and until now this one was
        // pinned nowhere, so "20 diverged" was prose backed by nothing.
        let diverged = pairs
            .iter()
            .enumerate()
            .filter(|(idx, (r, h))| punctuation_error_rate(r, h, HI_MARKS) != theirs[*idx])
            .count();
        assert_eq!(
            diverged, 20,
            "module doc quotes 20 of 400 diverging on this corpus"
        );

        let mine_total: PunctCounts = pairs
            .iter()
            .map(|(r, h)| punctuation_error_rate(r, h, HI_MARKS))
            .sum();
        let their_total: PunctCounts = theirs.iter().copied().sum();

        let gap = (mine_total.rate() - their_total.rate()).abs() * 100.0;
        assert!(
            (4.0..4.5).contains(&gap),
            "module doc quotes 4.22 PER points of Indic divergence on this \
             corpus; measured {gap:.2} (ours {:.4} from {mine_total:?}, \
             theirs {:.4} from {their_total:?})",
            mine_total.rate(),
            their_total.rate()
        );
    }

    /// The same Devanagari text scored with only punctuation differing
    /// agrees exactly — which is why the 4.22 above must always be quoted
    /// with its corpus. A benchmark that only perturbs punctuation shows a
    /// 0.00-point gap and would wrongly suggest full agreement with NeMo.
    ///
    /// This is the module doc's "12 realistic pairs, 0 diverged, 0.00 PER
    /// points".
    #[test]
    #[ignore = "spawns python3; run with: cargo test --lib eval::per -- --ignored"]
    fn oracle_indic_agrees_when_only_punctuation_differs() {
        let pairs = [
            ("मैंने रिपोर्ट भेज दी।", "मैंने रिपोर्ट भेज दी"),
            ("नमस्ते, दुनिया।", "नमस्ते दुनिया।"),
            ("क्या आप ठीक हैं?", "क्या आप ठीक हैं।"),
            ("यह मेरी किताब है।", "यह मेरी किताब है।"),
            ("पहले चाय, फिर बात।", "पहले चाय फिर बात।"),
            ("वह स्कूल गया, लेकिन देर से।", "वह स्कूल गया लेकिन देर से।"),
            ("मुझे पानी चाहिए।", "मुझे, पानी चाहिए।"),
            ("आपका नाम क्या है?", "आपका नाम क्या है?"),
            ("रिपोर्ट तैयार है, भेज दूँ?", "रिपोर्ट तैयार है भेज दूँ।"),
            ("शुक्रिया, दोस्त।", "शुक्रिया दोस्त"),
            ("कल मिलते हैं।", "कल मिलते हैं।"),
            ("ठीक है, चलो।", "ठीक है चलो।"),
        ];
        assert_eq!(pairs.len(), 12);
        let theirs = run(&pairs, HI_MARKS);
        for (idx, (r, h)) in pairs.iter().enumerate() {
            assert_eq!(
                punctuation_error_rate(r, h, HI_MARKS),
                theirs[idx],
                "pair {idx} ({r} / {h}) should agree: identical words on both \
                 sides shatter identically, so mark alignment is preserved"
            );
        }

        // ...and therefore the corpus PER is bit-identical, 0.00 points
        // apart, on the very script where the 400-pair sweep finds 4.22.
        let mine_total: PunctCounts = pairs
            .iter()
            .map(|(r, h)| punctuation_error_rate(r, h, HI_MARKS))
            .sum();
        let their_total: PunctCounts = theirs.iter().copied().sum();
        assert_eq!(mine_total, their_total);
        assert_eq!(
            mine_total,
            PunctCounts {
                correct: 8,
                substitutions: 2,
                insertions: 1,
                deletions: 8
            }
        );
    }

    // --- The oracle must not report a score it did not compute. ---
    //
    // The same silent-perfect-score family this module already guards on the
    // Rust side (`empty_mark_set_panics_instead_of_silently_scoring_zero`,
    // and `PunctCounts::rate`'s note that 0/0 is "n/a", not "flawless"),
    // pinned on the Python side. A validation tool that reports success on
    // input it never processed is worse than no tool.

    /// Regression: the oracle's own documented manual invocation scored
    /// non-ASCII fixtures as PERFECT.
    ///
    /// `sys.stdin` decodes with the *locale* encoding on Python < 3.15, and
    /// cp1252 — the stock Windows default — maps nearly every byte, so a
    /// UTF-8 fixture did not raise, it mojibaked. `।` U+0964 arrived as three
    /// Latin-1 characters, matched no entry in `--marks`, and was dropped by
    /// the tokenizer: the pair below, whose only error is that dropped danda,
    /// came back `C=S=I=D=0`, rate 0.000, exit 0. Verified by hand before the
    /// fix, on this exact pair.
    ///
    /// `spawn` clears `PYTHONUTF8`/`PYTHONIOENCODING` rather than setting
    /// them, so this really is the unassisted environment a human gets.
    #[test]
    #[ignore = "spawns python3; run with: cargo test --lib eval::per -- --ignored"]
    fn oracle_scores_non_ascii_stdin_instead_of_silently_perfecting_it() {
        let pairs = [("नमस्ते दुनिया।", "नमस्ते दुनिया")];
        let theirs = run(&pairs, HI_MARKS);
        let dropped_danda = PunctCounts {
            correct: 0,
            substitutions: 0,
            insertions: 0,
            deletions: 1,
        };
        assert_ne!(
            theirs[0],
            PunctCounts::default(),
            "all-zero counts here mean the danda never survived decoding; \
             rate() would print 0.000 and read as flawless punctuation"
        );
        assert_eq!(theirs[0], dropped_danda);
        assert_eq!(theirs[0].rate(), 1.0);
        assert_eq!(
            punctuation_error_rate(pairs[0].0, pairs[0].1, HI_MARKS),
            dropped_danda
        );
    }

    /// The other half of the same failure: a mark set that simply does not
    /// occur in the corpus. `--marks ".,?"` is the oracle's default and the
    /// value its usage examples show, and against Devanagari it matches
    /// nothing, so every count is zero and the rate prints 0.000 — again
    /// indistinguishable from flawless.
    ///
    /// There is no correct number to return here, so the oracle exits
    /// non-zero and names the punctuation it *did* find. The assertion is on
    /// the codepoint spelling, not the character: the oracle writes
    /// diagnostics as `U+XXXX NAME` precisely so they survive a cp1252
    /// stderr.
    #[test]
    #[ignore = "spawns python3; run with: cargo test --lib eval::per -- --ignored"]
    fn oracle_refuses_a_mark_set_that_never_occurs_in_the_input() {
        let out = spawn(&[("नमस्ते दुनिया।", "नमस्ते दुनिया")], MARKS);
        assert!(
            !out.status.success(),
            "ASCII marks against Devanagari must refuse, not score; stdout: {}",
            String::from_utf8_lossy(&out.stdout)
        );
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            stderr.contains("U+0964 DEVANAGARI DANDA"),
            "the refusal must name the punctuation it found so the fix is \
             obvious; got: {stderr}"
        );
    }

    /// An empty `--marks` is the CLI's version of the panic
    /// `empty_mark_set_panics_instead_of_silently_scoring_zero` pins on the
    /// Rust side. Without this the regex degrades to an empty character class
    /// and Python dies with an obscure `re.error` traceback.
    ///
    /// Doubles as the regression test for `spawn`'s writer-thread handling,
    /// which is why it feeds 20 000 pairs instead of one. `--marks` is
    /// rejected in `_parse_args`, before a byte of stdin is read, so the
    /// oracle is already gone while we are still pushing ~1.2 MB at a pipe
    /// nobody will ever read; the OS swallows the first ~60 KB and then
    /// `write_all` fails with "the pipe has been ended (os error 109)",
    /// measured, not assumed. The join used to run before the status check
    /// and `.expect()` that error, so the reported failure was our broken
    /// pipe and the oracle's own explanation went in the bin. Restore that
    /// and this test stops reporting the refusal below and reports an io
    /// error instead.
    #[test]
    #[ignore = "spawns python3; run with: cargo test --lib eval::per -- --ignored"]
    fn oracle_refuses_an_empty_mark_set() {
        let pairs = vec![("Hello, world.", "Hello world."); 20_000];
        let out = spawn(&pairs, &[]);
        assert!(!out.status.success(), "empty --marks must refuse");
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            stderr.contains("--marks is empty"),
            "expected the empty-mark-set refusal to survive the broken stdin \
             pipe; got: {stderr}"
        );
    }
}
