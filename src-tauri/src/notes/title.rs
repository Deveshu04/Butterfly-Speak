//! Auto-title: a short title for a note, written by the model when the
//! person asks for one.
//!
//! [`generate_title`] sends the start of the note under [`TITLE_SCAFFOLD`] and
//! passes the reply through [`clean_title_reply`]. A reply with no usable
//! title in it is an `Err` carrying a sentence for the person, never an empty
//! string, so a failed attempt cannot leave the note silently untitled.
//!
//! Note text and generated titles are never logged here. Log lines carry
//! counts and errors only.

use crate::canonical::belongs_in_word;
use crate::format::backend::Backend;

// ---------------------------------------------------------------------------
// The request.
// ---------------------------------------------------------------------------

/// The whole system prompt of a title request.
///
/// `chat::note_title` sends it in place of the rewrite envelope and appends
/// the end-marker rule after it, so it asks for a bare title
/// without ever saying that nothing may follow the title. Six words is what
/// fits on one line of a Notes list row, a Ctrl+K palette row and the editor's
/// title field at the default window size, in English and in Tamil, whose
/// words run longest on screen.
pub const TITLE_SCAFFOLD: &str = "You write short titles for a person's notes. Read the note in the user's message and give it a title of two to six words that says what the note is mainly about.
Write the title in the same language and script as the note itself. When the note is written in English, the title is in English words and Latin letters, never in Hindi or Devanagari, even if the note mentions Indian names or places. When the note is written in Hindi, Tamil or another Indian language, the title is in that language and in the note's own script, or in Latin letters when the note is typed that way.
Write the title by itself on the first line, with no quotation marks or brackets around it and no label such as \"Title:\" in front of it.";

/// How much of the note goes into a title request, counted in characters.
/// Measured on long English, Hindi and Tamil notes: titles stopped getting
/// better past this point, while every extra character is still paid for.
pub const TITLE_INPUT_CHARS: usize = 1500;

/// The longest reply accepted as a title, inclusive: six words, the top of
/// the scaffold's range, at twenty characters each. Vowel signs and viramas
/// are characters of their own, so a long Tamil or Malayalam word reaches
/// fifteen or more; a paragraph of prose is still far past the cap.
pub const MAX_TITLE_CHARS: usize = 120;

// ---------------------------------------------------------------------------
// Cleaning the reply.
// ---------------------------------------------------------------------------
//
// The scaffold asks for a title with nothing around it, and the cleaning
// takes off what a model adds anyway: quotation marks or brackets around the
// whole title, and whatever it writes after the first line. Marks are paired
// the way a reader pairs them, so a quoted name inside the title keeps its
// marks even when it sits at one end.

/// Marks a model puts around a title, as opening and closing partners:
/// straight and curly quotes, the low opening quotes that close with a high
/// one, guillemets, and round and square brackets.
const PARTNERS: [(char, char); 12] = [
    ('"', '"'),
    ('\u{201C}', '\u{201D}'), // “ ”
    ('\u{201E}', '\u{201C}'), // „ “
    ('\u{201E}', '\u{201D}'), // „ ”
    ('\'', '\''),
    ('\u{2018}', '\u{2019}'), // ‘ ’
    ('\u{201A}', '\u{2018}'), // ‚ ‘
    ('\u{201A}', '\u{2019}'), // ‚ ’
    ('\u{00AB}', '\u{00BB}'), // « »
    ('\u{2039}', '\u{203A}'), // ‹ ›
    ('(', ')'),
    ('[', ']'),
];

/// The two marks that also write an apostrophe: the typewriter one and
/// U+2019, the one typography uses.
const ELISION_MARKS: [char; 2] = ['\'', '\u{2019}'];

fn is_mark(c: char) -> bool {
    PARTNERS.iter().any(|&(open, close)| c == open || c == close)
}

fn can_open(c: char) -> bool {
    PARTNERS.iter().any(|&(open, _)| c == open)
}

