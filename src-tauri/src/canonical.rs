//! Canonical equivalence: text that means the same thing but is spelled with
//! different codepoints.
//!
//! Unicode lets the same word be written more than one way. `क़` is one
//! codepoint (U+0958) or two (`क` U+0915 + nukta U+093C); `é` is U+00E9 or
//! `e` + U+0301. The two spellings are *canonically equivalent* — the same
//! text — and every Unicode-aware comparison is supposed to treat them as
//! equal. Rust's `==`, `to_lowercase`, `eq_ignore_ascii_case` and SQLite's
//! `COLLATE NOCASE` all compare scalar-for-scalar and do not.
//!
//! That matters here because Butterfly Speak's two sources of text disagree
//! about form. The field monitor NFC-normalizes what it reads before diffing
//! (`learn::monitor::nfc`), so a learned correction is stored in NFC — and for
//! the Devanagari nukta letters NFC is the *decomposed* spelling, because
//! U+0958 and its siblings are canonical composition exclusions and NFC pulls
//! them apart. Sarvam transcripts arrive in whatever form the API emits,
//! typically precomposed. Without this module a learned Devanagari rule is
//! stored in one spelling and hunted for in the other, so it never fires, and
//! a hand-typed rule fails to suppress re-learning the same word.
//!
//! ASCII is canonically inert — it has no decompositions, so NFC and NFD are
//! both the identity — which is why every English path here is a fast path
//! that returns exactly what a plain scalar comparison returns. That is also
//! why an English-only test suite cannot see a canonical-equivalence bug.
//!
//! Two operations, for the two things the app does with a rule:
//!
//! - [`fold`] / [`same_word`] — *comparing* a rule against other text
//!   (is this word already known?).
//! - [`spellings`] — *finding* a rule inside a transcript, by enumerating the
//!   forms it could be written in rather than by rewriting the transcript.

use unicode_normalization::char::{
    canonical_combining_class, compose, decompose_canonical, is_combining_mark,
};
use unicode_normalization::UnicodeNormalization;

/// Letters, numbers and combining marks: the characters a word is made of.
/// Marks count so that Indic vowel signs, nukta and virama stay on the word
/// they belong to. Connector punctuation such as the underscore does not.
pub fn belongs_in_word(c: char) -> bool {
    c.is_alphanumeric() || is_combining_mark(c)
}

/// The identity under which two words are "the same word": canonical form,
/// then case.
///
/// NFC is applied on both sides of the lowercasing. The second pass is not
/// superstition — `char::to_lowercase` is defined on scalars and a few
/// mappings can leave a string that is no longer composed, so folding without
/// it would produce keys that differ for words that are the same. On one word
/// it costs nothing.
pub fn fold(s: &str) -> String {
    // ASCII has no decompositions and no multi-char lowercase mappings, so
    // this is bit-identical to the NFC path below — and it is the only branch
    // an English-only user ever takes.
    if s.is_ascii() {
        return s.to_ascii_lowercase();
    }
    let composed: String = s.nfc().collect();
    let lowered = composed.to_lowercase();
    lowered.nfc().collect()
}

/// Whether two words are the same word, ignoring spelling form and case.
pub fn same_word(a: &str, b: &str) -> bool {
    if a.is_ascii() && b.is_ascii() {
        return a.eq_ignore_ascii_case(b);
    }
    fold(a) == fold(b)
}

/// Canonical composition, case preserved. The form text is *stored* in, so
/// that anything comparing stored text scalar-for-scalar (SQLite, which has
/// no notion of Unicode normalization) is comparing like with like.
pub fn normalized(s: &str) -> String {
    if s.is_ascii() {
        return s.to_string();
    }
    s.nfc().collect()
}

/// How many composition-exclusion letters in a single word are enumerated
/// across. The subsets grow as 2^n, and the result is a regex alternation
/// someone pays to compile on every dictation, so it is capped: a word with
/// more than three nukta letters is not what a dictation correction looks
/// like, and past the cap the word simply keeps the spellings NFC and NFD
/// already give it.
const MAX_EXCLUSION_SITES: usize = 3;

/// The resulting ceiling on [`spellings`]: the verbatim, NFC and NFD forms,
/// plus every non-empty subset of the exclusion sites.
const MAX_SPELLINGS: usize = 3 + (1 << MAX_EXCLUSION_SITES) - 1;

