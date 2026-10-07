//! fmtbench — the formatting benchmark runner.
//!
//! Scores the formatting engine against the committed corpus fixtures and
//! writes a Markdown report. Two modes:
//!
//! - **Offline** (default): scores only the deterministic rule pipeline
//!   (`cleanup::run_pipeline` with the ONNX punctuator absent — its weights
//!   are user-downloaded, not in this repo). No network, no API key, runs in
//!   CI.
//! - **`--live`**: additionally runs each `--model` through the same
//!   rules → chat-completion → guardrail flow the shipped cloud path uses
//!   (`sarvam::ws`), and reports latency percentiles and token usage.
//!   Needs `SARVAM_API_KEY` in the environment (the app itself keeps the key
//!   in the Windows credential store, which a CI-friendly binary should not
//!   reach into). Two measurement-policy departures from the shipped call,
//!   both documented in the report: replies get [`LIVE_CHAT_TIMEOUT`]
//!   instead of the app's 6 s budget (the endpoint's queuing tail is a
//!   latency fact to measure, not a failure to manufacture — the report
//!   counts budget misses separately), and request starts are spaced by
//!   [`LIVE_PACE_FLOOR`] so a long run measures the service rather than its
//!   own 429 throttle.
//!
//! ```text
//! fmtbench --fixtures tests/fixtures --level balanced \
//!     [--live [--model sarvam-105b]... [--guard-dump cases.jsonl]] \
//!     [--limit N] [--out report.md]
//! ```
//!
//! `--guard-dump` (live only) writes one JSON line per live case — the
//! guardrail's measured retention/ratio (via `format::guard::metrics`, the
//! same arithmetic `check` enforces) plus the rule output and model reply
//! they were measured over — so guard thresholds can be recalibrated offline
//! against real model behaviour without repeating the live run.
//!
//! # Why this binary compiles the sources directly
//!
//! `lib.rs` declares `mod eval;` / `mod format;` / `mod cleanup;` privately,
//! so none of them is reachable through the `butterfly_speak_lib` rlib. This
//! binary therefore compiles the same source files into itself via `#[path]`
//! includes — the code under test is the product's own, byte for byte, not a
//! copy. The one exception is a two-line `settings` shim (see below), pinned
//! against the real `settings.rs` by a test.
//!
//! # Reporting discipline (the point of this tool)
//!
//! - Corpus figures are **micro**: counts summed across cases, one ratio at
//!   the end. `PunctCounts`/`CasingCounts`/`RemovalCounts`/`WerCounts`
//!   implement `Sum` for exactly this; a mean of per-case rates is a
//!   different number and is never printed — content-WER included, now that
//!   `eval::rawer` exposes a count type for it too.
//! - A zero denominator prints **"n/a"**, never `0.0` or `1.0`, and the
//!   count of unmeasurable cases appears beside every affected figure.
//! - Every per-source score is printed **next to that source's caveat** from
//!   `tests/fixtures/sources.json`. A number without its caveat is worse
//!   than no number.

// The `#[path]`-included trees are product code, linted where they live (the
// lib crate); re-linting them here would only duplicate diagnostics, and the
// bin's reachability analysis would flag every item this benchmark happens
// not to call. The benchmark's own code below carries no such allowance.

#[allow(dead_code, clippy::all)]
#[path = "../eval/mod.rs"]
mod eval;

#[allow(dead_code, clippy::all)]
#[path = "../format/mod.rs"]
mod format;

#[allow(dead_code, clippy::all)]
#[path = "../cleanup/mod.rs"]
mod cleanup;

/// `cleanup::snippets` matches a rule against every canonically-equivalent
/// spelling of itself, so it reaches for `crate::canonical`. Included rather
/// than shimmed like `settings` below: it is pure and pulls in nothing but
/// `unicode-normalization`, and a shim would be a second implementation of
/// the app's "are these the same word" rule that could silently disagree with
/// the one the benchmark is supposed to be measuring.
#[allow(dead_code, clippy::all)]
#[path = "../canonical.rs"]
mod canonical;

/// `cleanup::CleanupSettings::default()` reads
/// `crate::settings::DEFAULT_POLISH_MODEL`. The real `settings` module drags
/// in the model catalog and RAM probing, none of which a benchmark binary
/// needs, so this shim carries the one constant. It is pinned against the
/// real `src/settings.rs` by `the_settings_shim_matches_the_real_default` —
/// if the app's default model changes, that test fails and this line follows.
mod settings {
    pub const DEFAULT_POLISH_MODEL: &str = "sarvam-105b";
}

mod sarvam {
    // Only `chat.rs`: the rest of `sarvam` (websocket transport, credential
    // store) is dictation plumbing this benchmark must not depend on.
    #[allow(dead_code, clippy::all)]
    #[path = "../../sarvam/chat.rs"]
    pub mod chat;
}

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};

use cleanup::{CleanupSettings, ModelCaps};
use eval::casing::CasingCounts;
use eval::corpus::{Case, SourceMeta, Sources};
use eval::disfluency::RemovalCounts;
use eval::per::PunctCounts;
use eval::rawer::WerCounts;
use format::backend::{Backend, ChatReply};
use format::guard::{
    check, metrics, RejectReason, Verdict, CATASTROPHIC_CONTENT_FLOOR,
    CATASTROPHIC_RATIO_CEILING, REPORT_ONLY,
};
use format::level::CleanupLevel;
use format::timing::{percentiles, Percentiles};
use sarvam::chat::{polish_with_timeout, PolishOutcome, POLISH_TIMEOUT};

// ---------------------------------------------------------------------------
// Live-call policy: measure the service's tail, don't trip its throttle
// ---------------------------------------------------------------------------

/// HTTP deadline for the benchmark's chat calls. Deliberately much longer than
/// the app's 6 s `chat::POLISH_TIMEOUT`: Sarvam's chat endpoint queues under
/// sustained load (measured: identical trivial requests answered in 150–400 ms
/// normally and 1.5–51.4 s in the tail), and a benchmark that cuts the tail off
/// at 6 s reports it as a 26% transport- failure rate instead of the latency it
/// is. 90 s covers the worst observed tail with headroom; anything slower is
/// reported as the timeout it is. The app's own budget is NOT changed by this —
/// the latency section reports how many replies would have missed it.
const LIVE_CHAT_TIMEOUT: Duration = Duration::from_secs(90);

/// Minimum spacing between request starts. Sequential-but-back-to-back calls
/// (~4/s) start drawing HTTP 429 from the endpoint after roughly a hundred
/// requests (measured); the benchmark's job is to measure the
/// service, not its own throttling, so it stays under that ceiling (~75
/// requests/min). Successful-call latency usually exceeds this floor anyway,
/// so it mostly prices in only after fast failures.
const LIVE_PACE_FLOOR: Duration = Duration::from_millis(800);

// ---------------------------------------------------------------------------
// Punctuation mark sets
// ---------------------------------------------------------------------------

/// Marks scored for English cases: NeMo `punct_er`'s default set, which is
/// also what `eval::per`'s own tests and oracle agreement claims are stated
/// over. Matches the measured fixture inventory: across all 800 English
/// targets (five sources, `earnings22-subset10` among them) the only other
/// punctuation is apostrophes and hyphens (word orthography that survives in
/// `input` and is not restorable), `%` and `&` (ITN symbols, not punctuation
/// slots), and four ellipses. `!` occurs zero times in any committed English
/// target, so listing it would be a no-op.
const EN_MARKS: &[char] = &['.', ',', '?'];

/// Marks scored for Hindi: the Devanagari danda plus the Latin marks the
/// fixture actually uses. Parentheses are deliberately EXCLUDED here even
/// though the fixture's targets carry 956 of them: they are the
/// english-gloss transcription convention (`sources.json` caveat), not
/// punctuation a dictation formatter should restore. The with-parentheses
/// figure is computed separately (see [`HI_MARKS_WITH_GLOSS_PARENS`]) so the
/// size of that artifact is visible instead of silently folded in.
const HI_MARKS: &[char] = &['\u{0964}', '.', ',', '?'];

/// The Hindi set *with* the gloss parentheses, reported only as a labelled
/// secondary number sizing the annotation artifact.
const HI_MARKS_WITH_GLOSS_PARENS: &[char] = &['\u{0964}', '.', ',', '?', '(', ')'];

fn marks_for(lang: &str) -> &'static [char] {
    if lang == "hi" {
        HI_MARKS
    } else {
        EN_MARKS
    }
}

// ---------------------------------------------------------------------------
// Per-case scoring and per-source (micro) aggregation
// ---------------------------------------------------------------------------

/// Everything one (case, hypothesis) pair contributes. Counts, not ratios:
/// ratios happen once, at the end, per source or rollup.
struct CaseScore {
    punct: PunctCounts,
    /// A secondary Hindi-only count with the gloss parentheses scored.
    punct_gloss: Option<PunctCounts>,
    casing: CasingCounts,
    /// This case's content-WER edit tally (substitutions/insertions/
    /// deletions against the lowercased, punctuation-stripped target).
    /// Summed into `Agg::content` and rated once at the corpus level — see
    /// `eval::rawer`'s module doc for why a per-case mean would be a
    /// different, non-comparable number.
    content: WerCounts,
    /// Present only for disfluency cases (those carry `verbatim`).
    removal: Option<RemovalScoreBits>,
}

struct RemovalScoreBits {
    counts: RemovalCounts,
    /// The formatter removed nothing, so per-case precision was undefined.
    precision_undefined: bool,
    /// Nothing needed removing, so per-case recall was undefined.
    recall_undefined: bool,
}

fn score_case(case: &Case, hypothesis: &str) -> CaseScore {
    let punct = eval::per::punctuation_error_rate(&case.target, hypothesis, marks_for(&case.lang));
    let punct_gloss = (case.lang == "hi").then(|| {
        eval::per::punctuation_error_rate(&case.target, hypothesis, HI_MARKS_WITH_GLOSS_PARENS)
    });
    let casing = eval::casing::score(&case.target, hypothesis).counts;
    let content = eval::rawer::content_wer_counts(&case.target, hypothesis);
    let removal = case.verbatim.as_deref().map(|verbatim| {
        let s = eval::disfluency::score(verbatim, &case.target, hypothesis);
        RemovalScoreBits {
            counts: s.counts,
            precision_undefined: s.precision.is_none(),
            recall_undefined: s.recall.is_none(),
        }
    });
    CaseScore {
        punct,
        punct_gloss,
        casing,
        content,
        removal,
    }
}

