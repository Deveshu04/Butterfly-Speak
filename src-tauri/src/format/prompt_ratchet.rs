//! The hash ratchet: every prompt text the app ships, pinned by SHA-256.
//!
//! # Why
//!
//! A prompt edit changes what the model does on every install, and a diff of
//! a long string does not show how much. The editable halves are what "Reset
//! to default" on the Prompts page restores and what
//! `settings::PromptOverrides::normalize` compares a save against. The fixed
//! halves matter more because nobody can see or undo them: the
//! injection-hardening stanza is what keeps a dictated "ignore your rules"
//! on the page instead of obeyed, its worked example is tuned per level (one
//! shared example cost English disfluency F1 0.538 → 0.468 on the live
//! benchmark), and the agent output rules are all that keeps a preamble out
//! of the user's document.
//!
//! So a change to any shipped prompt fails the test below until the table is
//! updated in the same commit, which makes every prompt edit a visible,
//! deliberate one.
//!
//! # The hash
//!
//! SHA-256 over the UTF-8 bytes of the text exactly as it ships — no salt,
//! no trimming, no normalisation — rendered lowercase hex, so a hash can be
//! checked by hand with any `sha256sum`.

use sha2::{Digest, Sha256};

fn sha256(text: &str) -> String {
    let digest = Sha256::digest(text.as_bytes());
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

/// Every shipped prompt text, keyed `<kind>.<half>`.
///
/// **Editable halves** (`*.rules`, `agent.brief`, `agent.selectionRules`) are
/// what `settings::PromptOverrides` can replace. **Fixed halves**
/// (`*.hardening`, `agent.languageRule`, `agent.outputRules`,
/// `agent.selectionEnvelopeRule`, `polish.transcriptDelimiterRule`,
/// `polish.beforeCursorRule`, `polish.beforeCursorMidSentenceRule`,
/// `polish.userTurnContract`) are added around whatever the user wrote or
/// dictated and cannot be edited on the Prompts page at all. They are in the
/// table because they are prompts the app ships, and nothing but this test
/// would show a change to one.
///
/// Not in the table: `sarvam::chat::build_transform_system`'s scaffold (it
/// interpolates the user's own instruction, so there is no fixed default
/// text to hash) and the end-marker rule (it interpolates the
/// four-symbol code minted for each request).
///
/// To update: run the test and replace this table with the one it prints.
const SHIPPED_PROMPT_HASHES: &[(&str, &str)] = &[
    (
        "light.rules",
        "c6132ee0c7c38fd80e9ec813dfa8dcbf88566e8a5e44b7a52a66546e6118f039",
    ),
    (
        "balanced.rules",
        "cbf2c42d4a6abbbd85bd51bb386e604bbbc4fd4ee8d726f99735446bc5851248",
    ),
    (
        "high.rules",
        "9a357f45b58637e951d24b6aae245953352cd58a1c4642e958b433210fa378bc",
    ),
    (
        "high.localRules",
        "82d4f194d4fbc4c0cbc28b4fa7751be76b4a286d6a924ac42a666675d5aed071",
    ),
    (
        "light.hardening",
        "f8204bb38b13e82f91d1edccd58060f0209ff9f2a291927ee0a7c4083b7ff07a",
    ),
    (
        "balanced.hardening",
        "8631516c2d2a449fe039e61dcf734ae67fb10814f61f29e87bae42cfe9e0ee27",
    ),
    (
        "high.hardening",
        "2b1dac29507dd6563f7ad31dccac2b97904b48397f9377a0cdf783f4cd1c5ece",
    ),
    (
        "polish.transcriptDelimiterRule",
        "2858df1b8cd5543373cbe3cb9d707e536d4f87b626a0d0d633825fa79ef23259",
    ),
    (
        "polish.beforeCursorRule",
        "ea075fe05c5f3b065dc7883804e12c93001b88631a26e4de0ca31e5b45825c49",
    ),
    (
        "polish.beforeCursorMidSentenceRule",
        "e6c9ba99321b0f74faf4069db7a0ff2e9f473bfd43c338af722d5712debfed78",
    ),
    (
        "polish.userTurnContract",
        "3596f1f232620387011f11f7ee41963d3603125d8376685b57b6b961875aaa9c",
    ),
    (
        "agent.brief",
        "48ddac9b04c6349b5f74c84379fda5f2eb20b499b490dccc58591e18fabdd4c3",
    ),
    (
        "agent.languageRule",
        "64ee38417a20037219bcc4d032c81fef8f008ba77a529f152b5136585daa551b",
    ),
    (
        "agent.outputRules",
        "b3e0139717a4b020fb140d4c33b3d1322b2569a26cf18b6ec8f2d7dac1fff011",
    ),
    (
        "agent.selectionRules",
        "c0c03b9986e75b2b1bc4de73d50fc12befb0f8f1ced57f361b83c0f7be3cbcc6",
    ),
    (
        "agent.selectionEnvelopeRule",
        "8e74fcd2fb604df76b24954d6ba023664233c351747ffbf07ce84caba3bca59c",
    ),
];

/// The live texts, in the same key order as the table.
fn shipped() -> Vec<(&'static str, String)> {
    use crate::format::level::CleanupLevel;
    use crate::sarvam::chat;

    vec![
        ("light.rules", CleanupLevel::Light.default_rules()),
        ("balanced.rules", CleanupLevel::Balanced.default_rules()),
        ("high.rules", CleanupLevel::High.default_rules()),
        // The on-device engine gets different High rules (the ladder text);
        // it is shipped, so it is pinned. Light and Balanced are identical to
        // their `.rules` entries and would only duplicate a hash.
        ("high.localRules", CleanupLevel::High.local_rules()),
        ("light.hardening", CleanupLevel::Light.hardening().to_string()),
        (
            "balanced.hardening",
            CleanupLevel::Balanced.hardening().to_string(),
        ),
        ("high.hardening", CleanupLevel::High.hardening().to_string()),
        (
            "polish.transcriptDelimiterRule",
            chat::TRANSCRIPT_DELIMITER_RULE.to_string(),
        ),
        ("polish.beforeCursorRule", chat::BEFORE_CURSOR_RULE.to_string()),
        (
            "polish.beforeCursorMidSentenceRule",
            chat::BEFORE_CURSOR_MID_SENTENCE_RULE.to_string(),
        ),
        (
            "polish.userTurnContract",
            chat::POLISH_REPLY_CONTRACT.to_string(),
        ),
        ("agent.brief", chat::AGENT_BRIEF.to_string()),
        (
            "agent.languageRule",
            chat::AGENT_LANGUAGE_RULE.to_string(),
        ),
        ("agent.outputRules", chat::AGENT_OUTPUT_RULES.to_string()),
        (
            "agent.selectionRules",
            chat::AGENT_SELECTION_RULES.to_string(),
        ),
        (
            "agent.selectionEnvelopeRule",
            chat::AGENT_SELECTION_ENVELOPE_RULE.to_string(),
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// THE RATCHET. A shipped prompt cannot change without this failing.
    #[test]
    fn every_shipped_prompt_matches_its_pinned_hash() {
        let live: Vec<(&str, String)> = shipped()
            .into_iter()
            .map(|(key, text)| (key, sha256(&text)))
            .collect();
        let pinned: Vec<(&str, String)> = SHIPPED_PROMPT_HASHES
            .iter()
            .map(|(key, hash)| (*key, (*hash).to_string()))
            .collect();

        if live != pinned {
            // Print the whole table rather than the first difference: a
            // wording pass usually touches several at once, and one
            // recompile per hash is the kind of friction that gets a ratchet
            // deleted instead of updated.
            let table: String = live
                .iter()
                .map(|(key, hash)| format!("    (\n        {key:?},\n        {hash:?},\n    ),\n"))
                .collect();
            panic!(
                "a pinned prompt text has changed: its SHA-256 no longer matches \
                 the one recorded in SHIPPED_PROMPT_HASHES.\n\
                 If you meant to change it, replace the whole table with this:\n\n\
                 {table}\n\
                 If you did not, find the edit and undo it instead. Check the fixed \
                 halves first (*.hardening, polish.*, agent.languageRule, \
                 agent.outputRules, agent.selectionEnvelopeRule): users cannot see or \
                 reset them, so a mistake there reaches every install unnoticed."
            );
        }
    }

    /// A key in the table with no live text (or the reverse) would make the
    /// test above pass for the wrong reason once the lists were re-sorted.
    #[test]
    fn the_table_covers_exactly_the_shipped_prompts() {
        let live: Vec<&str> = shipped().into_iter().map(|(key, _)| key).collect();
        let pinned: Vec<&str> = SHIPPED_PROMPT_HASHES.iter().map(|(key, _)| *key).collect();
        assert_eq!(live, pinned, "prompt_ratchet's key list drifted");
    }

    /// Every entry is a lowercase 64-hex digest — what catches a hash pasted
    /// in with a stray space or in upper case.
    #[test]
    fn every_pinned_hash_is_lowercase_hex() {
        for (key, hash) in SHIPPED_PROMPT_HASHES {
            assert_eq!(hash.len(), 64, "{key}: not a SHA-256 digest");
            assert!(
                hash.chars().all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c)),
                "{key}: not lowercase hex"
            );
        }
    }

    /// The hash definition itself, against a vector anyone can reproduce
    /// (`echo -n abc | sha256sum`). Without this, a change to `sha256`
    /// above would rewrite every hash in the table and still pass.
    #[test]
    fn the_hash_is_plain_sha256_over_utf8() {
        assert_eq!(
            sha256("abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            sha256(""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        // Non-ASCII is hashed as UTF-8 bytes — the shipped prompts contain
        // em dashes and Devanagari danda characters.
        assert_eq!(
            sha256("—"),
            "bda050585a00f0f6cb502350559d75532ae3b244c9498b996e7c5df2d98dfc8d"
        );
    }
}
