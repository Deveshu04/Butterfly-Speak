//! Filler removal, in two forms that the pipeline (`super::run_pipeline`)
//! chooses between.
//!
//! [`apply_plain`] runs the original rules, before the self-correction stage
//! (`backtrack`): hesitation sounds go, and "you know" and "i mean" go unless
//! a guard word comes before ("as you know") or, for "i mean", it does not
//! open a clause. A "you know" or "i mean" around a trigger is gone by the
//! time the trigger looks for its anchor, so "the price is 20, no wait, i
//! mean, 25" resolves to "the price is 25". When the self-correction stage
//! corrects anything in that text, the pipeline keeps it: a dictation with a
//! self-correction in it is cleaned as the original rules cleaned it, apart
//! from the hesitation-sound changes below.
//!
//! [`apply`] adds the guards that keep the user's own words, and the
//! pipeline uses it when the self-correction stage found nothing to correct:
//!
//! - "you know" stays when it opens a sentence and runs straight on into a
//!   word ("You know the answer."), unless another "you know" or "i mean"
//!   comes right before or after it, and when it ends a sentence with no
//!   comma, ';' or ':' on the word before it ("Tell me what you know.");
//! - "i mean" stays when its one word ends the sentence ("I mean it.");
//! - removing fillers never empties a sentence: when every word of one would
//!   go ("You know, I mean."), they all stay;
//! - the sentence end on a dropped filler moves to the word kept before it
//!   ("It was fine, you know. We left" becomes "It was fine. We left").
//!
//! Both forms treat sounds alike: "um", "uh", "er", "erm" and "errr" go;
//! "err" (the verb), "mm" and anything in capitals ("ER") stay. In
//! aggressive mode "like" goes too, unless the word before makes it a verb
//! or a comparison.

use regex::{Captures, Regex};
use std::borrow::Cow;
use std::sync::OnceLock;

/// Tokens after which "you know" is meaningful, not filler.
const YOU_KNOW_GUARDS: &[&str] = &[
    "do", "did", "don't", "didn't", "would", "wouldn't", "you'd", "as", "to", "should",
    "shouldn't", "could", "couldn't", "can", "can't", "cannot", "if", "let",
];

/// Tokens after which "like" is a verb/comparison, not filler.
const LIKE_GUARDS: &[&str] = &[
    "is", "was", "be", "been", "feels", "looks", "sounds", "seems", "felt", "looked",
    "sounded", "seemed", "something", "things", "stuff", "much", "really", "would",
    "i'd", "you'd", "we'd", "they'd", "i", "you", "we", "they",
];

fn norm(token: &str) -> String {
    token
        .trim_matches(|c: char| c.is_ascii_punctuation())
        .to_lowercase()
}

fn is_sentinel(token: &str) -> bool {
    token.ends_with(super::NEWLINE) || token.ends_with(super::PARAGRAPH)
}

/// A token that is a word: it starts with a letter or a digit.
fn is_word(token: &str) -> bool {
    token.chars().next().is_some_and(char::is_alphanumeric)
}

/// A phrase's last token with nothing after the word itself: no comma, dash,
/// ellipsis or sentence end, so no pause follows it.
fn no_pause_after(token: &str) -> bool {
    token.chars().last().is_some_and(char::is_alphanumeric)
}

/// Whether the two tokens say "you know" or "i mean".
fn is_phrase(first: &str, second: &str) -> bool {
    let (first, second) = (norm(first), norm(second));
    (first == "you" && second == "know") || (first == "i" && second == "mean")
}

/// The '.', '?' or '!' a token ends a sentence with (or a run of them,
/// "?!"), or `None` when it ends none. An ellipsis is a pause, not a
/// sentence end.
fn sentence_end(token: &str) -> Option<&str> {
    let mark = &token[token.trim_end_matches(['.', '?', '!']).len()..];
    (!mark.is_empty() && !mark.starts_with("..")).then_some(mark)
}