/// Micro accumulator for one source (or one slice of one source, or one
/// rollup of several sources). Sums counts; every ratio is taken exactly
/// once, by the accessors, which return `None` for a zero denominator.
#[derive(Default, Clone)]
struct Agg {
    cases: usize,
    punct: PunctCounts,
    /// Cases where neither side had any scored mark: 0/0, "nothing to
    /// measure", excluded from nothing (they add zero counts) but reported.
    punct_na_cases: usize,
    punct_gloss: PunctCounts,
    has_gloss_variant: bool,
    casing: CasingCounts,
    /// Cases with zero caseable aligned token pairs.
    casing_na_cases: usize,
    /// Summed content-WER edit tally across every case — a corpus micro
    /// figure exactly like `punct`/`casing`/`removal`, not a mean. Rate it
    /// once via `WerCounts::rate`, which returns `None` only when this WHOLE
    /// slice has zero reference tokens and zero errors (nothing measurable
    /// at all), never as an artifact of any single case.
    content: WerCounts,
    /// Cases where `content` changed at all (any S/I/D). Not derivable from
    /// the summed `content` once other cases are folded in, so it is tracked
    /// alongside it.
    content_drift_cases: usize,
    /// The single worst-drifting case seen so far: its id and its own
    /// per-case content-WER rate. Also not derivable from the sum.
    content_worst: Option<(String, f64)>,
    removal: RemovalCounts,
    removal_cases: usize,
    removal_precision_na_cases: usize,
    removal_recall_na_cases: usize,
}

impl Agg {
    fn add(&mut self, case_id: &str, s: &CaseScore) {
        self.cases += 1;

        self.punct += s.punct;
        let ops = s.punct.correct + s.punct.substitutions + s.punct.insertions + s.punct.deletions;
        if ops == 0 {
            self.punct_na_cases += 1;
        }
        if let Some(g) = s.punct_gloss {
            self.punct_gloss += g;
            self.has_gloss_variant = true;
        }

        self.casing += s.casing;
        let caseable = s.casing.true_positives
            + s.casing.false_positives
            + s.casing.false_negatives
            + s.casing.true_negatives;
        if caseable == 0 {
            self.casing_na_cases += 1;
        }

        self.content += s.content;
        if s.content.errors() > 0 {
            self.content_drift_cases += 1;
            // `errors() > 0` guarantees `rate()` is `Some` — either a real
            // ratio, or `Some(inf)` when this one case's own reference was
            // empty; see `WerCounts::rate`'s doc. The `if let` is defensive
            // rather than an `.unwrap()`, not a case this is expected to
            // skip.
            if let Some(w) = s.content.rate() {
                if self.content_worst.as_ref().is_none_or(|(_, worst)| w > *worst) {
                    self.content_worst = Some((case_id.to_string(), w));
                }
            }
        }

        if let Some(r) = &s.removal {
            self.removal += r.counts;
            self.removal_cases += 1;
            if r.precision_undefined {
                self.removal_precision_na_cases += 1;
            }
            if r.recall_undefined {
                self.removal_recall_na_cases += 1;
            }
        }
    }

    fn merge(&mut self, other: &Agg) {
        self.cases += other.cases;
        self.punct += other.punct;
        self.punct_na_cases += other.punct_na_cases;
        self.punct_gloss += other.punct_gloss;
        self.has_gloss_variant |= other.has_gloss_variant;
        self.casing += other.casing;
        self.casing_na_cases += other.casing_na_cases;
        self.content += other.content;
        self.content_drift_cases += other.content_drift_cases;
        if other
            .content_worst
            .as_ref()
            .is_some_and(|(_, w)| self.content_worst.as_ref().is_none_or(|(_, mine)| w > mine))
        {
            self.content_worst = other.content_worst.clone();
        }
        self.removal += other.removal;
        self.removal_cases += other.removal_cases;
        self.removal_precision_na_cases += other.removal_precision_na_cases;
        self.removal_recall_na_cases += other.removal_recall_na_cases;
    }

    /// Corpus PER: ONE ratio over the summed counts, or `None` when there
    /// were no punctuation operations at all. `PunctCounts::rate` returns
    /// `0.0` for its 0/0 and documents that callers must not present that as
    /// a score — this accessor is that check.
    fn per(&self) -> Option<f64> {
        let ops =
            self.punct.correct + self.punct.substitutions + self.punct.insertions + self.punct.deletions;
        (ops > 0).then(|| self.punct.rate())
    }

    fn per_gloss(&self) -> Option<f64> {
        if !self.has_gloss_variant {
            return None;
        }
        let g = self.punct_gloss;
        let ops = g.correct + g.substitutions + g.insertions + g.deletions;
        (ops > 0).then(|| g.rate())
    }
}

// ---------------------------------------------------------------------------
// The two hypothesis producers: rules-only, and live (rules -> model -> guard)
// ---------------------------------------------------------------------------

/// The deterministic rule pipeline, exactly as the local dictation path runs
/// it (`cleanup::run_pipeline`), with one absence that must be reported
/// beside every offline number: the ONNX punctuation/casing model is NOT
/// loaded (`punct: None`) because its weights are downloaded by the user, not
/// committed. The shipped local path therefore restores MORE punctuation
/// than this baseline; the offline figures understate it by the punctuator's
/// whole contribution.
fn rules_hypothesis(input: &str, settings: &CleanupSettings) -> String {
    cleanup::run_pipeline(
        input.to_string(),
        ModelCaps { native_punct: false },
        settings,
        None,
    )
    .0
}

/// What the guardrail decided for one live case.
#[derive(Debug, PartialEq)]
enum GuardOutcome {
    Accepted,
    /// Rejected by a provisional-tier (calibrated) bound while `REPORT_ONLY` is
    /// true: the model's text is still used, but the flag is counted. With
    /// `REPORT_ONLY` false — the shipped state — this outcome cannot occur;
    /// those rejections are [`GuardOutcome::Enforced`].
    Provisional(&'static str),
    /// Enforced rejection: the rule-pipeline output was used instead.
    Enforced(&'static str),
}

/// Mirrors `sarvam::ws::resolve_format_inner` — which is `pub(crate)` to the
/// app crate and unreachable from this standalone binary — decision for
/// decision: the empty-reply check that runs before `guard::check`, the
/// always-enforced tier (truncation, catastrophic content loss, catastrophic
/// over-expansion), and the `report_only` gate for everything else. The
/// thresholds themselves come from the shared `format::guard` constants, so
/// only the ~20 lines of tiering below can drift; the tests pin them to the
/// same canonical cases `sarvam::ws`'s own tests use.
fn resolve_live(
    rule_output: &str,
    reply: &ChatReply,
    level: CleanupLevel,
    report_only: bool,
) -> (String, GuardOutcome) {
    let formatted = reply.text.trim();
    if formatted.is_empty() {
        return (rule_output.to_string(), GuardOutcome::Enforced("empty reply"));
    }
    match check(rule_output, formatted, reply, level) {
        Verdict::Accept => (formatted.to_string(), GuardOutcome::Accepted),
        Verdict::Reject(reason) => {
            let (label, always_enforced) = match &reason {
                RejectReason::Truncated => ("truncated", true),
                RejectReason::ContentLost { retained } => {
                    if *retained <= CATASTROPHIC_CONTENT_FLOOR {
                        ("content lost (catastrophic)", true)
                    } else {
                        ("content lost", false)
                    }
                }
                RejectReason::LengthRatio { ratio } => {
                    if *ratio >= CATASTROPHIC_RATIO_CEILING {
                        ("length ratio (catastrophic)", true)
                    } else {
                        ("length ratio", false)
                    }
                }
            };
            if always_enforced || !report_only {
                (rule_output.to_string(), GuardOutcome::Enforced(label))
            } else {
                (formatted.to_string(), GuardOutcome::Provisional(label))
            }
        }
    }
}

/// Nearest-millisecond rounding, ties away from zero — the same rule as the
/// private `format::timing::round_ms`, for the same reason: truncation is a
/// one-directional downward bias a latency report must not contain.
fn to_ms(d: Duration) -> u64 {
    ((d.as_micros() + 500) / 1000) as u64
}

// ---------------------------------------------------------------------------
// Per-case guard dump (--guard-dump): the calibration instrument
// ---------------------------------------------------------------------------

/// Where a dump line comes from: fixed for a whole live run except `source`,
/// which tracks the fixture currently being scored.
struct DumpCtx<'a> {
    model: &'a str,
    level: CleanupLevel,
    source: &'a str,
}

/// One JSONL line per live case: what the guard measured (via
/// `guard::metrics`, the exact arithmetic `check` runs, so the dumped values
/// cannot drift from the verdicts) plus the two texts the measurement was
/// taken over, so thresholds and tokenizer changes can be re-evaluated
/// offline against real model output without another 15-minute live run.
/// The fixtures already carry `input`/`target` keyed by case id; the dump
/// deliberately repeats neither.
fn guard_dump_line(
    ctx: &DumpCtx,
    case_id: &str,
    rule_out: &str,
    reply: &ChatReply,
    outcome: &GuardOutcome,
    latency_ms: u64,
) -> serde_json::Value {
    let m = metrics(rule_out, reply.text.trim(), ctx.level);
    let (outcome_str, label) = match outcome {
        GuardOutcome::Accepted => ("accepted", None),
        GuardOutcome::Provisional(l) => ("provisional", Some(*l)),
        GuardOutcome::Enforced(l) => ("enforced", Some(*l)),
    };
    serde_json::json!({
        "model": ctx.model,
        "level": format!("{:?}", ctx.level).to_lowercase(),
        "source": ctx.source,
        "case": case_id,
        "outcome": outcome_str,
        "label": label,
        "finish_reason": reply.finish_reason,
        "latency_ms": latency_ms,
        "input_words": m.input_words,
        "output_words": m.output_words,
        "expected": m.expected,
        "kept": m.kept,
        "retained": m.retained,
        "ratio": m.ratio,
        "rule_out": rule_out,
        "reply": reply.text,
    })
}

/// The failed-call sibling of [`guard_dump_line`]: no reply, no metrics —
/// but the row still exists, so an offline analysis over the dump sees the
/// same denominator the report does instead of silently healthier data.
fn guard_dump_failure_line(ctx: &DumpCtx, case_id: &str, reason: &str) -> serde_json::Value {
    serde_json::json!({
        "model": ctx.model,
        "level": format!("{:?}", ctx.level).to_lowercase(),
        "source": ctx.source,
        "case": case_id,
        "outcome": "call_failed",
        "reason": reason,
    })
}

/// Append one dump line, flushing immediately: a live run is long and
/// interruptible, and a partial dump of real replies is still valuable while
/// a buffered-then-lost one is not.
fn write_dump_line(dump: &mut std::fs::File, line: &serde_json::Value) -> Result<()> {
    use std::io::Write;
    writeln!(dump, "{line}").context("writing --guard-dump line")?;
    dump.flush().context("flushing --guard-dump")
}

// ---------------------------------------------------------------------------
// Corpus loading
// ---------------------------------------------------------------------------

struct LoadedSource {
    key: String,
    meta: SourceMeta,
    cases: Vec<Case>,
    target_words: usize,
}

fn load_corpus(fixtures: &Path, limit: Option<usize>) -> Result<(Sources, Vec<LoadedSource>)> {
    let sources = eval::corpus::load_sources(&fixtures.join("sources.json"))?;
    let mut loaded = Vec::new();
    for (key, meta) in &sources.sources {
        let path = fixtures.join(&meta.fixture);
        let mut cases = eval::corpus::load(&path)?;
        if cases.len() != meta.cases {
            bail!(
                "{}: fixture holds {} cases but sources.json records {} — \
                 refusing to report on a silently drifted corpus (re-run \
                 tools/fetch_corpora.py or fix the metadata first)",
                path.display(),
                cases.len(),
                meta.cases
            );
        }
        if let Some(n) = limit {
            cases.truncate(n);
        }
        let target_words = cases.iter().map(|c| c.target.split_whitespace().count()).sum();
        loaded.push(LoadedSource {
            key: key.clone(),
            meta: meta.clone(),
            cases,
            target_words,
        });
    }
    Ok((sources, loaded))
}

// ---------------------------------------------------------------------------
// A benchmark run (rules-only, or one live model)
// ---------------------------------------------------------------------------

struct ModelRun {
    /// "rules only (deterministic pipeline)" or the model id.
    label: String,
    /// `Some(base URL)` for a live run; `None` marks the offline baseline.
    backend_url: Option<String>,
    per_source: BTreeMap<String, Agg>,
    /// (source key, slice label, agg) — tag-based slices of a source.
    slices: Vec<(String, String, Agg)>,
    latency: Percentiles,
    /// Live calls that produced no latency sample (the chat call failed and
    /// the case fell back to rule output). The latency denominator below is
    /// `latency.n` of `latency.n + skipped` attempts — dropping these
    /// silently would overstate health, exactly like a timing log that
    /// ignored its `skipped` lines.
    latency_skipped: usize,
    /// Successful live calls whose measured span exceeded the app's own
    /// `chat::POLISH_TIMEOUT`. The benchmark waits out the service's tail
    /// (`LIVE_CHAT_TIMEOUT`) so the reply can be scored and the latency
    /// measured — but the shipped app would have fallen back on these, and
    /// a report that hid that would present a 30 s reply as product-ready.
    over_app_budget: usize,
    guard_enforced: BTreeMap<&'static str, usize>,
    guard_provisional: BTreeMap<&'static str, usize>,
    prompt_tokens: u64,
    completion_tokens: u64,
    calls: usize,
    /// (case id, reason) for failed chat calls, every one listed in the
    /// rendering.
    failures: Vec<(String, String)>,
}

impl ModelRun {
    fn new(label: String, backend_url: Option<String>) -> Self {
        ModelRun {
            label,
            backend_url,
            per_source: BTreeMap::new(),
            slices: Vec::new(),
            latency: Percentiles::default(),
            latency_skipped: 0,
            over_app_budget: 0,
            guard_enforced: BTreeMap::new(),
            guard_provisional: BTreeMap::new(),
            prompt_tokens: 0,
            completion_tokens: 0,
            calls: 0,
            failures: Vec::new(),
        }
    }

