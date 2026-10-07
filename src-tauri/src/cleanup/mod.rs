//! Text cleanup pipeline: raw ASR output → polished dictation.
//!
//! Fixed stage order (each stage is a plain function; all are pure except
//! punctuation, which wraps the ONNX punctuation/casing model):
//!
//! 1. spoken commands  — "new line"/"new paragraph" → sentinel chars
//! 2. filler removal   — "um", "uh", guarded "you know" / "i mean"
//! 3. backtrack        — "scratch that", "no wait" self-corrections
//!
//!    Stages 2 and 3 run in two passes: fillers by the original rules,
//!    then self-corrections; if one was corrected, that result stands,
//!    and if none was, the fillers are removed again with the guards
//!    that keep the user's own words (see `remove_fillers_and_corrections`).
//!
//! 4. punctuation      — model-based, skipped when the recognizer punctuates
//! 5. itn              — spoken numbers/times/dates → digits
//! 6. tidy             — sentinels → newlines, casing, whitespace
//! 7. corrections      — the user's "wrong → right" replacements
//! 8. snippets         — trigger phrases → saved expansions

pub mod backtrack;
pub mod commands;
pub mod fillers;
pub mod itn;
#[cfg(feature = "polish")]
pub mod polish;
pub mod punctuation;
pub mod snippets;
pub mod style;
pub mod tidy;

/// Private-use sentinels so later stages can treat command boundaries as
/// hard separators.
pub const NEWLINE: char = '\u{E000}';
pub const PARAGRAPH: char = '\u{E001}';

#[derive(Clone, Debug)]
pub struct CleanupSettings {
    pub spoken_commands: bool,
    pub fillers: bool,
    pub aggressive_fillers: bool,
    pub backtrack: bool,
    pub punctuation: bool,
    pub itn: bool,
    pub level: crate::format::level::CleanupLevel,
    /// Correction rules (wrong → right), matched case-insensitively
    /// (`snippets::apply_replacements`).
    pub replacements: Vec<(String, String)>,
    /// (trigger phrase, expansion) pairs, replaced case-insensitively.
    pub snippets: Vec<(String, String)>,
    /// Personal vocabulary passed to the polish model as preferred spellings.
    pub dictionary: Vec<String>,
    /// Sarvam chat model used for the cloud polish pass.
    pub polish_model: String,
    /// The user's override for `level`'s rules from the Prompts page, if they saved
    /// one (`settings::PromptOverrides`). `None` — the default — means the
    /// shipped rules; it is never `Some("")` for "no override", because the
    /// save-time guard stores absence as absence.
    ///
    /// The *rules half* only. The injection-hardening stanza is not stored
    /// here and is not the user's to change: `CleanupLevel::prompt_with_rules`
    /// re-appends it, which is what keeps `sarvam::chat::system_prompt`'s
    /// splice matching (`format::level::PromptParts`).
    pub prompt_rules: Option<String>,
}

impl Default for CleanupSettings {
    fn default() -> Self {
        Self {
            spoken_commands: true,
            fillers: true,
            aggressive_fillers: false,
            backtrack: true,
            punctuation: true,
            itn: true,
            level: crate::format::level::CleanupLevel::default(),
            replacements: Vec::new(),
            snippets: Vec::new(),
            dictionary: Vec::new(),
            polish_model: crate::settings::DEFAULT_POLISH_MODEL.into(),
            prompt_rules: None,
        }
    }
}

/// What the active ASR model already provides.
#[derive(Clone, Copy, Debug, Default)]
pub struct ModelCaps {
    pub native_punct: bool,
}

/// Returns the cleaned text plus how many dictionary/snippet rules fired.
pub fn run_pipeline(
    text: String,
    caps: ModelCaps,
    s: &CleanupSettings,
    punct: Option<&punctuation::Punctuator>,
) -> (String, u32) {
    let mut t = text;
    if s.spoken_commands {
        t = commands::apply(t);
    }
    t = remove_fillers_and_corrections(t, s);
    if s.punctuation && !caps.native_punct {
        if let Some(p) = punct {
            t = p.apply(t);
        }
    }
    if s.itn {
        t = itn::apply(t);
    }
    t = tidy::apply(t, s.punctuation || caps.native_punct);
    // Last, so tidy can't mangle expansion text (URLs, saved prose).
    // Corrections first, then snippets.
    let (t, hits_a) = snippets::apply_replacements(t, &s.replacements);
    let (t, hits_b) = snippets::apply(t, &s.snippets);
    (t, hits_a + hits_b)
}

