//! Disfluency-removal precision/recall/F1: did the formatter drop the right
//! words, and only the right words?
//!
//! # The three inputs, and the two multisets derived from them
//!
//! [`score`] takes three texts for the same utterance:
//!
//! - `verbatim` — what was actually said, fillers and all ("um so we
//!   should ship it").
//! - `reference_clean` — the gold-standard disfluency-free version ("we
//!   should ship it").
//! - `hypothesis` — what the formatter actually produced.
//!
//! From these it derives two **multisets of normalised tokens** (word
//! counts, not just word identities — see below for why that distinction
//! matters):
//!
//! - the gold removed set = `verbatim − reference_clean`: the tokens a
//!   correct disfluency pass should remove.
//! - the predicted removed set = `verbatim − hypothesis`: the tokens the
//!   formatter actually removed.
//!
//! Precision and recall are then the usual set-retrieval ratios over those
//! two multisets: precision = `|gold ∩ predicted| / |predicted|` (of what
//! the formatter removed, how much should have gone), recall =
//! `|gold ∩ predicted| / |gold|` (of what should have gone, how much did
//! the formatter actually remove).
//!
//! # The comparison is on a NORMALISED form
//!
//! Every token goes through [`crate::eval::rawer::normalize_token`] —
//! lowercased, edge punctuation trimmed — before it enters a multiset. This
//! is not a nicety; without it the metric silently measures the wrong thing.
//!
//! A multiset difference cancels a token only when both sides spell it
//! **identically**. Disfl-QA's `verbatim` and `target` carry real casing and
//! punctuation, and disfluency removal routinely changes the casing of the
//! word that ends up first: `"What was no where was Dyrrachium located?"`
//! becomes `"Where was Dyrrachium located?"`, so raw-string `where` does not
//! cancel against `Where`. A raw diff calls that a fourth removed token
//! (`where`) when only three words (`What was no`) were removed — the
//! formatter's *casing* decision scored as a *disfluency* error.
//!
//! Measured on the 200 committed `disflqa.jsonl` cases: **87 of them** have
//! a gold-removed set that changes size once tokens are normalised, and the
//! corpus-wide gold-removed total falls from **1 117 raw tokens to 1 007
//! normalised** — a 10.9 % inflation, in the denominator of recall and in
//! the intersection that feeds both ratios. `casing_differences_are_not_
//! counted_as_disfluency_removals` pins the Dyrrachium case directly.
//!
//! Casing and punctuation have their own metrics (`eval::casing`,
//! `eval::per`); this one's job is which *words* survived.
//!
//! # Why a multiset, not a set
//!
//! Repetitions — "the the cat sat", gold "the cat sat" — are one of the
//! three disfluency types this metric has to score, and a repetition
//! removes a token that also survives once elsewhere in the same sentence.
//! A **set** difference would see "the" present in both the verbatim set
//! and the reference-clean set and conclude nothing was removed at all —
//! silently blind to every repetition. Counting occurrences fixes it:
//! verbatim has "the" twice, reference-clean has it once, so the gold
//! removed set correctly contains one "the". Pinned in
//! `repeated_words_are_counted_not_just_present`.
//!
//! # Corpus aggregation: MICRO, from the counts
//!
//! Sum [`RemovalCounts`] across the corpus and take **one**
//! precision/recall/F1 at the end. Averaging per-utterance F1 is a
//! different number that weights a one-filler utterance the same as a
//! fourteen-token restart, and it is not what any published disfluency
//! figure means. Same warning, same reason, as
//! [`crate::eval::per::PunctCounts`] and [`crate::eval::casing::CasingCounts`].
//!
//! ```ignore
//! cases.iter()
//!     .map(|c| disfluency::score(&c.verbatim, &c.target, &hyp).counts)
//!     .sum::<RemovalCounts>()
//!     .score()
//! ```
//!
//! # What this still cannot measure
//!
//! This is blind to *which* occurrence was removed when a token repeats,
//! and to word order entirely — it is a bag-of-words comparison. Normalising
//! deliberately makes it blind to casing and punctuation too, which is the
//! point above. It inherits
//! [`crate::eval::rawer::normalize_token`]'s own limits, chiefly that
//! nothing here normalises Unicode canonical form.
//!
//! # Baselines (Switchboard-style spoken disfluency, by type)
//!
//! Repetitions ("the the cat") are the easiest to catch, F1 ~97.5.
//! Corrections ("go left, I mean right") are harder, ~80.0. Restarts
//! (abandoned, unfinished phrases) are the hardest, ~57.1. A corpus-level
//! **micro** score well below these — especially on repetitions — points at
//! a bug in the removal stage, not an inherently hard case.

