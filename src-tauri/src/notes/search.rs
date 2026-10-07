//! Turns a search-box string into an FTS5 `MATCH` expression.
//!
//! The input is cut into terms exactly where the index tokenizer cuts text
//! into tokens, so each term stands for one indexed token. Each term becomes
//! an FTS5 string followed by `*` (`"word"*`, a prefix match), or, when the
//! word can be spelled more than one way, an OR group of one such string per
//! spelling. The terms are joined by `AND`. Every character of the input
//! either ends up inside a quoted string or is dropped as a separator, so no
//! input can reach the query grammar as an operator, a column filter or a
//! syntax error.
//!
//! Notes search and History search both build their queries here.

use regex::Regex;
use std::sync::OnceLock;
use unicode_normalization::UnicodeNormalization;

/// The Unicode general categories the index counts as token characters,
/// written the way the `unicode61` tokenizer's `categories` option takes them.
/// A test checks that [`super::schema::SCHEMA`] declares the same list.
pub const TOKEN_CATEGORIES: &str = "L* N* Co Mn Mc";

/// A maximal run of token characters: one token, as the index would cut it.
///
/// The regex crate's Unicode tables are newer than SQLite's. For a character
/// only one of them knows, the two can disagree about where a token ends;
/// FTS5 tokenizes the inside of each quoted term again, so the cost is that
/// such pieces must then sit next to each other rather than anywhere.
fn token_run() -> &'static Regex {
    static RUN: OnceLock<Regex> = OnceLock::new();
    RUN.get_or_init(|| {
        let class: String = TOKEN_CATEGORIES
            .split_whitespace()
            .map(|category| format!(r"\p{{{}}}", category.trim_end_matches('*')))
            .collect();
        Regex::new(&format!("[{class}]+")).expect("the token pattern compiles")
    })
}

/// Whether a run holds a letter or a digit. A run of combining marks alone, or
/// of private-use characters alone, is not something a person searches for.
fn has_letter_or_digit(run: &str) -> bool {
    static LETTER_OR_DIGIT: OnceLock<Regex> = OnceLock::new();
    LETTER_OR_DIGIT
        .get_or_init(|| Regex::new(r"[\p{L}\p{N}]").expect("the pattern compiles"))
        .is_match(run)
}

/// Build the `MATCH` expression for `input`, or `""` when it holds nothing
/// searchable. The input is normalised to NFC first, so canonically
/// equivalent spellings build the same query.
pub fn sanitize_query(input: &str) -> String {
    let text: String = input.nfc().collect();
    token_run()
        .find_iter(&text)
        .map(|run| run.as_str())
        .filter(|run| has_letter_or_digit(run))
        .map(term)
        .collect::<Vec<_>>()
        .join(" AND ")
}

/// One search term, matched in every spelling of it that stored text can
/// hold.
///
/// The index keeps text as it was stored, and Sarvam's transcripts spell the
/// nukta letters precomposed (ड़ as U+095C). NFC splits those letters into
/// consonant + nukta, so an NFC query can never hold the precomposed form;
/// [`crate::canonical::spellings`] gives it back. One spelling (every ASCII
/// word) is one prefix string; several are an OR group in parentheses, which
/// is why the terms are joined by an explicit `AND`: FTS5 reads a space as
/// AND only between two strings, not next to a group.
fn term(word: &str) -> String {
    let spellings = crate::canonical::spellings(word);
    if spellings.len() == 1 {
        return prefix_term(word);
    }
    let group: Vec<String> = spellings.iter().map(|s| prefix_term(s)).collect();
    format!("({})", group.join(" OR "))
}