/// Whether the character at `at` is an apostrophe inside a word, as in
/// "Sharma ji's": part of the title, never a quotation mark.
fn joins_a_word(chars: &[char], at: usize) -> bool {
    ELISION_MARKS.contains(&chars[at])
        && at.checked_sub(1).is_some_and(|i| belongs_in_word(chars[i]))
        && chars.get(at + 1).is_some_and(|&c| belongs_in_word(c))
}

/// For each character, where its partner is, when it is a mark that has one.
///
/// One pass from the left with a stack of marks still open: a mark that
/// closes the newest open one pairs with it, any other mark that can open is
/// pushed, and a mark that does neither has no partner.
fn partners(chars: &[char]) -> Vec<Option<usize>> {
    let mut partner = vec![None; chars.len()];
    let mut open: Vec<usize> = Vec::new();
    for (at, &c) in chars.iter().enumerate() {
        if !is_mark(c) || joins_a_word(chars, at) {
            continue;
        }
        match open.last() {
            Some(&top) if PARTNERS.contains(&(chars[top], c)) => {
                open.pop();
                partner[top] = Some(at);
                partner[at] = Some(top);
            }
            _ if can_open(c) => open.push(at),
            _ => {}
        }
    }
    partner
}

/// One cut off the ends of `title`, or `None` when nothing more comes off.
///
/// The first and last characters come off together when they make a pair and
/// what is left has no loose mark in it, or when each is the other's partner
/// on the stack. The first test covers a title that quotes something in the
/// same style as its wrapper, or ends on an apostrophe, where the stack would
/// pair a wrapper with a mark inside. Failing both, a loose mark at either end
/// comes off alone: a quote the model opened and never closed, or one end of a
/// pair it wrote in two different styles.
fn cut_once(title: &[char]) -> Option<&[char]> {
    let last = title.len().checked_sub(1)?;
    if last > 0 && PARTNERS.contains(&(title[0], title[last])) {
        let inside = trim_spaces(&title[1..last]);
        if !has_loose_mark(inside) {
            return Some(inside);
        }
    }
    let partner = partners(title);
    if last > 0 && partner[0] == Some(last) {
        return Some(&title[1..last]);
    }
    if is_loose(title, &partner, 0) {
        return Some(&title[1..]);
    }
    if is_loose(title, &partner, last) {
        return Some(&title[..last]);
    }
    None
}

/// Whether the character at `at` is a loose mark: one with no partner that
/// is not an apostrophe leaning on the word beside it ("'90s", "the
/// Sharmas'").
fn is_loose(title: &[char], partner: &[Option<usize>], at: usize) -> bool {
    let word_at = |i: Option<usize>| i.and_then(|i| title.get(i)).is_some_and(|&c| belongs_in_word(c));
    let leans_on_word =
        ELISION_MARKS.contains(&title[at]) && (word_at(at.checked_sub(1)) || word_at(Some(at + 1)));
    is_mark(title[at]) && !joins_a_word(title, at) && partner[at].is_none() && !leans_on_word
}

fn has_loose_mark(title: &[char]) -> bool {
    let partner = partners(title);
    (0..title.len()).any(|at| is_loose(title, &partner, at))
}

fn trim_spaces(chars: &[char]) -> &[char] {
    let start = chars.iter().position(|c| !c.is_whitespace()).unwrap_or(chars.len());
    let end = chars.iter().rposition(|c| !c.is_whitespace()).map_or(start, |i| i + 1);
    &chars[start..end]
}

/// Turn the model's reply into a title, or into `""` when it holds none.
///
/// Reads only the first line with something on it, since a title is one line
/// and a reply can carry on past it. Cuts at the ends with [`cut_once`] until
/// nothing more comes off, trimming spaces after every cut. A result with no
/// letter or digit in it (nothing, marks, symbols) or longer than
/// [`MAX_TITLE_CHARS`] characters is not a title. Everything works on whole
/// characters and only the ends are ever cut, so the vowel signs, viramas and
/// marks inside a title come through as the model wrote them.
pub fn clean_title_reply(raw: &str) -> String {
    let line = raw.lines().map(str::trim).find(|line| !line.is_empty());
    let chars: Vec<char> = line.unwrap_or("").chars().collect();
    let mut title = &chars[..];
    while let Some(rest) = cut_once(title) {
        title = trim_spaces(rest);
    }
    if !title.iter().any(|c| c.is_alphanumeric()) || title.len() > MAX_TITLE_CHARS {
        return String::new();
    }
    title.iter().collect()
}

