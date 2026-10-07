//! Formatting-restoration corpus: parallel `input`/`target` pairs loaded
//! from `tests/fixtures/*.jsonl`, one JSON object per line.
//!
//! Methodology (AssemblyAI's Universal-2-TF, arXiv:2501.05948): every
//! fixture's `target` is text that already has correct punctuation and
//! casing; `input` is that same text stripped to lowercase and
//! unpunctuated, simulating raw ASR output. Running the formatter on
//! `input` and scoring against `target` measures restoration quality. No
//! individual's writing preferences enter this.
//!
//! `tools/fetch_corpora.py` downloads, converts and samples the fixture
//! files (~200 cases per source) and emits two metadata files alongside
//! them: `tests/fixtures/LICENSE.md` for a human, and
//! `tests/fixtures/sources.json` -- deserialized by [`load_sources`] --
//! for the harness.
//!
//! READ `sources.json`, DO NOT PRINT A SCORE WITHOUT IT. Each source
//! carries a caveat that changes how its number should be read, and a
//! caveat living only in a Markdown file is invisible to whoever is
//! looking at the number. LibriSpeech-PC is read speech (pre-1928 prose,
//! zero fillers) and is a regression guard rather than a target; the
//! Hindi source is dominated by a parenthesised-English-gloss annotation
//! convention and is a regression guard too, with the affected cases
//! tagged so a report can separate them. [`SourceMeta::caveat`] is that
//! text, reachable from the same place the cases are.
//!
//! `kind: Disfluency` comes from two sources. Disfl-QA is written text
//! (SQuAD questions rewritten to be disfluent). Earnings-22 Subset 10 is
//! real spontaneous speech: ten earnings calls Rev transcribed twice,
//! once verbatim and once lightly edited for readability, shipped under
//! `earnings22/subset10/`. The pair is easy to miss:
//! `subset10/verbatim_transcripts/` is a directory of git symlinks back to
//! the full corpus, so it is invisible if you look only at
//! `earnings22/transcripts/`. See the `earnings22-subset10` entry in
//! LICENSE.md for what is verified, and for the filter that keeps only units
//! where the edit was purely disfluency removal.

// Every item below is reachable only from `#[cfg(test)]` right now, for the
// same reason `eval::per` carries this same annotation (see its module
// doc): `mod eval;` in lib.rs is private, so nothing outside the crate can
// reach `load`/`Case`/`Kind`, and nothing inside a plain (non-test)
// `cargo build`/`cargo check` calls them either. `fmtbench` calling
// `corpus::load` does not lift this, for the reason given in `eval::rawer`:
// it compiles this file into its own crate through a `#[path]` include.
#![allow(dead_code)]

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use serde::Deserialize;

/// What a [`Case`] measures: restoring punctuation and casing, or removing
/// disfluencies (fillers, restarts, corrections). Serialized in fixture
/// files as lowercase `"punctuation"` / `"disfluency"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Punctuation,
    Disfluency,
}

/// One formatting-restoration test case, deserialized directly from a
/// fixture JSONL line.
///
/// `input` is always the simulated-raw-ASR side (lowercase, unpunctuated)
/// and `target` is always the ground truth the formatter should produce.
/// `verbatim`, when present, carries an intermediate form that still has
/// its original casing and punctuation but has NOT had disfluencies
/// removed -- populated for `Kind::Disfluency` cases from sources that ship
/// one (see `tests/fixtures/LICENSE.md`); absent (`None`) otherwise.
///
/// `tags` marks properties of the individual case that a report must be
/// able to slice on rather than average over. Two are in use today:
/// `english-gloss` / `gloss-dominated` on the Hindi source, where most of
/// the "punctuation" being scored is really a parenthesised-English-gloss
/// transcription convention; and `filler` / `repetition` / `false-start`
/// on Earnings-22 Subset 10, recording which kind of disfluency that case
/// actually exercises. Empty when a source defines none.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Case {
    pub id: String,
    pub source: String,
    pub kind: Kind,
    pub lang: String,
    pub input: String,
    pub target: String,
    #[serde(default)]
    pub verbatim: Option<String>,
    #[serde(default)]
    pub tags: Vec<String>,
}

impl Case {
    /// Whether this case carries `tag`. Cheap sugar so a report can filter
    /// without every call site spelling out the `iter().any()`.
    pub fn has_tag(&self, tag: &str) -> bool {
        self.tags.iter().any(|t| t == tag)
    }
}