// Exercised only by this file's own `#[cfg(test)]` module as far as the lib
// crate is concerned, same situation as `eval::per` and `eval::rawer` — see
// `rawer` for why `fmtbench` calling into this tree does not lift the allow.
#![allow(dead_code)]

use std::collections::HashMap;

use super::rawer::normalize_token;

/// Splits on whitespace and normalises each token, dropping the ones that
/// were pure punctuation. The single place this metric's notion of "the
/// same word" is defined; see the module doc for why it is normalised.
fn normalised_tokens(text: &str) -> Vec<String> {
    text.split_whitespace().filter_map(normalize_token).collect()
}

/// Word counts, not just word identities — see the module doc for why a
/// multiset rather than a set is load-bearing here (repetitions).
fn multiset(tokens: &[String]) -> HashMap<&str, u32> {
    let mut counts = HashMap::new();
    for t in tokens {
        *counts.entry(t.as_str()).or_insert(0) += 1;
    }
    counts
}

/// Multiset subtraction `a − b`: for each key, `max(a[k] − b[k], 0)`. Keys
/// with nothing left after subtracting are omitted rather than kept at
/// zero, so [`total`] and [`intersection_size`] don't have to filter them
/// back out.
fn difference<'a>(a: &HashMap<&'a str, u32>, b: &HashMap<&'a str, u32>) -> HashMap<&'a str, u32> {
    let mut out = HashMap::new();
    for (&token, &count_a) in a {
        let count_b = b.get(token).copied().unwrap_or(0);
        let remaining = count_a.saturating_sub(count_b);
        if remaining > 0 {
            out.insert(token, remaining);
        }
    }
    out
}

fn total(m: &HashMap<&str, u32>) -> u32 {
    m.values().sum()
}

/// `Σ min(a[k], b[k])` over all keys — the size of the multiset
/// intersection, i.e. how many of the tokens in `a` also appear (up to
/// that many times) in `b`.
fn intersection_size(a: &HashMap<&str, u32>, b: &HashMap<&str, u32>) -> u32 {
    a.iter()
        .map(|(token, &count_a)| count_a.min(b.get(token).copied().unwrap_or(0)))
        .sum()
}

/// `Some(numerator / denominator)`, or `None` when the denominator is zero.
///
/// A zero denominator means nothing was measured: the formatter removed
/// nothing at all (for precision), or nothing needed removing at all (for
/// recall). The earlier implementation returned `1.0` there, which made a
/// never-tested utterance indistinguishable from a verified-correct one —
/// and read-speech corpora such as `librispeech-pc.jsonl` are *entirely*
/// "nothing needed removing", so that free `1.0` was available in bulk.
///
/// `None` cannot be averaged into a corpus figure without the caller
/// deciding what to do about it. The corpus number that is meaningful is
/// micro: sum [`RemovalCounts`], then take one ratio.
///
/// **Not used by [`RemovalCounts::f1`]**, whose denominator is empty in
/// strictly fewer situations than these two; see that method.
fn ratio(numerator: u32, denominator: u32) -> Option<f64> {
    if denominator == 0 {
        None
    } else {
        Some(f64::from(numerator) / f64::from(denominator))
    }
}

/// The raw removal counts one utterance contributes. **Sum these across a
/// corpus and call [`score`](RemovalCounts::score) once** — see the module
/// doc on micro vs macro.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct RemovalCounts {
    /// Tokens the formatter removed that should have been removed.
    pub true_positives: u32,
    /// Tokens the formatter removed that should have survived.
    pub false_positives: u32,
    /// Tokens that should have been removed and were not.
    pub false_negatives: u32,
}

