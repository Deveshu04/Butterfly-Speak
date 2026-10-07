//! Output tone, applied last (after polish): three presets.
//!
//! - formal      → caps + full punctuation (no-op; the pipeline's default)
//! - casual      → keep caps, drop the stiff terminal period
//! - veryCasual  → all lowercase + drop the terminal period
//!
//! A snippet expansion is the user's saved text, so wherever one appears in
//! the result it is left exactly as saved: not lowercased, and its own final
//! period is not taken.

/// `keep` holds the texts to leave exactly as they are: the user's snippet
/// expansions.
pub fn apply(text: String, style: &str, keep: &[String]) -> String {
    match style {
        "casual" => strip_terminal_period(text, keep),
        "veryCasual" => {
            let lowered = lowercase_outside(&text, &kept_spans(&text, keep));
            strip_terminal_period(lowered, keep)
        }
        _ => text,
    }
}

/// Byte ranges of `text` covered by an occurrence of one of `keep`.
fn kept_spans(text: &str, keep: &[String]) -> Vec<std::ops::Range<usize>> {
    let mut spans: Vec<std::ops::Range<usize>> = keep
        .iter()
        .map(|k| k.trim())
        .filter(|k| !k.is_empty())
        .flat_map(|k| text.match_indices(k).map(|(at, found)| at..at + found.len()))
        .collect();
    spans.sort_by_key(|span| span.start);
    spans
}

/// `text` lowercased except inside `spans`.
fn lowercase_outside(text: &str, spans: &[std::ops::Range<usize>]) -> String {
    let mut out = String::with_capacity(text.len());
    let mut at = 0;
    for span in spans {
        if span.start < at {
            // Overlaps a span already copied as it was: copy the rest of it
            // the same way.
            if span.end > at {
                out.push_str(&text[at..span.end]);
                at = span.end;
            }
            continue;
        }
        out.push_str(&text[at..span.start].to_lowercase());
        out.push_str(&text[span.clone()]);
        at = span.end;
    }
    out.push_str(&text[at..].to_lowercase());
    out
}

/// Removes a single trailing '.' (never '!', '?', '…' or the Devanagari
/// danda — those carry meaning), unless it is the end of a kept text.
fn strip_terminal_period(mut text: String, keep: &[String]) -> String {
    let trimmed = text.trim_end();
    let ends_kept = keep
        .iter()
        .map(|k| k.trim())
        .any(|k| !k.is_empty() && trimmed.ends_with(k));
    if trimmed.ends_with('.') && !trimmed.ends_with("..") && !ends_kept {
        let cut = trimmed.len() - 1;
        text.truncate(cut);
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formal_is_untouched() {
        assert_eq!(apply("Hey, lunch tomorrow?".into(), "formal", &[]), "Hey, lunch tomorrow?");
    }

    #[test]
    fn casual_drops_terminal_period_only() {
        assert_eq!(
            apply("Let's do 12 if that works.".into(), "casual", &[]),
            "Let's do 12 if that works"
        );
        assert_eq!(apply("Are you free?".into(), "casual", &[]), "Are you free?");
        assert_eq!(apply("Wait...".into(), "casual", &[]), "Wait...");
    }

    #[test]
    fn very_casual_lowercases() {
        assert_eq!(
            apply("Hey, are you free for lunch tomorrow? Let's do 12.".into(), "veryCasual", &[]),
            "hey, are you free for lunch tomorrow? let's do 12"
        );
    }

    #[test]
    fn danda_survives() {
        assert_eq!(apply("मैं ठीक हूँ।".into(), "casual", &[]), "मैं ठीक हूँ।");
    }

    /// A snippet's expansion is the user's saved text: the tone presets leave
    /// its case and its final period alone.
    #[test]
    fn a_snippet_expansion_keeps_its_case_and_its_period() {
        let keep = vec!["https://bit.ly/3XyZ".to_string(), "Best regards, Asha.".to_string()];
        assert_eq!(
            apply("Here is my link https://bit.ly/3XyZ".into(), "veryCasual", &keep),
            "here is my link https://bit.ly/3XyZ"
        );
        assert_eq!(
            apply("Thanks. Best regards, Asha.".into(), "casual", &keep),
            "Thanks. Best regards, Asha."
        );
        assert_eq!(
            apply("Thanks. Best regards, Asha.".into(), "veryCasual", &keep),
            "thanks. Best regards, Asha."
        );
        // Text outside the expansion is still styled.
        assert_eq!(
            apply("Best regards, Asha. See you Monday.".into(), "veryCasual", &keep),
            "Best regards, Asha. see you monday"
        );
    }

    /// Two expansions that overlap are kept as one stretch, with nothing of
    /// either lost.
    #[test]
    fn overlapping_expansions_keep_all_of_their_text() {
        let keep = vec!["Asha Rao".to_string(), "Rao Labs".to_string()];
        assert_eq!(
            apply("Asha Rao Labs Rocks".into(), "veryCasual", &keep),
            "Asha Rao Labs rocks"
        );
    }
}