    fn record(&mut self, source_key: &str, case: &Case, hypothesis: &str) {
        let s = score_case(case, hypothesis);
        self.per_source.entry(source_key.to_string()).or_default().add(&case.id, &s);
        for (key, label, agg) in &mut self.slices {
            if key == source_key && slice_matches(label, case) {
                agg.add(&case.id, &s);
            }
        }
    }
}

/// The tag slices a report must be able to separate, per the source caveats:
/// the Hindi gloss artifact, and which disfluency kind a subset10 case
/// exercises. Labels double as the membership predicate (see
/// [`slice_matches`]) so a slice's name can never disagree with its
/// contents.
fn slice_plan(loaded: &[LoadedSource]) -> Vec<(String, String)> {
    let mut plan = Vec::new();
    for src in loaded {
        let tags = &src.meta.tags;
        if tags.contains_key("english-gloss") {
            plan.push((src.key.clone(), "gloss-dominated".to_string()));
            plan.push((src.key.clone(), "english-gloss, not dominated".to_string()));
            plan.push((src.key.clone(), "no gloss".to_string()));
        }
        for tag in ["filler", "repetition", "false-start"] {
            if tags.contains_key(tag) {
                plan.push((src.key.clone(), format!("tag: {tag}")));
            }
        }
    }
    plan
}

fn slice_matches(label: &str, case: &Case) -> bool {
    match label {
        "gloss-dominated" => case.has_tag("gloss-dominated"),
        "english-gloss, not dominated" => {
            case.has_tag("english-gloss") && !case.has_tag("gloss-dominated")
        }
        "no gloss" => !case.has_tag("english-gloss"),
        _ => label
            .strip_prefix("tag: ")
            .is_some_and(|tag| case.has_tag(tag)),
    }
}

fn run_rules(loaded: &[LoadedSource], settings: &CleanupSettings) -> ModelRun {
    let mut run = ModelRun::new("rules only (deterministic pipeline)".into(), None);
    run.slices = slice_plan(loaded)
        .into_iter()
        .map(|(k, l)| (k, l, Agg::default()))
        .collect();
    for src in loaded {
        for case in &src.cases {
            let hyp = rules_hypothesis(&case.input, settings);
            run.record(&src.key, case, &hyp);
        }
    }
    run
}

/// One live model over the whole corpus, sequentially (deterministic order,
/// no self-inflicted rate limiting). Mirrors the shipped cloud path:
/// `run_cloud_pipeline` first (the model sees rule output, per
/// `format::guard`'s comparison-basis requirement), then `chat::polish` with
/// the level's prompt and an empty personal dictionary, then the guard
/// tiering of [`resolve_live`]. The measured span is the polish call plus
/// the guard check — the same span the product logs as `format_ms` — so it
/// EXCLUDES audio capture, ASR, and the drain wait; it is not an end-to-end
/// dictation latency and the report says so.
async fn run_live_model(
    loaded: &[LoadedSource],
    settings: &CleanupSettings,
    level: CleanupLevel,
    api_key: &str,
    model: &str,
    mut dump: Option<&mut std::fs::File>,
) -> Result<ModelRun> {
    let backend = Backend::sarvam(api_key, model);
    let mut run = ModelRun::new(model.to_string(), Some(backend.base_url.clone()));
    run.slices = slice_plan(loaded)
        .into_iter()
        .map(|(k, l)| (k, l, Agg::default()))
        .collect();
    let http = reqwest::Client::new();
    let prompt = level.prompt();
    let mut samples: Vec<u64> = Vec::new();
    let mut last_start: Option<Instant> = None;

    for src in loaded {
        for case in &src.cases {
            let (rule_out, _) = cleanup::run_cloud_pipeline(case.input.clone(), settings);
            // Keep request starts at least LIVE_PACE_FLOOR apart so a run
            // never measures the endpoint's own 429 throttle instead of the
            // service (an async sleep: the reactor stays live).
            if let Some(prev) = last_start {
                let since = prev.elapsed();
                if since < LIVE_PACE_FLOOR {
                    tokio::time::sleep(LIVE_PACE_FLOOR - since).await;
                }
            }
            run.calls += 1;
            let started = Instant::now();
            last_start = Some(started);
            let outcome =
                polish_with_timeout(
                    &http,
                    &backend,
                    &rule_out,
                    &[],
                    &prompt,
                    // The benchmark measures the SHIPPED prompt. A user's
                    // override from the Prompts page would make two runs
                    // incomparable, so this is `None` on purpose.
                    None,
                    // No text before the cursor: the benchmark measures
                    // one-shot polish, and context would change the prompt.
                    None,
                    LIVE_CHAT_TIMEOUT,
                )
                .await;
            let hyp = match outcome {
                PolishOutcome::Formatted(reply) => {
                    run.prompt_tokens += u64::from(reply.prompt_tokens);
                    run.completion_tokens += u64::from(reply.completion_tokens);
                    let (text, verdict) = resolve_live(&rule_out, &reply, level, REPORT_ONLY);
                    let elapsed = started.elapsed();
                    if elapsed > POLISH_TIMEOUT {
                        run.over_app_budget += 1;
                    }
                    samples.push(to_ms(elapsed));
                    if let Some(dump) = dump.as_deref_mut() {
                        let ctx = DumpCtx { model, level, source: &src.key };
                        let line = guard_dump_line(
                            &ctx,
                            &case.id,
                            &rule_out,
                            &reply,
                            &verdict,
                            to_ms(elapsed),
                        );
                        write_dump_line(dump, &line)?;
                    }
                    match verdict {
                        GuardOutcome::Accepted => {}
                        GuardOutcome::Provisional(label) => {
                            *run.guard_provisional.entry(label).or_insert(0) += 1;
                        }
                        GuardOutcome::Enforced(label) => {
                            *run.guard_enforced.entry(label).or_insert(0) += 1;
                        }
                    }
                    text
                }
                PolishOutcome::Failed(failure) => {
                    // The benchmark's own report, on the developer's machine,
                    // so it keeps the server's message (`shown`).
                    let reason = failure.shown().to_string();
                    run.latency_skipped += 1;
                    if let Some(dump) = dump.as_deref_mut() {
                        let ctx = DumpCtx { model, level, source: &src.key };
                        let line = guard_dump_failure_line(&ctx, &case.id, &reason);
                        write_dump_line(dump, &line)?;
                    }
                    run.failures.push((case.id.clone(), reason));
                    rule_out
                }
            };
            run.record(&src.key, case, &hyp);
        }
    }
    run.latency = percentiles(&mut samples);
    Ok(run)
}

// ---------------------------------------------------------------------------
// Rollups: per-(language, kind) groups, never one blended aggregate
// ---------------------------------------------------------------------------

/// One (language, kind) group of sources. There is deliberately NO
/// all-corpus aggregate anywhere in the report: read speech + earnings
/// calls + Hindi glosses averaged together is a number that describes
/// nothing. See [`Report::run_rollups`].
struct Rollup {
    label: String,
    source_keys: Vec<String>,
    agg: Agg,
}

// ---------------------------------------------------------------------------
// The report
// ---------------------------------------------------------------------------

struct Report {
    generated_utc: String,
    git_commit: String,
    fixtures_dir: String,
    level: String,
    limit: Option<usize>,
    methodology: String,
    sources: Vec<SourceInfo>,
    runs: Vec<ModelRun>,
}

struct SourceInfo {
    key: String,
    meta: SourceMeta,
    cases_scored: usize,
    target_words: usize,
}

fn fmt3(x: f64) -> String {
    format!("{x:.3}")
}

/// "n/a" is a different fact from 0.000 and the two must never be conflated.
fn opt3(x: Option<f64>) -> String {
    x.map(fmt3).unwrap_or_else(|| "n/a".into())
}

impl Report {
    fn to_markdown(&self) -> String {
        let mut md = String::new();
        let live = self.runs.iter().any(|r| r.backend_url.is_some());

        md.push_str("# Butterfly Speak formatting benchmark\n\n");

        // -- Provenance ----------------------------------------------------
        md.push_str("## Provenance\n\n");
        md.push_str(&format!("- Generated: {}\n", self.generated_utc));
        md.push_str(&format!("- Git commit: {}\n", self.git_commit));
        md.push_str(&format!("- Fixtures: `{}`\n", self.fixtures_dir));
        md.push_str(&format!(
            "- Cleanup level: `{}` (sets the model prompt and guardrail bounds; \
             the deterministic rule stages ignore it)\n",
            self.level
        ));
        match self.limit {
            Some(n) => md.push_str(&format!(
                "- **SMOKE RUN — first {n} cases per source only.** These are \
                 not the corpus figures; re-run without `--limit` before \
                 quoting any number below.\n"
            )),
            None => md.push_str("- Full corpus: every committed case was scored.\n"),
        }
        for run in &self.runs {
            match &run.backend_url {
                Some(url) => md.push_str(&format!(
                    "- Model `{}` via `{}` (live; sequential requests from this machine)\n",
                    run.label, url
                )),
                None => md.push_str(&format!(
                    "- `{}`: no network, no model — the offline baseline\n",
                    run.label
                )),
            }
        }
        md.push_str("\n| source | cases scored | target words | license | pinned upstream revision |\n");
        md.push_str("|---|---|---|---|---|\n");
        for s in &self.sources {
            md.push_str(&format!(
                "| {} | {} | {} | {} | `{}` ({}) |\n",
                s.key, s.cases_scored, s.target_words, s.meta.license, s.meta.revision,
                s.meta.revision_kind
            ));
        }
        md.push('\n');

        // -- Methodology ---------------------------------------------------
        md.push_str("## Methodology\n\n");
        md.push_str(&format!("{}\n\n", self.methodology));
        md.push_str(
            "- **Offline (\"rules only\")**: `cleanup::run_pipeline` — spoken \
             commands, filler removal, backtrack self-corrections, ITN, tidy \
             (sentence-initial casing + terminal period). The app's ONNX \
             punctuation/casing model is **not** loaded (its weights are \
             user-downloaded, not committed), so this baseline understates \
             the shipped local path by that model's whole contribution.\n",
        );
        if live {
            md.push_str(&format!(
                "- **Live**: the shipped cloud flow — `cleanup::run_cloud_pipeline`, \
                 then one chat completion per case (system prompt = the \
                 level's prompt, empty personal dictionary, temperature 0), \
                 then the output guardrail with the product's current \
                 tiering. A rejected or failed call falls back to the rule \
                 output, exactly as the app would, and the fallback is what \
                 gets scored. Two deliberate departures from the shipped \
                 call, both measurement policy: the benchmark waits up to \
                 {} s per reply where the app's own budget is {} s (the \
                 endpoint queues under sustained load — a 6 s cutoff would \
                 mislabel that tail as transport failures; the latency \
                 section counts the replies that missed the app's budget), \
                 and request starts are spaced at least {} ms apart to stay \
                 under the endpoint's measured HTTP 429 throttle. Note the \
                 benchmark feeds the model simulated raw ASR (lowercased, \
                 unpunctuated); in production the cloud model input is \
                 Saaras output, which already carries punctuation, so the \
                 live task here is strictly harder than the shipped cloud \
                 path.\n",
                LIVE_CHAT_TIMEOUT.as_secs(),
                POLISH_TIMEOUT.as_secs(),
                LIVE_PACE_FLOOR.as_millis(),
            ));
        }
        md.push_str(&format!(
            "- **PER mark sets** — stated because the score means nothing \
             without them: English `{}` (NeMo `punct_er`'s default; `!` does \
             not occur in any committed English target). Hindi `{}` — \
             parentheses excluded because 956 of the fixture's 1,432 \
             punctuation marks are its english-gloss transcription \
             convention, not formatter-restorable punctuation; a \
             with-parentheses figure is reported separately to size that \
             artifact.\n",
            EN_MARKS.iter().collect::<String>(),
            HI_MARKS.iter().collect::<String>(),
        ));
        md.push_str(
            "- **Aggregation is micro, always, with no exception**: counts \
             summed over cases, one ratio at the end. A mean of per-case \
             rates is a different, non-comparable number and does not \
             appear in this report — including for the content-WER guard, \
             which sums a `WerCounts` tally exactly like the PER, casing and \
             disfluency figures above it.\n",
        );
        md.push_str(
            "- **\"n/a\" means a zero denominator** — nothing of that kind \
             was measurable (e.g. capitalisation over caseless Devanagari). \
             It is never folded into a score as 0 or 1; the count of such \
             cases is printed beside every affected figure.\n",
        );
        md.push_str(
            "- **What the content-WER guard counts, per source family**: on \
             punctuation sources it is content-word drift introduced by the \
             pipeline itself (e.g. a filler heuristic or ITN editing words \
             the reference spells differently). On disfluency sources the \
             reference is the CLEANED side, so disfluent words left in place \
             count as drift — there it overlaps with removal recall rather \
             than being an independent integrity signal. On indic-diarbench \
             the gloss artifact inflates it too: a formatter that outputs \
             the gloss as two plain words instead of `word(gloss)` is \
             charged a substitution plus an insertion.\n",
        );
        md.push_str(
            "- **Reference baselines** (from the metric modules' docs): \
             truecasing token F1 — Stanford CoreNLP CRF 90.89, LSTM-LARGE \
             93.19, both micro, English clean prose. Switchboard-style \
             disfluency F1 by type — repetitions ~97.5, corrections ~80.0, \
             restarts ~57.1. Compare like with like: English micro figures \
             only.\n\n",
        );

        // -- Results per run ----------------------------------------------
        for run in &self.runs {
            md.push_str(&format!("## Results: {}\n\n", run.label));
            let groups = self.run_rollups(run);

            md.push_str("| slice | sources | cases | PER (micro) | PER n/a cases | casing F1 (micro) | casing n/a cases | sentence acc | disfluency F1 (micro) | content-WER guard (micro) |\n");
            md.push_str("|---|---|---|---|---|---|---|---|---|---|\n");
            for g in &groups {
                md.push_str(&render_rollup_row(g));
            }
            md.push('\n');

            // Per source, caveat beside score.
            md.push_str("### By source\n\n");
            for s in &self.sources {
                let Some(agg) = run.per_source.get(&s.key) else {
                    continue;
                };
                md.push_str(&format!("#### {} — {}\n\n", s.key, s.meta.name));
                md.push_str(&format!("> {}\n\n", s.meta.caveat));
                md.push_str(&render_source_table(agg));
                let slice_lines: String = run
                    .slices
                    .iter()
                    .filter(|(k, _, a)| k == &s.key && a.cases > 0)
                    .map(|(_, label, a)| render_slice_row(label, a))
                    .collect();
                if !slice_lines.is_empty() {
                    md.push_str("\nSlices (from the source's own case tags):\n\n");
                    md.push_str(&slice_lines);
                }
                md.push('\n');
            }

            if run.backend_url.is_some() {
                md.push_str(&render_live_sections(run));
            }
        }

        if self.runs.len() > 1 {
            md.push_str(&self.render_comparison());
        }

        md
    }

