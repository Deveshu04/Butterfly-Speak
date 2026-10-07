//! Validates the model's output before it can reach the user's document.
//!
//! The formatter is a general LLM with a prompt, not a fine-tuned model that
//! can only emit edits. Its known failure mode is over-editing — swapping in
//! words the user never said — so the output is checked rather than
//! trusted.
//!
//! **Checked against what the model actually saw, not the verbatim ASR
//! transcript.** By the time any dictation reaches the model, the
//! deterministic rule pipeline (`cleanup::run_pipeline` /
//! `run_cloud_pipeline`) has already applied spoken commands, snippet and
//! dictionary-replacement expansion, tidy, and — on the local path — filler
//! removal, backtrack and ITN, none of which the model had any say in.
//! Comparing the model's reply against the pre-pipeline transcript charges
//! the model for every one of those rule-only edits, which is a guaranteed
//! false rejection for exactly the normalization the prompts ask the model
//! to do (Light's own prompt: "write numbers, dates, times, currencies,
//! emails and URLs the way people type them" — the rule pipeline usually
//! already got there first via ITN, so the model's untouched pass-through
//! only matches the *rule output*, not the raw words). See `check`'s doc
//! comment for a worked example.
//!
//! Thresholds were calibrated against full 1000-case live runs of
//! `sarvam-105b` at Balanced and Light over the evaluation corpora
//! (`eval::corpus`), per-case adjudicated. Enforcement is tiered (see
//! `sarvam::ws::resolve_format_inner`): truncation, an outright empty reply,
//! catastrophic content loss (`CATASTROPHIC_CONTENT_FLOOR`) and catastrophic
//! over-expansion (`CATASTROPHIC_RATIO_CEILING`) are enforced regardless of
//! `REPORT_ONLY`; the calibrated provisional tier is enforced while
//! `REPORT_ONLY` is `false`. Measured false-rejection rate on read speech
//! (`librispeech-pc`): 0/200 at Balanced, and at most 1% at Light — the low
//! tail there is the model editing where Light forbids it.
//!
//! Wired into the polish call sites via `sarvam::ws::resolve_format`, shared
//! by both `sarvam::ws` (cloud) and `asr::offline` (local).

use crate::format::backend::ChatReply;
use crate::format::level::CleanupLevel;

/// While true, *provisional*-tier rejections are logged but the output is still
/// used. Calibration ran with it true; it is `false` now that the bounds below
/// are calibrated against full live runs — every rejection falls back to the
/// rule-pipeline output and tells the user. Does not gate the always-enforced
/// tier (`Truncated`, an empty reply, catastrophic content loss, or
/// catastrophic over-expansion) — see `sarvam::ws::resolve_format_inner` for
/// the tier split and why calibration readiness must never gate those four.
pub const REPORT_ONLY: bool = false;

/// A content-retention floor no legitimate formatting pass can ever reach —
/// distinct from (and well below) the calibrated `RETAIN_STRICT`/
/// `RETAIN_LENIENT`. `ContentLost` at or below this floor is enforced
/// unconditionally, `REPORT_ONLY` or not — and, unlike the calibrated
/// floors, it is never excused by the novel-loss allowance: it is the
/// difference between "the model over-edited" and "the model answered the
/// dictation instead of formatting it" (e.g. "Sure, I can help with that."
/// for an invoice reminder — see
/// `sarvam::ws::tests::a_rejected_format_falls_back_to_the_rule_pipeline`).
/// Calibration corroborated it: in the 1000-case live balanced run, every
/// case at or under this floor was a translation, a transliteration or an
/// answer — except a single one-word spelling normalization ("बोहोत" ->
/// "बहुत", retention 0/1), the one known false positive in this tier.
pub const CATASTROPHIC_CONTENT_FLOOR: f32 = 0.15;

/// A length-ratio ceiling no legitimate formatting pass can ever reach — the
/// mirror image of `CATASTROPHIC_CONTENT_FLOOR` on the other side of `check`'s
/// two signals, and just as unconditional. Over-expansion structurally cannot
/// trip `ContentLost`: every original word can still be present (retention
/// stays 1.0) while the model pads the reply with invented content, so
/// `LengthRatio` is the *only* signal that ever sees it. Without this ceiling,
/// over-expansion would always land in the still-provisional band above
/// `MAX_RATIO`, and a fabricated addition would reach the user's document with
/// no warning even while `REPORT_ONLY` was true — see
/// `sarvam::ws::tests::a_wildly_expanded_reply_is_enforced_even_while_report_only`.
/// A reply three times the length of what the model was given is definitionally
/// adding information — every active level's prompt forbids that absolutely
/// ("Never add information, commentary, or a reply", see
/// `format::level::RULES_COMMON`) — so this is an invariant to enforce, not a
/// threshold that needs calibration. Ratios between the calibrated expansion
/// bound (`MAX_RATIO` plus `EXPANSION_SLACK_WORDS`) and this ceiling are
/// provisional-tier `LengthRatio` rejections — enforced like everything else
/// now that `REPORT_ONLY` is `false`, but tiered so the distinction stays
/// observable in logs and benchmarks.
pub const CATASTROPHIC_RATIO_CEILING: f32 = 3.0;

