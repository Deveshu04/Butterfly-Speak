//! Exporting one note to a file the user picks.
//!
//! Both formats start from [`super::body_source`], the text the editor shows.
//! `.md` writes it byte for byte; `.txt` writes a plain-text rendering that
//! drops the Markdown syntax and keeps the words ([`strip_markdown`]).

use super::Note;

/// The most characters of a title the suggested file name keeps.
pub const MAX_NAME_CHARS: usize = 200;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    Md,
    Txt,
}

impl Format {
    /// Anything but `txt` (ignoring ASCII case and surrounding spaces) is
    /// Markdown, which writes the note unchanged and so can lose nothing.
    pub fn parse(raw: &str) -> Self {
        if raw.trim().eq_ignore_ascii_case("txt") {
            Format::Txt
        } else {
            Format::Md
        }
    }

    pub fn ext(self) -> &'static str {
        match self {
            Format::Md => "md",
            Format::Txt => "txt",
        }
    }

    /// The name the save dialog shows for this format's file-type filter.
    pub fn filter_label(self) -> &'static str {
        match self {
            Format::Md => "Markdown",
            Format::Txt => "Text",
        }
    }
}

/// Render Markdown as plain text: the syntax goes, the words stay.
///
/// Works a line at a time. The fence lines of a fenced code block are dropped
/// and the lines between them kept exactly. Elsewhere, blockquote and heading
/// markers at the start of a line go, then the inline syntax: emphasis,
/// strikethrough, code spans, links (their text stays) and images (their alt
/// text stays). List markers, and characters that only look like markup, are
/// left as written. The result is trimmed.
pub fn strip_markdown(source: &str) -> String {
    let mut lines: Vec<String> = Vec::new();
    let mut fence: Option<Fence> = None;
    for line in source.lines() {
        match fence {
            Some(open) if open.closed_by(line) => fence = None,
            Some(_) => lines.push(line.to_string()),
            None => match Fence::opened_by(line) {
                Some(open) => fence = Some(open),
                None => lines.push(strip_inline(heading_text(unquote(line)))),
            },
        }
    }
    lines.join("\n").trim().to_string()
}

/// The opening line of a fenced code block: three or more backticks or tildes.
#[derive(Clone, Copy)]
struct Fence {
    mark: char,
    len: usize,
}

impl Fence {
    fn opened_by(line: &str) -> Option<Fence> {
        let rest = unindent(line)?;
        let mark = rest.chars().next().filter(|c| *c == '`' || *c == '~')?;
        let len = rest.chars().take_while(|c| *c == mark).count();
        (len >= 3).then_some(Fence { mark, len })
    }

    /// A closing fence is a run of the same character, at least as long,
    /// with nothing after it.
    fn closed_by(self, line: &str) -> bool {
        unindent(line).is_some_and(|rest| {
            let len = rest.chars().take_while(|c| *c == self.mark).count();
            // The mark is ASCII, so `len` characters are `len` bytes.
            len >= self.len && rest[len..].trim().is_empty()
        })
    }
}

/// `line` without up to three leading spaces, or `None` when it is indented
/// further than a block marker may be.
fn unindent(line: &str) -> Option<&str> {
    let spaces = line.len() - line.trim_start_matches(' ').len();
    (spaces <= 3).then(|| &line[spaces..])
}

/// `line` without its blockquote markers, however deeply nested: each is a
/// `>` at the start, with one optional space after it.
fn unquote(line: &str) -> &str {
    let mut rest = line;
    while let Some(after) = unindent(rest).and_then(|r| r.strip_prefix('>')) {
        rest = after.strip_prefix(' ').unwrap_or(after);
    }
    rest
}