// ---------------------------------------------------------------------------
// The call.
// ---------------------------------------------------------------------------

/// Ask the model for a title for `text`, and return it cleaned.
///
/// Runs over [`crate::sarvam::chat::note_title`]: the same engine, sampling
/// and per-request end marker as a note action, on the notes lane's
/// budgets, with [`TITLE_SCAFFOLD`] as the whole system prompt. A reply the
/// API reports as cut off is therefore an `Err` here, never a half-title.
///
/// Every failure is an `Err` with a sentence written to be shown as-is, and
/// the three kinds read differently: an empty note, a reply with no usable
/// title in it, and a request that failed. The last one never carries the
/// transport's own words, because `reqwest` renders the URL it was handed and
/// for the custom endpoint that is a pasted string that can hold a
/// credential. It is logged with its whole source chain instead and returned
/// as one fixed sentence.
pub async fn generate_title(
    http: &reqwest::Client,
    backend: &Backend,
    text: &str,
) -> anyhow::Result<String> {
    if text.trim().is_empty() {
        anyhow::bail!("There's nothing in this note to make a title from yet.");
    }
    let head: String = text.trim().chars().take(TITLE_INPUT_CHARS).collect();
    // `note_title` runs on the notes lane's deadline and output budget, and
    // sends the scaffold as the whole system prompt instead of nesting it in
    // the rewrite envelope. There is no user override for it: auto-title has
    // no prompt rules of its own for a person to edit.
    let raw = crate::sarvam::chat::note_title(http, backend, TITLE_SCAFFOLD, &head)
        .await
        .map_err(|e| {
            // `{e:#}` — the whole source chain, never `{e}`. Counts and
            // statuses only; never the note, never the title — and through
            // `redact_urls` first, because reqwest's Display embeds the request
            // URL, which for the custom slot is a pasted string that can carry
            // a credential (`https://user:pw@host`, `?api_key=`).
            let reason = crate::format::backend::redact_urls(&format!("{e:#}"));
            tracing::warn!("title call failed: {reason}");
            anyhow::anyhow!(crate::sarvam::chat::failure_sentence(&e)
                .unwrap_or("Couldn't reach the model to make a title. Try again."))
        })?;
    let title = clean_title_reply(&raw);
    if title.is_empty() {
        // Counts only — never the reply. A reply that cleaned to nothing
        // either had no letter or digit in it or ran past
        // `MAX_TITLE_CHARS`, and the length is what tells those apart.
        tracing::warn!(
            reply_chars = raw.chars().count(),
            "title reply had nothing usable in it"
        );
        anyhow::bail!("The model didn't return a usable title. Try again, or type one yourself.");
    }
    Ok(title)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clean(raw: &str) -> String {
        clean_title_reply(raw)
    }

    #[test]
    fn the_scaffold_keeps_its_language_rule() {
        assert!(
            TITLE_SCAFFOLD.contains("the same language and script as the note itself"),
            "the scaffold lost its language rule"
        );
    }

    // -- Replies to English notes -------------------------------------------

    mod english {
        use super::*;

        #[test]
        fn one_pair_of_marks_around_the_title_comes_off() {
            for (raw, want) in [
                ("\"Ration card renewal\"", "Ration card renewal"),
                ("“Ration card renewal”", "Ration card renewal"),
                ("'Gas cylinder booking'", "Gas cylinder booking"),
                ("‘Gas cylinder booking’", "Gas cylinder booking"),
                ("„Society maintenance meeting“", "Society maintenance meeting"),
                ("«Aadhaar address update»", "Aadhaar address update"),
                ("(Train tickets to Pune)", "Train tickets to Pune"),
                ("[Train tickets to Pune]", "Train tickets to Pune"),
            ] {
                assert_eq!(clean(raw), want, "input {raw}");
            }
        }

        #[test]
        fn nested_pairs_come_off_from_the_outside_in() {
            for (raw, want) in [
                ("\"“Rabi crop plan”\"", "Rabi crop plan"),
                ("('“Monsoon trip to Coorg”')", "Monsoon trip to Coorg"),
                ("  “  Rabi crop plan  ”  ", "Rabi crop plan"),
                ("\t[ « School fee dates » ]\t", "School fee dates"),
            ] {
                assert_eq!(clean(raw), want, "input {raw:?}");
            }
        }

        /// A mark whose partner is inside the title belongs to the title,
        /// even at an end. Two quoted names at the two ends are two pairs,
        /// not one wrapper.
        #[test]
        fn marks_paired_inside_the_title_belong_to_it() {
            for title in [
                "Minutes from “Kaveri Sync”",
                "‘Green Commute’ survey results",
                "“Kaveri” versus “Godavari”",
                "Budget plan (draft)",
                "(Draft) budget plan",
            ] {
                assert_eq!(clean(title), title, "input {title}");
            }
        }

        /// A wrapper in the same style as a quote inside the title, or around
        /// a title that ends on an apostrophe, still comes off.
        #[test]
        fn a_wrapper_matching_a_mark_inside_still_comes_off() {
            for (raw, want) in [
                ("\"Minutes from \"Kaveri Sync\"\"", "Minutes from \"Kaveri Sync\""),
                ("„Minutes from “Kaveri Sync”“", "Minutes from “Kaveri Sync”"),
                ("'Rock 'n' roll night'", "Rock 'n' roll night"),
                ("\"\"Ration card renewal\"\"", "Ration card renewal"),
                ("'Dinner at the Sharmas''", "Dinner at the Sharmas'"),
                ("‘Dinner at the Iyers’’", "Dinner at the Iyers’"),
            ] {
                assert_eq!(clean(raw), want, "input {raw}");
            }
        }

        /// An apostrophe written against a word is part of it: an elided
        /// year, a plural possessive, or one inside a word, which is never
        /// taken as a quotation mark.
        #[test]
        fn an_apostrophe_leaning_on_a_word_is_kept() {
            for title in [
                "Dinner at the Sharmas'",
                "Dinner at the Sharmas’",
                "'90s film songs to download",
                "’90s film songs to download",
            ] {
                assert_eq!(clean(title), title, "input {title}");
            }
            for (raw, want) in [
                ("“Sharma ji's retirement party”", "Sharma ji's retirement party"),
                ("'Rahul's tuition fees'", "Rahul's tuition fees"),
            ] {
                assert_eq!(clean(raw), want, "input {raw}");
            }
        }

        /// A quote opened and never closed, or a pair written in two styles,
        /// leaves marks with no partner; each comes off its own end.
        #[test]
        fn a_mark_with_no_partner_drops_off_its_end() {
            for (raw, want) in [
                ("“Electricity bill dispute", "Electricity bill dispute"),
                ("Electricity bill dispute»", "Electricity bill dispute"),
                ("(Electricity bill dispute", "Electricity bill dispute"),
                ("‘Electricity bill dispute", "Electricity bill dispute"),
                ("“Society maintenance meeting\"", "Society maintenance meeting"),
                ("Rahul's tuition fees”", "Rahul's tuition fees"),
            ] {
                assert_eq!(clean(raw), want, "input {raw}");
            }
        }

        /// A model sometimes explains its title or offers another one; only
        /// the first line that has something on it is read.
        #[test]
        fn a_second_line_is_never_read() {
            for (raw, want) in [
                ("Ration card renewal\nThis title sums up the note.", "Ration card renewal"),
                ("\n\n“Ration card renewal”\r\n\r\nOr: Ration card update", "Ration card renewal"),
            ] {
                assert_eq!(clean(raw), want, "input {raw:?}");
            }
        }
    }

    // -- Replies to notes in Indian languages -------------------------------

    mod indic {
        use super::*;

        /// Only the ends are cut, so every vowel sign, virama and chillu in
        /// between comes out as the model wrote it.
        #[test]
        fn wrappers_come_off_and_every_sign_survives() {
            for (raw, want) in [
                ("“बिजली बिल विवाद”", "बिजली बिल विवाद"),
                ("\"வாராந்திர கூட்டம்\"", "வாராந்திர கூட்டம்"),
                ("«পূজার কেনাকাটা»", "পূজার কেনাকাটা"),
                ("(ഓണം യാത്രാ പദ്ധതി)", "ഓണം യാത്രാ പദ്ധതി"),
                ("‘ਕਣਕ ਦੀ ਵਾਢੀ’", "ਕਣਕ ਦੀ ਵਾਢੀ"),
            ] {
                let got = clean(raw);
                assert_eq!(got, want, "input {raw}");
                assert_eq!(got.chars().count(), want.chars().count());
            }
        }

        #[test]
        fn a_mark_with_no_partner_drops_off_an_indic_title() {
            for (raw, want) in [
                ("\"गैस सिलेंडर बुकिंग", "गैस सिलेंडर बुकिंग"),
                ("பொங்கல் பயணத் திட்டம்”", "பொங்கல் பயணத் திட்டம்"),
                ("«दिवाली की खरीदारी", "दिवाली की खरीदारी"),
            ] {
                assert_eq!(clean(raw), want, "input {raw}");
            }
        }

        #[test]
        fn a_quoted_phrase_opening_an_indic_title_stays() {
            for title in ["“जल जीवन” बैठक के नोट्स", "‘ரேஷன் கார்டு’ புதுப்பித்தல்"] {
                assert_eq!(clean(title), title, "input {title}");
            }
        }

        /// A model that titles a Hindi note in Hindi sometimes adds an
        /// English rendering underneath; the title is the first line.
        #[test]
        fn a_translation_under_the_title_is_not_read() {
            assert_eq!(clean("मासिक बजट\nMonthly budget"), "मासिक बजट");
        }

        /// Six long words, with every vowel sign and virama counted as a
        /// character of its own, still fit.
        #[test]
        fn six_long_indic_words_fit_under_the_cap() {
            for title in [
                "மாவட்ட ஆட்சியர் அலுவலக கூட்டத்திற்கான முன்னேற்பாடுகள் பட்டியல்",
                "പഞ്ചായത്ത് തിരഞ്ഞെടുപ്പ് പ്രചാരണ പ്രവർത്തനങ്ങളുടെ അവലോകന യോഗം",
            ] {
                assert_eq!(clean(title), title, "input {title}");
            }
        }

        /// The cap is inclusive and counts characters: a three-byte
        /// Devanagari letter counts once, the same as a Latin one.
        #[test]
        fn the_cap_counts_characters_not_bytes() {
            for letter in ["a", "क"] {
                let at_cap = letter.repeat(MAX_TITLE_CHARS);
                assert_eq!(clean(&at_cap), at_cap, "{letter} at the cap");
                let over = letter.repeat(MAX_TITLE_CHARS + 1);
                assert_eq!(clean(&over), "", "{letter} one past the cap");
            }
        }
    }

    // -- Replies with no title in them --------------------------------------

    mod no_title {
        use super::*;

        #[test]
        fn a_reply_with_nothing_on_any_line() {
            for raw in ["", "\t \r\n  ", "\n\r\n"] {
                assert_eq!(clean(raw), "", "input {raw:?}");
            }
        }

        #[test]
        fn marks_with_nothing_between_them() {
            for raw in ["\"\"", "“ ”", "«“”»", "()", "[ ]", "'", " ’ ", "»", "„", "\"'\""] {
                assert_eq!(clean(raw), "", "input {raw:?}");
            }
        }

        /// A title has to have a letter or a digit in it somewhere.
        #[test]
        fn symbols_without_a_letter_or_digit() {
            for raw in ["...", "—", "?!", "★★★"] {
                assert_eq!(clean(raw), "", "input {raw:?}");
            }
        }

        #[test]
        fn a_paragraph_of_prose() {
            let prose = "The note describes the society maintenance meeting held on Sunday, \
                         where residents discussed the lift repair, the water tank cleaning \
                         schedule and the new parking rules for visitors.";
            assert_eq!(clean(prose), "");
        }
    }

    // -- The call, against the loopback stub in `notes::actions` ------------

    mod the_call {
        use super::*;
        use crate::notes::actions::stub;

        /// The prompt reaches the model, only the first [`TITLE_INPUT_CHARS`]
        /// of the note goes with it, and the answer comes back cleaned.
        #[tokio::test]
        async fn the_prompt_and_a_capped_note_reach_the_model() {
            let s = stub::chat_once("“Ration card renewal”", stub::WITH_MARKER).await;
            let mut backend = Backend::sarvam("k", "sarvam-105b");
            backend.base_url = s.url.clone();

            let long = "क".repeat(TITLE_INPUT_CHARS + 500);
            let title = generate_title(&reqwest::Client::new(), &backend, &long)
                .await
                .expect("the stub answers with a marker");
            assert_eq!(title, "Ration card renewal", "the quotes should be gone");

            let sent = s.request().await;
            let system = sent["messages"][0]["content"].as_str().unwrap();
            // Starting with the scaffold proves it is the whole system prompt
            // and not something wrapped in the rewrite envelope.
            assert!(
                system.starts_with(TITLE_SCAFFOLD),
                "the system prompt should open with the title scaffold: {system}"
            );
            let note = sent["messages"][1]["content"].as_str().unwrap();
            assert_eq!(
                note.chars().count(),
                TITLE_INPUT_CHARS,
                "the note should be cut to the input cap, in characters"
            );
        }

        /// A reply the cleaning rejects is an `Err` the caller can show,
        /// never a silent empty title.
        #[tokio::test]
        async fn an_unusable_reply_is_an_error_not_an_empty_string() {
            let s = stub::chat_once(&"a".repeat(MAX_TITLE_CHARS + 20), stub::WITH_MARKER).await;
            let mut backend = Backend::sarvam("k", "sarvam-105b");
            backend.base_url = s.url.clone();

            let err = generate_title(&reqwest::Client::new(), &backend, "some notes")
                .await
                .expect_err("a reply past the length cap is not a title")
                .to_string();
            assert!(err.contains("usable title"), "got: {err}");
        }

        /// A transport failure and an unusable reply are different sentences,
        /// so the person can tell a broken connection from a bad answer, and
        /// neither carries the endpoint's address.
        #[tokio::test]
        async fn a_failed_call_says_so_without_naming_the_endpoint() {
            let s = stub::chat_once_cut_off("Ration card renewal").await;
            let mut backend = Backend::sarvam("k", "sarvam-105b");
            backend.base_url = s.url.clone();

            let err = generate_title(&reqwest::Client::new(), &backend, "some notes")
                .await
                .expect_err("a reply the API cut off is a truncation")
                .to_string();
            assert!(err.contains("Couldn't reach the model"), "got: {err}");
            assert!(!err.contains("usable title"), "the two must not collapse");
            assert!(
                !err.contains(&s.host()),
                "the endpoint leaked into a user-facing message: {err}"
            );
        }

        #[tokio::test]
        async fn an_empty_note_never_reaches_the_model() {
            let backend = Backend::sarvam("k", "m");
            let err = generate_title(&reqwest::Client::new(), &backend, "  \n ")
                .await
                .expect_err("an empty note has nothing to title")
                .to_string();
            assert!(err.contains("nothing in this note"), "got: {err}");
        }
    }
}