impl RemovalCounts {
    /// Of the tokens the formatter removed, the fraction that should have
    /// been removed. `None` when it removed nothing.
    pub fn precision(&self) -> Option<f64> {
        ratio(self.true_positives, self.true_positives + self.false_positives)
    }

    /// Of the tokens that should have been removed, the fraction the
    /// formatter actually removed. `None` when nothing needed removing.
    pub fn recall(&self) -> Option<f64> {
        ratio(self.true_positives, self.true_positives + self.false_negatives)
    }

    /// `2·TP / (2·TP + FP + FN)` — the harmonic mean of
    /// [`precision`](Self::precision) and [`recall`](Self::recall), computed
    /// straight from the counts. `None` only when all three counts are zero,
    /// which for this metric means nothing needed removing *and* nothing was
    /// removed.
    ///
    /// Deriving this as `precision()? , recall()?` conflated an empty
    /// denominator with a zero score, and both of the shapes it got wrong
    /// are the ones a broken removal stage actually produces:
    ///
    /// - the stage is switched off, so nothing was removed and everything
    ///   that should have gone stayed (`TP 0, FP 0, FN n`): precision is
    ///   undefined, F1 is `0.0`;
    /// - the stage removed only the wrong words while the utterance needed
    ///   nothing removed (`TP 0, FP n, FN 0`): recall is undefined, F1 is
    ///   `0.0`.
    ///
    /// Both used to report `None`, which the benchmark prints as "n/a" — a
    /// total failure reading as "not measurable". See
    /// [`crate::eval::casing::CasingCounts::f1`], which carries the same fix
    /// and the same reasoning; precision and recall themselves are
    /// unaffected and still return `None` for their own genuinely empty
    /// denominators.
    pub fn f1(&self) -> Option<f64> {
        if self.true_positives == 0 && self.false_positives == 0 && self.false_negatives == 0 {
            return None;
        }
        let tp = f64::from(self.true_positives);
        let denominator = 2.0 * tp + f64::from(self.false_positives) + f64::from(self.false_negatives);
        Some(2.0 * tp / denominator)
    }

    /// Bundles the three derived ratios alongside `self`. Every field of
    /// [`RemovalScore`] comes from here, so the ratios and the counts
    /// cannot drift apart.
    pub fn score(self) -> RemovalScore {
        RemovalScore {
            precision: self.precision(),
            recall: self.recall(),
            f1: self.f1(),
            counts: self,
        }
    }
}

impl std::ops::Add for RemovalCounts {
    type Output = RemovalCounts;

    fn add(self, other: RemovalCounts) -> RemovalCounts {
        RemovalCounts {
            true_positives: self.true_positives + other.true_positives,
            false_positives: self.false_positives + other.false_positives,
            false_negatives: self.false_negatives + other.false_negatives,
        }
    }
}

impl std::ops::AddAssign for RemovalCounts {
    fn add_assign(&mut self, other: RemovalCounts) {
        *self = *self + other;
    }
}

impl std::iter::Sum for RemovalCounts {
    fn sum<I: Iterator<Item = RemovalCounts>>(iter: I) -> RemovalCounts {
        iter.fold(RemovalCounts::default(), std::ops::Add::add)
    }
}

/// Precision, recall and F1 of disfluency removal, plus the
/// [`RemovalCounts`] they were derived from. See the module doc for exactly
/// what "removal" means here (a multiset diff over normalised tokens) and
/// what it cannot see (order, which occurrence, canonical form).
///
/// Every ratio is [`Option`]: `None` means *that field's own* denominator
/// was zero, i.e. nothing of that kind was measured — **not** that the
/// formatter was perfect, and **not** that it scored zero. F1's denominator
/// is not precision's or recall's, so it stays defined through the one-sided
/// total failures that leave those undefined. For a corpus, do not average
/// these fields; sum `counts` and call [`RemovalCounts::score`] once.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RemovalScore {
    /// Of the tokens the formatter removed, the fraction that should have
    /// been removed.
    pub precision: Option<f64>,
    /// Of the tokens that should have been removed, the fraction the
    /// formatter actually removed.
    pub recall: Option<f64>,
    /// `2·TP / (2·TP + FP + FN)`; `None` only when all three are zero — see
    /// [`RemovalCounts::f1`].
    pub f1: Option<f64>,
    /// The counts every field above was derived from. This is what a corpus
    /// aggregate must be built out of.
    pub counts: RemovalCounts,
}

