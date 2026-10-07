//! Evaluation harness: metrics that score the formatting engine's output
//! against public corpora.
//!
//! # The modules
//!
//! - [`corpus`] — loads the committed `tests/fixtures/*.jsonl` cases: **1 000
//!   of them, 200 each from five sources**. LibriSpeech-PC, Earnings-22 and
//!   IndicDiarBench are `kind: "punctuation"`; Disfl-QA and
//!   Earnings-22 Subset 10 are `kind: "disfluency"` and carry the extra
//!   `verbatim` field [`disfluency`] needs. 800 cases are `lang: "en"`, 200
//!   are `lang: "hi"`.
//! - [`per`] — Punctuation Error Rate, the LibriSpeech-PC definition, with
//!   a Python oracle for cross-checking.
//! - [`rawer`] — two WERs with opposite normalisation: `ra_wer` (sees
//!   formatting) and `content_wer` (the content-word regression guard).
//! - [`casing`] — capitalisation precision/recall/F1 and sentence accuracy.
//! - [`disfluency`] — removed-token precision/recall/F1.
//!
//! # Corpus numbers are MICRO, always
//!
//! Every metric here returns per-pair **counts** alongside its ratios, and a
//! corpus figure is built by summing the counts and taking **one** ratio at
//! the end. Averaging the per-pair ratios is a different number: it gives a
//! one-mark sentence the same weight as a twenty-mark one, and it is not
//! what any of the published baselines these metrics are compared against
//! (NeMo's PER, Stanford's truecasing F1, the Switchboard disfluency
//! figures) actually mean.
//!
//! [`per::PunctCounts`], [`rawer::WerCounts`], [`casing::CasingCounts`] and
//! [`disfluency::RemovalCounts`] all implement
//! [`Add`](std::ops::Add)/[`AddAssign`](std::ops::AddAssign)/[`Sum`](std::iter::Sum)
//! so the correct formula is also the shortest one to write, and each has a
//! test proving micro and macro land on different numbers. "Every metric" is
//! a rule this module has to keep: a metric added without a count type is
//! one a caller *cannot* aggregate correctly.
//!
//! # A zero denominator is "not measured". Everything else is a score.
//!
//! The failure mode these metrics are most exposed to is flattery, and it
//! has two shapes:
//!
//! 1. A ratio whose denominator is zero, reported as `1.0` — a flawless
//!    score for text that was never tested. Not a corner case here: 200 of
//!    the 1 000 committed fixtures are caseless Devanagari, which offers no
//!    capitalisation to get right, and the read-speech and earnings-call
//!    sources offer no disfluency to remove.
//! 2. A real zero reported as "not measured" — the same flattery wearing
//!    the fix's clothes. `None` renders as "n/a" in the benchmark report,
//!    so a metric that answers `None` where the honest answer is `0.0` tells
//!    the reader the engine was untested when in fact it failed completely.
//!
//! So `None` means **only** "this ratio's own denominator is genuinely
//! empty", and each ratio has its *own* denominator. Precision's is
//! `TP + FP`, recall's is `TP + FN`, and F1's is `2·TP + FP + FN` — which is
//! empty in strictly fewer cases, so [`casing::CasingCounts::f1`] and
//! [`disfluency::RemovalCounts::f1`] return `Some(0.0)` for the one-sided
//! total failures where one of the other two is `None`. [`rawer::WerCounts`]
//! draws the same line, and [`per::PunctCounts::rate`] documents the hazard
//! for its own `0/0`. Whatever a caller does about a `None`, it has to do it
//! deliberately.
//!
//! # Scope: English is the target, Indic is a regression guard
//!
//! The product priority is English formatting quality. The Hindi fixture is
//! here so a change that mangles Devanagari gets caught, not so a Hindi
//! score can be optimised — which is exactly why the metrics must not
//! *flatter* it either: a guard that structurally cannot fire reads as a
//! pass. `content_wer` trims a named punctuation set rather than
//! "everything non-alphanumeric" for that reason (it was silently erasing
//! word-final viramas and nuktas), and `casing` reports the caseless
//! denominator as zero rather than perfect.
//!
//! The same rule cuts the other way, and it is easy to miss: an English-only
//! artefact must not be filed under "Indic". `casing`'s caseless-token
//! count excludes tokens with no letters at all, because `2022` and `28%`
//! are not a caseless script — and 135 of the 138 letterless target tokens
//! in this corpus are in the *English* fixtures. `rawer` trims `₹` for the
//! same symmetry reason it already trimmed `$`.

pub mod casing;
pub mod corpus;
pub mod disfluency;
pub mod per;
pub mod rawer;