/// Stages 2 and 3, in two passes. The first removes fillers by the original
/// rules (`fillers::apply_plain`) and resolves self-corrections on that: the
/// self-correction stage is built against that text, so when it corrects
/// anything, its result is kept and the dictation is cleaned as the
/// original rules cleaned it, apart from the hesitation-sound changes. When
/// it finds nothing to correct, the
/// first pass is set aside and the fillers are removed again from the same
/// text with the guards that keep the user's own words (`fillers::apply`):
/// nothing is left for a correction to undo, so the self-correction stage
/// does not run again.
fn remove_fillers_and_corrections(text: String, s: &CleanupSettings) -> String {
    if !s.backtrack {
        return if s.fillers {
            fillers::apply(text, s.aggressive_fillers)
        } else {
            text
        };
    }
    if !s.fillers {
        return backtrack::correct(text).0;
    }
    let plain = fillers::apply_plain(text.clone(), s.aggressive_fillers);
    match backtrack::correct(plain) {
        (corrected, true) => corrected,
        (_, false) => fillers::apply(text, s.aggressive_fillers),
    }
}

/// Cleanup for Sarvam cloud transcripts. Saaras `mode=transcribe` already
/// punctuates, cases, and normalizes numbers — and the text can be in any of
/// 23 languages, so the English-only regex stages (fillers, backtrack, ITN)
/// must not touch it. Four stages run:
///
/// - spoken commands: "new line"/"new paragraph" stay useful dictation
///   controls; the `(?i)` regex matches cloud casing and simply never fires
///   on non-Latin scripts.
/// - tidy: converts the command sentinels into real newlines, collapses
///   whitespace, writes a lone "i" as "I" and capitalises sentence starts.
///   Called with `punctuated=false` so it never appends an ASCII '.' after
///   text ending in a Devanagari danda (।) or other non-ASCII terminator.
/// - corrections and snippets, as on the local path.
pub fn run_cloud_pipeline(text: String, s: &CleanupSettings) -> (String, u32) {
    let t = if s.spoken_commands {
        commands::apply(text)
    } else {
        text
    };
    let t = tidy::apply(t, false);
    let (t, hits_a) = snippets::apply_replacements(t, &s.replacements);
    let (t, hits_b) = snippets::apply(t, &s.snippets);
    (t, hits_a + hits_b)
}

/// Word-level edit count between the raw transcript and the final text, for
/// the Insights "words corrected" stat: tokens are lowercased and stripped of
/// edge punctuation, then compared by longest-common-subsequence.
pub fn words_changed(before: &str, after: &str) -> u32 {
    fn toks(s: &str) -> Vec<String> {
        s.split_whitespace()
            .take(600) // bound the O(n·m) LCS for very long dictations
            .map(|w| {
                w.trim_matches(|c: char| !c.is_alphanumeric())
                    .to_lowercase()
            })
            .filter(|w| !w.is_empty())
            .collect()
    }
    let a = toks(before);
    let b = toks(after);
    if a.is_empty() && b.is_empty() {
        return 0;
    }
    let mut dp = vec![0u32; b.len() + 1];
    for item in &a {
        let mut prev = 0;
        for (j, other) in b.iter().enumerate() {
            let tmp = dp[j + 1];
            dp[j + 1] = if item == other {
                prev + 1
            } else {
                dp[j + 1].max(dp[j])
            };
            prev = tmp;
        }
    }
    let lcs = dp[b.len()];
    (a.len().max(b.len()) as u32).saturating_sub(lcs)
}

#[cfg(test)]
mod cloud_tests {
    use super::*;

    fn run(input: &str) -> String {
        run_cloud_pipeline(input.to_string(), &CleanupSettings::default()).0
    }

    #[test]
    fn words_changed_counts_edits() {
        assert_eq!(words_changed("hello world", "hello world"), 0);
        // One substitution.
        assert_eq!(words_changed("send it to sam", "send it to alex"), 1);
        // Two removals (fillers dropped).
        assert_eq!(words_changed("um so the meeting", "the meeting"), 2);
        // Punctuation/casing differences don't count as edits.
        assert_eq!(words_changed("hello world", "Hello, world!"), 0);
        assert_eq!(words_changed("", ""), 0);
    }

    #[test]
    fn hindi_passes_through_untouched() {
        assert_eq!(run("नमस्ते, आप कैसे हैं?"), "नमस्ते, आप कैसे हैं?");
        // Danda-terminated text must not gain an ASCII period.
        assert_eq!(run("मैं ठीक हूँ।"), "मैं ठीक हूँ।");
    }