/// Every canonically-equivalent spelling of `word` that could plausibly appear
/// in a transcript.
///
/// The order is unspecified — at most one of them can match at any given
/// position, since they differ at the first letter they disagree about — and
/// [`crate::cleanup::snippets`] sorts them for its own reasons anyway.
///
/// This is the *matching* half, and it deliberately works on the rule rather
/// than on the transcript. The rule is one short word known in advance; the
/// transcript is the whole dictation and arrives on the hot path. Normalizing
/// the small side is both cheaper and safer: a matcher that normalized the
/// transcript would have to map match offsets back through a form change to
/// avoid rewriting the parts it did not match, and any slip there turns the
/// paste into a stealth normalizer for text the user never asked it to touch.
///
/// An ASCII word returns exactly itself, one element, so the pattern built
/// from it is the plain escaped word.
pub fn spellings(word: &str) -> Vec<String> {
    if word.is_ascii() {
        return vec![word.to_string()];
    }

    let mut out: Vec<String> = Vec::new();
    let push = |s: String, out: &mut Vec<String>| {
        if !out.contains(&s) {
            out.push(s);
        }
    };

    push(word.to_string(), &mut out);
    let composed: String = word.nfc().collect();
    push(composed, &mut out);
    let decomposed: String = word.nfd().collect();
    push(decomposed.clone(), &mut out);

    // The spellings NFC refuses to produce. NFC composes `e`+acute back into
    // `é`, so the two Latin forms are already covered by the three above — but
    // it deliberately will NOT compose `क`+nukta back into `क़`, because that
    // character is a composition exclusion. Those are exactly the letters this
    // module exists for, so the precomposed spelling has to be found the long
    // way round.
    for variant in exclusion_variants(&decomposed) {
        push(variant, &mut out);
    }
    debug_assert!(out.len() <= MAX_SPELLINGS);
    out
}

/// Spellings of `decomposed` in which some subset of its starter+mark pairs
/// has been replaced by a precomposed composition-exclusion character.
///
/// All subsets rather than only the all-composed one, because a transcript is
/// under no obligation to be internally consistent — a word can arrive with
/// one nukta precomposed and the next spelled out. Bounded by
/// [`MAX_EXCLUSION_SITES`].
fn exclusion_variants(decomposed: &str) -> Vec<String> {
    let chars: Vec<char> = decomposed.chars().collect();
    // Where a precomposed spelling exists, and what it is.
    let mut sites: Vec<(usize, char)> = Vec::new();
    for i in 0..chars.len().saturating_sub(1) {
        if let Some(c) = precomposed_exclusion(chars[i], chars[i + 1]) {
            sites.push((i, c));
        }
    }
    if sites.is_empty() {
        return Vec::new();
    }
    sites.truncate(MAX_EXCLUSION_SITES);

    let mut out = Vec::new();
    // Every non-empty subset.
    for mask in 1u32..(1 << sites.len()) {
        let mut s = String::with_capacity(decomposed.len());
        let mut skip = false;
        for (i, &c) in chars.iter().enumerate() {
            if skip {
                skip = false;
                continue;
            }
            match sites
                .iter()
                .position(|&(at, _)| at == i)
                .filter(|k| mask & (1 << k) != 0)
            {
                Some(k) => {
                    s.push(sites[k].1);
                    skip = true; // the mark is folded into the letter
                }
                None => s.push(c),
            }
        }
        out.push(s);
    }
    out
}

/// The single character that decomposes to exactly `base` + `mark`, when one
/// exists and NFC will not produce it.
///
/// Found by scanning the 256-codepoint block `base` lives in. There is no
/// reverse-composition table in `unicode-normalization` (composition
/// exclusions are, by construction, the pairs its `compose` refuses), and
/// hardcoding the list would be a table to keep in step with Unicode by hand.
/// The block assumption holds for every Indic exclusion — U+0958 sits with
/// U+0915 in Devanagari, U+09DC with U+09A1 in Bengali, U+0A33 in Gurmukhi,
/// U+0B5C in Oriya — and the scan is 256 table lookups, run only for a
/// non-ASCII rule whose pair `compose` has already declined.
fn precomposed_exclusion(base: char, mark: char) -> Option<char> {
    // Canonical composition only ever joins a starter to a following
    // non-starter, so anything else cannot have a precomposed form and must
    // not pay for the scan. Without this, every adjacent pair of ordinary
    // Devanagari letters in a word would scan a whole block to learn nothing.
    if canonical_combining_class(base) != 0 || canonical_combining_class(mark) == 0 {
        return None;
    }
    // If it composes, NFC already produced this spelling and there is nothing
    // left to find. This is also what keeps Latin off the scan entirely.
    if compose(base, mark).is_some() {
        return None;
    }
    let block = (base as u32) & !0xFF;
    (block..block + 0x100)
        .filter_map(char::from_u32)
        .find(|&c| c != base && decomposes_to(c, base, mark))
}