/// The text of a heading line (one to six `#`, then a space or the end of
/// the line), without the opening `#`s or an optional closing run of them.
/// Any other line comes back unchanged.
fn heading_text(line: &str) -> &str {
    let Some(rest) = unindent(line) else {
        return line;
    };
    let level = rest.bytes().take_while(|b| *b == b'#').count();
    let after = &rest[level..];
    let spaced = after.is_empty() || after.starts_with(|c: char| c == ' ' || c == '\t');
    if level == 0 || level > 6 || !spaced {
        return line;
    }
    let text = after.trim();
    // A closing run counts only after a space, or when it is all there is.
    let open = text.trim_end_matches('#');
    if open.is_empty() {
        ""
    } else if open.ends_with(|c: char| c == ' ' || c == '\t') {
        open.trim_end()
    } else {
        text
    }
}

/// A stretch of a line: literal text, or a run of one emphasis delimiter
/// with what its neighbours allow it to do.
enum Piece {
    Text(String),
    Run {
        mark: char,
        len: usize,
        opens: bool,
        closes: bool,
    },
}

/// Remove the inline syntax from one line of text.
fn strip_inline(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut pieces: Vec<Piece> = Vec::new();
    let mut plain = String::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        match c {
            // An escaped character is never syntax. Both characters stay.
            '\\' if chars.get(i + 1).is_some_and(|n| n.is_ascii_punctuation()) => {
                plain.push(c);
                plain.push(chars[i + 1]);
                i += 2;
            }
            '`' => {
                let len = run_length(&chars, i);
                match closing_backticks(&chars, i + len, len) {
                    Some(end) => {
                        plain.push_str(&code_span_text(&chars[i + len..end]));
                        i = end + len;
                    }
                    None => {
                        plain.extend(&chars[i..i + len]);
                        i += len;
                    }
                }
            }
            '!' | '[' => {
                let open = if c == '!' { i + 1 } else { i };
                match link_at(&chars, open) {
                    Some((label, next)) => {
                        plain.push_str(&strip_inline(&label));
                        i = next;
                    }
                    None => {
                        plain.push(c);
                        i += 1;
                    }
                }
            }
            '*' | '_' | '~' => {
                let len = run_length(&chars, i);
                let before = i.checked_sub(1).map(|j| chars[j]);
                let after = chars.get(i + len).copied();
                let (opens, closes) = flanking(c, before, after);
                // Strikethrough is exactly two tildes; any other run is text.
                let (opens, closes) = if c == '~' && len != 2 {
                    (false, false)
                } else {
                    (opens, closes)
                };
                if !plain.is_empty() {
                    pieces.push(Piece::Text(std::mem::take(&mut plain)));
                }
                pieces.push(Piece::Run { mark: c, len, opens, closes });
                i += len;
            }
            _ => {
                plain.push(c);
                i += 1;
            }
        }
    }
    if !plain.is_empty() {
        pieces.push(Piece::Text(plain));
    }
    pair_delimiters(&mut pieces);

    let mut out = String::new();
    for piece in pieces {
        match piece {
            Piece::Text(text) => out.push_str(&text),
            Piece::Run { mark, len, .. } => out.extend(std::iter::repeat(mark).take(len)),
        }
    }
    out
}

/// How many times the character at `start` repeats from there.
fn run_length(chars: &[char], start: usize) -> usize {
    chars[start..].iter().take_while(|c| **c == chars[start]).count()
}

/// Where a backtick run of exactly `len` starts, at or after `from`.
fn closing_backticks(chars: &[char], from: usize, len: usize) -> Option<usize> {
    let mut j = from;
    while j < chars.len() {
        if chars[j] == '`' {
            let run = run_length(chars, j);
            if run == len {
                return Some(j);
            }
            j += run;
        } else {
            j += 1;
        }
    }
    None
}

/// A code span's text. One space is dropped from each end when both ends have
/// one, which is how a span that starts or ends with a backtick is written.
fn code_span_text(inner: &[char]) -> String {
    let text: String = inner.iter().collect();
    let padded = text.len() >= 2 && text.starts_with(' ') && text.ends_with(' ');
    if padded && !text.trim().is_empty() {
        text[1..text.len() - 1].to_string()
    } else {
        text
    }
}