    #[test]
    fn spoken_commands_match_cloud_casing() {
        // The command regex also swallows the punctuation Saaras adds around
        // the spoken command, same as the local pipeline's golden behavior.
        assert_eq!(
            run("First point. New line second point."),
            "First point\nsecond point."
        );
        assert_eq!(
            run("Summary. New paragraph details follow."),
            "Summary\n\ndetails follow."
        );
    }

    #[test]
    fn commands_toggle_off_leaves_text_alone() {
        let s = CleanupSettings {
            spoken_commands: false,
            ..CleanupSettings::default()
        };
        assert_eq!(
            run_cloud_pipeline("say new line literally".into(), &s).0,
            "Say new line literally"
        );
    }

    #[test]
    fn english_fillers_survive_cloud_mode() {
        // Filler/backtrack/ITN stages must NOT run on cloud text — that's
        // the polish pass's job (or the user's own words).
        assert_eq!(run("Um, so the price is twenty."), "Um, so the price is twenty.");
    }

    fn with_corrections(rules: &[(&str, &str)]) -> CleanupSettings {
        CleanupSettings {
            replacements: rules
                .iter()
                .map(|(from, to)| (from.to_string(), to.to_string()))
                .collect(),
            ..CleanupSettings::default()
        }
    }

    /// A correction replaces one word inside a sentence, so the period or
    /// comma after that word stays where it was.
    #[test]
    fn a_correction_keeps_the_punctuation_after_it() {
        let s = with_corrections(&[("Sidharth", "Siddharth")]);
        assert_eq!(
            run_cloud_pipeline("I spoke to Sidharth. He agreed.".into(), &s).0,
            "I spoke to Siddharth. He agreed."
        );
        assert_eq!(
            run_cloud_pipeline("Sidharth, can you call?".into(), &s).0,
            "Siddharth, can you call?"
        );
    }