fn decomposes_to(c: char, base: char, mark: char) -> bool {
    let mut seen = [None, None];
    let mut n = 0usize;
    decompose_canonical(c, |d| {
        if n < 2 {
            seen[n] = Some(d);
        }
        n += 1;
    });
    n == 2 && seen[0] == Some(base) && seen[1] == Some(mark)
}

#[cfg(test)]
mod tests {
    use super::*;

    // `क़` QA: one codepoint, or `क` + nukta. A canonical composition
    // exclusion, so NFC pulls it APART rather than putting it together.
    const QA_PRECOMPOSED: &str = "\u{0958}";
    const QA_DECOMPOSED: &str = "\u{0915}\u{093C}";

    /// The fact the whole module rests on, asserted rather than assumed: NFC
    /// does not round-trip this character, so "just NFC everything" is not a
    /// fix on its own — one side of a comparison can still hold U+0958.
    #[test]
    fn nfc_decomposes_the_nukta_letter_instead_of_composing_it() {
        let composed: String = QA_PRECOMPOSED.nfc().collect();
        assert_eq!(composed, QA_DECOMPOSED, "NFC(क़) is क + nukta");
        let round_trip: String = QA_DECOMPOSED.nfc().collect();
        assert_eq!(round_trip, QA_DECOMPOSED, "and it stays apart");
        // Which is exactly what makes it different from Latin.
        let e_acute: String = "e\u{0301}".nfc().collect();
        assert_eq!(e_acute, "é", "Latin composes, so NFC alone would do");
    }

    // -----------------------------------------------------------------
    // fold / same_word
    // -----------------------------------------------------------------

    #[test]
    fn the_two_spellings_of_a_nukta_word_are_the_same_word() {
        assert!(same_word(QA_PRECOMPOSED, QA_DECOMPOSED));
        assert_eq!(fold(QA_PRECOMPOSED), fold(QA_DECOMPOSED));
    }

    #[test]
    fn the_two_spellings_of_an_accented_word_are_the_same_word() {
        assert!(same_word("café", "cafe\u{0301}"));
        assert!(same_word("CAFÉ", "cafe\u{0301}"));
    }

    #[test]
    fn different_words_are_still_different() {
        assert!(!same_word(QA_DECOMPOSED, "\u{0916}"), "क़ is not ख");
        assert!(!same_word("cafe", "café"));
        assert!(!same_word("Vaibav", "Vaibhav"));
    }

    /// The English guarantee: for ASCII, folding is exactly lowercasing.
    #[test]
    fn folding_ascii_is_exactly_lowercasing() {
        for w in ["Vaibhav", "VAIBAV", "co-op", "don't", "a_1", ""] {
            assert_eq!(fold(w), w.to_lowercase());
            assert_eq!(fold(w), w.to_ascii_lowercase());
        }
        assert!(same_word("Vaibhav", "vaibhav"));
    }

    #[test]
    fn normalized_leaves_ascii_alone_and_composes_the_rest() {
        assert_eq!(normalized("Vaibhav"), "Vaibhav");
        assert_eq!(normalized(QA_PRECOMPOSED), QA_DECOMPOSED);
        assert_eq!(normalized("cafe\u{0301}"), "café");
        // Case is preserved: this is a storage form, not an identity.
        assert_eq!(normalized("CAFE\u{0301}"), "CAFÉ");
    }

    // -----------------------------------------------------------------
    // spellings
    // -----------------------------------------------------------------

    /// The English guarantee, stated at the source: one spelling, the word
    /// itself, so the regex built from it is the plain escaped word.
    #[test]
    fn an_ascii_word_has_exactly_one_spelling() {
        assert_eq!(spellings("Vaibhav"), vec!["Vaibhav".to_string()]);
        assert_eq!(spellings("hello"), vec!["hello".to_string()]);
    }

    /// Both directions of the case this module exists for: whichever spelling
    /// the rule was stored in, the other one is enumerated.
    #[test]
    fn a_nukta_word_enumerates_both_of_its_spellings() {
        let from_precomposed = spellings(QA_PRECOMPOSED);
        assert!(from_precomposed.iter().any(|s| s == QA_PRECOMPOSED));
        assert!(from_precomposed.iter().any(|s| s == QA_DECOMPOSED));

        let from_decomposed = spellings(QA_DECOMPOSED);
        assert!(
            from_decomposed.iter().any(|s| s == QA_PRECOMPOSED),
            "the direction NFC cannot reach: stored apart, written together"
        );
        assert!(from_decomposed.iter().any(|s| s == QA_DECOMPOSED));
    }