    fn run_rollups(&self, run: &ModelRun) -> Vec<Rollup> {
        // Rebuild rollups from the stored per-source aggregates and the
        // source metadata already carried by the report.
        let mut groups: BTreeMap<(String, String), Rollup> = BTreeMap::new();
        for s in &self.sources {
            let Some(agg) = run.per_source.get(&s.key) else {
                continue;
            };
            let lang = s.meta.langs.join("+");
            let kind = s.meta.kinds.join("+");
            let entry = groups.entry((lang.clone(), kind.clone())).or_insert_with(|| Rollup {
                label: match (lang.as_str(), kind.as_str()) {
                    ("en", "punctuation") => "English punctuation + casing restoration".into(),
                    ("en", "disfluency") => "English disfluency removal".into(),
                    ("hi", "punctuation") => {
                        "Hindi restoration (REGRESSION GUARD, not a target)".into()
                    }
                    (l, k) => format!("{l} {k}"),
                },
                source_keys: Vec::new(),
                agg: Agg::default(),
            });
            entry.source_keys.push(s.key.clone());
            entry.agg.merge(agg);
        }
        groups.into_values().collect()
    }

    fn render_comparison(&self) -> String {
        let mut md = String::new();
        md.push_str("## Comparison across runs\n\n");
        md.push_str(
            "Micro figures per (language, kind) group; the rules-only column \
             is the no-model floor, not a competitor. Latency rows are \
             live-only by construction.\n\n",
        );
        md.push_str("| metric |");
        for run in &self.runs {
            md.push_str(&format!(" {} |", run.label));
        }
        md.push('\n');
        md.push_str("|---|");
        for _ in &self.runs {
            md.push_str("---|");
        }
        md.push('\n');

        let mut rows: Vec<(String, Vec<String>)> = Vec::new();
        // Metric rows from rollups, keyed by group label.
        let mut labels: Vec<String> = Vec::new();
        for run in &self.runs {
            for g in self.run_rollups(run) {
                if !labels.contains(&g.label) {
                    labels.push(g.label.clone());
                }
            }
        }
        for label in &labels {
            let mut per_row = Vec::new();
            let mut casing_row = Vec::new();
            let mut disfl_row = Vec::new();
            for run in &self.runs {
                let g = self.run_rollups(run).into_iter().find(|g| &g.label == label);
                per_row.push(g.as_ref().map_or("—".into(), |g| opt3(g.agg.per())));
                casing_row.push(g.as_ref().map_or("—".into(), |g| opt3(g.agg.casing.f1())));
                disfl_row.push(g.as_ref().map_or("—".into(), |g| {
                    if g.agg.removal_cases > 0 {
                        opt3(g.agg.removal.f1())
                    } else {
                        "—".into()
                    }
                }));
            }
            rows.push((format!("{label} — PER"), per_row));
            rows.push((format!("{label} — casing F1"), casing_row));
            rows.push((format!("{label} — disfluency F1"), disfl_row));
        }
        let lat = |f: fn(&Percentiles) -> u64| -> Vec<String> {
            self.runs
                .iter()
                .map(|r| {
                    if r.latency.n > 0 {
                        format!("{} ms", f(&r.latency))
                    } else {
                        "—".into()
                    }
                })
                .collect()
        };
        rows.push(("format latency p90".into(), lat(|p| p.p90)));
        rows.push(("format latency p99".into(), lat(|p| p.p99)));
        rows.push((
            "guardrail rejections (enforced)".into(),
            self.runs
                .iter()
                .map(|r| {
                    if r.backend_url.is_some() {
                        r.guard_enforced.values().sum::<usize>().to_string()
                    } else {
                        "—".into()
                    }
                })
                .collect(),
        ));
        rows.push((
            "tokens per case (prompt+completion)".into(),
            self.runs
                .iter()
                .map(|r| {
                    if r.backend_url.is_some() && r.calls > 0 {
                        fmt3((r.prompt_tokens + r.completion_tokens) as f64 / r.calls as f64)
                    } else {
                        "—".into()
                    }
                })
                .collect(),
        ));

        for (name, cells) in rows {
            // Skip disfluency rows that are dashes everywhere, etc.
            if cells.iter().all(|c| c == "—") {
                continue;
            }
            md.push_str(&format!("| {name} |"));
            for c in cells {
                md.push_str(&format!(" {c} |"));
            }
            md.push('\n');
        }
        md.push('\n');
        md
    }
}

fn render_rollup_row(g: &Rollup) -> String {
    let a = &g.agg;
    let disfl = if a.removal_cases > 0 {
        format!(
            "{} (tp {}, fp {}, fn {})",
            opt3(a.removal.f1()),
            a.removal.true_positives,
            a.removal.false_positives,
            a.removal.false_negatives
        )
    } else {
        "—".into()
    };
    let content = match a.content.rate() {
        Some(m) => format!("{} ({} cases with drift)", fmt3(m), a.content_drift_cases),
        None => "n/a".into(),
    };
    format!(
        "| {} | {} | {} | {} | {} cases | {} | {} cases | {} | {} | {} |\n",
        g.label,
        g.source_keys.join(" + "),
        a.cases,
        opt3(a.per()),
        a.punct_na_cases,
        opt3(a.casing.f1()),
        a.casing_na_cases,
        opt3(a.casing.sentence_accuracy()),
        disfl,
        content
    )
}

fn render_source_table(a: &Agg) -> String {
    let mut md = String::new();
    md.push_str("| metric | value | detail |\n|---|---|---|\n");
    md.push_str(&format!(
        "| PER (micro) | {} | C {}, S {}, I {}, D {}; {} of {} cases had no scored mark on either side |\n",
        opt3(a.per()),
        a.punct.correct,
        a.punct.substitutions,
        a.punct.insertions,
        a.punct.deletions,
        a.punct_na_cases,
        a.cases
    ));
    if a.has_gloss_variant {
        md.push_str(&format!(
            "| PER incl. gloss parentheses | {} | the annotation artifact priced in — not a formatting-quality number |\n",
            opt3(a.per_gloss())
        ));
    }
    md.push_str(&format!(
        "| casing precision / recall / F1 (micro) | {} / {} / {} | tp {}, fp {}, fn {}, tn {}; {} caseless tokens set aside; {} of {} cases had nothing caseable |\n",
        opt3(a.casing.precision()),
        opt3(a.casing.recall()),
        opt3(a.casing.f1()),
        a.casing.true_positives,
        a.casing.false_positives,
        a.casing.false_negatives,
        a.casing.true_negatives,
        a.casing.caseless_tokens,
        a.casing_na_cases,
        a.cases
    ));
    md.push_str(&format!(
        "| sentence accuracy | {} | {} of {} reference sentences reproduced exactly |\n",
        opt3(a.casing.sentence_accuracy()),
        a.casing.sentences_matched,
        a.casing.sentences_total
    ));
    if a.removal_cases > 0 {
        md.push_str(&format!(
            "| disfluency removal P / R / F1 (micro) | {} / {} / {} | tp {}, fp {}, fn {}; per-case precision undefined (removed nothing) in {} cases, recall undefined (nothing to remove) in {} |\n",
            opt3(a.removal.precision()),
            opt3(a.removal.recall()),
            opt3(a.removal.f1()),
            a.removal.true_positives,
            a.removal.false_positives,
            a.removal.false_negatives,
            a.removal_precision_na_cases,
            a.removal_recall_na_cases
        ));
    }
    let worst = a
        .content_worst
        .as_ref()
        .map(|(id, w)| format!("worst: `{}` at {}", id, fmt3(*w)))
        .unwrap_or_else(|| "no case drifted".into());
    md.push_str(&format!(
        "| content-WER guard (micro) | {} | {} of {} cases changed a content word; {} |\n",
        opt3(a.content.rate()),
        a.content_drift_cases,
        a.cases,
        worst
    ));
    md
}

fn render_slice_row(label: &str, a: &Agg) -> String {
    let disfl = if a.removal_cases > 0 {
        format!(", disfluency F1 {}", opt3(a.removal.f1()))
    } else {
        String::new()
    };
    format!(
        "- slice `{}` ({} cases): PER {}{}\n",
        label,
        a.cases,
        opt3(a.per()),
        disfl
    )
}

fn render_live_sections(run: &ModelRun) -> String {
    let mut md = String::new();

    md.push_str(&format!("### Guardrail outcomes ({})\n\n", run.label));
    let enforced: usize = run.guard_enforced.values().sum();
    md.push_str(&format!(
        "- Guardrail rejections (enforced — the rule-pipeline fallback was scored instead): {enforced}\n"
    ));
    for (label, n) in &run.guard_enforced {
        md.push_str(&format!("  - {label}: {n}\n"));
    }
    let provisional: usize = run.guard_provisional.values().sum();
    md.push_str(&format!(
        "- Provisional flags (report-only tier — the model output was still used): {provisional}\n"
    ));
    for (label, n) in &run.guard_provisional {
        md.push_str(&format!("  - {label}: {n}\n"));
    }
    if REPORT_ONLY {
        md.push_str(
            "- `format::guard::REPORT_ONLY` is `true`: thresholds other than \
             the always-enforced tier are uncalibrated, so their rejections \
             are logged above but the model output was still used.\n\n",
        );
    } else {
        md.push_str(
            "- `format::guard::REPORT_ONLY` is `false`: the calibrated \
             provisional tier is enforced, so every rejection above was \
             scored on the rule-pipeline fallback — exactly what the \
             shipped app injects.\n\n",
        );
    }

    md.push_str(&format!("### Latency ({})\n\n", run.label));
    md.push_str(&format!(
        "Measured span: the chat-completion call plus the guardrail check — \
         the same span the product logs as `format_ms`. It excludes audio \
         capture, ASR, and the end-of-speech drain, so it is NOT an \
         end-to-end dictation latency; treat it as the formatting stage's \
         contribution only. Percentiles are nearest-rank. The benchmark \
         waits up to {} s for each reply (the endpoint queues under \
         sustained load) so the tail is measured, not censored; the app's \
         own {} s budget is accounted for below.\n\n",
        LIVE_CHAT_TIMEOUT.as_secs(),
        POLISH_TIMEOUT.as_secs(),
    ));
    if run.latency.n > 0 {
        md.push_str(&format!(
            "- p90: {} ms, p99: {} ms (lead metrics — the tail is what a \
             user feels; p50 is secondary)\n",
            run.latency.p90, run.latency.p99
        ));
        md.push_str(&format!("- p50: {} ms\n", run.latency.p50));
        md.push_str(&format!(
            "- samples: {} of {} attempted calls; {} produced no sample \
             (failed calls, listed below) — dropping them silently would \
             overstate health\n",
            run.latency.n,
            run.calls,
            run.latency_skipped
        ));
        md.push_str(&format!(
            "- {} of {} successful calls exceeded the app's {} s formatting \
             budget (`chat::POLISH_TIMEOUT`): the shipped app would have \
             fallen back to the rule output on those, so from the product's \
             point of view they count against reliability even though their \
             replies were scored here\n\n",
            run.over_app_budget,
            run.latency.n,
            POLISH_TIMEOUT.as_secs(),
        ));
    } else {
        md.push_str(&format!(
            "- no successful calls out of {} attempted; no latency is claimed\n\n",
            run.calls
        ));
    }

    md.push_str(&format!("### Token usage ({})\n\n", run.label));
    md.push_str(&format!(
        "- prompt tokens: {}, completion tokens: {}, over {} calls",
        run.prompt_tokens, run.completion_tokens, run.calls
    ));
    if run.calls > 0 {
        md.push_str(&format!(
            " ({} tokens per case on average)",
            fmt3((run.prompt_tokens + run.completion_tokens) as f64 / run.calls as f64)
        ));
    }
    md.push_str(
        ".\n- Token counts only: this tool does not assert Sarvam pricing, \
         so no currency figure is derived.\n\n",
    );

    if !run.failures.is_empty() {
        md.push_str(&format!("### Failed calls ({})\n\n", run.label));
        md.push_str(&format!(
            "{} calls failed; those cases were scored on the rule-pipeline \
             fallback, exactly as the app would behave. Every failed case \
             is listed, grouped by reason — nothing is truncated.\n\n",
            run.failures.len()
        ));
        // Group by reason in first-seen order; ids keep run order within a
        // group. Every failure is listed: for a report meant to leave this
        // repo, completeness beats brevity — a reader must be able to check
        // any failed case, not a sample the tool chose to show.
        let mut groups: Vec<(&str, Vec<&str>)> = Vec::new();
        for (id, reason) in &run.failures {
            match groups.iter_mut().find(|(r, _)| r == reason) {
                Some((_, ids)) => ids.push(id),
                None => groups.push((reason, vec![id])),
            }
        }
        for (reason, ids) in &groups {
            md.push_str(&format!("- {} × {reason}\n", ids.len()));
            let list = ids
                .iter()
                .map(|id| format!("`{id}`"))
                .collect::<Vec<_>>()
                .join(", ");
            md.push_str(&format!("  - cases: {list}\n"));
        }
        md.push('\n');
    }

    md
}

// ---------------------------------------------------------------------------
// Provenance helpers (std only — no chrono, no new dependencies)
// ---------------------------------------------------------------------------

/// Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

fn utc_now_string() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let (y, m, d) = civil_from_days((secs / 86_400) as i64);
    let tod = secs % 86_400;
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02} UTC",
        tod / 3600,
        (tod % 3600) / 60,
        tod % 60
    )
}