    /// A rule in lower case takes the case of the word it replaces; a rule
    /// that spells its own case keeps it.
    #[test]
    fn a_correction_takes_the_case_of_the_word_it_replaces() {
        let s = with_corrections(&[("recieve", "receive"), ("jason", "JSON")]);
        assert_eq!(
            run_cloud_pipeline("Please recieve the jason file.".into(), &s).0,
            "Please receive the JSON file."
        );
        assert_eq!(run_cloud_pipeline("Recieve it now.".into(), &s).0, "Receive it now.");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(input: &str) -> String {
        // Golden tests exercise the rule-based stages; the punctuation model
        // (ONNX) is skipped by passing None.
        run_pipeline(
            input.to_string(),
            ModelCaps { native_punct: true },
            &CleanupSettings::default(),
            None,
        )
        .0
    }

    #[test]
    fn golden_corpus() {
        let cases: &[(&str, &str)] = &[
            // fillers
            ("um so the meeting is at noon", "So the meeting is at noon."),
            ("uh yeah uh let's do it", "Yeah let's do it."),
            ("it was um kind of hmm strange", "It was kind of strange."),
            ("do you know the answer", "Do you know the answer."),
            ("this is you know pretty good", "This is pretty good."),
            ("as you know the server is down", "As you know the server is down."),
            ("i mean we could try again", "We could try again."),
            ("that is what i mean to say", "That is what I mean to say."),
            ("you know, the server is down", "The server is down."),
            ("i mean, we could try again", "We could try again."),
            // fillers: a "you know" that opens a sentence and runs straight on
            // into a word is the user's own; with a pause or another filler
            // after it, it is a filler
            ("you know the answer", "You know the answer."),
            ("you know what i mean", "You know what I mean."),
            ("you know it works", "You know it works."),
            ("you know, it works.", "It works."),
            ("you know you know it works", "It works."),
            // fillers: an "i mean" whose one word ends the sentence is the
            // user's own; an ellipsis ends no sentence
            ("i mean it", "I mean it."),
            ("i mean that.", "I mean that."),
            ("yes, i mean it.", "Yes, I mean it."),
            ("yeah, i mean the... the thing", "Yeah, the... The thing."),
            // fillers: a "you know" that ends a sentence is the user's own,
            // unless a comma, ';' or ':' sets it off; the comma it leaves
            // before the sentence end goes too
            ("tell me what you know.", "Tell me what you know."),
            ("tell me what you know", "Tell me what you know."),
            ("it was fine, you know.", "It was fine."),
            ("it was fine, i mean you know.", "It was fine."),
            ("it was fine; you know. we left", "It was fine; we left."),
            // fillers: the sentence end on a dropped filler stays, on the last
            // word kept before it; a tag question leaves a statement, and an
            // abbreviation's dot serves
            ("it was fine, you know. we left", "It was fine. We left."),
            ("it was fine, you know?", "It was fine."),
            ("it's true, you know?", "It's true."),
            ("it was fine, you know!", "It was fine!"),
            ("we tried, i mean. it failed", "We tried. It failed."),
            ("they live in the U.S., you know. we left", "They live in the U.S. We left."),
            ("bring pens, paper, etc., you know. we left", "Bring pens, paper, etc. We left."),
            ("meet at 10 a.m., you know. then go", "Meet at 10 AM. Then go."),
            ("they live in the U.S., you know", "They live in the U.S."),
            ("bring pens, paper, etc., you know", "Bring pens, paper, etc."),
            ("meet at 10 a.m., you know", "Meet at 10 AM."),
            ("it works... you know... we left", "It works... We left."),
            // fillers: removing them never empties a sentence
            ("you know?", "You know?"),
            ("i mean.", "I mean."),
            ("i mean, you know.", "You know."),
            (
                "it works. you know, i mean. we left",
                "It works. You know, I mean. We left.",
            ),
            // fillers: a soft trigger with nothing to anchor to corrects
            // nothing, so the words the guards keep stay
            (
                "Tell me what you know. Oh no, it's late.",
                "Tell me what you know. Oh no, it's late.",
            ),
            (
                "You know the answer. Oh no, I forgot the keys.",
                "You know the answer. Oh no, I forgot the keys.",
            ),
            (
                "we waited, you know. there was no wait at all",
                "We waited. There was no wait at all.",
            ),
            // self-corrections: a dictation with one in it is cleaned as the
            // original filler rules cleaned it, guards or no
            ("it was red. you know no wait it was blue.", "It was blue."),
            ("it was red. no wait. you know it was blue.", "It was blue."),
            ("the price is 20. you know i mean no wait 25", "The price is 25."),
            (
                "we meet on monday. you know the office is near the station no wait on tuesday",
                "We meet on tuesday.",
            ),
            ("the price is 20, no wait, i mean 25.", "The price is 25."),
            ("the price is 20 you know. no wait 25", "The price is 25."),
            (
                "tell them the plan, you know. scratch that ask them",
                "Ask them.",
            ),
            ("the price is 20. you know. scratch that 25", "The price is 25."),
            ("the price is 20. i mean. no wait 25", "The price is 25."),
            ("the price is twenty. you know. no wait twenty five", "The price is 25."),
            ("we said 20. you know. i mean. no wait 25", "We said 25."),
            (
                "let's meet on monday at the office near the station. you know. no wait on tuesday",
                "Let's meet on tuesday.",
            ),
            // self-corrections: a trigger split by a filler is still a trigger
            ("the price is 20. you know. actually, you know, no, 25", "The price is 25."),
            ("the price is 20. you know. no, you know, wait, 25", "The price is 25."),
            ("the price is 20. you know. no you know wait 25", "The price is 25."),
            ("the price is 20. you know. oh, i mean, no, 25", "The price is 25."),
            ("the price is 20. you know. scratch, you know, that, 25", "The price is 25."),
            ("The price is 20. You know. No, I mean, wait, 25.", "The price is 25."),
            ("the price is 20 you know. actually, you know, no, 25", "The price is 25."),
            ("the price is 20. i mean it. no, you know, wait, 25", "The price is 25."),
            (
                "tell them the plan. you know. scratch you know that ask them",
                "Tell them the plan. Ask them.",
            ),
            (
                "tell them the plan, you know. scratch, you know, that ask them",
                "Ask them.",
            ),
            (
                "it was red. you know it was. no, you know, wait, it was blue",
                "It was red. It was blue.",
            ),
            (
                "we meet on monday. you know the office is near the station no, you know, wait, on tuesday",
                "We meet on tuesday.",
            ),
            // self-corrections: an anchor behind fillers, a trigger eight
            // words on, and a trigger an earlier correction brings closer
            (
                "let's meet on monday, you know, at noon, i mean, with the team. you know. no wait on tuesday",
                "Let's meet on tuesday.",
            ),
            (
                "we meet on monday, you know, at noon, i mean, at the office. i mean it. no wait on tuesday",
                "We meet on tuesday.",
            ),
            (
                "you know the answer. one two three four five six scratch that the answer is no",
                "The answer is no.",
            ),
            (
                "you know the answer. one two three four five six no wait the answer is no",
                "The answer is no.",
            ),
            (
                "i mean it. one two three four five six seven no wait it is fine",
                "It is fine.",
            ),
            (
                "on monday z y q r. you know. a b c d e f g h no wait a x no wait on tuesday",
                "On tuesday.",
            ),
            (
                "let's meet on monday at noon. you know we can. you know. i mean. you know. no wait on tuesday",
                "Let's meet on tuesday.",
            ),
            // fillers: "er" on its own, and "errr", are sounds; "err", "ER"
            // and "mm" are words
            ("er i think so", "I think so."),
            ("errr let me see", "Let me see."),
            ("to err is human", "To err is human."),
            ("rushed to the ER last night", "Rushed to the ER last night."),
            ("it is 5 mm wide", "It is 5 mm wide."),
            // backtrack: soft trigger with anchor
            (
                "let's meet on monday no wait on tuesday",
                "Let's meet on tuesday.",
            ),
            (
                "the deadline is on friday oh no on thursday",
                "The deadline is on thursday.",
            ),
            ("send it to sam no i meant to alex", "Send it to alex."),
            // backtrack: numeric anchor
            ("the price is 20 no wait 25", "The price is 25."),
            // backtrack: hard trigger
            (
                "tell them the plan scratch that ask them for the plan",
                "Ask them for the plan.",
            ),
            // backtrack: the filler stage runs first, so a "you know" or "i
            // mean" around a trigger is gone before the trigger looks for
            // its anchor
            ("the price is 20, no wait, i mean, 25.", "The price is 25."),
            (
                "let's meet on monday, no wait, you know, on tuesday.",
                "Let's meet on tuesday.",
            ),
            (
                "let's meet on monday no wait you know on tuesday",
                "Let's meet on tuesday.",
            ),
            ("the price is 20, you know, scratch that, 25.", "The price is 25."),
            ("the price is 20 you know no wait 25", "The price is 25."),
            (
                "send it to sam you know and the whole team by noon no wait to alex",
                "Send it to alex.",
            ),
            ("i love this scratch that i mean it", "I mean it."),
            // soft trigger without anchor: not a correction, every word kept
            ("oh no i forgot the keys", "Oh no I forgot the keys."),
            ("there is no wait at the counter", "There is no wait at the counter."),
            (
                "i said we should leave at noon, no wait, i mean, tomorrow.",
                "I said we should leave at noon, no wait, tomorrow.",
            ),
            // backtrack: ordinary phrases that are not corrections
            ("i meant to call you yesterday", "I meant to call you yesterday."),
            ("that's not what i meant", "That's not what I meant."),
            (
                "i have one correction for the report",
                "I have one correction for the report.",
            ),
            // spoken commands
            (
                "first point new line second point",
                "First point\nsecond point.",
            ),
            (
                "summary new paragraph details follow",
                "Summary\n\ndetails follow.",
            ),
            // itn: numbers
            ("we sold twenty five units", "We sold 25 units."),
            ("about one hundred and three people came", "About 103 people came."),
            ("i have three cats", "I have three cats."),
            ("give me fifteen percent", "Give me 15%."),
            ("that costs fifty dollars", "That costs $50."),
            ("send five hundred rupees", "Send ₹500."),
            // itn: times
            ("the meeting is at three thirty pm", "The meeting is at 3:30 PM."),
            ("wake me at ten am", "Wake me at 10 AM."),
            // itn: years
            ("back in twenty twenty six", "Back in 2026."),
            ("it happened in nineteen ninety nine", "It happened in 1999."),
            // itn: dates
            ("due on january fifth", "Due on January 5th."),
            ("born on march twenty second", "Born on March 22nd."),
            // combined
            (
                "um so the meeting is at three thirty pm scratch that four pm new line thanks",
                "So the meeting is at 4 PM\nthanks.",
            ),
            // casing
            ("i think i'm ready", "I think I'm ready."),
        ];
        let mut failures = Vec::new();
        for (input, expected) in cases {
            let got = run(input);
            if got != *expected {
                failures.push(format!("input: {input:?}\n  expected: {expected:?}\n  got:      {got:?}"));
            }
        }
        assert!(
            failures.is_empty(),
            "{} golden case(s) failed:\n{}",
            failures.len(),
            failures.join("\n")
        );
    }

    /// In aggressive mode "like" goes too, so it can split a trigger as
    /// "you know" can.
    #[test]
    fn golden_aggressive() {
        let s = CleanupSettings {
            aggressive_fillers: true,
            ..CleanupSettings::default()
        };
        let cases: &[(&str, &str)] = &[
            ("the price is 20. you know. no like wait 25", "The price is 25."),
            ("the price is 20. like. no like wait 25", "The price is 25."),
            ("it was fine, like. we left", "It was fine. We left."),
        ];
        for (input, expected) in cases {
            let got = run_pipeline(input.to_string(), ModelCaps { native_punct: true }, &s, None).0;
            assert_eq!(got, *expected, "{input:?}");
        }
    }
}