/// Fraction of `model_input`'s (the rule-pipeline output's) content words
/// that must survive. Levels that may not remove words are held to a
/// near-total bound.
///
/// Calibrated against a full 1000-case live `--level light` run of
/// `sarvam-105b`: Light's legitimate outputs keep retention ≥ 0.90 on read
/// speech and the low tail below that is the model editing where Light forbids
/// it, which is exactly what this floor is for.
const RETAIN_STRICT: f32 = 0.95;
/// Calibrated against a full 1000-case live `--level balanced` run of
/// `sarvam-105b`, adjudicated case by case. Measured legitimate edits bottom
/// out at 0.545 (false-start removal, `earnings22-subset10-0118`; whole-clause
/// dedup, `diarbench-hi-0174`) with a gloss-artifact Hindi case at 0.474 —
/// while the band below is translations and transliterations, which must be
/// rejected. 0.45 sits under every adjudicated-legitimate case with ~0.07
/// margin and above the translation cluster. Do not "tidy" it upward: 0.55
/// measurably rejects real, correct formatting (0118 at 0.545). The 0.45–0.62
/// band still contains over-edits this floor knowingly accepts (a rewrite at
/// 0.625, `disflqa-test-0544`) — retention cannot order those below the
/// legitimate cases, and a floor high enough to catch them false-rejects good
/// output first; the catastrophic tier and Undo are the backstops.
const RETAIN_LENIENT: f32 = 0.45;
/// Novel losses (see `GuardMetrics::novel_lost`) forgiven outright before
/// the retention floor is consulted: echo losses are always free (collapsed
/// repetition preserves content), and this many words of genuinely-gone
/// content are tolerated because on short inputs each dropped word swings
/// the retention fraction enormously ("जॉइंट joint" -> "joint" is retention
/// 0.5 on a one-real-word utterance). The catastrophic floor is NOT subject
/// to this allowance — see `check`.
const NOVEL_LOSS_ALLOWANCE_LENIENT: usize = 3;
/// Light/Off may not remove words at all, so a single novel loss is the
/// most that formatting jitter is allowed to explain, and only when the
/// reply has a word left over in its place: one word written the way people
/// type it ("i need five minutes" -> "I need 5 minutes."), however short the
/// dictation. A reply with nothing left over dropped one ("turn right at the
/// light" -> "Turn at the light.").
const NOVEL_LOSS_ALLOWANCE_STRICT: usize = 1;
/// Below this retention the allowance at Balanced and High forgives only a
/// reply that says nothing the input did not. Midway between the
/// catastrophic floor and `RETAIN_LENIENT`: on a short input three forgiven
/// words can be most of it, and "send the report tonight" -> "I'll send it."
/// (0.25) is a rewrite, not jitter, while "Monday, no, sorry, Tuesday" ->
/// "Tuesday." (0.25) is the self-correction those levels resolve.
const ALLOWANCE_FLOOR_LENIENT: f32 = 0.30;
/// `check()` runs retention before this bound — see its doc comment. That
/// ordering is load-bearing: it is what lets `MIN_RATIO` stay a real,
/// nonzero floor (a fast guard against drastic, duplicate/filler-padded
/// shrinkage that could otherwise fool word-for-word retention alone)
/// without stealing `ContentLost` from a fully empty reply, whose ratio is
/// always exactly 0.0.
///
/// Calibration (1000-case live balanced run): legitimate heavy
/// edits bottom out at ratio ≈ 0.46 (`earnings22-subset10`); everything
/// below ≈ 0.45 was translation or content loss, and every such case was
/// already caught by retention first — the corpus produced ZERO
/// `LengthRatio` rejections. 0.30 therefore stays: comfortably under the
/// legitimate band, live only as the anti-fooling backstop.
const MIN_RATIO: f32 = 0.30;
/// Calibration: the maximum ratio any of the 1000 live balanced
/// outputs reached was 1.50 (a two-word input gaining a title word), p99 ≈
/// 1.2. A doubling is far outside anything the model actually does when
/// formatting, while numbered-list output stays admissible through
/// `EXPANSION_SLACK_WORDS` below.
const MAX_RATIO: f32 = 2.00;
/// Flat word allowance on top of `MAX_RATIO` before over-expansion rejects:
/// the bound is `output_words > input_words * MAX_RATIO +
/// EXPANSION_SLACK_WORDS`, not a bare ratio. A pure ratio ceiling is
/// structurally wrong for short inputs — numbered-list formatting (which
/// High's own prompt demands) turns n content words into ~2n+3 ("milk
/// eggs" -> "Shopping list: 1. Milk 2. Eggs" is 2 -> 6, exactly ratio 3.0,
/// which a bare ceiling would enforce as catastrophic against a perfectly
/// correct format). The slack admits `2n + slack` for every n, so list
/// formatting passes at any length while a padded reply still has to beat
/// a doubling PLUS the slack to sneak through. `RejectReason::LengthRatio`
/// still reports the plain ratio, and the catastrophic tier still keys on
/// `CATASTROPHIC_RATIO_CEILING`: for inputs of <= 4 words the slack means
/// any expansion big enough to reject at all is already >= 3x, so it is
/// enforced rather than provisional — there is no meaningful provisional
/// band on tiny inputs.
const EXPANSION_SLACK_WORDS: usize = 4;

/// Hesitation sounds: never content at any level, so their removal never
/// counts against retention.
const HESITATIONS: &[&str] = &["um", "uh", "erm", "hmm", "mm"];

/// What a level that may remove words may drop without it counting against
/// retention: the hesitation sounds, and the words that are fillers as often
/// as not. At Light and Off every one of these words is content ("turn
/// right", "I know"), because those levels may not remove words at all.
const FILLERS: &[&str] = &[
    "um", "uh", "erm", "hmm", "mm", "like", "so", "basically", "actually",
    "literally", "right", "well", "okay", "yeah", "you", "know", "i", "mean",
];

/// The words `level` may drop for free.
fn fillers_for(level: CleanupLevel) -> &'static [&'static str] {
    if level.may_remove_words() {
        FILLERS
    } else {
        HESITATIONS
    }
}

/// Words that turn a sentence round. Losing one is never jitter, so the
/// novel-loss allowance does not cover it. `t` is what `content_words`
/// leaves of "n't" ("don't" is "don" and "t").
const NEGATIONS: &[&str] = &[
    "not", "no", "never", "nor", "neither", "none", "nothing", "nobody", "nowhere",
    "cannot", "t", "नहीं", "न", "मत",
];

/// The negations `level` may not drop. A level that may remove words
/// resolves self-corrections, and there "no" is the word that marks one
/// ("Tuesday, no, Wednesday." -> "Wednesday."), not a negation.
fn negations_in(words: &[String], level: CleanupLevel) -> usize {
    let may_drop_no = level.may_remove_words();
    words
        .iter()
        .filter(|w| NEGATIONS.contains(&w.as_str()) && !(may_drop_no && w.as_str() == "no"))
        .count()
}

/// Spoken connective words a formatter legitimately replaces with the symbol
/// people actually type — every active level's prompt asks for exactly this
/// ("write numbers, dates, times, currencies, emails and URLs the way people
/// type them", `format::level::RULES_COMMON`). An unmatched input word from
/// this list is exempted from retention only when the reply actually
/// contains its symbol *inside* a token (alphanumerics on both sides, the
/// signature of "asha at example dot com" -> "asha@example.com"), and each
/// interior symbol occurrence can excuse at most one word. Without this,
/// dictating a bare email address scores retention 0.375 and is rejected
/// as catastrophic content loss — see `spelled_out_email_...` tests.
const ABSORBED_INTO_SYMBOLS: &[(&str, char)] = &[
    ("at", '@'),
    ("dot", '.'),
    ("point", '.'),
    ("dash", '-'),
    ("hyphen", '-'),
    ("minus", '-'),
    ("underscore", '_'),
    ("slash", '/'),
    ("colon", ':'),
    ("plus", '+'),
    ("hash", '#'),
];