/// A link whose `[` is at `open`: its text, and the index just past its
/// closing `)`. `None` unless the `]` is followed straight away by `(`.
fn link_at(chars: &[char], open: usize) -> Option<(String, usize)> {
    if chars.get(open) != Some(&'[') {
        return None;
    }
    let close = matching(chars, open, '[', ']')?;
    if chars.get(close + 1) != Some(&'(') {
        return None;
    }
    let end = matching(chars, close + 1, '(', ')')?;
    Some((chars[open + 1..close].iter().collect(), end + 1))
}

/// The index of the bracket that closes the one at `open`, counting nesting
/// and skipping escaped characters.
fn matching(chars: &[char], open: usize, left: char, right: char) -> Option<usize> {
    let mut depth = 0usize;
    let mut j = open;
    while j < chars.len() {
        let c = chars[j];
        if c == '\\' {
            j += 2;
            continue;
        }
        if c == left {
            depth += 1;
        } else if c == right {
            depth -= 1;
            if depth == 0 {
                return Some(j);
            }
        }
        j += 1;
    }
    None
}

/// Whether a delimiter run between `before` and `after` can open and close
/// emphasis. A run opens when it leans on the text after it and closes when
/// it leans on the text before it; an underscore inside a word does neither,
/// so `file_name` stays as written.
fn flanking(mark: char, before: Option<char>, after: Option<char>) -> (bool, bool) {
    let space = |c: Option<char>| c.map_or(true, char::is_whitespace);
    let punct = |c: Option<char>| c.is_some_and(|c| !c.is_alphanumeric() && !c.is_whitespace());
    let leans_right = !space(after) && (!punct(after) || space(before) || punct(before));
    let leans_left = !space(before) && (!punct(before) || space(after) || punct(after));
    if mark == '_' {
        (
            leans_right && (!leans_left || punct(before)),
            leans_left && (!leans_right || punct(after)),
        )
    } else {
        (leans_right, leans_left)
    }
}

/// Pair closing runs with the nearest open run of the same character and
/// shorten both by what they share. Whatever is left unpaired stays as text.
fn pair_delimiters(pieces: &mut [Piece]) {
    fn run(piece: &Piece) -> Option<(char, usize)> {
        match piece {
            Piece::Run { mark, len, .. } => Some((*mark, *len)),
            Piece::Text(_) => None,
        }
    }
    fn shorten(piece: &mut Piece, by: usize) {
        if let Piece::Run { len, .. } = piece {
            *len -= by;
        }
    }

    let mut waiting: Vec<usize> = Vec::new();
    for i in 0..pieces.len() {
        let Piece::Run { mark, opens, closes, .. } = pieces[i] else {
            continue;
        };
        if closes {
            while let Some((_, left)) = run(&pieces[i]).filter(|(_, len)| *len > 0) {
                let Some(pos) = waiting
                    .iter()
                    .rposition(|&o| run(&pieces[o]).is_some_and(|(m, _)| m == mark))
                else {
                    break;
                };
                let opener = waiting[pos];
                let (_, available) = run(&pieces[opener]).unwrap_or((mark, 0));
                let used = available.min(left);
                shorten(&mut pieces[opener], used);
                shorten(&mut pieces[i], used);
                // Runs opened inside this pair cannot close past it.
                waiting.truncate(pos + 1);
                if run(&pieces[opener]).is_some_and(|(_, len)| len == 0) {
                    waiting.pop();
                }
            }
        }
        if opens && run(&pieces[i]).is_some_and(|(_, len)| len > 0) {
            waiting.push(i);
        }
    }
}

/// What gets written to the file: the same document either way, plain-texted
/// for `.txt`.
pub fn body(note: &Note, format: Format) -> String {
    let source = super::body_source(note);
    match format {
        Format::Md => source.to_string(),
        Format::Txt => strip_markdown(source),
    }
}