    #[test]
    fn a_nukta_word_in_context_keeps_the_rest_of_its_letters() {
        // क़लम — "pen", spelled with the letter apart.
        let word = "\u{0915}\u{093C}लम";
        let forms = spellings(word);
        assert!(forms.iter().any(|s| s == word));
        assert!(
            forms.iter().any(|s| s == "\u{0958}लम"),
            "got {forms:?}"
        );
    }

    /// The other Indic exclusions live in their own blocks, and each script
    /// has its own nukta — Devanagari U+093C, Bengali U+09BC, Gurmukhi
    /// U+0A3C, Oriya U+0B3C. The scan has to find them there, and must not
    /// pair a letter with a neighbouring script's mark.
    #[test]
    fn exclusions_outside_devanagari_are_found_as_well() {
        // Bengali RRA: U+09DC = U+09A1 + Bengali nukta.
        assert_eq!(
            precomposed_exclusion('\u{09A1}', '\u{09BC}'),
            Some('\u{09DC}')
        );
        // Oriya RRA: U+0B5C = U+0B21 + Oriya nukta.
        assert_eq!(
            precomposed_exclusion('\u{0B21}', '\u{0B3C}'),
            Some('\u{0B5C}')
        );
        // Gurmukhi LLA: U+0A33 = U+0A32 + Gurmukhi nukta.
        assert_eq!(
            precomposed_exclusion('\u{0A32}', '\u{0A3C}'),
            Some('\u{0A33}')
        );
        // And the wrong script's mark composes with nothing.
        assert_eq!(precomposed_exclusion('\u{09A1}', '\u{093C}'), None);
    }

    /// Two ordinary letters are not a composition site, and must not cost a
    /// block scan to find that out — every adjacent pair of a Devanagari word
    /// would otherwise pay for one on every dictation.
    #[test]
    fn only_a_starter_followed_by_a_mark_is_a_composition_site() {
        assert_eq!(precomposed_exclusion('\u{0915}', '\u{0932}'), None, "क then ल");
        assert_eq!(precomposed_exclusion('a', 'b'), None);
        // A mark cannot be the base.
        assert_eq!(precomposed_exclusion('\u{093C}', '\u{093C}'), None);
        // The real site still answers.
        assert_eq!(
            precomposed_exclusion('\u{0915}', '\u{093C}'),
            Some('\u{0958}')
        );
    }

    /// Latin never reaches the block scan: NFC already produces `é`, so
    /// `compose` answering means there is nothing left to enumerate.
    #[test]
    fn a_composable_pair_is_not_an_exclusion() {
        assert_eq!(precomposed_exclusion('e', '\u{0301}'), None);
        assert_eq!(spellings("café").len(), 2, "just NFC and NFD");
    }

    /// Devanagari matras and halants have no precomposed form at all, so a
    /// word full of them enumerates once and the pattern stays small.
    #[test]
    fn a_word_with_no_exclusion_letters_enumerates_once() {
        assert_eq!(spellings("अच्छी"), vec!["अच्छी".to_string()]);
        assert_eq!(spellings("नमस्ते"), vec!["नमस्ते".to_string()]);
    }

    #[test]
    fn two_exclusion_letters_enumerate_every_combination() {
        // क़ + ज़ (U+095B = U+091C + nukta), both spelled apart.
        let word = "\u{0915}\u{093C}\u{091C}\u{093C}";
        let forms = spellings(word);
        for expected in [
            "\u{0958}\u{095B}",
            "\u{0958}\u{091C}\u{093C}",
            "\u{0915}\u{093C}\u{095B}",
            "\u{0915}\u{093C}\u{091C}\u{093C}",
        ] {
            assert!(forms.iter().any(|s| s == expected), "missing {expected:?} in {forms:?}");
        }
        assert!(forms.len() <= MAX_SPELLINGS);
    }

    #[test]
    fn the_enumeration_is_bounded() {
        // Five exclusion letters would be 32 subsets without the cap.
        let word = "\u{0915}\u{093C}".repeat(5);
        assert!(spellings(&word).len() <= MAX_SPELLINGS, "unbounded alternation");
    }

    #[test]
    fn degenerate_input_does_not_panic() {
        for w in ["", " ", "\u{093C}", "\u{0915}", "؀", "𝄞"] {
            let forms = spellings(w);
            assert!(!forms.is_empty(), "{w:?} produced nothing");
            let _ = fold(w);
        }
    }
}