/// Zero-width joiners that shape Indic conjuncts and must not split a word.
/// Every script's virama and nukta are combining marks, which
/// `canonical::belongs_in_word` already keeps; these two are format
/// characters, which it does not.
const WORD_JOINERS: &[char] = &['\u{200C}', '\u{200D}'];

#[derive(Debug, PartialEq)]
pub enum Verdict {
    Accept,
    Reject(RejectReason),
}

#[derive(Debug, PartialEq)]
pub enum RejectReason {
    Truncated,
    ContentLost { retained: f32 },
    LengthRatio { ratio: f32 },
}

impl RejectReason {
    pub fn user_message(&self) -> &'static str {
        match self {
            RejectReason::Truncated => "Formatting was cut short — used the plain transcript",
            RejectReason::ContentLost { .. } => "Formatting changed too much — used the plain transcript",
            RejectReason::LengthRatio { .. } => "Formatting went off track — used the plain transcript",
        }
    }
}

/// Lowercased runs of alphanumeric characters. Formatting deliberately
/// changes casing, punctuation and number style, so the comparison must be
/// blind to all three or it would reject correct output.
///
/// Splits *inside* tokens, not just at their edges (trimming only the edges
/// leaves `asha@example.com` as one token that can never multiset-match
/// the spelled-out `example` / `com` it was formatted from — the
/// calibration's worst false positive, retention 0.375 on a plain email
/// dictation). Splitting is symmetric, so text the model leaves alone still
/// matches itself exactly; it only changes outcomes where one side joined
/// or un-joined words around a symbol, which is precisely the formatting
/// (emails, URLs, times, hyphenation) the prompts ask for.
///
/// Word characters are `canonical::belongs_in_word`'s — Unicode letters,
/// numbers and combining marks, so every script's vowel signs, nukta and
/// virama stay inside their word — plus the [`WORD_JOINERS`]. Each word is
/// then `canonical::fold`ed, so two spellings of one word (a nukta letter
/// precomposed or written apart) compare equal, and "ok" is "okay".
///
/// A run with no letter or digit in it is not a word: U+FE0F after an emoji
/// is a mark, and so is kept by the split, but it only picks the emoji's
/// colour form.
pub fn content_words(text: &str) -> Vec<String> {
    text.split(|c: char| !crate::canonical::belongs_in_word(c) && !WORD_JOINERS.contains(&c))
        .filter(|w| w.chars().any(char::is_alphanumeric))
        .map(crate::canonical::fold)
        .map(|w| if w == "ok" { "okay".to_string() } else { w })
        .collect()
}

/// How many of each symbol occur *inside* a token of `text` — a
/// non-alphanumeric char with alphanumerics immediately on both sides, the
/// written form of a spoken connective ("asha at example dot com" carries
/// its '@' and '.' this way, a sentence-final period does not). Feeds the
/// [`ABSORBED_INTO_SYMBOLS`] exemption budget.
fn interior_symbol_counts(text: &str) -> Vec<(char, usize)> {
    let chars: Vec<char> = text.chars().collect();
    let mut counts: Vec<(char, usize)> = Vec::new();
    for i in 1..chars.len().saturating_sub(1) {
        let c = chars[i];
        if !c.is_alphanumeric() && chars[i - 1].is_alphanumeric() && chars[i + 1].is_alphanumeric()
        {
            match counts.iter_mut().find(|(k, _)| *k == c) {
                Some((_, n)) => *n += 1,
                None => counts.push((c, 1)),
            }
        }
    }
    counts
}

/// The two quantitative signals `check` weighs, computed exactly as `check`
/// computes them — this IS `check`'s arithmetic, split out so calibration
/// tooling (`fmtbench --guard-dump`) can record per-case values without
/// duplicating the multiset-retention logic and drifting from it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GuardMetrics {
    /// Content words in `model_input` (the rule-pipeline output).
    pub input_words: usize,
    /// Content words in the model's reply.
    pub output_words: usize,
    /// Non-filler input words — the retention denominator.
    pub expected: usize,
    /// How many of `expected` survive in the output (multiset semantics).
    pub kept: usize,
    /// `kept / expected`; `None` when the input had no non-filler words to
    /// preserve (retention is then meaningless, not perfect).
    pub retained: Option<f32>,
    /// Of the words counted lost, how many are *novel* losses — no copy of
    /// the word survives anywhere in the output. A lost word whose duplicate
    /// was kept is a collapse of repetition ("हम्म हम्म" -> "हम्म।",
    /// "no no no" -> "No."), which Balanced/High are entitled to do and
    /// which crushes the retention fraction on short inputs precisely
    /// because there was so little else to keep. `check` forgives a small
    /// number of novel losses outright and never charges echo losses beyond
    /// the retention fraction itself.
    pub novel_lost: usize,
    /// The reply has fewer negations ([`negations_in`]) than the input: a
    /// "not" is gone, or at Light and Off a "no". Counted as a class, so
    /// "don't" written out as "do not" loses nothing.
    pub negation_lost: bool,
    /// The reply has a word the input does not have anywhere.
    pub added_words: bool,
    /// Output words that matched no input word, by any of the credits
    /// below: where a changed word went ("five" written as "5" leaves the
    /// "5" over).
    pub unmatched_output: usize,
    /// `output_words / input_words`; `None` when the input had no content
    /// words at all (the case where `check` accepts only a reply that has
    /// none either).
    pub ratio: Option<f32>,
}

