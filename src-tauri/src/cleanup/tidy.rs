//! Final tidy: sentinels → newlines, whitespace, casing, terminal punctuation.

use regex::{Captures, Regex};
use std::sync::OnceLock;

pub fn apply(text: String, punctuated: bool) -> String {
    static PARA: OnceLock<Regex> = OnceLock::new();
    static NL: OnceLock<Regex> = OnceLock::new();
    static SPACES: OnceLock<Regex> = OnceLock::new();
    static SPACE_PUNCT: OnceLock<Regex> = OnceLock::new();
    static LONE_I: OnceLock<Regex> = OnceLock::new();
    static I_CONTRACT: OnceLock<Regex> = OnceLock::new();

    let mut t = PARA
        .get_or_init(|| Regex::new(r"\s*\x{E001}\s*").unwrap())
        .replace_all(&text, "\n\n")
        .into_owned();
    t = NL
        .get_or_init(|| Regex::new(r"\s*\x{E000}\s*").unwrap())
        .replace_all(&t, "\n")
        .into_owned();
    t = SPACES
        .get_or_init(|| Regex::new(r"[ \t]{2,}").unwrap())
        .replace_all(&t, " ")
        .into_owned();
    t = SPACE_PUNCT
        .get_or_init(|| Regex::new(r" +([.,!?;:])").unwrap())
        .replace_all(&t, "$1")
        .into_owned();
    t = LONE_I
        .get_or_init(|| Regex::new(r"\bi\b").unwrap())
        .replace_all(&t, "I")
        .into_owned();
    t = I_CONTRACT
        .get_or_init(|| Regex::new(r"\bi'(m|ll|ve|d)\b").unwrap())
        .replace_all(&t, "I'$1")
        .into_owned();

    let mut t = capitalise_sentences(t.trim());

    // Terminal punctuation for sentence-like output.
    if punctuated {
        let words = t.split_whitespace().count();
        if words >= 2 && !t.ends_with(['.', '!', '?', ':', '\n']) {
            t.push('.');
        }
    }

    // A comma right before a sentence end goes: "It was fine,." once the
    // filler after the comma is gone. When the word before the comma already
    // ends with a '.', that dot ends the sentence and a second '.' goes too:
    // "the U.S.,." becomes "the U.S.". Only a comma touching a '.', '!' or
    // '?' that ends the sentence, so "1,000." and "wait,..." stay as they are.
    static COMMA_END: OnceLock<Regex> = OnceLock::new();
    COMMA_END
        .get_or_init(|| Regex::new(r"(\.?),([.!?])(\s|$)").unwrap())
        .replace_all(&t, |caps: &Captures| {
            let mark = if &caps[1] == "." && &caps[2] == "." { "" } else { &caps[2] };
            format!("{}{mark}{}", &caps[1], &caps[3])
        })
        .into_owned()
}

/// Capitalise the first letter, and the first letter after a sentence end: a
/// '.', '!' or '?' with whitespace after it. A dot inside a token
/// ("asha@example.com", "a.m.") ends nothing, and neither does the last dot
/// of a dotted abbreviation ("a.m. tomorrow", "e.g. the readme").
fn capitalise_sentences(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut capitalize_next = true;
    let mut token_start = 0;
    for (i, &c) in chars.iter().enumerate() {
        if c.is_whitespace() {
            token_start = i + 1;
            out.push(c);
            continue;
        }
        if capitalize_next && c.is_alphabetic() {
            out.push(c.to_ascii_uppercase());
            capitalize_next = false;
            continue;
        }
        out.push(c);
        capitalize_next = matches!(c, '.' | '!' | '?')
            && chars.get(i + 1).is_some_and(|n| n.is_whitespace())
            && !(c == '.' && dotted_abbreviation(&chars[token_start..=i]));
    }
    out
}

/// Single letters each followed by a dot, two or more of them: "a.m.",
/// "e.g.", "U.S.".
fn dotted_abbreviation(token: &[char]) -> bool {
    token.len() >= 4
        && token.len() % 2 == 0
        && token
            .chunks(2)
            .all(|pair| pair[0].is_alphabetic() && pair[1] == '.')
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A dot inside a token ends no sentence, and neither does the last dot
    /// of a dotted abbreviation.
    #[test]
    fn a_dot_inside_a_word_is_not_a_sentence_end() {
        assert_eq!(
            apply("write to asha@example.com today".into(), true),
            "Write to asha@example.com today."
        );
        assert_eq!(apply("meet at 10 a.m. tomorrow".into(), true), "Meet at 10 a.m. tomorrow.");
        assert_eq!(
            apply("see the docs, e.g. the readme".into(), true),
            "See the docs, e.g. the readme."
        );
    }

    #[test]
    fn a_sentence_end_followed_by_a_space_still_capitalises() {
        assert_eq!(
            apply("done. next one! and then? yes".into(), true),
            "Done. Next one! And then? Yes."
        );
        assert_eq!(apply("it grew 3.5. then it fell".into(), true), "It grew 3.5. Then it fell.");
        assert_eq!(apply("see example.com. then call".into(), true), "See example.com. Then call.");
    }

    /// A comma left right before a sentence end goes; a comma anywhere else
    /// stays.
    #[test]
    fn a_comma_before_a_sentence_end_goes() {
        assert_eq!(apply("it was fine,".into(), true), "It was fine.");
        assert_eq!(apply("it was fine,. we left".into(), true), "It was fine. We left.");
        assert_eq!(apply("are you sure,? yes".into(), true), "Are you sure? Yes.");
        assert_eq!(apply("it was fine,.".into(), false), "It was fine.");
        // Numbers, an ellipsis and a comma inside a token are not touched.
        assert_eq!(apply("it cost 1,000.".into(), true), "It cost 1,000.");
        assert_eq!(apply("it cost 1,000,000. then".into(), true), "It cost 1,000,000. Then.");
        assert_eq!(apply("wait,... what".into(), true), "Wait,... What.");
        assert_eq!(apply("see a,b.c for it".into(), true), "See a,b.c for it.");
    }

    /// A word that already ends with a '.' ends the sentence itself, so the
    /// comma after it goes and so does a second '.'; any other mark stays.
    #[test]
    fn an_abbreviation_before_the_comma_keeps_one_dot() {
        assert_eq!(apply("they live in the U.S.,".into(), true), "They live in the U.S.");
        assert_eq!(apply("bring pens, paper, etc.,".into(), true), "Bring pens, paper, etc.");
        assert_eq!(apply("meet at 10 a.m.,".into(), true), "Meet at 10 a.m.");
        assert_eq!(
            apply("they live in the U.S.,. we left".into(), true),
            "They live in the U.S. We left."
        );
        assert_eq!(apply("they live in the U.S.,!".into(), true), "They live in the U.S.!");
        // The cloud path, which adds no period of its own.
        assert_eq!(apply("They live in the U.S.,.".into(), false), "They live in the U.S.");
        assert_eq!(apply("Bring pens, etc.,. Then go.".into(), false), "Bring pens, etc. Then go.");
        // Numbers and an ellipsis are still not touched.
        assert_eq!(apply("it cost 1,000.".into(), true), "It cost 1,000.");
        assert_eq!(apply("wait,... what".into(), true), "Wait,... What.");
    }
}