/// The name the save dialog opens with.
///
/// Case and spacing are kept, and the name goes through the same Windows
/// sanitiser the mirror uses, so a note called `Q1/Q2` or `CON` produces a
/// name the save dialog will accept.
pub fn default_file_name(title: &str, format: Format) -> String {
    let name = super::mirror::sanitize_component(title, "Untitled", MAX_NAME_CHARS);
    format!("{name}.{}", format.ext())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::notes::{create_note, schema, update_note, NewNote, NoteUpdate};
    use rusqlite::Connection;

    fn note_with(content: &str, polished: Option<&str>) -> Note {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(schema::SCHEMA).unwrap();
        let id = create_note(
            &conn,
            &NewNote {
                title: Some("A note".into()),
                content: Some(content.into()),
                ..Default::default()
            },
        )
        .unwrap();
        if let Some(text) = polished {
            update_note(
                &conn,
                id,
                &NoteUpdate {
                    polished_body: Some(Some(text.into())),
                    ..Default::default()
                },
            )
            .unwrap();
        }
        crate::notes::get_note(&conn, id).unwrap().unwrap()
    }

    // -----------------------------------------------------------------------
    // Which text is exported.
    // -----------------------------------------------------------------------

    /// Choosing Text exports the same document as Markdown, not the draft from
    /// before enhancement.
    #[test]
    fn both_formats_export_the_same_document() {
        let note = note_with("raw dictation, unpolished", Some("# Polished\n\nThe real thing."));

        assert_eq!(body(&note, Format::Md), "# Polished\n\nThe real thing.");
        assert_eq!(body(&note, Format::Txt), "Polished\n\nThe real thing.");
        assert!(
            !body(&note, Format::Txt).contains("raw dictation"),
            "the .txt branch must not fall back to the raw content"
        );
    }

    #[test]
    fn without_a_polished_body_both_formats_use_the_content() {
        let note = note_with("# Just typed\n\nplain", None);
        assert_eq!(body(&note, Format::Md), "# Just typed\n\nplain");
        assert_eq!(body(&note, Format::Txt), "Just typed\n\nplain");
    }

    /// A polished body cleared to `""` falls through to the content — the
    /// JavaScript `||` semantics the mirror follows too, and the reason
    /// `body_source` is one shared function.
    #[test]
    fn an_empty_polished_body_falls_through_rather_than_exporting_nothing() {
        let note = note_with("the draft survives", Some(""));
        assert_eq!(body(&note, Format::Md), "the draft survives");
        assert_eq!(body(&note, Format::Txt), "the draft survives");
    }

    // -----------------------------------------------------------------------
    // Plain-text rendering.
    // -----------------------------------------------------------------------

    /// Nothing of the image syntax is left, not even the `!`.
    #[test]
    fn an_image_becomes_its_alt_text() {
        assert_eq!(strip_markdown("![alt](img.png)"), "alt");
        assert_eq!(strip_markdown("![](img.png)"), "");
        assert_eq!(
            strip_markdown("See ![the chart](chart.png) and [the doc](doc.md)."),
            "See the chart and the doc."
        );
    }

    #[test]
    fn the_whole_strip_chain_in_order() {
        let source = "\
# Heading

Some **bold** and _italic_ and `code` and ~~struck~~ text.

> A quotation
> over two lines

![diagram](d.png) then [a link](https://example.com/x?y=1).

## Another heading";
        assert_eq!(
            strip_markdown(source),
            "\
Heading

Some bold and italic and code and struck text.

A quotation
over two lines

diagram then a link.

Another heading"
        );
    }

    #[test]
    fn plain_text_survives_the_strip_unchanged() {
        for text in [
            "Nothing to strip here.",
            "एक साधारण नोट, बिना किसी markdown के।",
            "A line with (parentheses) and a stray ] bracket.",
        ] {
            assert_eq!(strip_markdown(text), text);
        }

        // Characters that only look like markup.
        for text in [
            "2 * 3 = 6",
            "See issue #12 for details.",
            "Open file_name_v2 and keep snake_case_value.",
            "if a > b then swap them",
            "- first\n* second\n1. third",
            "#hashtag without a space",
        ] {
            assert_eq!(strip_markdown(text), text, "{text:?}");
        }
    }

    #[test]
    fn a_fenced_code_block_keeps_its_lines_and_loses_its_fences() {
        let source = "Before\n\n```rust\nlet x = *y*;\n# not a heading\n```\n\nAfter";
        assert_eq!(
            strip_markdown(source),
            "Before\n\nlet x = *y*;\n# not a heading\n\nAfter"
        );
        let tildes = "~~~\n> kept as written\n~~~";
        assert_eq!(strip_markdown(tildes), "> kept as written");
    }

    #[test]
    fn list_markers_are_left_as_written() {
        let source = "- one **bold** item\n* two\n+ three\n1. four\n  - nested";
        assert_eq!(
            strip_markdown(source),
            "- one bold item\n* two\n+ three\n1. four\n  - nested"
        );
    }

    #[test]
    fn the_harder_inline_cases() {
        let cases: &[(&str, &str)] = &[
            // Nested emphasis.
            ("***both***", "both"),
            ("**bold with _italic_ inside**", "bold with italic inside"),
            ("*a **b** c*", "a b c"),
            // Emphasis around a link, and a link inside emphasis.
            ("*see [the doc](doc.md)*", "see the doc"),
            ("[**bold link**](x.md)", "bold link"),
            // An unclosed delimiter stays as text.
            ("a *b", "a *b"),
            ("**not closed", "**not closed"),
            ("~~half", "~~half"),
            ("`open code", "`open code"),
            // Link text with brackets in it.
            ("[see [1] here](https://example.com)", "see [1] here"),
            ("[text](https://example.com/a_(b))", "text"),
            // Brackets that are not a link.
            ("[not a link] (spaced)", "[not a link] (spaced)"),
            // Inline code keeps what is inside it verbatim.
            ("`a*b*c` and ``x`y``", "a*b*c and x`y"),
            // Heading markers only at the start of a line, closing ones too.
            ("## Title ##", "Title"),
            ("   # Indented", "Indented"),
            ("#", ""),
            ("###", ""),
            ("####### seven is not a heading", "####### seven is not a heading"),
            // Nested quotes, and a heading inside a quote.
            ("> > deep", "deep"),
            ("> # Quoted title", "Quoted title"),
        ];
        for (source, want) in cases {
            assert_eq!(&strip_markdown(source), want, "stripping {source:?}");
        }
    }

    #[test]
    fn paragraph_breaks_stay_and_the_ends_are_trimmed() {
        assert_eq!(strip_markdown("\n\n# Title\n\n\nBody\n\n"), "Title\n\n\nBody");
        assert_eq!(strip_markdown("one\r\n\r\ntwo"), "one\n\ntwo");
    }

    // -----------------------------------------------------------------------
    // Format + filename.
    // -----------------------------------------------------------------------

    #[test]
    fn only_txt_asks_for_plain_text() {
        for raw in ["txt", "TXT", "Txt", " txt\n"] {
            assert_eq!(Format::parse(raw), Format::Txt, "{raw:?}");
        }
        for raw in ["md", "MD", "", "   ", "markdown", "text", "pdf", "t x t", "txt.md"] {
            assert_eq!(Format::parse(raw), Format::Md, "{raw:?}");
        }
    }

    #[test]
    fn the_offered_filename_is_one_windows_accepts() {
        assert_eq!(default_file_name("Mandi rates", Format::Md), "Mandi rates.md");
        assert_eq!(default_file_name("Q1/Q2 review", Format::Txt), "Q1-Q2 review.txt");
        assert_eq!(default_file_name("", Format::Md), "Untitled.md");
        assert_eq!(default_file_name("CON", Format::Md), "CON_.md");
        assert_eq!(default_file_name("Report.", Format::Md), "Report.md");

        // A very long title is cut to a name the save dialog accepts.
        let long = default_file_name(&"n".repeat(400), Format::Md);
        assert_eq!(long.chars().count(), MAX_NAME_CHARS + 3);

        // The export name keeps the title's case and spaces.
        assert_eq!(
            default_file_name("Monsoon Checklist FINAL", Format::Md),
            "Monsoon Checklist FINAL.md"
        );
    }
}