/// Compute [`GuardMetrics`] for one (model input, model output) pair. See
/// [`check`] for what the values mean and how they are enforced; `check`
/// itself is built on this function, so a dumped metric can never disagree
/// with the verdict that was actually reached.
///
/// Retention counts how many of the model-input content words survive,
/// with multiset semantics (a word repeated twice in the input must appear
/// twice in the output to count twice), after three formatter-entitled
/// exemptions, each of which exists because a legitimate, prompt-requested
/// edit would otherwise be charged as content loss:
///
/// 1. The fillers `level` may drop ([`fillers_for`]) never count at all.
/// 2. **Join credit**, in both directions: an output token that is exactly
///    the concatenation of two or more consecutive input words counts as
///    keeping all of them — "butterfly labs dot com" ->
///    `butterflylabs.com` keeps "butterfly" and "labs" through the joined
///    token — and an input token the model legitimately un-joined ("iam"
///    -> "I am", a real ASR artifact from the calibration corpus,
///    `earnings22-subset10-0118`) is kept through its output pieces.
///    Order matters (a reversed pair earns nothing) and the match must be
///    exact and complete.
/// 3. **Absorbed connectives**: an unmatched [`ABSORBED_INTO_SYMBOLS`] word
///    is excused if the reply still has budget of its symbol *inside* a
///    token — "at" is excused by the '@' of `asha@example.com`, not by a
///    sentence-final period.
pub fn metrics(model_input: &str, formatted: &str, level: CleanupLevel) -> GuardMetrics {
    let fillers = fillers_for(level);
    let input_words = content_words(model_input);
    let out_words = content_words(formatted);
    let n_in = input_words.len();
    let n_out = out_words.len();

    let mut in_used = vec![false; input_words.len()];
    let mut out_used = vec![false; out_words.len()];
    let mut expected = 0usize;
    let mut kept = 0usize;

    // Pass 1 — join credit. Byte-offset prefix matching is safe here:
    // `acc_len` only ever grows by the byte length of a word that was just
    // verified to be the exact next chunk of `t`, so it always lands on a
    // char boundary.
    for (oi, t) in out_words.iter().enumerate() {
        'starts: for si in 0..input_words.len() {
            let mut acc_len = 0usize;
            let mut k = si;
            while k < input_words.len() && acc_len < t.len() {
                let w = &input_words[k];
                if in_used[k] || fillers.contains(&w.as_str()) || !t[acc_len..].starts_with(w.as_str()) {
                    break;
                }
                acc_len += w.len();
                k += 1;
                if acc_len == t.len() && k - si >= 2 {
                    for flag in &mut in_used[si..k] {
                        *flag = true;
                    }
                    out_used[oi] = true;
                    expected += k - si;
                    kept += k - si;
                    break 'starts;
                }
            }
        }
    }

    // Pass 1b — the same credit mirrored: the model un-joined an
    // ASR-concatenated input token into consecutive output words. Output
    // fillers may participate ("iam" -> "I am" needs the "i"): the unit
    // being credited is the input word, so the filler exemption — which is
    // about what the INPUT expects — does not apply to the pieces.
    for (ii, w) in input_words.iter().enumerate() {
        if in_used[ii] || fillers.contains(&w.as_str()) {
            continue;
        }
        'starts: for so in 0..out_words.len() {
            let mut acc_len = 0usize;
            let mut k = so;
            while k < out_words.len() && acc_len < w.len() {
                let t = &out_words[k];
                if out_used[k] || !w[acc_len..].starts_with(t.as_str()) {
                    break;
                }
                acc_len += t.len();
                k += 1;
                if acc_len == w.len() && k - so >= 2 {
                    for flag in &mut out_used[so..k] {
                        *flag = true;
                    }
                    in_used[ii] = true;
                    expected += 1;
                    kept += 1;
                    break 'starts;
                }
            }
        }
    }

    // Pass 2 — multiset matching over what's left, with the absorbed-
    // connective exemption for what still fails. A lost word with no copy
    // of itself anywhere in the output is a *novel* loss (see
    // `GuardMetrics::novel_lost`); a lost word whose duplicate survived is
    // an echo loss — repetition collapsed, content intact.
    let mut symbol_budget = interior_symbol_counts(formatted);
    let mut novel_lost = 0usize;
    for (ii, word) in input_words.iter().enumerate() {
        if in_used[ii] || fillers.contains(&word.as_str()) {
            continue;
        }
        if let Some(oi) = (0..out_words.len()).find(|&oi| !out_used[oi] && out_words[oi] == *word)
        {
            out_used[oi] = true;
            expected += 1;
            kept += 1;
            continue;
        }
        let absorbed = ABSORBED_INTO_SYMBOLS
            .iter()
            .filter(|(w, _)| *w == word.as_str())
            .any(|(_, symbol)| {
                symbol_budget
                    .iter_mut()
                    .find(|(c, n)| c == symbol && *n > 0)
                    .map(|(_, n)| *n -= 1)
                    .is_some()
            });
        if !absorbed {
            expected += 1;
            if !out_words.contains(word) {
                novel_lost += 1;
            }
        }
    }

    GuardMetrics {
        input_words: n_in,
        output_words: n_out,
        expected,
        kept,
        retained: (expected > 0).then(|| kept as f32 / expected as f32),
        novel_lost,
        negation_lost: negations_in(&out_words, level) < negations_in(&input_words, level),
        added_words: out_words.iter().any(|w| !input_words.contains(w)),
        unmatched_output: out_used.iter().filter(|used| !**used).count(),
        ratio: (n_in > 0).then(|| n_out as f32 / n_in as f32),
    }
}