/// `Path::canonicalize` on Windows yields a `\\?\D:\...` verbatim path;
/// strip that prefix for display.
fn display_path(p: &Path) -> String {
    let s = p.display().to_string();
    s.strip_prefix(r"\\?\").map(str::to_string).unwrap_or(s)
}

/// Best-effort `git rev-parse HEAD` plus a dirty marker. "unknown" beats a
/// fabricated value; a report whose numbers cannot be tied to a commit says
/// so instead of guessing.
fn git_commit() -> String {
    let head = std::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string());
    match head {
        Some(commit) => {
            let dirty = std::process::Command::new("git")
                .args(["status", "--porcelain", "--untracked-files=no"])
                .output()
                .ok()
                .filter(|o| o.status.success())
                .map(|o| !o.stdout.is_empty())
                .unwrap_or(false);
            if dirty {
                format!("{commit} (dirty working tree)")
            } else {
                commit
            }
        }
        None => "unknown (git unavailable)".into(),
    }
}

// ---------------------------------------------------------------------------
// Argument parsing (std::env::args only: no CLI dependency)
// ---------------------------------------------------------------------------

const USAGE: &str = "Usage: fmtbench --fixtures <dir> [--level off|light|balanced|high] \
[--live [--model <id>]... [--guard-dump <path>]] [--limit <n>] [--out <path>]\n\
\n\
Offline by default: scores the deterministic rule pipeline only (no network,\n\
no key). With --live, also runs each --model through the shipped\n\
rules -> chat -> guardrail flow; requires SARVAM_API_KEY in the environment.\n\
--guard-dump writes one JSON line per live case (guard metrics + the exact\n\
texts they were measured over) for threshold calibration; live only.\n\
--limit N scores only the first N cases per source (smoke runs; the report\n\
is marked as partial). --out writes the Markdown report to a file instead of\n\
stdout.";

