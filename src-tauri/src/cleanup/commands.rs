//! Spoken commands: "new line" / "new paragraph" become sentinel characters
//! that survive all later stages and turn into real newlines in `tidy`.

use super::{NEWLINE, PARAGRAPH};
use regex::Regex;
use std::sync::OnceLock;

pub fn apply(text: String) -> String {
    static NL: OnceLock<Regex> = OnceLock::new();
    static NP: OnceLock<Regex> = OnceLock::new();
    let np = NP.get_or_init(|| Regex::new(r"(?i)[.,]?\s*\bnew paragraph\b[.,]?").unwrap());
    let nl = NL.get_or_init(|| Regex::new(r"(?i)[.,]?\s*\bnew ?line\b[.,]?").unwrap());
    // Sentinels get surrounding spaces so they are always standalone tokens
    // for the later token-based stages; tidy collapses the spacing.
    let t = np
        .replace_all(&text, format!(" {PARAGRAPH} "))
        .into_owned();
    nl.replace_all(&t, format!(" {NEWLINE} ")).into_owned()
}