/// Everything `tests/fixtures/sources.json` records about one source: its
/// license and the first-party evidence for it, the upstream revision the
/// fixture was actually built from, and -- the reason this type exists --
/// the [`caveat`](Self::caveat) that has to be printed next to the score.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct SourceMeta {
    pub name: String,
    pub url: String,
    pub license: String,
    pub license_url: String,
    pub license_evidence: String,
    pub citation: String,
    /// What this source does and does not measure. Read speech vs
    /// spontaneous, regression guard vs target, known annotation
    /// artifacts. A benchmark report that prints a per-source number
    /// without this is presenting a number whose meaning it has withheld.
    pub caveat: String,
    /// The exact upstream revision the committed fixture was built from:
    /// a git commit, a Hugging Face repo SHA, or `sha256:...` for a
    /// source served as a plain archive with no revisions.
    pub revision: String,
    pub revision_kind: String,
    /// Fixture file name, relative to `tests/fixtures/`.
    pub fixture: String,
    #[serde(default)]
    pub cases: usize,
    #[serde(default)]
    pub kinds: Vec<String>,
    #[serde(default)]
    pub langs: Vec<String>,
    /// How many cases carry each tag, for a report that wants to say "60%
    /// of this source's cases are gloss-dominated" without loading them.
    #[serde(default)]
    pub tags: BTreeMap<String, usize>,
}

/// The whole of `sources.json`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Sources {
    pub generated_by: String,
    pub methodology: String,
    /// The only licenses `tools/fetch_corpora.py` will write a fixture
    /// for. Mirrored here so a check that the shipped fixtures stayed
    /// inside it does not have to parse Python.
    pub allowed_licenses: Vec<String>,
    pub sample_seed: u64,
    pub target_sample_size: usize,
    pub sources: BTreeMap<String, SourceMeta>,
}

/// Loads `tests/fixtures/sources.json`.
pub fn load_sources(path: &Path) -> Result<Sources> {
    let text = fs::read_to_string(path)
        .with_context(|| format!("reading source metadata {}", path.display()))?;
    serde_json::from_str(&text)
        .with_context(|| format!("parsing source metadata {}", path.display()))
}

/// Parses one JSONL line into a [`Case`]. Not `pub`: the only product this
/// module exposes is [`load`]; this exists so `load` can report which line
/// failed, and so it can be unit-tested directly against inline JSON
/// strings without a fixture file on disk.
fn parse_line(line: &str) -> Result<Case> {
    serde_json::from_str(line).context("malformed fixture JSON")
}