#[derive(Debug, PartialEq)]
struct Args {
    fixtures: PathBuf,
    level: CleanupLevel,
    live: bool,
    models: Vec<String>,
    limit: Option<usize>,
    out: Option<PathBuf>,
    guard_dump: Option<PathBuf>,
}

fn parse_level(s: &str) -> Result<CleanupLevel> {
    Ok(match s.to_ascii_lowercase().as_str() {
        "off" => CleanupLevel::Off,
        "light" => CleanupLevel::Light,
        "balanced" => CleanupLevel::Balanced,
        "high" => CleanupLevel::High,
        other => bail!("unknown --level {other:?} (expected off|light|balanced|high)"),
    })
}

fn parse_args<I: Iterator<Item = String>>(mut it: I) -> Result<Args> {
    let mut fixtures: Option<PathBuf> = None;
    let mut level = CleanupLevel::Balanced;
    let mut live = false;
    let mut models: Vec<String> = Vec::new();
    let mut limit: Option<usize> = None;
    let mut out: Option<PathBuf> = None;
    let mut guard_dump: Option<PathBuf> = None;

    while let Some(arg) = it.next() {
        let mut value = |name: &str| -> Result<String> {
            it.next().with_context(|| format!("{name} needs a value\n\n{USAGE}"))
        };
        match arg.as_str() {
            "--fixtures" => fixtures = Some(PathBuf::from(value("--fixtures")?)),
            "--level" => level = parse_level(&value("--level")?)?,
            "--live" => live = true,
            "--model" => models.push(value("--model")?),
            "--limit" => {
                limit = Some(
                    value("--limit")?
                        .parse::<usize>()
                        .context("--limit needs a positive integer")?,
                );
            }
            "--out" => out = Some(PathBuf::from(value("--out")?)),
            "--guard-dump" => guard_dump = Some(PathBuf::from(value("--guard-dump")?)),
            "--help" | "-h" => bail!("{USAGE}"),
            other => bail!("unknown argument {other:?}\n\n{USAGE}"),
        }
    }

    let fixtures = fixtures.with_context(|| format!("--fixtures is required\n\n{USAGE}"))?;
    if !models.is_empty() && !live {
        bail!("--model only makes sense with --live\n\n{USAGE}");
    }
    if guard_dump.is_some() && !live {
        bail!("--guard-dump records live guard metrics; it needs --live\n\n{USAGE}");
    }
    if live && level == CleanupLevel::Off {
        bail!("--live with --level off is contradictory: level Off never calls the model");
    }
    if live && models.is_empty() {
        models.push(settings::DEFAULT_POLISH_MODEL.to_string());
    }
    if limit == Some(0) {
        bail!("--limit 0 would score nothing");
    }
    Ok(Args {
        fixtures,
        level,
        live,
        models,
        limit,
        out,
        guard_dump,
    })
}

// ---------------------------------------------------------------------------
// main
// ---------------------------------------------------------------------------

fn main() -> std::process::ExitCode {
    if std::env::args().skip(1).any(|a| a == "--help" || a == "-h") {
        println!("{USAGE}");
        return std::process::ExitCode::SUCCESS;
    }
    match real_main() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("fmtbench: {e:#}");
            std::process::ExitCode::FAILURE
        }
    }
}

