//! Self-correction handling: "let's meet on monday no wait on tuesday"
//! → "let's meet on tuesday".
//!
//! Soft triggers ("no wait", "oh no", "no i meant") are a correction only
//! when the words after the trigger anchor to words before it; otherwise
//! they are ordinary speech ("there is no wait at the counter") and every
//! word stays. Hard triggers ("scratch that") fall back to deleting the whole
//! clause when no anchor is found. A trigger has to be a phrase people use
//! for correcting themselves and little else: "correction" and "I meant" on
//! their own are ordinary words ("I meant to call you").

use super::{NEWLINE, PARAGRAPH};

const HARD: &[&[&str]] = &[
    &["scratch", "that"],
    &["strike", "that"],
    &["let", "me", "rephrase"],
];

const SOFT: &[&[&str]] = &[
    &["no", "i", "meant"],
    &["no", "wait"],
    &["wait", "no"],
    &["oh", "no"],
    &["actually", "no"],
];

const NUMBER_WORDS: &[&str] = &[
    "zero", "one", "two", "three", "four", "five", "six", "seven", "eight", "nine", "ten",
    "eleven", "twelve", "thirteen", "fourteen", "fifteen", "sixteen", "seventeen", "eighteen",
    "nineteen", "twenty", "thirty", "forty", "fifty", "sixty", "seventy", "eighty", "ninety",
    "hundred", "thousand", "million",
];

fn norm(token: &str) -> String {
    token
        .trim_matches(|c: char| c.is_ascii_punctuation())
        .to_lowercase()
}

fn is_numeric(token: &str) -> bool {
    let n = norm(token);
    n.parse::<u64>().is_ok() || NUMBER_WORDS.contains(&n.as_str())
}

/// `text` with its self-corrections resolved, and whether it had any. The
/// pipeline keeps a corrected text as the original filler rules left it;
/// one with nothing to correct gets the filler guards instead
/// (`super::run_pipeline`).
pub fn correct(text: String) -> (String, bool) {
    let mut segments: Vec<String> = Vec::new();
    let mut current = String::new();
    for c in text.chars() {
        if c == NEWLINE || c == PARAGRAPH {
            segments.push(std::mem::take(&mut current));
            segments.push(c.to_string());
        } else {
            current.push(c);
        }
    }
    segments.push(current);

    let mut corrected = false;
    let text = segments
        .into_iter()
        .map(|seg| {
            if seg.chars().all(|c| c == NEWLINE || c == PARAGRAPH) {
                seg
            } else {
                let (seg, any) = apply_segment(seg);
                corrected |= any;
                seg
            }
        })
        .collect::<Vec<_>>()
        .join("");
    (text, corrected)
}

fn apply_segment(seg: String) -> (String, bool) {
    let had_leading_space = seg.starts_with(' ');
    let had_trailing_space = seg.ends_with(' ');
    let mut tokens: Vec<String> = seg.split_whitespace().map(String::from).collect();

    let mut corrected = false;
    for _ in 0..5 {
        let Some(next) = first_correction(&tokens) else {
            break;
        };
        tokens = next;
        corrected = true;
    }

    let body = tokens.join(" ");
    let seg = format!(
        "{}{}{}",
        if had_leading_space { " " } else { "" },
        body,
        if had_trailing_space { " " } else { "" }
    );
    (seg, corrected)
}

/// `tokens` with the first trigger that really is a correction resolved, or
/// `None` when no trigger in them is one.
fn first_correction(tokens: &[String]) -> Option<Vec<String>> {
    let mut from = 0;
    while let Some((ti, tj, hard)) = find_trigger(tokens, from) {
        if let Some(next) = resolve(tokens, ti, tj, hard) {
            return Some(next);
        }
        from = tj;
    }
    None
}

/// Find the first trigger at or after `from`; longer matches win at the same
/// position.
fn find_trigger(tokens: &[String], from: usize) -> Option<(usize, usize, bool)> {
    let normed: Vec<String> = tokens.iter().map(|t| norm(t)).collect();
    for i in from..normed.len() {
        let mut best: Option<(usize, bool)> = None;
        for (patterns, hard) in [(HARD, true), (SOFT, false)] {
            for pat in patterns {
                if normed.len() - i >= pat.len()
                    && pat.iter().enumerate().all(|(k, w)| normed[i + k] == *w)
                {
                    let len = pat.len();
                    if best.is_none_or(|(blen, _)| len > blen) {
                        best = Some((len, hard));
                    }
                }
            }
        }
        if let Some((len, hard)) = best {
            return Some((i, i + len, hard));
        }
    }
    None
}

/// The tokens with the trigger at `ti..tj` resolved, or `None` when it is not
/// a correction: a soft trigger with nothing before it to anchor to.
fn resolve(tokens: &[String], ti: usize, tj: usize, hard: bool) -> Option<Vec<String>> {
    let pre = &tokens[..ti];
    let post = &tokens[tj..];

    // Exact anchor: first post-trigger token appears in the last 8 pre tokens.
    if let Some(p0) = post.first().map(|t| norm(t)) {
        let start = pre.len().saturating_sub(8);
        if let Some(idx) = (start..pre.len()).rev().find(|&k| norm(&pre[k]) == p0) {
            return Some([&pre[..idx], post].concat());
        }

        // Numeric anchor: replacement starts with a number → delete the most
        // recent number run, if it ends within one token of the trigger.
        if is_numeric(&p0) {
            let mut end = pre.len();
            // allow one non-numeric token (e.g. "pm") between run and trigger
            for _ in 0..2 {
                if end > 0 && !is_numeric(&pre[end - 1]) {
                    end -= 1;
                } else {
                    break;
                }
            }
            if end > 0 && is_numeric(&pre[end - 1]) && pre.len() - end <= 1 {
                let mut run_start = end - 1;
                while run_start > 0 && is_numeric(&pre[run_start - 1]) {
                    run_start -= 1;
                }
                return Some([&pre[..run_start], post].concat());
            }
        }
    }

    if hard {
        // Delete back to the previous sentence boundary.
        let boundary = (0..pre.len())
            .rev()
            .find(|&k| pre[k].ends_with(['.', '!', '?']))
            .map(|k| k + 1)
            .unwrap_or(0);
        return Some([&pre[..boundary], post].concat());
    }

    // A soft trigger with no anchor is not a correction.
    None
}