/// `model_input` must be the text the model was actually given — the
/// rule-pipeline output (`cleanup::run_pipeline` / `run_cloud_pipeline`),
/// **not** the verbatim ASR transcript. The model never sees the raw
/// transcript; it sees rule output that has already had spoken commands,
/// snippets/replacements, tidy, and (locally) fillers/backtrack/ITN applied.
/// Comparing against the pre-pipeline transcript instead charges the model
/// for edits the rules made and turns ordinary, correct formatting into a
/// guaranteed rejection — concretely, at Light, "the meeting is at three
/// thirty pm" -> "The meeting is at 3:30 PM." scores retention 0.714
/// against the raw transcript (the spelled-out "three thirty" can't match
/// "3:30") but 1.0 against the rule-pipeline output, which already
/// normalized the time via ITN before the model ran. The same is true of
/// currency, and of anything a spoken command, snippet, replacement or
/// backtrack self-correction changed upstream of the model. A verbatim/raw
/// transcript here produces false rejections across Light/Balanced and
/// every rule-pipeline stage.
///
/// Retention runs before the length-ratio check, not after. A fully empty
/// reply has `ratio == 0.0`, and with a nonzero `MIN_RATIO` that would trip
/// the ratio check first and report `LengthRatio` for what is really total
/// content loss — retention is the more precise signal and must get first
/// look. Running it first also means the ratio check is only ever the
/// second opinion: something that survives word-for-word retention (e.g. by
/// padding or repeating real transcript words) but still comes back at an
/// implausible length still gets caught, just under `LengthRatio` rather
/// than `ContentLost`.
///
/// When `model_input` has no content words of its own (punctuation-only
/// noise, an emoji), there was nothing to preserve and nothing for the model
/// to write: a reply with no content words either is accepted, and a reply
/// with any is an answer, rejected as over-expansion (`LengthRatio` with an
/// infinite ratio, so the catastrophic tier enforces it). Callers that need
/// "the reply itself must be non-empty" as an unconditional guarantee (this
/// codebase does — see `sarvam::ws::resolve_format_inner`) must check that
/// independently rather than relying on this function for it.
pub fn check(model_input: &str, formatted: &str, reply: &ChatReply, level: CleanupLevel) -> Verdict {
    if reply.was_truncated() {
        return Verdict::Reject(RejectReason::Truncated);
    }

    let m = metrics(model_input, formatted, level);

    // `ratio` is `None` exactly when the input had no content words.
    let Some(ratio) = m.ratio else {
        return if m.output_words == 0 {
            Verdict::Accept
        } else {
            Verdict::Reject(RejectReason::LengthRatio { ratio: f32::INFINITY })
        };
    };

    let (floor, allowance) = if level.may_remove_words() {
        (RETAIN_LENIENT, NOVEL_LOSS_ALLOWANCE_LENIENT)
    } else {
        (RETAIN_STRICT, NOVEL_LOSS_ALLOWANCE_STRICT)
    };

    if let Some(retained) = m.retained {
        // The catastrophic band is never excused by the novel-loss
        // allowance: "थैंक्यू सर" answered with "You're welcome!" is a
        // two-word dictation whose novel losses fit inside any reasonable
        // allowance, and it is exactly the replied-instead-of-formatted
        // failure the unconditional floor exists for. Nor is a reply that
        // dropped a negation; nor, at Balanced and High, one that kept too
        // little for its losses to be jitter and says something new; nor, at
        // Light and Off, one with no word left over to stand for each word
        // lost, which is a word dropped rather than changed.
        let forgiven = if m.negation_lost {
            0
        } else if level.may_remove_words() {
            if retained >= ALLOWANCE_FLOOR_LENIENT || !m.added_words {
                allowance
            } else {
                0
            }
        } else if m.unmatched_output >= m.novel_lost {
            allowance
        } else {
            0
        };
        let catastrophic = retained <= CATASTROPHIC_CONTENT_FLOOR;
        if catastrophic || (m.novel_lost > forgiven && retained < floor) {
            return Verdict::Reject(RejectReason::ContentLost { retained });
        }
    }

    // The shrinkage backstop fires only when the missing mass includes
    // novel losses: shrinkage fully explained by fillers, echo losses and
    // join/absorb credits IS the requested edit ("no no no no" -> "No." is
    // ratio 0.25 and a perfect format). Over-expansion has no such excuse —
    // padding with repeats of real words keeps novel_lost at zero while
    // inventing emphasis the user never dictated — so it stays
    // unconditional.
    let shrunk = ratio < MIN_RATIO && m.novel_lost > allowance;
    let over_expanded = m.output_words as f32
        > m.input_words as f32 * MAX_RATIO + EXPANSION_SLACK_WORDS as f32;
    if shrunk || over_expanded {
        return Verdict::Reject(RejectReason::LengthRatio { ratio });
    }

    Verdict::Accept
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::backend::ChatReply;
    use crate::format::level::CleanupLevel;

    fn reply(text: &str, finish: &str) -> ChatReply {
        ChatReply {
            text: text.into(),
            finish_reason: finish.into(),
            prompt_tokens: 0,
            completion_tokens: 0,
            first_token_ms: None,
        }
    }

    #[test]
    fn a_normal_format_is_accepted() {
        let raw = "um so the meeting is at three thirty pm";
        let out = "The meeting is at 3:30 PM.";
        assert_eq!(
            check(raw, out, &reply(out, "stop"), CleanupLevel::Balanced),
            Verdict::Accept
        );
    }

    /// max_tokens cut the reply off mid-sentence; a non-empty check alone
    /// would let it reach the user's document.
    #[test]
    fn a_truncated_reply_is_rejected() {
        let raw = "tell them the release ships on thursday and the docs follow";
        let out = "Tell them the release ships on Thursday and the do";
        assert!(matches!(
            check(raw, out, &reply(out, "length"), CleanupLevel::Balanced),
            Verdict::Reject(RejectReason::Truncated)
        ));
    }

    /// The model answered the dictation instead of formatting it.
    #[test]
    fn a_reply_that_drops_the_content_is_rejected() {
        let raw = "remind me to send the invoice to priya before friday afternoon";
        let out = "Sure, I can help with that.";
        assert!(matches!(
            check(raw, out, &reply(out, "stop"), CleanupLevel::Balanced),
            Verdict::Reject(RejectReason::ContentLost { .. })
        ));
    }

    /// Light must not remove words, so filler removal at Light is a defect
    /// even though it would be correct at Balanced.
    #[test]
    fn light_is_held_to_a_stricter_retention_bound_than_balanced() {
        let raw = "um so basically i mean the thing is we should probably just ship it";
        let out = "We should just ship it.";
        assert!(matches!(
            check(raw, out, &reply(out, "stop"), CleanupLevel::Light),
            Verdict::Reject(RejectReason::ContentLost { .. })
        ));
        assert_eq!(
            check(raw, out, &reply(out, "stop"), CleanupLevel::Balanced),
            Verdict::Accept
        );
    }

    /// This only proves `check()` returns the right `Verdict` — it says
    /// nothing about whether that verdict is actually *enforced*. Tiering
    /// (which rejections `REPORT_ONLY` is allowed to silence) happens one
    /// layer up in `sarvam::ws::resolve_format_inner`; see
    /// `CATASTROPHIC_RATIO_CEILING`'s doc comment and
    /// `sarvam::ws::tests::a_wildly_expanded_reply_is_enforced_even_while_report_only`
    /// for the case where this verdict alone was not enough.
    #[test]
    fn a_wildly_expanded_reply_is_rejected() {
        let raw = "lunch at noon";
        let out = "Lunch at noon. I have also taken the liberty of booking a table \
                   for four people at the Italian restaurant on the corner, and \
                   added a calendar invitation for everyone on the team.";
        assert!(matches!(
            check(raw, out, &reply(out, "stop"), CleanupLevel::High),
            Verdict::Reject(RejectReason::LengthRatio { .. })
        ));
    }

    /// Formatting deliberately changes surface form; the check must be blind
    /// to casing, punctuation and number style or it rejects correct output.
    #[test]
    fn content_words_ignore_case_and_punctuation() {
        assert_eq!(content_words("The meeting is at 3:30 PM."), content_words("the meeting is at 3:30 pm"));
    }

    /// THE calibration's worst false positive: an edge-trimming tokenizer
    /// leaves `asha@example.com` as one token that can never match the
    /// spelled-out words it was formatted from, so a plain email dictation
    /// scores retention 0.375 — an enforced, catastrophic-tier rejection of
    /// exactly the formatting every level's prompt asks for. Split tokens + the
    /// absorbed-connective budget make it retention 1.0: `asha`/`example`/`com`
    /// match their split-out selves, and the unmatched "at"/"dot" are excused
    /// by the '@' and '.' sitting *inside* the reply's token. Must hold at
    /// Light too — the strictest retention floor — because Light's prompt
    /// equally requires it.
    #[test]
    fn a_spelled_out_email_joined_by_the_model_is_accepted_at_every_level() {
        let rule_out = "my email is asha at example dot com";
        let out = "My email is asha@example.com.";
        for level in [CleanupLevel::Light, CleanupLevel::Balanced, CleanupLevel::High] {
            assert_eq!(
                check(rule_out, out, &reply(out, "stop"), level),
                Verdict::Accept,
                "{level:?} must accept a correctly formatted email"
            );
        }
        // The bare address on its own — the worst measured case (0.375
        // with an edge-trimming tokenizer).
        let m = metrics("asha at example dot com", "asha@example.com", CleanupLevel::Light);
        assert_eq!((m.expected, m.kept), (3, 3), "at/dot excused, identities kept");
        assert_eq!(m.retained, Some(1.0));
    }

    /// The '@' budget is one exemption per interior symbol occurrence, and a
    /// sentence-final period is not an interior dot: dropping the word "at"
    /// from plain prose earns no credit from unrelated punctuation.
    #[test]
    fn absorbed_connective_credit_needs_the_symbol_inside_a_token() {
        // "at" dropped, reply has no interior '@' or '.': the loss counts.
        let m = metrics("meet me at the office", "Meet me the office.", CleanupLevel::Light);
        assert_eq!((m.expected, m.kept), (5, 4));
        // Same drop, but the reply carries an email: ONE interior '@'
        // excuses ONE "at" — the second dropped "at" still counts.
        let m = metrics(
            "mail bob at hq dot com to meet me at the office at nine",
            "Mail bob@hq.com to meet me the office nine.",
            CleanupLevel::Light,
        );
        // bob/hq/com/to/meet/me/the/office/nine kept via match or join;
        // first unmatched "at" and the "dot" excused by '@'/'.'; the
        // remaining two "at"s are real losses.
        assert_eq!(m.kept + 2, m.expected, "exactly two words lost: {m:?}");
    }

    /// Join credit (order-sensitive, exact): "butterfly labs dot com slash
    /// docs" -> "butterflylabs.com/docs" keeps every identity word through
    /// the joined token; a REVERSED pair earns nothing, so the credit
    /// cannot be used to smuggle in a rewrite.
    #[test]
    fn words_joined_into_a_url_are_kept_not_lost() {
        let rule_out = "the docs are at butterfly labs dot com slash docs";
        let out = "The docs are at butterflylabs.com/docs.";
        let m = metrics(rule_out, out, CleanupLevel::Light);
        assert_eq!(m.retained, Some(1.0), "{m:?}");
        assert_eq!(
            check(rule_out, out, &reply(out, "stop"), CleanupLevel::Light),
            Verdict::Accept
        );

        // Reversed order must NOT be credited.
        let m = metrics("labs butterfly", "butterflylabs", CleanupLevel::Light);
        assert_eq!((m.expected, m.kept), (2, 0));
    }

    /// The mirror credit: an ASR-concatenated token the model legitimately
    /// un-joins is kept, not lost. Real calibration case
    /// (`earnings22-subset10-0118`): "iam" -> "I am" — note the un-joined
    /// "i" is a `FILLERS` word, which must not disqualify it as a piece.
    #[test]
    fn an_asr_concatenation_unjoined_by_the_model_is_kept() {
        let m = metrics("iam now ninety one", "I am now ninety-one.", CleanupLevel::Balanced);
        assert_eq!((m.expected, m.kept), (4, 4), "{m:?}");
        assert_eq!(m.retained, Some(1.0));
    }

    /// A bare ratio ceiling makes every short list dictation an enforced
    /// rejection — "milk eggs" formatted as a shopping list is 2 -> 6 content
    /// words, exactly the catastrophic 3.0. Numbered-list output is ~2n+3 words
    /// for n items, so the ceiling must carry a flat slack, not scale purely
    /// with input length.
    #[test]
    fn a_two_word_note_formatted_as_a_list_is_accepted() {
        let rule_out = "milk eggs";
        let out = "Shopping list: 1. Milk 2. Eggs";
        assert_eq!(
            check(rule_out, out, &reply(out, "stop"), CleanupLevel::High),
            Verdict::Accept
        );
        // And the slack scales to longer lists: 4 items -> header + 4
        // numbers is still within 2n + slack.
        let rule_out = "milk eggs bread butter";
        let out = "Shopping list: 1. Milk 2. Eggs 3. Bread 4. Butter";
        assert_eq!(
            check(rule_out, out, &reply(out, "stop"), CleanupLevel::High),
            Verdict::Accept
        );
    }

    /// Non-Latin scripts must survive: the app supports 23 languages.
    #[test]
    fn devanagari_passes_the_retention_check() {
        let raw = "नमस्ते आप कैसे हैं";
        let out = "नमस्ते, आप कैसे हैं?";
        assert_eq!(
            check(raw, out, &reply(out, "stop"), CleanupLevel::Balanced),
            Verdict::Accept
        );
    }

    #[test]
    fn an_empty_reply_is_rejected() {
        assert!(matches!(
            check("hello world", "", &reply("", "stop"), CleanupLevel::Light),
            Verdict::Reject(RejectReason::ContentLost { .. })
        ));
    }

    /// The ratio floor is the backstop for shrinkage that retention cannot see
    /// — but only when the missing mass includes NOVEL losses. A filler wall
    /// stripped down to its intact core is what a correct Balanced format of
    /// filler-heavy dictation looks like (and "no no no no" -> "No." is ratio
    /// 0.25), so that shape is pinned as Accept below. What `MIN_RATIO` still
    /// catches: a reply that ALSO silently dropped real words while the filler
    /// wall makes the retention fraction look healthy — here full/and/sanjay/by
    /// are gone (novel loss 4) yet retention is 8/12 ≈ 0.67, comfortably above
    /// the floor, and only the ratio bound notices anything.
    #[test]
    fn a_drastically_shortened_reply_is_rejected_on_ratio_only_when_novel_content_is_missing() {
        let fillers = "um uh erm hmm mm like so basically actually literally right \
                       well okay yeah you know i mean um uh erm hmm mm like so basically";

        // Intact core under the filler wall: ratio 5/31 but nothing novel
        // is missing — a correct format, accepted.
        let raw = format!("{fillers} meeting is at three thirty");
        let out = "Meeting is at three thirty.";
        assert_eq!(
            check(&raw, out, &reply(out, "stop"), CleanupLevel::Balanced),
            Verdict::Accept
        );

        // Same wall, but the reply also lost full/and/sanjay/by — novel
        // losses retention's floor does not catch (8/12 kept): the ratio
        // backstop must.
        let raw = format!("{fillers} please email the full quarterly report to priya and sanjay by friday");
        let out = "Please email the quarterly report to Priya, Friday.";
        assert!(matches!(
            check(&raw, out, &reply(out, "stop"), CleanupLevel::Balanced),
            Verdict::Reject(RejectReason::LengthRatio { .. })
        ));
    }

    // --- Comparison-basis tests ---
    //
    // These pair the SAME `formatted` output against two different first
    // arguments: the pre-rule-pipeline transcript (wrong — rejects) and the
    // rule-pipeline output the model actually saw (right — accepts). Each
    // one reproduces a real rule-pipeline stage (ITN, spoken commands) that
    // runs before the model ever sees the dictation.

    /// ITN (numbers/times) already normalized this before the model ran.
    /// Comparing against the pre-pipeline transcript rejects the model for
    /// the rule pipeline's own work; comparing against what the model
    /// actually saw accepts it, as Light's own prompt requires ("write
    /// numbers, dates, times... the way people type them").
    #[test]
    fn time_itn_done_by_the_rule_pipeline_is_not_charged_to_the_model() {
        let pre_pipeline = "the meeting is at three thirty pm";
        let rule_output = "The meeting is at 3:30 PM."; // cleanup::run_pipeline's own golden case
        let formatted = rule_output; // the model changed nothing further
        assert!(
            matches!(
                check(pre_pipeline, formatted, &reply(formatted, "stop"), CleanupLevel::Light),
                Verdict::Reject(RejectReason::ContentLost { .. })
            ),
            "sanity check: comparing against the pre-pipeline transcript must reject this"
        );
        assert_eq!(
            check(rule_output, formatted, &reply(formatted, "stop"), CleanupLevel::Light),
            Verdict::Accept
        );
    }

    /// Same failure mode for currency ITN, and this time the model *does*
    /// still do real work (₹ -> "Rs."), which must not be penalized either.
    #[test]
    fn currency_itn_done_by_the_rule_pipeline_is_not_charged_to_the_model() {
        let pre_pipeline = "send five hundred rupees";
        let rule_output = "Send ₹500."; // cleanup::run_pipeline's own golden case
        let formatted = "Send Rs. 500.";
        assert!(
            matches!(
                check(pre_pipeline, formatted, &reply(formatted, "stop"), CleanupLevel::Light),
                Verdict::Reject(RejectReason::ContentLost { .. })
            ),
            "sanity check: comparing against the pre-pipeline transcript must reject this"
        );
        assert_eq!(
            check(rule_output, formatted, &reply(formatted, "stop"), CleanupLevel::Light),
            Verdict::Accept
        );
    }

    /// `commands::apply` consumes "new line" into a real newline before the
    /// model ever runs — those two words can never appear in any legitimate
    /// model output again. Comparing against the pre-pipeline transcript
    /// falsely rejects every spoken-command dictation.
    #[test]
    fn spoken_command_words_consumed_by_the_rule_pipeline_are_not_charged_to_the_model() {
        let pre_pipeline = "first point new line second point";
        let rule_output = "First point\nsecond point."; // cleanup::run_pipeline's own golden case
        let formatted = rule_output; // the model changed nothing further
        assert!(
            matches!(
                check(pre_pipeline, formatted, &reply(formatted, "stop"), CleanupLevel::Light),
                Verdict::Reject(RejectReason::ContentLost { .. })
            ),
            "sanity check: comparing against the pre-pipeline transcript must reject this"
        );
        assert_eq!(
            check(rule_output, formatted, &reply(formatted, "stop"), CleanupLevel::Light),
            Verdict::Accept
        );
    }

    /// Measuring against the rule output does not mean giving up on real
    /// over-editing: when the model itself drops content the rule pipeline
    /// preserved, it must still be rejected.
    ///
    /// Calibration note: the milder trim "Remind me to send the invoice."
    /// (retention 0.545, novel loss 4) sits exactly where the measured live
    /// corpus put *legitimate* Balanced edits (false-start removal at 0.545,
    /// whole-clause dedup at 0.545), so a floor that rejects the mild trim
    /// provably rejects correct formatting too — the calibrated
    /// `RETAIN_LENIENT` (0.45) deliberately accepts it, and the case pinned
    /// here is the deeper cut that stays inside the guard's reach (retention
    /// 0.273, novel loss 6: the recipient, the deadline, and the framing all
    /// gone).
    #[test]
    fn a_genuine_over_edit_by_the_model_is_still_rejected_against_the_rule_output() {
        let rule_output = "Remind me to send the invoice to Priya before Friday afternoon.";
        let formatted = "Send the invoice.";
        assert!(matches!(
            check(rule_output, formatted, &reply(formatted, "stop"), CleanupLevel::Balanced),
            Verdict::Reject(RejectReason::ContentLost { .. })
        ));

        // The mild trim is the documented conscious trade: accepted.
        let mild = "Remind me to send the invoice.";
        assert_eq!(
            check(rule_output, mild, &reply(mild, "stop"), CleanupLevel::Balanced),
            Verdict::Accept,
            "the 0.545-retention band is where measured-legitimate edits live; \
             rejecting it costs correct formatting (see RETAIN_LENIENT's doc)"
        );
    }

    /// Repetition collapse must never be charged as content loss, however
    /// hard it crushes the retention fraction: every "lost" word still has
    /// a surviving copy (echo loss), so nothing the user said is gone.
    /// Real calibration cases: "हम्म हम्म" -> "हम्म।" (retention 0.5) and a
    /// whole repeated clause collapsed at 0.545 (`diarbench-hi-0174`).
    #[test]
    fn collapsed_repetition_is_accepted_even_at_brutal_retention_fractions() {
        let m = metrics("no no no no", "No.", CleanupLevel::Balanced);
        assert_eq!(m.novel_lost, 0, "{m:?}");
        assert_eq!(
            check("no no no no", "No.", &reply("No.", "stop"), CleanupLevel::Balanced),
            Verdict::Accept
        );
        let rule_out = "फूलन देवी वहां की नहीं फूलन देवी वहां की नहीं थी";
        let out = "फूलन देवी वहां की नहीं थी।";
        assert_eq!(
            check(rule_out, out, &reply(out, "stop"), CleanupLevel::Balanced),
            Verdict::Accept
        );
    }

    /// An input with no words ("?", an ellipsis, an emoji) gives the model
    /// nothing to write. A reply with words in it is an answer.
    #[test]
    fn a_reply_with_words_to_an_input_without_any_is_rejected() {
        let answer = "How can I help?";
        for input in ["?", "…", "🙂"] {
            assert!(
                matches!(
                    check(input, answer, &reply(answer, "stop"), CleanupLevel::Balanced),
                    Verdict::Reject(RejectReason::LengthRatio { .. })
                ),
                "{input:?}"
            );
            assert_eq!(
                check(input, input, &reply(input, "stop"), CleanupLevel::Balanced),
                Verdict::Accept,
                "{input:?}"
            );
        }
    }

    /// The novel-loss allowance is for jitter on short inputs, not for a
    /// reply that kept a quarter of the words.
    #[test]
    fn the_allowance_does_not_forgive_a_reply_that_kept_almost_nothing() {
        let out = "I'll send it.";
        assert!(matches!(
            check("send the report tonight", out, &reply(out, "stop"), CleanupLevel::Balanced),
            Verdict::Reject(RejectReason::ContentLost { .. })
        ));
    }

    /// A dropped "not" turns the sentence round, and no allowance covers it.
    /// Spelling a contraction out keeps the negation, so that stays fine.
    #[test]
    fn a_lost_negation_is_never_forgiven() {
        let out = "Do delete the file.";
        assert!(matches!(
            check("do not delete the file", out, &reply(out, "stop"), CleanupLevel::Light),
            Verdict::Reject(RejectReason::ContentLost { .. })
        ));
        let out = "I do not know where it is.";
        assert_eq!(
            check("i don't know where it is", out, &reply(out, "stop"), CleanupLevel::Balanced),
            Verdict::Accept
        );
    }

    /// Light may not remove words, so the words other levels may drop as
    /// fillers are content there: "right" in "turn right" is a direction.
    #[test]
    fn light_counts_the_words_other_levels_may_drop_as_fillers() {
        let out = "Turn at the light.";
        assert!(matches!(
            check("turn right at the light", out, &reply(out, "stop"), CleanupLevel::Light),
            Verdict::Reject(RejectReason::ContentLost { .. })
        ));
        // Hesitation sounds are never content, at Light either.
        let out = "Turn right at the light.";
        assert_eq!(
            check("um turn right at the light", out, &reply(out, "stop"), CleanupLevel::Light),
            Verdict::Accept
        );
    }

    /// Light may change a word the way people type it, even in a short
    /// dictation where that one word is a large share of it, but it may not
    /// drop one.
    #[test]
    fn light_forgives_one_changed_word_but_not_a_dropped_one() {
        for (input, out) in [
            ("i need five minutes", "I need 5 minutes."),
            ("he dont know", "He doesn't know."),
            ("ok thanks", "Okay, thanks."),
            // Beside a join or a collapsed stutter the reply has fewer
            // words than were expected, yet still holds the changed word.
            ("open the note book in five minutes", "Open the notebook in 5 minutes."),
            ("i can not come at five", "I cannot come at 5."),
            ("i i need five minutes", "I need 5 minutes."),
        ] {
            assert_eq!(
                check(input, out, &reply(out, "stop"), CleanupLevel::Light),
                Verdict::Accept,
                "{input:?} -> {out:?}"
            );
        }
        let out = "Turn at the light.";
        assert!(matches!(
            check("turn right at the light", out, &reply(out, "stop"), CleanupLevel::Light),
            Verdict::Reject(RejectReason::ContentLost { .. })
        ));
    }

    /// The Cloud path has no self-correction stage before the model, so the
    /// model resolving "X, no, Y" to "Y" is the edit Balanced and High ask
    /// for. That "no" is not a negation, and the reply, short as it is, says
    /// nothing the user did not.
    #[test]
    fn a_short_self_correction_is_accepted_at_balanced_and_high() {
        for level in [CleanupLevel::Balanced, CleanupLevel::High] {
            for (input, out) in [
                ("Tuesday, no, Wednesday.", "Wednesday."),
                ("Call Sam, no, call Alex.", "Call Alex."),
                ("Monday, no, sorry, Tuesday", "Tuesday."),
            ] {
                assert_eq!(
                    check(input, out, &reply(out, "stop"), level),
                    Verdict::Accept,
                    "{level:?}: {input:?} -> {out:?}"
                );
            }
            let out = "I'll send it.";
            assert!(matches!(
                check("send the report tonight", out, &reply(out, "stop"), level),
                Verdict::Reject(RejectReason::ContentLost { .. })
            ));
        }
    }

    /// U+FE0F asks for the colour form of the emoji before it. It is a mark,
    /// but it is not a word.
    #[test]
    fn a_variation_selector_does_not_make_an_emoji_a_word() {
        assert!(content_words("❤\u{FE0F}").is_empty());
        let answer = "How can I help?";
        assert!(matches!(
            check("❤\u{FE0F}", answer, &reply(answer, "stop"), CleanupLevel::Balanced),
            Verdict::Reject(RejectReason::LengthRatio { .. })
        ));
    }

    /// The nukta and every script's virama belong to the word they sit in,
    /// and two spellings of one word are one word.
    #[test]
    fn a_nukta_spelling_change_is_not_a_lost_word() {
        let precomposed = "\u{0958}लम और \u{095B}रा";
        let apart = "\u{0915}\u{093C}लम और \u{091C}\u{093C}रा";
        assert_eq!(
            check(precomposed, apart, &reply(apart, "stop"), CleanupLevel::Light),
            Verdict::Accept
        );
        // Bengali and Tamil viramas inside a word.
        assert_eq!(content_words("বন্ধু").len(), 1);
        assert_eq!(content_words("நன்றி").len(), 1);
    }
}