/// "You know the answer" and "You know what I mean" are sentences, not
/// fillers: a "you know" that opens a sentence and runs straight on into a
/// word stays. With a pause after it ("You know, it works") or another "you
/// know" or "i mean" right before or after it (a run of fillers), it is a
/// filler.
fn you_know_is_own_words(out: &[Cow<'_, str>], tokens: &[&str], i: usize) -> bool {
    let opens_sentence = out
        .last()
        .is_none_or(|t| sentence_end(t).is_some() || is_sentinel(t));
    let run = (i >= 2 && is_phrase(tokens[i - 2], tokens[i - 1]))
        || matches!(
            (tokens.get(i + 2), tokens.get(i + 3)),
            (Some(a), Some(b)) if is_phrase(a, b)
        );
    opens_sentence
        && no_pause_after(tokens[i + 1])
        && tokens.get(i + 2).is_some_and(|t| is_word(t))
        && !run
}

/// "Tell me what you know." is a sentence too: a "you know" that ends a
/// sentence (a '.', '?' or '!' right after it, or the end of the line)
/// stays, unless the word kept before it sets it off with a comma, ';' or
/// ':' ("It was fine, you know.").
fn you_know_ends_own_sentence(out: &[Cow<'_, str>], tokens: &[&str], i: usize) -> bool {
    let know = tokens[i + 1];
    let ends_sentence = if know.chars().all(char::is_alphabetic) {
        tokens.get(i + 2).is_none_or(|t| is_sentinel(t))
    } else {
        sentence_end(know).is_some()
    };
    let set_off = out.last().is_some_and(|t| t.ends_with([',', ';', ':']));
    ends_sentence && !set_off
}

/// "I mean it." / "I mean that.": an "i mean" that runs straight on into one
/// word and the sentence ends there stays.
fn i_mean_is_own_words(tokens: &[&str], i: usize) -> bool {
    let Some(word) = tokens.get(i + 2).copied() else {
        return false;
    };
    let ends_sentence =
        sentence_end(word).is_some() || tokens.get(i + 3).is_none_or(|t| is_sentinel(t));
    no_pause_after(tokens[i + 1]) && is_word(word) && ends_sentence
}

/// Filler removal with the guards that keep the user's own words (see the
/// module docs). For text the self-correction stage finds nothing to
/// correct in.
pub fn apply(text: String, aggressive: bool) -> String {
    remove(text, aggressive, true)
}

/// Filler removal by the original rules alone, which the self-correction
/// stage is built to run after.
pub fn apply_plain(text: String, aggressive: bool) -> String {
    remove(text, aggressive, false)
}

fn remove(text: String, aggressive: bool, guards: bool) -> String {
    // Stage 1: standalone hesitation sounds via regex: "er" and "errr" go,
    // "err" (a verb) and "mm" ("5 mm") stay, and so does any all-capitals
    // token: "ER" or "UM" in capitals is an abbreviation, not a sound.
    static SOUNDS: OnceLock<Regex> = OnceLock::new();
    let re = SOUNDS.get_or_init(|| {
        Regex::new(r"(?i)\b(?:u+m+|u+h+|uhm+|er+m|er{3,}|er|a+h+|h+m+|mhm+|m{3,})\b[.,]?\s?")
            .unwrap()
    });
    let t = re
        .replace_all(&text, |caps: &Captures| {
            let found = &caps[0];
            let letters: Vec<char> = found.chars().filter(|c| c.is_alphabetic()).collect();
            if letters.iter().all(|c| c.is_uppercase()) {
                found.to_string()
            } else {
                String::new()
            }
        })
        .into_owned();

    // Stage 2: phrase fillers, token-wise.
    let tokens: Vec<&str> = t.split_whitespace().collect();
    let mut out: Vec<Cow<str>> = Vec::with_capacity(tokens.len());
    // Where the sentence being read starts, in `tokens` and in `out`.
    let (mut sentence, mut kept) = (0, 0);
    // A sentence end moved onto a kept word makes the next kept word open a
    // sentence, which tidy cannot tell after an abbreviation ("U.S.").
    let mut capitalise = false;
    let mut i = 0;
    while i < tokens.len() {
        match filler_len(&out, &tokens, i, aggressive, guards) {
            0 => {
                let mut token = Cow::Borrowed(tokens[i]);
                if std::mem::take(&mut capitalise) {
                    capitalise_first(&mut token);
                }
                out.push(token);
                i += 1;
            }
            n => {
                i += n;
                // The sentence end on a dropped filler stays, on the last
                // word kept in its sentence.
                if guards && out.len() > kept {
                    if let (Some(mark), Some(last)) = (sentence_end(tokens[i - 1]), out.last_mut())
                    {
                        capitalise = carry_sentence_end(last, mark);
                    }
                }
            }
        }
        if guards && sentence_ends_before(&tokens, i) {
            // Removing fillers never empties a sentence.
            if out.len() == kept {
                out.extend(tokens[sentence..i].iter().map(|t| Cow::Borrowed(*t)));
                capitalise = false;
            }
            sentence = i;
            kept = out.len();
        }
    }
    out.join(" ")
}

/// Put the sentence end `mark` of a dropped filler on `last`, in place of a
/// comma there or after a word with nothing after it, and say whether it
/// went on. A dropped "you know?" or "i mean?" is a tag on a statement, so
/// its '?' becomes a '.'; a '!' stays. A word that already ends with a '.'
/// ("U.S.", "etc.") takes no second one. Any other mark on `last` (';',
/// ':', a quote) stays and takes nothing.
fn carry_sentence_end(last: &mut Cow<'_, str>, mark: &str) -> bool {
    let mark: String = if mark.contains('!') {
        mark.chars().filter(|&c| c != '?').collect()
    } else {
        ".".into()
    };
    let text: &str = last;
    let ended = if let Some(word) = text.strip_suffix(',') {
        let mark = if word.ends_with('.') && mark == "." { "" } else { mark.as_str() };
        format!("{word}{mark}")
    } else if text.chars().last().is_some_and(char::is_alphanumeric) {
        format!("{text}{mark}")
    } else {
        return false;
    };
    *last = Cow::Owned(ended);
    true
}

/// `token` with its first letter in upper case, when it starts with a lower
/// case one.
fn capitalise_first(token: &mut Cow<'_, str>) {
    let mut chars = token.chars();
    let Some(first) = chars.next().filter(|c| c.is_lowercase()) else {
        return;
    };
    let capitalised = format!("{}{}", first.to_uppercase(), chars.as_str());
    *token = Cow::Owned(capitalised);
}

/// How many tokens from `tokens[i]` are a filler to drop, or 0 to keep it.
/// `out` is what has been kept so far; `guards` turns on the ones that keep
/// the user's own words.
fn filler_len(
    out: &[Cow<'_, str>],
    tokens: &[&str],
    i: usize,
    aggressive: bool,
    guards: bool,
) -> usize {
    let cur = norm(tokens[i]);
    let next = tokens.get(i + 1).map(|t| norm(t));

    // "you know"
    if cur == "you" && next.as_deref() == Some("know") {
        let prev = out.last().map(|t| norm(t));
        let guarded = prev.as_deref().is_some_and(|p| YOU_KNOW_GUARDS.contains(&p))
            || (guards
                && (you_know_is_own_words(out, tokens, i)
                    || you_know_ends_own_sentence(out, tokens, i)));
        if !guarded {
            return 2;
        }
    }
    // "i mean" at clause start only
    if cur == "i" && next.as_deref() == Some("mean") {
        let clause_start = out.last().is_none_or(|t| {
            t.ends_with(['.', ',', ';', ':', '!', '?'])
                || t.ends_with(super::NEWLINE)
                || t.ends_with(super::PARAGRAPH)
        });
        if clause_start && !(guards && i_mean_is_own_words(tokens, i)) {
            return 2;
        }
    }
    // "like" in aggressive mode
    if aggressive && cur == "like" {
        let prev = out.last().map(|t| norm(t));
        let guarded = prev.as_deref().is_some_and(|p| LIKE_GUARDS.contains(&p));
        if !guarded {
            return 1;
        }
    }
    0
}

/// Whether a sentence ends just before `tokens[i]`: the token before closes
/// one with '.', '!' or '?' (not an ellipsis), a spoken new line or
/// paragraph is on either side, or the text ends.
fn sentence_ends_before(tokens: &[&str], i: usize) -> bool {
    let Some(last) = i.checked_sub(1).and_then(|k| tokens.get(k)) else {
        return false;
    };
    sentence_end(last).is_some() || is_sentinel(last) || tokens.get(i).is_none_or(|t| is_sentinel(t))
}