/// `term` as an FTS5 string followed by the prefix marker. A `"` inside the
/// string is written twice, the FTS5 rule for a literal quote.
fn prefix_term(term: &str) -> String {
    format!("\"{}\"*", term.replace('"', "\"\""))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::notes::{create_note, schema, search_notes, NewNote, SEARCH_LIMIT};
    use rusqlite::Connection;

    /// Every spelling of every term of a query this module built, unquoted
    /// again: one list per term, the spelling as typed (after NFC) first.
    fn spellings_of(query: &str) -> Vec<Vec<String>> {
        if query.is_empty() {
            return Vec::new();
        }
        query
            .split(" AND ")
            .map(|t| {
                let group = t.strip_prefix('(').and_then(|t| t.strip_suffix(')'));
                group
                    .unwrap_or(t)
                    .split(" OR ")
                    .map(|s| {
                        let inner = s
                            .strip_prefix('"')
                            .and_then(|s| s.strip_suffix("\"*"))
                            .unwrap_or_else(|| panic!("{s:?} is not a quoted prefix term"));
                        inner.replace("\"\"", "\"")
                    })
                    .collect()
            })
            .collect()
    }

    /// The terms of a query this module built, each as typed (after NFC).
    fn terms_of(query: &str) -> Vec<String> {
        spellings_of(query).into_iter().map(|mut s| s.remove(0)).collect()
    }

    /// The tokens FTS5 itself cuts `text` into, using the tokenizer declared on
    /// `notes_fts`, read back in order through an `fts5vocab` instance table.
    fn index_tokens(text: &str) -> Vec<String> {
        let at = schema::SCHEMA.find("tokenize=").expect("notes_fts names a tokenizer");
        let tokenizer = schema::SCHEMA[at..].lines().next().unwrap().trim();
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(&format!(
            "CREATE VIRTUAL TABLE scratch USING fts5(body, {tokenizer});
             CREATE VIRTUAL TABLE scratch_terms USING fts5vocab(scratch, 'instance');"
        ))
        .unwrap();
        conn.execute("INSERT INTO scratch(body) VALUES (?1)", [text]).unwrap();
        let mut stmt = conn
            .prepare("SELECT term FROM scratch_terms ORDER BY doc, col, offset")
            .unwrap();
        let tokens = stmt
            .query_map([], |r| r.get::<_, String>(0))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        tokens
    }

    fn db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(schema::SCHEMA).unwrap();
        conn
    }

    fn add(conn: &Connection, title: &str, content: &str) -> i64 {
        create_note(
            conn,
            &NewNote {
                title: Some(title.into()),
                content: Some(content.into()),
                ..Default::default()
            },
        )
        .unwrap()
    }

    fn found(conn: &Connection, query: &str) -> Vec<i64> {
        search_notes(conn, query, SEARCH_LIMIT)
            .unwrap_or_else(|e| panic!("searching {query:?} failed: {e}"))
            .iter()
            .map(|n| n.id)
            .collect()
    }

    #[test]
    fn the_schema_declares_the_same_token_categories() {
        let declared = format!("categories '{TOKEN_CATEGORIES}'");
        assert!(
            schema::SCHEMA.contains(&declared),
            "notes_fts must tokenize with {declared}"
        );
    }

    #[test]
    fn terms_split_exactly_where_the_index_splits() {
        let samples = [
            "Café naïve résumé Ångström",
            "नमस्ते दुनिया किताब स्कूल ज़रूर क़लम",
            "வணக்கம் உலகம்",
            "নমস্কার পৃথিবী",
            "Привет, мир! Ёлка",
            "東京タワー 北京 서울",
            "مَرْحَبًا بالعالم",
            "2026 ٤٥ १२३ 3.14",
            "snake_case_name well-known e-mail@example.com",
            "क्\u{200D}ष and می\u{200C}خواهم",
            "(brackets) [and] {braces} \"quotes\" 'single' #tag ^caret",
        ];
        for sample in samples {
            // Both sides get the same normalised form, so the comparison is
            // about token boundaries alone. Finding text stored in another
            // spelling is `history::store`'s nukta test.
            let text: String = sample.nfc().collect();
            let ours: Vec<String> = terms_of(&sanitize_query(&text))
                .iter()
                .map(|t| t.to_lowercase())
                .collect();
            let theirs: Vec<String> = index_tokens(&text)
                .iter()
                .map(|t| t.to_lowercase())
                .collect();
            assert_eq!(ours, theirs, "tokens of {sample:?}");
        }
    }

    #[test]
    fn the_query_table() {
        let cases: &[(&str, &str)] = &[
            // Words become quoted prefix terms, joined by AND.
            ("hello", r#""hello"*"#),
            ("hello world", r#""hello"* AND "world"*"#),
            ("  spaced   out  ", r#""spaced"* AND "out"*"#),
            // Operator words are ordinary terms.
            ("gold OR silver", r#""gold"* AND "OR"* AND "silver"*"#),
            ("AND NOT", r#""AND"* AND "NOT"*"#),
            ("NEAR(a b)", r#""NEAR"* AND "a"* AND "b"*"#),
            // Column filters, negation, required and initial-token markers,
            // and a typed prefix star all fall away as separators.
            ("title:plan", r#""title"* AND "plan"*"#),
            (
                "-minus +plus ^caret star*",
                r#""minus"* AND "plus"* AND "caret"* AND "star"*"#,
            ),
            // Unbalanced quotes and brackets.
            ("\"unterminated", r#""unterminated"*"#),
            ("(unbalanced", r#""unbalanced"*"#),
            ("say \"hi\" there", r#""say"* AND "hi"* AND "there"*"#),
            // Underscore, hyphen and apostrophe separate terms, as they
            // separate tokens in the index.
            ("snake_case", r#""snake"* AND "case"*"#),
            ("well-known", r#""well"* AND "known"*"#),
            ("don't", r#""don"* AND "t"*"#),
            // A word with more than one spelling is an OR group of them.
            ("Caf\u{00E9}", "(\"Caf\u{00E9}\"* OR \"Cafe\u{0301}\"*)"),
            (
                "\u{095B}\u{0930}\u{0942}\u{0930} ok",
                "(\"\u{091C}\u{093C}\u{0930}\u{0942}\u{0930}\"* OR \
                 \"\u{095B}\u{0930}\u{0942}\u{0930}\"*) AND \"ok\"*",
            ),
            // A run of marks alone, or of private-use characters alone, is not
            // a term; next to a letter they stay part of it.
            ("\u{0301}\u{0302} word", r#""word"*"#),
            ("\u{E000}\u{E001}", ""),
            ("\u{E000}x", "\"\u{E000}x\"*"),
        ];
        for (input, want) in cases {
            assert_eq!(&sanitize_query(input), want, "query for {input:?}");
        }
    }

    #[test]
    fn a_double_quote_is_never_inside_a_term() {
        for input in ["a\"b", "\"\"\"", "x\"\"y", "\u{201C}curly\u{201D}", "\"caf\u{00E9}\""] {
            for term in spellings_of(&sanitize_query(input)).concat() {
                assert!(!term.contains('"'), "{term:?} from {input:?}");
            }
        }
    }

    #[test]
    fn a_term_is_quoted_by_the_fts5_string_rule() {
        assert_eq!(prefix_term("plain"), r#""plain"*"#);
        assert_eq!(prefix_term("a\"b"), r#""a""b"*"#);
        assert_eq!(prefix_term("\"\""), "\"\"\"\"\"\"*");
    }

    #[test]
    fn hostile_input_is_searched_as_text() {
        let conn = db();
        let or = add(&conn, "Metals", "gold OR silver");
        let near = add(&conn, "Distance", "NEAR alpha beta");
        let title = add(&conn, "Filters", "title plan");
        let signs = add(&conn, "Signs", "minus plus caret starfish");
        let bracket = add(&conn, "close the parenthesis)", "unterminated quote");
        let not = add(&conn, "Logic", "AND NOT");

        for (query, want) in [
            ("OR", vec![or]),
            ("gold OR silver", vec![or]),
            ("NEAR(alpha beta)", vec![near]),
            ("title:plan", vec![title]),
            ("-minus", vec![signs]),
            ("+plus", vec![signs]),
            ("^caret", vec![signs]),
            ("star*", vec![signs]),
            ("\"unterminated", vec![bracket]),
            ("(parenthesis", vec![bracket]),
            ("AND NOT", vec![not]),
        ] {
            assert_eq!(found(&conn, query), want, "query {query:?}");
        }
        for query in ["\"", "(", ")", "*", "^", "-", "+", ":", "NEAR(", "\"a", "a\""] {
            assert!(
                search_notes(&conn, query, SEARCH_LIMIT).is_ok(),
                "{query:?} must not be a syntax error"
            );
        }
    }

    #[test]
    fn every_term_must_match_in_any_order_and_any_column() {
        let conn = db();
        let both = add(&conn, "Monsoon checklist", "pump repair before June");
        add(&conn, "Monsoon menu", "pakode and chai");
        add(&conn, "Pune flat", "pump needs a new washer");

        assert_eq!(found(&conn, "pump monsoon"), vec![both]);
        assert_eq!(found(&conn, "monsoon pump"), vec![both]);
        assert_eq!(found(&conn, "mons pum"), vec![both], "each term is a prefix");
        assert!(found(&conn, "monsoon diesel").is_empty());
    }

    #[test]
    fn a_decomposed_accent_finds_the_composed_note() {
        let conn = db();
        let id = add(&conn, "Caf\u{00E9} notes", "");
        assert_eq!(found(&conn, "Cafe\u{0301}"), vec![id]);
        assert_eq!(
            sanitize_query("Cafe\u{0301}"),
            sanitize_query("Caf\u{00E9}"),
            "canonically equivalent input builds one query"
        );
    }

    #[test]
    fn nothing_searchable_yields_the_empty_string() {
        // `_` is connector punctuation, which the index treats as a separator,
        // so `_` and `___` hold no term at all.
        for input in ["", "   ", "!!!", "-", "\"", "()", " \t\n ", "_", "___"] {
            assert_eq!(sanitize_query(input), "", "input {input:?}");
        }
    }

    /// Combining marks are token characters in the index, so a vowel sign or
    /// a virama never splits a word.
    #[test]
    fn combining_marks_stay_with_the_letter_they_belong_to() {
        // Devanagari: क + vowel sign I is one token, not two.
        assert_eq!(sanitize_query("क\u{093F}"), "\"क\u{093F}\"*");
        assert_eq!(sanitize_query("नमस्ते दुनिया"), "\"नमस्ते\"* AND \"दुनिया\"*");
        // Tamil and Bengali, same shape.
        assert_eq!(sanitize_query("வணக்கம் உலகம்"), "\"வணக்கம்\"* AND \"உலகம்\"*");
        assert_eq!(sanitize_query("নমস্কার পৃথিবী"), "\"নমস্কার\"* AND \"পৃথিবী\"*");
        // A stray mark before a letter is part of the same run, in the index
        // and here, so it stays in the term.
        assert_eq!(sanitize_query("\u{093F}क"), "\"\u{093F}क\"*");
        // Marks with no letter at all are not a token.
        assert_eq!(sanitize_query("\u{093F}\u{0942}"), "");
    }

    /// A nukta letter can be typed precomposed or as consonant plus nukta;
    /// normalising to NFC makes both spellings build the same query.
    #[test]
    fn nfc_folds_the_two_spellings_of_a_nukta_letter() {
        // क + nukta  vs  the precomposed क़.
        assert_eq!(
            sanitize_query("\u{0915}\u{093C}लम"),
            sanitize_query("\u{0958}लम")
        );
    }
}