fn real_main() -> Result<()> {
    let args = parse_args(std::env::args().skip(1))?;
    let (sources, loaded) = load_corpus(&args.fixtures, args.limit)?;

    let settings = CleanupSettings {
        level: args.level,
        ..CleanupSettings::default()
    };

    let mut runs = vec![run_rules(&loaded, &settings)];

    if args.live {
        let api_key = std::env::var("SARVAM_API_KEY")
            .context("--live needs SARVAM_API_KEY in the environment")?;
        let mut dump_file = match &args.guard_dump {
            Some(path) => {
                if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
                    std::fs::create_dir_all(parent)
                        .with_context(|| format!("creating {}", parent.display()))?;
                }
                Some(
                    std::fs::File::create(path)
                        .with_context(|| format!("creating {}", path.display()))?,
                )
            }
            None => None,
        };
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .context("building the tokio runtime")?;
        for model in &args.models {
            eprintln!(
                "fmtbench: live run of {model} over {} cases...",
                loaded.iter().map(|s| s.cases.len()).sum::<usize>()
            );
            runs.push(rt.block_on(run_live_model(
                &loaded,
                &settings,
                args.level,
                &api_key,
                model,
                dump_file.as_mut(),
            ))?);
        }
    }

    let report = Report {
        generated_utc: utc_now_string(),
        git_commit: git_commit(),
        fixtures_dir: display_path(
            &args.fixtures.canonicalize().unwrap_or_else(|_| args.fixtures.clone()),
        ),
        level: format!("{:?}", args.level).to_lowercase(),
        limit: args.limit,
        methodology: sources.methodology.clone(),
        sources: loaded
            .iter()
            .map(|s| SourceInfo {
                key: s.key.clone(),
                meta: s.meta.clone(),
                cases_scored: s.cases.len(),
                target_words: s.target_words,
            })
            .collect(),
        runs,
    };

    let md = report.to_markdown();
    match &args.out {
        Some(path) => {
            if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("creating {}", parent.display()))?;
            }
            std::fs::write(path, &md).with_context(|| format!("writing {}", path.display()))?;
            eprintln!("fmtbench: wrote {}", path.display());
        }
        None => print!("{md}"),
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn meta(key: &str, lang: &str, kind: &str, caveat: &str) -> SourceMeta {
        SourceMeta {
            name: key.to_string(),
            url: String::new(),
            license: "CC BY 4.0".into(),
            license_url: String::new(),
            license_evidence: "test".into(),
            citation: "test".into(),
            caveat: caveat.to_string(),
            revision: "deadbeef".into(),
            revision_kind: "git commit".into(),
            fixture: format!("{key}.jsonl"),
            cases: 200,
            kinds: vec![kind.to_string()],
            langs: vec![lang.to_string()],
            tags: BTreeMap::new(),
        }
    }

    /// An `Agg` whose micro ratios land on chosen values.
    fn agg_with(per_errors: u32, per_correct: u32, tp: u32, fp_fn: u32) -> Agg {
        Agg {
            cases: 200,
            punct: PunctCounts {
                correct: per_correct,
                substitutions: per_errors,
                insertions: 0,
                deletions: 0,
            },
            casing: CasingCounts {
                true_positives: tp,
                false_positives: fp_fn,
                false_negatives: fp_fn,
                true_negatives: 40,
                caseless_tokens: 0,
                sentences_total: 200,
                sentences_matched: 150,
            },
            ..Agg::default()
        }
    }

    fn report_with(run: ModelRun, lang: &str, kind: &str) -> Report {
        Report {
            generated_utc: "2026-08-19 00:00:00 UTC".into(),
            git_commit: "deadbeef".into(),
            fixtures_dir: "tests/fixtures".into(),
            level: "balanced".into(),
            limit: None,
            methodology: "strip and restore".into(),
            sources: vec![SourceInfo {
                key: "src-a".into(),
                meta: meta("src-a", lang, kind, "READ speech; a regression guard, not a target."),
                cases_scored: 200,
                target_words: 4000,
            }],
            runs: vec![run],
        }
    }

    /// The report's headline numbers, against the real report shape. PER
    /// 21/(21+79) = 0.210, so "0.21" appears; p90 780 comes from the
    /// percentile struct; guard section renders for a live run.
    #[test]
    fn the_report_carries_every_headline_number() {
        let mut run = ModelRun::new("sarvam-105b".into(), Some("https://api.sarvam.ai/v1/chat/completions".into()));
        run.per_source.insert("src-a".into(), agg_with(21, 79, 91, 9));
        run.latency = Percentiles {
            p50: 420,
            p90: 780,
            p99: 1510,
            n: 200,
        };
        run.calls = 200;
        run.guard_enforced.insert("truncated", 3);
        let md = report_with(run, "en", "punctuation").to_markdown();
        for needle in ["sarvam-105b", "PER", "0.21", "p90", "780", "Guardrail rejections"] {
            assert!(md.contains(needle), "report is missing {needle}:\n{md}");
        }
    }

    /// The failed-calls section must list EVERY failed case. An earlier
    /// revision printed the first 10 and a bare count of the rest; in a
    /// report meant for an external party that is a silent truncation — a
    /// reader could not check which cases fell back. Grouping by reason
    /// keeps 263 identical timeouts readable without dropping a single id.
    #[test]
    fn every_failed_call_is_listed_not_a_capped_sample() {
        let mut run = ModelRun::new(
            "sarvam-105b".into(),
            Some("https://api.sarvam.ai/v1/chat/completions".into()),
        );
        run.per_source.insert("src-a".into(), agg_with(21, 79, 91, 9));
        run.calls = 30;
        run.latency_skipped = 14;
        for i in 0..12 {
            run.failures.push((
                format!("case-{i:04}"),
                "error sending request for url (…): operation timed out".into(),
            ));
        }
        for i in 12..14 {
            run.failures
                .push((format!("case-{i:04}"), "chat backend returned HTTP 429: \"\"".into()));
        }
        let md = report_with(run, "en", "punctuation").to_markdown();

        for i in 0..14 {
            let id = format!("`case-{i:04}`");
            assert!(md.contains(&id), "failed case {id} missing from the report");
        }
        assert!(md.contains("14 calls failed"));
        assert!(md.contains("12 × error sending request"), "{md}");
        assert!(md.contains("2 × chat backend returned HTTP 429"), "{md}");
        assert!(
            !md.contains("... and"),
            "no '... and N more' truncation may remain: {md}"
        );
    }

    /// The benchmark waits out the endpoint's queuing tail
    /// (`LIVE_CHAT_TIMEOUT`), so a slow reply is a scored success — but the
    /// shipped app would have fallen back at `chat::POLISH_TIMEOUT`. The
    /// latency section must say how many replies missed that budget, or the
    /// report presents a 30 s reply as product-ready.
    #[test]
    fn the_latency_section_counts_app_budget_misses() {
        assert!(
            LIVE_CHAT_TIMEOUT > POLISH_TIMEOUT,
            "the benchmark leash must exceed the app budget or the budget \
             line is meaningless"
        );
        let mut run = ModelRun::new(
            "sarvam-105b".into(),
            Some("https://api.sarvam.ai/v1/chat/completions".into()),
        );
        run.per_source.insert("src-a".into(), agg_with(21, 79, 91, 9));
        run.calls = 10;
        run.latency = Percentiles {
            p50: 420,
            p90: 9800,
            p99: 31000,
            n: 10,
        };
        run.over_app_budget = 3;
        let md = report_with(run, "en", "punctuation").to_markdown();
        assert!(
            md.contains("3 of 10 successful calls exceeded the app's 6 s formatting budget"),
            "{md}"
        );
    }

    /// A report with no LLM run must not claim latency it never measured:
    /// the offline baseline renders no latency section at all, and no other
    /// prose mentions a percentile.
    #[test]
    fn an_offline_report_omits_latency() {
        let mut run = ModelRun::new("rules only (deterministic pipeline)".into(), None);
        run.per_source.insert("src-a".into(), agg_with(44, 56, 55, 45));
        assert_eq!(run.latency, Percentiles::default());
        let md = report_with(run, "en", "punctuation").to_markdown();
        assert!(!md.contains("p99"), "offline report claims latency:\n{md}");
        assert!(!md.contains("p90"));
        assert!(!md.contains("Guardrail rejections"));
        assert!(!md.contains("Token usage"));
    }

    /// The caveat must sit beside the score, not in a footnote the reader
    /// never reaches.
    #[test]
    fn the_source_caveat_is_printed_beside_its_score() {
        let mut run = ModelRun::new("rules only (deterministic pipeline)".into(), None);
        run.per_source.insert("src-a".into(), agg_with(10, 90, 50, 10));
        let md = report_with(run, "en", "punctuation").to_markdown();
        let caveat_at = md
            .find("a regression guard, not a target")
            .expect("caveat text missing entirely");
        let heading_at = md.find("#### src-a").expect("per-source section missing");
        assert!(
            caveat_at > heading_at,
            "caveat must appear inside the source's own section"
        );
    }

    /// A zero denominator is "not measured", never a score. An Agg whose
    /// casing counts are all zero must render n/a, and the PER of a source
    /// with no marks anywhere must also be n/a rather than a flattering
    /// 0.000. Same for content-WER: an all-zero `WerCounts` (zero reference
    /// tokens, zero errors) must print n/a too, never 0.000 — see
    /// `WerCounts::rate`'s doc on why that `None` is deliberate.
    #[test]
    fn zero_denominators_render_as_na_not_as_scores() {
        let agg = Agg {
            cases: 200,
            casing_na_cases: 200,
            punct_na_cases: 200,
            ..Agg::default()
        };
        assert_eq!(agg.per(), None);
        assert_eq!(agg.casing.f1(), None);
        assert_eq!(agg.content.rate(), None);

        let mut run = ModelRun::new("rules only (deterministic pipeline)".into(), None);
        run.per_source.insert("src-a".into(), agg);
        let md = report_with(run, "hi", "punctuation").to_markdown();
        assert!(md.contains("| PER (micro) | n/a |"), "{md}");
        assert!(md.contains("n/a / n/a / n/a"), "casing must be n/a: {md}");
        assert!(
            md.contains("| content-WER guard (micro) | n/a |"),
            "content-WER must be n/a, not 0.000, on an all-zero WerCounts: {md}"
        );
    }

    /// Corpus figures are micro: the aggregate is one ratio over summed
    /// counts, and it genuinely differs from a mean of per-case rates on the
    /// same data (same demonstration as `eval::per`'s own test).
    #[test]
    fn aggregation_sums_counts_it_does_not_average_rates() {
        let case = |correct: u32, deletions: u32, substitutions: u32| CaseScore {
            punct: PunctCounts {
                correct,
                substitutions,
                insertions: 0,
                deletions,
            },
            punct_gloss: None,
            casing: CasingCounts::default(),
            content: WerCounts::default(),
            removal: None,
        };
        let mut agg = Agg::default();
        let short = case(9, 1, 0); // rate 0.1
        let long = case(0, 0, 1); // rate 1.0
        agg.add("a", &short);
        agg.add("b", &long);

        let micro = agg.per().unwrap();
        assert!((micro - 2.0 / 11.0).abs() < 1e-9, "micro is {micro}");
        let macro_mean = (short.punct.rate() + long.punct.rate()) / 2.0;
        assert!(
            (macro_mean - micro).abs() > 0.3,
            "the two formulas must actually disagree on this data"
        );
    }

    /// The `settings` shim exists because the real module is out of reach of
    /// this binary; this pins it to the real source so it cannot drift
    /// silently.
    #[test]
    fn the_settings_shim_matches_the_real_default() {
        let real = include_str!("../settings.rs");
        let pinned = format!(
            "pub const DEFAULT_POLISH_MODEL: &str = \"{}\";",
            settings::DEFAULT_POLISH_MODEL
        );
        assert!(
            real.contains(&pinned),
            "src/settings.rs no longer defines {pinned:?} — update the shim in fmtbench.rs"
        );
    }

    /// The Hindi mark set scores the danda and excludes the gloss
    /// parentheses; the English set is NeMo's default. Stated in the report,
    /// pinned here.
    #[test]
    fn mark_sets_are_what_the_methodology_section_claims() {
        assert_eq!(EN_MARKS, &['.', ',', '?']);
        assert!(marks_for("hi").contains(&'\u{0964}'));
        assert!(!marks_for("hi").contains(&'('));
        assert!(HI_MARKS_WITH_GLOSS_PARENS.contains(&'('));
        assert_eq!(marks_for("en"), EN_MARKS);
        assert_eq!(marks_for("ta"), EN_MARKS); // future langs fall back, documented
    }

    // -- resolve_live mirrors sarvam::ws::resolve_format_inner --------------

    fn reply(text: &str, finish: &str) -> ChatReply {
        ChatReply {
            text: text.into(),
            finish_reason: finish.into(),
            prompt_tokens: 0,
            completion_tokens: 0,
            first_token_ms: None,
        }
    }

    /// The canonical enforced case from `sarvam::ws`'s own tests: the model
    /// answered the dictation instead of formatting it — catastrophic
    /// content loss, enforced even while REPORT_ONLY is true.
    #[test]
    fn mirror_enforces_a_catastrophic_content_loss() {
        let rule_out = "Remind me to send the invoice to Priya before Friday afternoon.";
        let bad = "Sure, I can help with that.";
        let (text, verdict) = resolve_live(rule_out, &reply(bad, "stop"), CleanupLevel::Balanced, true);
        assert_eq!(text, rule_out);
        assert_eq!(verdict, GuardOutcome::Enforced("content lost (catastrophic)"));
    }

    #[test]
    fn mirror_enforces_truncation_and_empty_replies() {
        let rule_out = "Tell them the release ships on Thursday and the docs follow.";
        let cut = "Tell them the release ships on Thursday and the do";
        let (text, verdict) = resolve_live(rule_out, &reply(cut, "length"), CleanupLevel::Balanced, true);
        assert_eq!(text, rule_out);
        assert_eq!(verdict, GuardOutcome::Enforced("truncated"));

        let (text, verdict) = resolve_live(rule_out, &reply("  ", "stop"), CleanupLevel::Balanced, true);
        assert_eq!(text, rule_out);
        assert_eq!(verdict, GuardOutcome::Enforced("empty reply"));
    }

    /// A mild over-edit sits in the provisional tier: while REPORT_ONLY the
    /// model text is used (as the product does) and the flag is counted;
    /// with enforcement on, it falls back.
    #[test]
    fn mirror_keeps_provisional_rejections_report_only() {
        // Light forbids word removal; dropping fillers trips ContentLost at
        // a retention well above the catastrophic floor.
        let rule_out = "um so basically i mean the thing is we should probably just ship it";
        let out = "We should just ship it.";
        let r = reply(out, "stop");

        let (text, verdict) = resolve_live(rule_out, &r, CleanupLevel::Light, true);
        assert_eq!(text, out);
        assert_eq!(verdict, GuardOutcome::Provisional("content lost"));

        let (text, verdict) = resolve_live(rule_out, &r, CleanupLevel::Light, false);
        assert_eq!(text, rule_out);
        assert_eq!(verdict, GuardOutcome::Enforced("content lost"));
    }

    #[test]
    fn mirror_accepts_a_normal_format() {
        let rule_out = "um so the meeting is at three thirty pm";
        let out = "The meeting is at 3:30 PM.";
        let (text, verdict) = resolve_live(rule_out, &reply(out, "stop"), CleanupLevel::Balanced, true);
        assert_eq!(text, out);
        assert_eq!(verdict, GuardOutcome::Accepted);
    }

    // -- argument parsing ----------------------------------------------------

    fn parse(args: &[&str]) -> Result<Args> {
        parse_args(args.iter().map(|s| s.to_string()))
    }

    #[test]
    fn args_parse_the_documented_invocation() {
        let a = parse(&[
            "--fixtures",
            "tests/fixtures",
            "--level",
            "balanced",
            "--live",
            "--model",
            "sarvam-105b",
            "--out",
            "report.md",
        ])
        .unwrap();
        assert_eq!(a.fixtures, PathBuf::from("tests/fixtures"));
        assert_eq!(a.level, CleanupLevel::Balanced);
        assert!(a.live);
        assert_eq!(a.models, vec!["sarvam-105b"]);
        assert_eq!(a.out, Some(PathBuf::from("report.md")));
        assert_eq!(a.limit, None);
    }

    #[test]
    fn args_take_multiple_models_for_a_comparison_run() {
        let a = parse(&[
            "--fixtures", "f", "--live", "--model", "sarvam-105b", "--model", "sarvam-m",
        ])
        .unwrap();
        assert_eq!(a.models, vec!["sarvam-105b", "sarvam-m"]);
    }

    #[test]
    fn args_default_the_live_model_to_the_products_default() {
        let a = parse(&["--fixtures", "f", "--live"]).unwrap();
        assert_eq!(a.models, vec![settings::DEFAULT_POLISH_MODEL]);
    }

    #[test]
    fn args_reject_the_meaningless_combinations() {
        assert!(parse(&[]).is_err(), "--fixtures is required");
        assert!(parse(&["--fixtures", "f", "--model", "x"]).is_err(), "--model without --live");
        assert!(parse(&["--fixtures", "f", "--live", "--level", "off"]).is_err());
        assert!(parse(&["--fixtures", "f", "--wat"]).is_err());
        assert!(parse(&["--fixtures", "f", "--limit", "0"]).is_err());
        assert!(parse(&["--fixtures", "f", "--level", "sideways"]).is_err());
        assert!(
            parse(&["--fixtures", "f", "--guard-dump", "d.jsonl"]).is_err(),
            "--guard-dump records live metrics; without --live there is nothing to dump"
        );
    }

    #[test]
    fn args_take_a_guard_dump_path_on_a_live_run() {
        let a = parse(&["--fixtures", "f", "--live", "--guard-dump", "cases.jsonl"]).unwrap();
        assert_eq!(a.guard_dump, Some(PathBuf::from("cases.jsonl")));
        let a = parse(&["--fixtures", "f", "--live"]).unwrap();
        assert_eq!(a.guard_dump, None);
    }

    /// The dump line must carry the guard's own measured values — the same
    /// `guard::metrics` numbers `check` enforces — and the exact texts they
    /// were measured over, or offline recalibration would be working from
    /// different data than the verdicts. Values here are hand-computed:
    /// input has 7 content words (6 expected after the "um" filler), output
    /// kept 6 of 6, ratio 6/7.
    #[test]
    fn a_guard_dump_line_carries_the_metrics_and_both_texts() {
        let rule_out = "um send the report to priya today";
        let r = reply("Send the report to Priya today.", "stop");
        let ctx = DumpCtx {
            model: "sarvam-105b",
            level: CleanupLevel::Balanced,
            source: "src-a",
        };
        let line = guard_dump_line(&ctx, "case-0001", rule_out, &r, &GuardOutcome::Accepted, 412);
        assert_eq!(line["case"], "case-0001");
        assert_eq!(line["outcome"], "accepted");
        assert_eq!(line["label"], serde_json::Value::Null);
        assert_eq!(line["level"], "balanced");
        assert_eq!(line["input_words"], 7);
        assert_eq!(line["output_words"], 6);
        assert_eq!(line["expected"], 6);
        assert_eq!(line["kept"], 6);
        assert_eq!(line["retained"], 1.0);
        assert!((line["ratio"].as_f64().unwrap() - 6.0 / 7.0).abs() < 1e-6);
        assert_eq!(line["rule_out"], rule_out);
        assert_eq!(line["reply"], "Send the report to Priya today.");

        let light_ctx = DumpCtx {
            model: "sarvam-105b",
            level: CleanupLevel::Light,
            source: "src-a",
        };
        let flagged = guard_dump_line(
            &light_ctx,
            "case-0002",
            rule_out,
            &r,
            &GuardOutcome::Provisional("content lost"),
            250,
        );
        assert_eq!(flagged["outcome"], "provisional");
        assert_eq!(flagged["label"], "content lost");
        assert_eq!(flagged["level"], "light");

        let failed = guard_dump_failure_line(&ctx, "case-0003", "HTTP 429");
        assert_eq!(failed["outcome"], "call_failed");
        assert_eq!(failed["reason"], "HTTP 429");
    }

    #[test]
    fn offline_parse_needs_no_model_and_no_key() {
        let a = parse(&["--fixtures", "tests/fixtures"]).unwrap();
        assert!(!a.live);
        assert!(a.models.is_empty());
        assert_eq!(a.level, CleanupLevel::Balanced, "balanced is the default level");
    }

    // -- provenance helpers --------------------------------------------------

    #[test]
    fn civil_from_days_hits_known_dates() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(19_723), (2024, 1, 1)); // well-known epoch-day
        assert_eq!(civil_from_days(-1), (1969, 12, 31));
    }

    // -- end-to-end over the real committed fixtures -------------------------

    /// The whole offline path over the real corpus: loads all five committed
    /// fixtures, scores the rule pipeline, and checks structural invariants
    /// of the report — not specific scores, which move with the pipeline.
    #[test]
    fn offline_run_over_the_real_fixtures_produces_a_coherent_report() {
        let fixtures = Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/../tests/fixtures"));
        let (sources, loaded) = load_corpus(fixtures, Some(5)).expect("corpus loads");
        assert_eq!(loaded.len(), 5, "all five committed sources");

        let settings = CleanupSettings::default();
        let run = run_rules(&loaded, &settings);
        assert_eq!(
            run.per_source.values().map(|a| a.cases).sum::<usize>(),
            25,
            "5 sources x limit 5"
        );

        // Hindi: PER must be measurable (dandas exist), casing mostly n/a.
        let hi = &run.per_source["indic-diarbench"];
        assert!(hi.per().is_some(), "Hindi PER must be measurable");

        // Disfluency sources actually accumulated removal counts.
        let dq = &run.per_source["disflqa"];
        assert_eq!(dq.removal_cases, 5);

        let report = Report {
            generated_utc: utc_now_string(),
            git_commit: "test".into(),
            fixtures_dir: fixtures.display().to_string(),
            level: "balanced".into(),
            limit: Some(5),
            methodology: sources.methodology.clone(),
            sources: loaded
                .iter()
                .map(|s| SourceInfo {
                    key: s.key.clone(),
                    meta: s.meta.clone(),
                    cases_scored: s.cases.len(),
                    target_words: s.target_words,
                })
                .collect(),
            runs: vec![run],
        };
        let md = report.to_markdown();
        assert!(md.contains("SMOKE RUN"), "a limited run must be marked loudly");
        for key in [
            "librispeech-pc",
            "earnings22",
            "earnings22-subset10",
            "disflqa",
            "indic-diarbench",
        ] {
            assert!(md.contains(&format!("#### {key}")), "missing section for {key}");
        }
        // Caveats travel with the scores.
        assert!(md.contains("regression guard"));
        assert!(md.contains("english-gloss"));
        // Offline: no latency claims.
        assert!(!md.contains("p99"));
    }
}
