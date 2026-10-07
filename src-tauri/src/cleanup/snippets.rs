//! Voice snippets: say a trigger phrase, the saved expansion gets typed
//! instead. Triggers match case-insensitively on word boundaries, so
//! "my linkedin" fires inside "send them my LinkedIn please" too. A period or
//! comma the recognizer stuck onto the trigger is absorbed, because the
//! expansion brings its own punctuation.
//!
//! Also the correction path: `cleanup::run_pipeline` runs the user's
//! `replacements` through [`apply_replacements`] before their snippets. A
//! correction matches the same way but replaces one word with another inside
//! a sentence, so it keeps the punctuation after the word, and a rule in
//! lower case takes the capital of a word it replaces at a sentence start
//! (see [`match_case`]).
//!
//! A trigger matches any canonically-equivalent spelling of itself
//! (`crate::canonical`), because the rule and the transcript come from
//! different places and are under no obligation to spell a word the same way.
//! The equivalence is handled by widening the *pattern*, never by normalizing
//! the transcript: the text a rule does not match is copied through
//! byte-for-byte, so a dictation can never come back silently re-spelled.

use regex::{Captures, NoExpand, Regex};

/// Expand the user's voice snippets. Returns the rewritten text and how many
/// snippets fired (for fix stats).
pub fn apply(text: String, snippets: &[(String, String)]) -> (String, u32) {
    let mut t = text;
    let mut hits = 0u32;
    for (trigger, expansion) in snippets {
        let Some(re) = matcher(trigger, r"[.,]?") else {
            continue;
        };
        let n = re.find_iter(&t).count() as u32;
        if n == 0 {
            continue;
        }
        hits += n;
        // NoExpand: expansions are literal text — a "$" in a saved snippet
        // must never be treated as a capture-group reference.
        t = re.replace_all(&t, NoExpand(expansion.as_str())).into_owned();
    }
    (t, hits)
}

/// Apply the user's corrections (wrong → right). Returns the rewritten text
/// and how many corrections fired.
///
/// Unlike a snippet, a correction leaves the punctuation after the word
/// alone: "Sidharth." becomes "Siddharth.", not "Siddharth".
pub fn apply_replacements(text: String, rules: &[(String, String)]) -> (String, u32) {
    let mut t = text;
    let mut hits = 0u32;
    for (from, to) in rules {
        let Some(re) = matcher(from, "") else {
            continue;
        };
        let n = re.find_iter(&t).count() as u32;
        if n == 0 {
            continue;
        }
        hits += n;
        // A closure rather than a template, so a "$" in the correction is
        // literal text here too.
        t = re
            .replace_all(&t, |caps: &Captures| match_case(&caps[0], from.trim(), to))
            .into_owned();
    }
    (t, hits)
}

/// The regex for one trigger, or `None` for a blank trigger.
fn matcher(trigger: &str, trailing: &str) -> Option<Regex> {
    let trigger = trigger.trim();
    if trigger.is_empty() {
        return None;
    }
    Regex::new(&pattern_for(trigger, trailing)).ok()
}

/// The whole trigger, with flexible whitespace between its words, followed
/// by `trailing` (a snippet's absorbed punctuation, or nothing).
fn pattern_for(trigger: &str, trailing: &str) -> String {
    let words: Vec<String> = trigger.split_whitespace().map(word_pattern).collect();
    format!(r"(?i)\b{}\b{trailing}", words.join(r"\s+"))
}

/// `to`, cased for the place it lands in.
///
/// A rule in plain lower case on both sides says nothing about the word's
/// case, so it takes a capital where the word it replaces had one, at the
/// start of a sentence: "recieve → receive" writes "Receive" there. Any
/// capital in a rule is part of the fix ("Sidharth → Siddharth", "jason →
/// JSON", "iphone → iPhone"), and the rule is written exactly as stored,
/// wherever it lands. Text without case, such as Devanagari, is untouched.
fn match_case(matched: &str, from: &str, to: &str) -> String {
    if !all_lower(from) || !all_lower(to) {
        return to.to_string();
    }
    match initial_case(matched) {
        Some(true) => with_initial(to, char::to_uppercase),
        _ => to.to_string(),
    }
}

/// Whether the first cased letter of `word` is upper case, or `None` when it
/// has no cased letter.
fn initial_case(word: &str) -> Option<bool> {
    word.chars()
        .find(|c| c.is_uppercase() || c.is_lowercase())
        .map(char::is_uppercase)
}

/// Whether `word` has a lower case letter and no upper case one.
fn all_lower(word: &str) -> bool {
    word.chars().any(char::is_lowercase) && !word.chars().any(char::is_uppercase)
}