/// Scores how well `hypothesis` reproduces the disfluency removal that
/// turns `verbatim` into `reference_clean`. See the module doc for the
/// exact derivation and its limits.
pub fn score(verbatim: &str, reference_clean: &str, hypothesis: &str) -> RemovalScore {
    let verbatim_tokens = normalised_tokens(verbatim);
    let clean_tokens = normalised_tokens(reference_clean);
    let hypothesis_tokens = normalised_tokens(hypothesis);

    let verbatim_ms = multiset(&verbatim_tokens);
    let gold_removed = difference(&verbatim_ms, &multiset(&clean_tokens));
    let predicted_removed = difference(&verbatim_ms, &multiset(&hypothesis_tokens));

    let true_positives = intersection_size(&gold_removed, &predicted_removed);
    let gold_total = total(&gold_removed);
    let predicted_total = total(&predicted_removed);

    RemovalCounts {
        true_positives,
        // Both subtractions are saturating and `true_positives` is a
        // per-key `min` of the two, so it can never exceed either total.
        false_positives: predicted_total - true_positives,
        false_negatives: gold_total - true_positives,
    }
    .score()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Compares every field, so a bug in any ONE field is visible instead of
    /// being hidden by only checking the field a given test happens to be
    /// "about". Ratios are compared with a tolerance (and `None` must match
    /// `None`), counts exactly.
    fn assert_scores_close(actual: RemovalScore, expected: RemovalScore) {
        let close = |a: Option<f64>, b: Option<f64>| match (a, b) {
            (Some(x), Some(y)) => (x - y).abs() < 1e-9,
            (None, None) => true,
            _ => false,
        };
        assert!(
            close(actual.precision, expected.precision)
                && close(actual.recall, expected.recall)
                && close(actual.f1, expected.f1)
                && actual.counts == expected.counts,
            "expected {expected:?}, got {actual:?}"
        );
    }

    #[test]
    fn removing_exactly_the_right_fillers_scores_one() {
        let s = score("um so we should ship it", "we should ship it", "we should ship it");
        assert_eq!(s.f1, Some(1.0));
    }

    #[test]
    fn removing_exactly_the_right_fillers_scores_one_on_every_field() {
        let s = score("um so we should ship it", "we should ship it", "we should ship it");
        assert_scores_close(
            s,
            RemovalCounts {
                true_positives: 2,
                false_positives: 0,
                false_negatives: 0,
            }
            .score(),
        );
    }

    #[test]
    fn removing_a_content_word_costs_precision() {
        let s = score("um so we should ship it", "we should ship it", "we should ship");
        // `Option`'s ordering puts `None` below every `Some`, so compare the
        // inner value: "< Some(1.0)" alone would also accept an undefined
        // precision, which is not what this test is claiming.
        assert!(s.precision.is_some_and(|p| p < 1.0), "{:?}", s.precision);
    }

    /// Exact value: predicted-removed is `{um, so, it}` (dropped "it" too),
    /// gold-removed is `{um, so}`, so 2 of the 3 removals were right.
    #[test]
    fn removing_a_content_word_costs_precision_exactly() {
        let s = score("um so we should ship it", "we should ship it", "we should ship");
        assert_scores_close(
            s,
            RemovalCounts {
                true_positives: 2,
                false_positives: 1,
                false_negatives: 0,
            }
            .score(),
        );
        assert_eq!(s.precision, Some(2.0 / 3.0));
        assert_eq!(s.recall, Some(1.0));
    }

    #[test]
    fn leaving_fillers_in_costs_recall() {
        let s = score("um so we should ship it", "we should ship it", "um we should ship it");
        assert!(s.recall.is_some_and(|r| r < 1.0), "{:?}", s.recall);
    }

    /// Exact value: predicted-removed is `{so}` only ("um" survived into
    /// the hypothesis), gold-removed is `{um, so}`, so 1 of the 2 required
    /// removals happened.
    #[test]
    fn leaving_fillers_in_costs_recall_exactly() {
        let s = score("um so we should ship it", "we should ship it", "um we should ship it");
        assert_scores_close(
            s,
            RemovalCounts {
                true_positives: 1,
                false_positives: 0,
                false_negatives: 1,
            }
            .score(),
        );
        assert_eq!(s.precision, Some(1.0));
        assert_eq!(s.recall, Some(0.5));
    }

    // --- Normalisation: casing and punctuation are other metrics' business. ---

    /// A casing change is not a removal, on the shipped bytes. This is
    /// `disflqa-test-0033` verbatim from `tests/fixtures/disflqa.jsonl`:
    /// removing the false start `"What was no"` promotes `where` to
    /// sentence-initial, so the gold `target` spells it `Where`.
    ///
    /// A raw-string multiset diff does not cancel `where` against `Where`
    /// and reports FOUR removed tokens instead of three — the formatter's
    /// casing decision charged to its disfluency removal. With
    /// normalisation the gold removal is exactly `{what, was, no}`, and a
    /// hypothesis that gets the disfluency right scores a clean 1.0
    /// whatever it does about the capital.
    ///
    /// 87 of the 200 committed `disflqa.jsonl` cases have this shape.
    #[test]
    fn casing_differences_are_not_counted_as_disfluency_removals() {
        let verbatim = "What was no where was Dyrrachium located?";
        let target = "Where was Dyrrachium located?";

        let s = score(verbatim, target, target);
        assert_scores_close(
            s,
            RemovalCounts {
                true_positives: 3, // what, was, no
                false_positives: 0,
                false_negatives: 0,
            }
            .score(),
        );

        // ...and a hypothesis that removed the right words but left the
        // promoted word lowercase is still a perfect DISFLUENCY score. Its
        // casing mistake belongs to `eval::casing`.
        let lowercased = "where was Dyrrachium located?";
        assert_eq!(score(verbatim, target, lowercased).f1, Some(1.0));
    }

    /// The same for punctuation: `disflqa-test-0067` drops the trailing
    /// `?` when its final word changes, and a raw diff would score the
    /// question mark's owner as removed-and-inserted rather than kept.
    #[test]
    fn punctuation_differences_are_not_counted_as_disfluency_removals() {
        let verbatim = "Who ruled Messina in 1191 no Cyprus?";
        let target = "Who ruled Cyprus in 1191?";

        let s = score(verbatim, target, target);
        assert_scores_close(
            s,
            RemovalCounts {
                true_positives: 2, // messina, no
                false_positives: 0,
                false_negatives: 0,
            }
            .score(),
        );
    }

    /// The normalisation must not be a *second* implementation that can
    /// drift from `eval::rawer`'s. One normaliser, shared, and this is the
    /// test that says so: `content_wer` treating two strings as identical
    /// content and this metric treating them as zero removals are the same
    /// claim.
    #[test]
    fn normalisation_is_the_same_one_content_wer_uses() {
        let verbatim = "Well-known, don't you think?";
        let hypothesis = "well-known don't you think";
        assert_eq!(crate::eval::rawer::content_wer(verbatim, hypothesis), 0.0);
        assert_eq!(
            score(verbatim, verbatim, hypothesis).counts,
            RemovalCounts::default(),
            "no words removed, so nothing to score — not a phantom removal"
        );
    }

    /// The whole reason this is a multiset diff and not a set diff: "the"
    /// occurs twice in `verbatim` and once in `reference_clean`, so exactly
    /// one occurrence is a gold removal (a repetition). A set-based
    /// implementation would see "the" present on both sides and conclude
    /// nothing needed removing at all — recall would read `None` (nothing
    /// measured) instead of catching the miss below.
    #[test]
    fn repeated_words_are_counted_not_just_present() {
        // Correct removal: the duplicate "the" is gone, "cat sat" is intact.
        let removed = score("the the cat sat", "the cat sat", "the cat sat");
        assert_scores_close(
            removed,
            RemovalCounts {
                true_positives: 1,
                false_positives: 0,
                false_negatives: 0,
            }
            .score(),
        );
        assert_eq!(removed.f1, Some(1.0));

        // Same repetition, left untouched: recall must be 0, not the `None`
        // a set-based diff would report for an empty gold set.
        let kept = score("the the cat sat", "the cat sat", "the the cat sat");
        assert_scores_close(
            kept,
            RemovalCounts {
                true_positives: 0,
                false_positives: 0,
                false_negatives: 1,
            }
            .score(),
        );
        assert_eq!(kept.recall, Some(0.0));
        assert_eq!(
            kept.f1,
            Some(0.0),
            "the removal stage did nothing: a zero score, not an unmeasurable"
        );
    }

    /// The removal stage is switched off — nothing removed, everything that
    /// should have gone left in. Precision genuinely has no denominator (no
    /// removal was predicted), but recall is `0.0` and so is F1:
    /// `2·0 / (2·0 + 0 + 2)`.
    ///
    /// `f1 == None` here would print as "n/a" in the benchmark — the single
    /// worst failure in this metric rendered as "we could not measure it".
    #[test]
    fn a_removal_stage_that_does_nothing_scores_zero_not_unmeasurable() {
        let s = score("um so we should ship it", "we should ship it", "um so we should ship it");
        assert_scores_close(
            s,
            RemovalCounts {
                true_positives: 0,
                false_positives: 0,
                false_negatives: 2,
            }
            .score(),
        );
        assert_eq!(s.precision, None, "nothing was removed, so nothing to judge");
        assert_eq!(s.recall, Some(0.0));
        assert_eq!(s.f1, Some(0.0));
    }

    /// The mirror: nothing needed removing, and the formatter removed two
    /// content words anyway. Recall is undefined, F1 is a real `0.0`.
    #[test]
    fn removing_words_from_a_clean_utterance_scores_zero_not_unmeasurable() {
        let s = score("we should ship it", "we should ship it", "we should");
        assert_scores_close(
            s,
            RemovalCounts {
                true_positives: 0,
                false_positives: 2,
                false_negatives: 0,
            }
            .score(),
        );
        assert_eq!(s.precision, Some(0.0));
        assert_eq!(s.recall, None, "nothing needed removing");
        assert_eq!(s.f1, Some(0.0));
    }

    /// `None` must still mean something, or the fix above is just "never say
    /// n/a". It survives in exactly one place: all three counts zero.
    #[test]
    fn f1_is_none_only_when_there_is_nothing_at_all_to_measure() {
        assert_eq!(RemovalCounts::default().f1(), None);
        for counts in [
            RemovalCounts {
                false_positives: 1,
                ..Default::default()
            },
            RemovalCounts {
                false_negatives: 1,
                ..Default::default()
            },
            RemovalCounts {
                true_positives: 1,
                ..Default::default()
            },
        ] {
            assert!(counts.f1().is_some(), "{counts:?}");
        }
    }

    /// The counts formula is a rewrite of the harmonic mean, not a new
    /// metric: wherever both ratios exist the two must agree.
    #[test]
    fn f1_agrees_with_the_harmonic_mean_where_both_ratios_exist() {
        for counts in [
            RemovalCounts {
                true_positives: 3,
                false_positives: 1,
                false_negatives: 0,
            },
            RemovalCounts {
                true_positives: 1,
                false_positives: 5,
                false_negatives: 4,
            },
            RemovalCounts {
                true_positives: 7,
                false_positives: 0,
                false_negatives: 0,
            },
        ] {
            let (p, r) = (counts.precision().unwrap(), counts.recall().unwrap());
            let harmonic = 2.0 * p * r / (p + r);
            let f1 = counts.f1().unwrap();
            assert!((f1 - harmonic).abs() < 1e-12, "{counts:?}: {f1} vs {harmonic}");
        }
    }

    /// A capitalised repetition is still a repetition. Without
    /// normalisation `"The the cat sat"` has no repeated token at all as
    /// far as the multiset is concerned, so the gold removal would be
    /// `{The}` plus a phantom, and the type of disfluency the baselines
    /// call easiest (F1 ~97.5) would be measured wrong.
    #[test]
    fn a_capitalised_repetition_is_still_a_repetition() {
        let s = score("The the cat sat", "The cat sat", "The cat sat");
        assert_scores_close(
            s,
            RemovalCounts {
                true_positives: 1,
                false_positives: 0,
                false_negatives: 0,
            }
            .score(),
        );
    }

    /// Zero words needed removing and zero were removed: nothing was
    /// measured, so every ratio is `None`. It used to be `1.0` on every
    /// field, which is what a genuinely-verified perfect removal looks like
    /// — and read-speech corpora are nothing but this case.
    #[test]
    fn a_perfectly_clean_utterance_measures_nothing() {
        let s = score("we should ship it", "we should ship it", "we should ship it");
        assert_scores_close(s, RemovalCounts::default().score());
        assert_eq!(s.precision, None);
        assert_eq!(s.recall, None);
        assert_eq!(s.f1, None);
    }

    /// Contrast with the vacuous case above: here both denominators are
    /// genuinely non-zero and the intersection is empty, so `0.0` is a real
    /// failure reading, not a placeholder. The hypothesis removed exactly
    /// the wrong two words: it kept the fillers ("um", "so") and dropped
    /// the content words ("we", "go") instead.
    #[test]
    fn disjoint_removed_sets_score_zero_not_a_vacuous_none() {
        let s = score("um so we go", "we go", "um so");
        assert_scores_close(
            s,
            RemovalCounts {
                true_positives: 0,
                false_positives: 2,
                false_negatives: 2,
            }
            .score(),
        );
        assert_eq!(s.precision, Some(0.0));
        assert_eq!(s.recall, Some(0.0));
        assert_eq!(s.f1, Some(0.0));
    }

    // --- Corpus aggregation. ---

    #[test]
    fn counts_sum_matches_manual_addition() {
        let a = RemovalCounts {
            true_positives: 2,
            ..Default::default()
        };
        let b = RemovalCounts {
            false_positives: 1,
            false_negatives: 3,
            ..Default::default()
        };
        let summed: RemovalCounts = [a, b].into_iter().sum();
        assert_eq!(summed, a + b);

        let mut acc = RemovalCounts::default();
        acc += a;
        acc += b;
        assert_eq!(acc, summed);
        assert_eq!(
            summed,
            RemovalCounts {
                true_positives: 2,
                false_positives: 1,
                false_negatives: 3,
            }
        );
    }

    /// Corpus F1 is micro — one ratio over summed counts — and it is a
    /// different number from the average of the per-utterance F1s. A clean
    /// utterance (nothing to remove, `f1() == None`) simply does not
    /// participate, instead of contributing a free `1.0` that would lift
    /// the average.
    #[test]
    fn micro_and_macro_f1_are_different_numbers() {
        let long = score(
            "um so we should really ship it today",
            "we should really ship it today",
            "we should really ship it",
        )
        .counts;
        let short = score("um go", "go", "go").counts;
        let clean = score("we ship", "we ship", "we ship").counts;

        assert_eq!(clean.f1(), None, "a clean utterance measures nothing");

        // long: removed {um, so, today} of which {um, so} were right -> P 2/3;
        // gold {um, so} both found -> R 1. short: 1/1 and 1/1.
        assert_eq!(long.precision(), Some(2.0 / 3.0));
        assert_eq!(short.f1(), Some(1.0));

        let macro_f1 = (long.f1().unwrap() + short.f1().unwrap()) / 2.0;
        let corpus: RemovalCounts = [long, short, clean].into_iter().sum();
        let micro_f1 = corpus.f1().unwrap();

        assert_eq!(
            corpus,
            RemovalCounts {
                true_positives: 3,
                false_positives: 1,
                false_negatives: 0,
            }
        );
        assert!((micro_f1 - 6.0 / 7.0).abs() < 1e-9, "{micro_f1}");
        assert!(
            (macro_f1 - micro_f1).abs() > 1e-6,
            "the two formulas must actually disagree: macro {macro_f1}, \
             micro {micro_f1}"
        );
    }

    /// The ratios and the counts cannot disagree, because the ratios are
    /// derived from the counts and nowhere else.
    #[test]
    fn a_score_is_exactly_its_counts_rescored() {
        for (v, c, h) in [
            ("um so we go", "we go", "um so"),
            ("we ship", "we ship", "we ship"),
            ("the the cat sat", "the cat sat", "the the cat sat"),
        ] {
            let s = score(v, c, h);
            assert_eq!(s, s.counts.score(), "{v:?} / {c:?} / {h:?}");
        }
    }
}