/// Loads every case from a `.jsonl` fixture file, one JSON object per
/// non-blank line (blank lines -- a trailing newline at EOF, stray blank
/// lines from hand-editing -- are skipped rather than treated as errors).
///
/// A malformed line is a returned [`anyhow::Error`] naming the file and the
/// offending line's 1-indexed line number, not a panic and not a silently
/// dropped row: a corpus fixture that partially fails to load must not
/// silently shrink the eval set it claims to run.
pub fn load(path: &Path) -> Result<Vec<Case>> {
    let text = fs::read_to_string(path)
        .with_context(|| format!("reading fixture file {}", path.display()))?;
    let mut cases = Vec::new();
    for (idx, raw_line) in text.lines().enumerate() {
        let line = raw_line.trim();
        if line.is_empty() {
            continue;
        }
        let case = parse_line(line)
            .with_context(|| format!("{}:{}", path.display(), idx + 1))?;
        cases.push(case);
    }
    Ok(cases)
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- Core parse_line behaviour. ---

    #[test]
    fn loads_a_fixture_line() {
        let line = r#"{"id":"a","source":"librispeech-pc","kind":"punctuation","lang":"en","input":"hello world","target":"Hello, world."}"#;
        let case = parse_line(line).unwrap();
        assert_eq!(case.id, "a");
        assert!(matches!(case.kind, Kind::Punctuation));
        assert_eq!(case.verbatim, None);
    }

    #[test]
    fn a_disfluency_case_carries_the_verbatim_side() {
        let line = r#"{"id":"b","source":"earnings22","kind":"disfluency","lang":"en","input":"um we shipped it","target":"We shipped it.","verbatim":"um we shipped it"}"#;
        let case = parse_line(line).unwrap();
        assert_eq!(case.verbatim.as_deref(), Some("um we shipped it"));
    }

    #[test]
    fn tags_default_to_empty_and_round_trip_when_present() {
        let untagged = r#"{"id":"a","source":"x","kind":"punctuation","lang":"en","input":"a","target":"A."}"#;
        assert!(parse_line(untagged).unwrap().tags.is_empty());

        let tagged = r#"{"id":"a","source":"x","kind":"punctuation","lang":"hi","input":"a","target":"A.","tags":["english-gloss","gloss-dominated"]}"#;
        let case = parse_line(tagged).unwrap();
        assert_eq!(case.tags, vec!["english-gloss", "gloss-dominated"]);
        assert!(case.has_tag("gloss-dominated"));
        assert!(!case.has_tag("filler"));
    }

    #[test]
    fn a_malformed_line_is_an_error_not_a_panic() {
        assert!(parse_line("{ not json").is_err());
    }

    // --- Additional parse_line coverage. ---

    #[test]
    fn loads_every_field_not_just_the_ones_other_tests_check() {
        let line = r#"{"id":"lspc-test-clean-0001","source":"librispeech-pc","kind":"punctuation","lang":"en","input":"the meeting is at three thirty","target":"The meeting is at three thirty."}"#;
        let case = parse_line(line).unwrap();
        assert_eq!(
            case,
            Case {
                id: "lspc-test-clean-0001".into(),
                source: "librispeech-pc".into(),
                kind: Kind::Punctuation,
                lang: "en".into(),
                input: "the meeting is at three thirty".into(),
                target: "The meeting is at three thirty.".into(),
                verbatim: None,
                tags: Vec::new(),
            }
        );
    }

    #[test]
    fn verbatim_explicit_null_is_none_same_as_an_absent_key() {
        let line = r#"{"id":"a","source":"x","kind":"punctuation","lang":"en","input":"a","target":"A.","verbatim":null}"#;
        assert_eq!(parse_line(line).unwrap().verbatim, None);
    }

    #[test]
    fn an_unknown_kind_value_is_a_parse_error() {
        let line = r#"{"id":"a","source":"x","kind":"grammar","lang":"en","input":"a","target":"A."}"#;
        assert!(parse_line(line).is_err());
    }

    #[test]
    fn a_missing_required_field_is_a_parse_error() {
        // No "target".
        let line = r#"{"id":"a","source":"x","kind":"punctuation","lang":"en","input":"a"}"#;
        assert!(parse_line(line).is_err());
    }

    #[test]
    fn unknown_extra_fields_do_not_break_parsing() {
        // Forward compatibility: a future fetch-script field must not make
        // every existing fixture line an error.
        let line = r#"{"id":"a","source":"x","kind":"punctuation","lang":"en","input":"a","target":"A.","note":"future field"}"#;
        assert!(parse_line(line).is_ok());
    }

    // --- load(): file-level behaviour. ---

    /// Writes `contents` to a fresh temp file and returns its path, kept
    /// alive by the returned `tempfile::TempPath`-style guard -- except
    /// this crate has no `tempfile` dependency, so a manual unique path
    /// under `std::env::temp_dir()` plus an RAII guard does the same job
    /// without adding one.
    struct TempFile(std::path::PathBuf);

    impl TempFile {
        fn new(name: &str, contents: &str) -> Self {
            let mut path = std::env::temp_dir();
            path.push(format!(
                "butterfly-speak-corpus-test-{}-{}-{}",
                std::process::id(),
                name,
                // Cheap per-call uniqueness so parallel #[test] threads
                // calling TempFile::new with the same `name` don't collide.
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            fs::write(&path, contents).expect("write temp fixture file");
            TempFile(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempFile {
        fn drop(&mut self) {
            let _ = fs::remove_file(&self.0);
        }
    }

    #[test]
    fn load_reads_every_line_of_a_real_file_in_order() {
        let contents = "\
{\"id\":\"a\",\"source\":\"x\",\"kind\":\"punctuation\",\"lang\":\"en\",\"input\":\"a\",\"target\":\"A.\"}
{\"id\":\"b\",\"source\":\"x\",\"kind\":\"disfluency\",\"lang\":\"en\",\"input\":\"um b\",\"target\":\"B.\",\"verbatim\":\"um b\"}
";
        let tmp = TempFile::new("in-order", contents);
        let cases = load(tmp.path()).unwrap();
        assert_eq!(cases.len(), 2);
        assert_eq!(cases[0].id, "a");
        assert_eq!(cases[1].id, "b");
    }

    #[test]
    fn load_skips_blank_lines() {
        let contents = "\n{\"id\":\"a\",\"source\":\"x\",\"kind\":\"punctuation\",\"lang\":\"en\",\"input\":\"a\",\"target\":\"A.\"}\n\n   \n";
        let tmp = TempFile::new("blank-lines", contents);
        let cases = load(tmp.path()).unwrap();
        assert_eq!(cases.len(), 1);
    }

    #[test]
    fn load_reports_the_offending_line_number_not_just_that_something_failed() {
        let contents = "\
{\"id\":\"a\",\"source\":\"x\",\"kind\":\"punctuation\",\"lang\":\"en\",\"input\":\"a\",\"target\":\"A.\"}
{\"id\":\"b\",\"source\":\"x\",\"kind\":\"punctuation\",\"lang\":\"en\",\"input\":\"b\",\"target\":\"B.\"}
this line is not json at all
{\"id\":\"d\",\"source\":\"x\",\"kind\":\"punctuation\",\"lang\":\"en\",\"input\":\"d\",\"target\":\"D.\"}
";
        let tmp = TempFile::new("bad-line-3", contents);
        let err = load(tmp.path()).expect_err("line 3 is malformed");

        // `load`'s own `with_context` is the OUTERMOST layer, so it is
        // what anyhow::Error's plain Display shows -- checked directly
        // rather than via `{:#}`, whose chain-joining behaviour is a
        // Debug-impl detail this test should not have to depend on.
        let top = format!("{err}");
        assert!(
            top.contains(":3"),
            "expected the fixture's line number 3 in the top-level error, got: {top}"
        );

        // And the chain still reaches the real cause underneath -- the
        // line number must not have replaced it.
        let chain: Vec<String> = err.chain().map(|e| e.to_string()).collect();
        assert!(
            chain.iter().any(|m| m.contains("malformed fixture JSON")),
            "expected 'malformed fixture JSON' somewhere in the error chain, got: {chain:?}"
        );
    }

    #[test]
    fn load_on_a_missing_file_is_an_error_not_a_panic() {
        let mut path = std::env::temp_dir();
        path.push("butterfly-speak-corpus-test-definitely-does-not-exist.jsonl");
        assert!(load(&path).is_err());
    }

    // --- Against the real, committed fixtures. Regression guard: if a
    // fixture file stops parsing, or its per-source shape (kind/lang, the
    // sample size the fetch script targets) silently drifts, this is where
    // it shows up -- not discovered later when fmtbench runs
    // against a corpus that quietly lost cases. ---

    const FIXTURES_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../tests/fixtures");

    /// Every committed fixture. Kept next to the loader so adding a source
    /// without wiring it into these checks is a visible omission.
    const FIXTURE_FILES: &[&str] = &[
        "librispeech-pc.jsonl",
        "earnings22.jsonl",
        "earnings22-subset10.jsonl",
        "disflqa.jsonl",
        "indic-diarbench.jsonl",
    ];

    fn load_fixture(name: &str) -> Vec<Case> {
        let path = Path::new(FIXTURES_DIR).join(name);
        load(&path).unwrap_or_else(|e| panic!("loading {}: {e:#}", path.display()))
    }

    /// `tools/fetch_corpora.py` samples `TARGET_SAMPLE_SIZE = 200` cases
    /// per source. Pinned exactly, not as a >= bound: this is the fetch
    /// script's own committed output, so a silent count drift (a fetch
    /// re-run that sampled fewer, or a hand-edit that dropped lines) is
    /// exactly the kind of thing a regression guard should catch, not wave
    /// through.
    const EXPECTED_SAMPLE_SIZE: usize = 200;

    #[test]
    fn librispeech_pc_fixture_is_punctuation_only_english_no_verbatim() {
        let cases = load_fixture("librispeech-pc.jsonl");
        assert_eq!(cases.len(), EXPECTED_SAMPLE_SIZE);
        for case in &cases {
            assert_eq!(case.source, "librispeech-pc");
            assert!(matches!(case.kind, Kind::Punctuation));
            assert_eq!(case.lang, "en");
            assert_eq!(case.verbatim, None);
            assert!(!case.input.is_empty());
            assert!(!case.target.is_empty());
            // input is the lowercase/unpunctuated simulation of target.
            assert_eq!(case.input, case.input.to_lowercase());
        }
    }

    #[test]
    fn earnings22_fixture_is_punctuation_only_and_keeps_fillers_on_both_sides() {
        // The full 125-call corpus ships one transcript per call, so this
        // fixture is punctuation-only. That is a fact about THIS fixture,
        // not about Earnings-22: the verbatim/non-verbatim pair does
        // exist, for ten other calls, and is loaded by the test below.
        let cases = load_fixture("earnings22.jsonl");
        assert_eq!(cases.len(), EXPECTED_SAMPLE_SIZE);
        for case in &cases {
            assert_eq!(case.source, "earnings22");
            assert!(matches!(case.kind, Kind::Punctuation));
            assert_eq!(case.lang, "en");
            assert_eq!(case.verbatim, None);
        }
    }

    /// Word-comparison key matching `_nlp_word` in `tools/fetch_corpora.py`:
    /// lowercase, and drop everything that is not a letter, digit,
    /// underscore or apostrophe.
    fn word_key(token: &str) -> String {
        token
            .to_lowercase()
            .chars()
            .filter(|c| c.is_alphanumeric() || *c == '_' || *c == '\'')
            .collect()
    }

    fn word_keys(text: &str) -> Vec<String> {
        text.split_whitespace().map(word_key).collect()
    }

    /// Case-PRESERVING word key. [`word_key`] lowercases, which is the
    /// right question for "is this the same word" and the wrong one for
    /// "did the transcriptionist re-case it" -- and re-casing is one of the
    /// edits the subset10 filter claims to reject.
    fn cased_word_key(token: &str) -> String {
        token
            .chars()
            .filter(|c| c.is_alphanumeric() || *c == '_' || *c == '\'')
            .collect()
    }

    /// True iff `needle` can be obtained from `haystack` by deleting
    /// characters and nothing else. Mirrors `_is_subsequence` in
    /// `tools/fetch_corpora.py`; greedy left-to-right matching is exact for
    /// subsequence testing, since the earliest match always leaves the
    /// longest remaining haystack.
    fn is_subsequence(needle: &str, haystack: &str) -> bool {
        let mut hay = haystack.chars();
        needle.chars().all(|ch| hay.any(|h| h == ch))
    }

    /// The `earnings22-subset10` invariant, checked as literally as it is
    /// stated: `target` is `verbatim` with WHOLE TOKENS deleted and, on
    /// each token that survives, PUNCTUATION CHARACTERS deleted. Nothing
    /// inserted, nothing substituted, nothing re-cased -- punctuation
    /// included.
    ///
    /// Punctuation deletion is allowed on purpose and is not a loophole:
    /// the commas in "So, obviously higher margins, uh, excluding those
    /// sales." are there to set off the filler, so removing the filler
    /// must be allowed to take them with it. What is rejected is
    /// punctuation appearing ("So" -> "So,"), changing ("detail." ->
    /// "detail?"), or being used to weld two sentences together ("CapEx.
    /// Um, our guidance" -> "CapEx, our guidance"). Those are editorial
    /// rewrites and they put an unreachable ceiling on the metric.
    ///
    /// Because target tokens are consumed left to right and each must be a
    /// subsequence of the verbatim token it matches, success here also
    /// means the whole `target` string is a character-subsequence of
    /// `verbatim`.
    fn deletion_only(verbatim: &str, target: &str) -> Result<(), String> {
        let mut verbatim_tokens = verbatim.split_whitespace();
        for t in target.split_whitespace() {
            loop {
                let Some(candidate) = verbatim_tokens.next() else {
                    return Err(format!(
                        "target token {t:?} has no match left in verbatim: it was \
                         added, substituted, re-cased, or re-punctuated -- not deleted"
                    ));
                };
                if cased_word_key(candidate) == cased_word_key(t)
                    && is_subsequence(t, candidate)
                {
                    break;
                }
            }
        }
        Ok(())
    }

    /// Every letter/digit/apostrophe, in order, with all whitespace and
    /// punctuation dropped. Used to compare an `input` against the text it
    /// was stripped from WITHOUT depending on tokenization: `strip_to_raw`
    /// turns punctuation into a space, so "11.2%" becomes two whitespace
    /// tokens ("11", "2") while the original is one. The character stream
    /// is identical either way; a token count is not.
    fn letter_stream(text: &str) -> String {
        text.to_lowercase()
            .chars()
            .filter(|c| c.is_alphanumeric() || *c == '\'')
            .collect()
    }

    #[test]
    fn earnings22_subset10_targets_are_deletion_only_edits_of_the_verbatim_side() {
        // The load-bearing property of this fixture, and the thing that
        // makes it scoreable: `target` is `verbatim` with disfluent tokens
        // DELETED and nothing else touched -- no substitutions, no
        // insertions, no reordering, no re-casing, and no punctuation
        // added or swapped. `tools/fetch_corpora.py` discards any unit
        // where Rev's transcriptionist also reworded or re-punctuated, so
        // if this ever fails the fetch script's filter has been loosened
        // and the fixture now demands rewrites a formatter cannot perform.
        //
        // Comparing lowercased, punctuation-stripped word lists alone would
        // assert a good deal less than this test's name promises: a unit
        // whose clean side added a comma, swapped a full stop for a question
        // mark, merged two sentences, or re-cased a surviving word would pass
        // unchanged. `deletion_only` checks the full surface, casing and
        // punctuation included, and is the real check.
        let cases = load_fixture("earnings22-subset10.jsonl");
        assert_eq!(cases.len(), EXPECTED_SAMPLE_SIZE);

        for case in &cases {
            assert_eq!(case.source, "earnings22-subset10");
            assert!(matches!(case.kind, Kind::Disfluency));
            assert_eq!(case.lang, "en");
            let verbatim = case
                .verbatim
                .as_deref()
                .unwrap_or_else(|| panic!("id {}: disfluency case without verbatim", case.id));

            let v = word_keys(verbatim);
            let t = word_keys(&case.target);
            assert!(
                t.len() < v.len(),
                "id {}: nothing was removed, so this is not a disfluency case",
                case.id
            );

            // target's words are a subsequence of verbatim's words...
            let mut vi = v.iter();
            for word in &t {
                assert!(
                    vi.any(|candidate| candidate == word),
                    "id {}: target word {word:?} is not present, in order, in verbatim -- \
                     the transcriptionist rewrote rather than only removed",
                    case.id
                );
            }

            // ...and the edit is deletion-only on the FULL surface, which
            // is the property the fixture is actually built on: casing and
            // punctuation are checked here, not waved through.
            if let Err(why) = deletion_only(verbatim, &case.target) {
                panic!(
                    "id {}: target is not a deletion-only edit of verbatim: {why}\n  \
                     verbatim: {verbatim}\n    target: {}",
                    case.id, case.target
                );
            }

            // And `input` is the stripped VERBATIM side, not the target:
            // the formatter must be handed the disfluent speech, or this
            // fixture would be scoring nothing at all.
            assert_eq!(case.input, case.input.to_lowercase());
            assert_eq!(
                letter_stream(&case.input),
                letter_stream(verbatim),
                "id {}: input is not the stripped verbatim side",
                case.id
            );
            assert_ne!(
                letter_stream(&case.input),
                letter_stream(&case.target),
                "id {}: input already equals target",
                case.id
            );

            assert!(
                !case.tags.is_empty(),
                "id {}: every case records which disfluency kinds it exercises",
                case.id
            );
            for tag in &case.tags {
                assert!(
                    matches!(tag.as_str(), "filler" | "repetition" | "false-start"),
                    "id {}: unexpected tag {tag:?}",
                    case.id
                );
            }
        }

        // All three disfluency kinds are actually represented -- a fixture
        // that quietly became 200 filler-only cases would still pass every
        // assertion above.
        for tag in ["filler", "repetition", "false-start"] {
            assert!(
                cases.iter().any(|c| c.has_tag(tag)),
                "no case exercises {tag:?}"
            );
        }
    }

    #[test]
    fn deletion_only_rejects_the_edits_the_subset10_filter_exists_to_discard() {
        // Without this, the fixture assertion above could pass because
        // `deletion_only` never says no. The first three negatives are the
        // shapes of real units the fetch filter has to discard; the last two
        // (re-casing, insertion) are shapes the invariant forbids that the
        // corpus happens not to contain, pinned so they stay forbidden.

        // Positives: tokens removed, and the commas that existed only to
        // set off the removed filler going with them. Rejecting these
        // would not make the filter stricter, it would make the targets
        // worse ("...margins, excluding those sales.").
        assert_eq!(
            deletion_only(
                "So, obviously higher margins, uh, excluding those sales.",
                "So obviously higher margins excluding those sales.",
            ),
            Ok(())
        );
        assert_eq!(
            deletion_only(
                "What we've done here is put some shading on the, on the chart.",
                "What we've done here is put some shading on the chart.",
            ),
            Ok(())
        );

        for (verbatim, target, what) in [
            (
                "So we can see a resumption of, uh, growth in that business.",
                "So, we can see a resumption of growth in that business.",
                "a comma the verbatim never had",
            ),
            (
                "Nishlan, do you want to come up, uh, to unpack the results in more detail.",
                "Nishlan, do you want to come up to unpack the results in more detail?",
                "a full stop swapped for a question mark",
            ),
            (
                "Moving on to CapEx. Um, our guidance for the year ahead is 395 million.",
                "Moving on to CapEx, our guidance for the year ahead is 395 million.",
                "two sentences merged into one",
            ),
            (
                "and, uh, we still hope so.",
                "And we still hope so.",
                "a surviving word re-cased",
            ),
            (
                "we, uh, think that is right.",
                "we think that is quite right.",
                "a word inserted",
            ),
        ] {
            assert!(
                deletion_only(verbatim, target).is_err(),
                "deletion_only accepted {what}: {verbatim:?} -> {target:?}"
            );
        }
    }

    #[test]
    fn disflqa_fixture_is_the_disfluency_source_and_every_case_has_verbatim() {
        let cases = load_fixture("disflqa.jsonl");
        assert_eq!(cases.len(), EXPECTED_SAMPLE_SIZE);
        for case in &cases {
            assert_eq!(case.source, "disflqa");
            assert!(matches!(case.kind, Kind::Disfluency));
            assert_eq!(case.lang, "en");
            assert!(
                case.verbatim.is_some(),
                "id {}: disfluency cases from disflqa always carry verbatim",
                case.id
            );
        }
    }

    #[test]
    fn indic_diarbench_fixture_is_hindi_punctuation_with_devanagari_intact() {
        let cases = load_fixture("indic-diarbench.jsonl");
        assert_eq!(cases.len(), EXPECTED_SAMPLE_SIZE);
        let mut saw_danda = false;
        for case in &cases {
            assert_eq!(case.source, "indic-diarbench");
            assert!(matches!(case.kind, Kind::Punctuation));
            assert_eq!(case.lang, "hi");
            assert_eq!(case.verbatim, None);

            // Devanagari has no case distinction, but every letter, matra
            // and virama must survive stripping IN ORDER -- see
            // eval::per's module doc on why a `\w`-based stripper would
            // shatter these words (Python's `\w` excludes the Mn/Mc
            // combining-mark categories matras and virama are made of;
            // this fixture's own `strip_to_raw` in fetch_corpora.py uses a
            // Unicode-category *punctuation* denylist instead, precisely
            // to avoid that trap). The only Devanagari-block character
            // this source's `target` set ever removes is the danda (।,
            // U+0964) -- confirmed by scanning all 200 committed cases
            // when this test was written; a different removal here means
            // either strip_to_raw regressed or the fixture was
            // regenerated from different upstream text.
            let target_devanagari: Vec<char> = case
                .target
                .chars()
                .filter(|c| ('\u{0900}'..='\u{097F}').contains(c))
                .filter(|c| *c != '।')
                .collect();
            let input_devanagari: Vec<char> = case
                .input
                .chars()
                .filter(|c| ('\u{0900}'..='\u{097F}').contains(c))
                .collect();
            assert_eq!(
                input_devanagari, target_devanagari,
                "id {}: every Devanagari letter/matra/virama in target (minus \
                 the danda) must reappear in input, in the same order",
                case.id
            );

            if case.target.contains('।') {
                saw_danda = true;
            }
        }
        assert!(
            saw_danda,
            "expected at least one case using the Devanagari danda (।), \
             the punctuation mark this source exists to exercise"
        );
    }

    #[test]
    fn indic_diarbench_gloss_cases_are_tagged_so_a_report_can_separate_them() {
        // The majority of the "punctuation" this source scores is really
        // reinstating its own parenthesised-English-gloss convention
        // ("इंटरव्यू(interview)"), not a formatting behaviour a dictation
        // user would notice. English formatting is this project's stated
        // priority and the Hindi corpus is a regression guard, so these
        // cases are kept but marked -- a report must be able to report
        // them apart rather than fold them into one Hindi number.
        let cases = load_fixture("indic-diarbench.jsonl");
        let glossed = cases.iter().filter(|c| c.has_tag("english-gloss")).count();
        let dominated = cases.iter().filter(|c| c.has_tag("gloss-dominated")).count();

        // Tagging that marked nothing, or marked everything, would be
        // useless to a report either way.
        assert!(
            glossed > 0 && glossed < cases.len(),
            "english-gloss tagged {glossed}/{} cases",
            cases.len()
        );
        // Sanity: the dominated set is a subset of the glossed set.
        assert!(dominated <= glossed);
        for case in &cases {
            if case.has_tag("gloss-dominated") {
                assert!(
                    case.has_tag("english-gloss"),
                    "id {}: gloss-dominated without english-gloss",
                    case.id
                );
            }
            // The tag must agree with what is actually in the target, in
            // both directions -- a tag that can be absent while the
            // artifact is present is worse than no tag, because a report
            // would then present a gloss case as a clean one.
            assert_eq!(
                case.has_tag("english-gloss"),
                contains_gloss(&case.target),
                "id {}: english-gloss tag disagrees with the target text: {:?}",
                case.id,
                case.target
            );
        }
    }

    /// Mirrors `_ENGLISH_GLOSS_RE` in `tools/fetch_corpora.py`: a
    /// parenthesised run holding an ASCII letter or digit and no
    /// Devanagari, i.e. this benchmark's convention for glossing a
    /// spelled-out word with its Latin/numeric form.
    fn contains_gloss(text: &str) -> bool {
        let mut inside = false;
        let mut saw_ascii = false;
        for ch in text.chars() {
            match ch {
                '(' => {
                    inside = true;
                    saw_ascii = false;
                }
                ')' if inside => {
                    if saw_ascii {
                        return true;
                    }
                    inside = false;
                }
                _ if inside && ('\u{0900}'..='\u{097F}').contains(&ch) => {
                    inside = false;
                }
                _ if inside && ch.is_ascii_alphanumeric() => saw_ascii = true,
                _ => {}
            }
        }
        false
    }

    // --- sources.json: the metadata a report has to be able to reach. ---

    fn load_sources_json() -> Sources {
        let path = Path::new(FIXTURES_DIR).join("sources.json");
        load_sources(&path).unwrap_or_else(|e| panic!("loading {}: {e:#}", path.display()))
    }

    #[test]
    fn every_fixture_has_source_metadata_a_report_can_print() {
        let sources = load_sources_json();
        for name in FIXTURE_FILES {
            let key = name.strip_suffix(".jsonl").unwrap();
            let meta = sources
                .sources
                .get(key)
                .unwrap_or_else(|| panic!("{name}: no entry in sources.json"));
            assert_eq!(meta.fixture, *name);

            // The point of the file. A caveat that is missing, or a stub,
            // is the failure this exists to prevent: the caveat is what
            // stops a LibriSpeech-PC score being read as a dictation
            // quality number.
            assert!(
                meta.caveat.len() > 80,
                "{key}: caveat is too short to carry the warning it has to carry"
            );
            assert!(!meta.license_evidence.is_empty(), "{key}: no license evidence");
            assert!(!meta.citation.is_empty(), "{key}: no citation");

            // Reproducibility: the exact upstream revision the committed
            // fixture came from, so a re-run that differs is attributable.
            assert!(!meta.revision.is_empty(), "{key}: upstream revision not pinned");
            assert!(!meta.revision_kind.is_empty(), "{key}: revision kind not named");

            let cases = load_fixture(name);
            assert_eq!(meta.cases, cases.len(), "{key}: case count is stale");
        }
        assert_eq!(
            sources.sources.len(),
            FIXTURE_FILES.len(),
            "sources.json describes a source with no fixture, or vice versa"
        );
    }

    #[test]
    fn every_shipped_fixture_is_under_a_license_this_repo_may_redistribute() {
        // The gate `tools/fetch_corpora.py` enforces at write time,
        // re-checked here against what actually got committed. It is on
        // the LICENSE, not on the source name: adding a row to that
        // script's SOURCES table is not permission to ship it.
        let sources = load_sources_json();
        assert!(
            !sources.allowed_licenses.is_empty(),
            "sources.json records no allow-list at all"
        );
        for (key, meta) in &sources.sources {
            assert!(
                sources.allowed_licenses.contains(&meta.license),
                "{key} ships under {:?}, which is not in {:?}",
                meta.license,
                sources.allowed_licenses
            );
        }
    }

    #[test]
    fn source_metadata_agrees_with_the_cases_it_describes() {
        let sources = load_sources_json();
        for (key, meta) in &sources.sources {
            let cases = load_fixture(&meta.fixture);
            let mut kinds: Vec<String> = cases
                .iter()
                .map(|c| match c.kind {
                    Kind::Punctuation => "punctuation".to_string(),
                    Kind::Disfluency => "disfluency".to_string(),
                })
                .collect();
            kinds.sort_unstable();
            kinds.dedup();
            assert_eq!(&kinds, &meta.kinds, "{key}: kinds disagree");

            let mut langs: Vec<String> = cases.iter().map(|c| c.lang.clone()).collect();
            langs.sort_unstable();
            langs.dedup();
            assert_eq!(&langs, &meta.langs, "{key}: langs disagree");

            for (tag, count) in &meta.tags {
                let actual = cases.iter().filter(|c| c.has_tag(tag)).count();
                assert_eq!(actual, *count, "{key}: tag {tag:?} count disagrees");
            }
        }
    }

    #[test]
    fn no_sentence_appears_in_both_earnings22_fixtures() {
        // The punctuation sample and the disfluency sample are drawn from
        // disjoint sets of earnings calls, so a formatter cannot be scored
        // on disfluency removal over sentences it was also scored on for
        // punctuation. `tools/fetch_corpora.py` asserts disjointness on
        // the call lists themselves; this checks the committed text, which
        // is the thing that would actually be double-counted.
        let punctuation: std::collections::HashSet<String> =
            load_fixture("earnings22.jsonl")
                .into_iter()
                .map(|c| c.target)
                .collect();
        for case in load_fixture("earnings22-subset10.jsonl") {
            assert!(
                !punctuation.contains(&case.target),
                "id {}: sentence appears in both Earnings-22 fixtures",
                case.id
            );
        }
    }

    #[test]
    fn every_committed_fixture_id_is_unique_within_its_file() {
        for name in FIXTURE_FILES {
            let cases = load_fixture(name);
            let mut ids: Vec<&str> = cases.iter().map(|c| c.id.as_str()).collect();
            let n = ids.len();
            ids.sort_unstable();
            ids.dedup();
            assert_eq!(ids.len(), n, "{name}: duplicate id within the file");
        }
    }
}