/// `word` with its first cased letter mapped through `case`.
fn with_initial<I: Iterator<Item = char>>(word: &str, case: fn(char) -> I) -> String {
    let mut out = String::with_capacity(word.len());
    let mut done = false;
    for c in word.chars() {
        if !done && (c.is_uppercase() || c.is_lowercase()) {
            out.extend(case(c));
            done = true;
        } else {
            out.push(c);
        }
    }
    out
}

/// One trigger word, widened to every spelling it could be written in.
///
/// ASCII returns `regex::escape(word)` and nothing else, so an English pattern
/// is the plain escaped word and matches exactly what it spells. Only a
/// non-ASCII word pays for the alternation, and only a word containing a
/// composition-exclusion letter gets more than the two forms NFC and NFD
/// already provide.
///
/// Alternatives are ordered longest-first: the regex crate's alternation is
/// leftmost-*first*, so a shorter spelling listed ahead of a longer one could
/// win a prefix of it.
fn word_pattern(word: &str) -> String {
    if word.is_ascii() {
        return regex::escape(word);
    }
    let forms = crate::canonical::spellings(word);
    if forms.len() == 1 {
        return regex::escape(&forms[0]);
    }
    let mut alts: Vec<String> = forms.iter().map(|f| regex::escape(f)).collect();
    alts.sort_by_key(|a| std::cmp::Reverse(a.len()));
    format!("(?:{})", alts.join("|"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snips() -> Vec<(String, String)> {
        vec![
            ("my linkedin".into(), "https://example.com/in/asha".into()),
            ("intro email".into(), "Hey, would love to chat!".into()),
        ]
    }

    #[test]
    fn whole_utterance_trigger() {
        let (out, hits) = apply("My LinkedIn.".into(), &snips());
        assert_eq!(out, "https://example.com/in/asha");
        assert_eq!(hits, 1);
    }

    #[test]
    fn embedded_trigger() {
        let (out, hits) = apply("Send them my linkedin please.".into(), &snips());
        assert_eq!(out, "Send them https://example.com/in/asha please.");
        assert_eq!(hits, 1);
    }

    #[test]
    fn no_partial_word_match() {
        let (out, hits) = apply("The introduction emails went out.".into(), &snips());
        assert_eq!(out, "The introduction emails went out.");
        assert_eq!(hits, 0);
    }

    #[test]
    fn empty_trigger_ignored() {
        let (out, hits) = apply("hello".into(), &[("  ".into(), "x".into())]);
        assert_eq!(out, "hello");
        assert_eq!(hits, 0);
    }

    #[test]
    fn dollar_signs_in_expansion_stay_literal() {
        let (out, _) = apply(
            "the rate".into(),
            &[("the rate".into(), "$50/hour ($1 discount)".into())],
        );
        assert_eq!(out, "$50/hour ($1 discount)");
    }

    // -----------------------------------------------------------------
    // Canonical equivalence. A rule and a transcript can spell the same
    // word with different codepoints: the field monitor NFC-normalizes
    // what it reads, and for the Devanagari nukta letters NFC is the
    // DECOMPOSED spelling, while a Sarvam transcript is typically
    // precomposed. Matched scalar-for-scalar, a learned Devanagari rule
    // never fires at all. See `crate::canonical`.
    // -----------------------------------------------------------------

    /// `क़लम` (pen) spelled apart, as a learned rule is stored.
    const PEN_APART: &str = "\u{0915}\u{093C}लम";
    /// The same word spelled with the precomposed letter.
    const PEN_TOGETHER: &str = "\u{0958}लम";

    /// The common direction: the rule is stored the way the monitor produces
    /// it (apart) and the transcript arrives precomposed.
    #[test]
    fn a_rule_stored_decomposed_fires_on_a_precomposed_transcript() {
        assert_ne!(PEN_APART, PEN_TOGETHER, "the two spellings really differ");
        let rules = vec![(PEN_APART.into(), "pen".into())];
        let (out, hits) = apply_replacements(format!("मुझे {PEN_TOGETHER} चाहिए"), &rules);
        assert_eq!(hits, 1, "the learned rule has to fire");
        assert_eq!(out, "मुझे pen चाहिए");
    }

    /// And the reverse: a rule typed with the precomposed letter, against a
    /// transcript that spells it out.
    #[test]
    fn a_rule_stored_precomposed_fires_on_a_decomposed_transcript() {
        let rules = vec![(PEN_TOGETHER.into(), "pen".into())];
        let (out, hits) = apply_replacements(format!("मुझे {PEN_APART} चाहिए"), &rules);
        assert_eq!(hits, 1);
        assert_eq!(out, "मुझे pen चाहिए");
    }

    /// The paste must not become a stealth normalizer: everything the rule
    /// did not replace comes out byte-for-byte as it went in, in whatever
    /// form it arrived.
    #[test]
    fn text_the_rule_does_not_replace_keeps_its_own_composition() {
        // Two nukta words; only the second is a rule.
        let untouched = "\u{0958}लम"; // precomposed, no rule for it
        let rules = vec![("\u{091C}\u{093C}रा".to_string(), "zara".to_string())];
        let input = format!("{untouched} और \u{095B}रा");
        let (out, hits) = apply_replacements(input, &rules);
        assert_eq!(hits, 1);
        assert_eq!(
            out,
            format!("{untouched} और zara"),
            "the untouched word must keep U+0958, not be normalized apart"
        );
        assert!(out.contains('\u{0958}'), "composition was silently changed");
    }

    /// Latin accents come free with NFC, but pin them: `é` as one codepoint
    /// against `e` + acute in the transcript.
    #[test]
    fn an_accented_rule_fires_across_both_of_its_forms() {
        let rules = vec![("café".to_string(), "coffee house".to_string())];
        let (out, hits) = apply_replacements("meet at the cafe\u{0301} later".into(), &rules);
        assert_eq!(hits, 1);
        assert_eq!(out, "meet at the coffee house later");
    }

    /// A nukta rule's pattern lists both spellings and stops at the word's
    /// edge, so it cannot fire inside a longer word.
    #[test]
    fn a_nukta_rule_does_not_fire_inside_a_longer_word() {
        let rules = vec![(PEN_APART.to_string(), "pen".into())];
        let (out, hits) = apply_replacements(format!("{PEN_TOGETHER}दान यहाँ"), &rules);
        assert_eq!(hits, 0, "matched inside a longer word");
        assert_eq!(out, format!("{PEN_TOGETHER}दान यहाँ"));
    }

    /// The English guarantee, at this seam: an ASCII rule builds exactly its
    /// own escaped text, with no alternation.
    #[test]
    fn an_ascii_trigger_builds_its_own_escaped_pattern() {
        assert_eq!(word_pattern("linkedin"), regex::escape("linkedin"));
        assert_eq!(word_pattern("don't"), regex::escape("don't"));
        assert_eq!(pattern_for("my linkedin", r"[.,]?"), r"(?i)\bmy\s+linkedin\b[.,]?");
        assert_eq!(pattern_for("my linkedin", ""), r"(?i)\bmy\s+linkedin\b");
    }

    // -----------------------------------------------------------------
    // Corrections.
    // -----------------------------------------------------------------

    fn rule(from: &str, to: &str) -> Vec<(String, String)> {
        vec![(from.to_string(), to.to_string())]
    }

    /// A snippet swallows the punctuation after its trigger; a correction
    /// must not, or every fixed word loses the period or comma after it.
    #[test]
    fn a_correction_leaves_the_punctuation_after_the_word() {
        let rules = rule("Sidharth", "Siddharth");
        let (out, hits) = apply_replacements("I spoke to Sidharth. He agreed.".into(), &rules);
        assert_eq!(out, "I spoke to Siddharth. He agreed.");
        assert_eq!(hits, 1);
        let (out, _) = apply_replacements("Sidharth, call me".into(), &rules);
        assert_eq!(out, "Siddharth, call me");
    }

    #[test]
    fn dollar_signs_in_a_correction_stay_literal() {
        let (out, _) = apply_replacements("the price".into(), &rule("price", "$5 fee"));
        assert_eq!(out, "the $5 fee");
    }

    /// A rule in plain lower case says nothing about the word's case, so it
    /// takes the case of the word it replaces; any capital in a rule is part
    /// of the fix.
    #[test]
    fn a_correction_takes_the_case_its_rule_leaves_open() {
        let case = |text: &str, from: &str, to: &str| {
            apply_replacements(text.to_string(), &rule(from, to)).0
        };
        assert_eq!(case("Recieve it", "recieve", "receive"), "Receive it");
        assert_eq!(case("please recieve it", "recieve", "receive"), "please receive it");
        assert_eq!(case("Btw it works", "btw", "by the way"), "By the way it works");
        // A name keeps its capital wherever it lands.
        assert_eq!(case("meet sidharth", "Sidharth", "Siddharth"), "meet Siddharth");
        // Casing that differs between the two sides is the user's own.
        assert_eq!(case("meet sidharth", "sidharth", "Siddharth"), "meet Siddharth");
        assert_eq!(case("Jason file", "jason", "JSON"), "JSON file");
        assert_eq!(case("my i phone", "i phone", "iPhone"), "my iPhone");
        // No case to take: Devanagari, and a digit-led replacement.
        assert_eq!(case("मुझे कलम चाहिए", "कलम", "क़लम"), "मुझे क़लम चाहिए");
        assert_eq!(case("Twenty people", "twenty", "20"), "20 people");
    }
}
