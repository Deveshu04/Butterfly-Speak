//! Cutting a growing raw transcript into polishable chunks while the user is
//! still speaking. Pure: no I/O, no async. `sarvam::ws` owns the
//! orchestration; this module owns the arithmetic and is where every cut
//! rule is tested.

/// A chunk is handed out only once at least this many words have closed a
/// sentence — bounds the number of background calls (~6 for 300 words).
pub const CHUNK_MIN_WORDS: usize = 50;
/// A run-on with no sentence end is cut anyway once it exceeds this, at the
/// last comma, else the last space — the fallback, never the normal case.
/// This is the *trigger* for that fallback, not a hard cap: a chunk cut at a
/// sentence end is as long as the sentence end is late, so it can exceed this
/// by up to one final's worth of words.
pub const MAX_CHUNK_WORDS: usize = 120;
/// How much of the already-polished text is shown to the model as the text
/// before the cursor. Enough for the previous sentence or two and a list's
/// numbering; small enough to cost ~150 prompt tokens.
pub const CONTEXT_MAX_CHARS: usize = 600;

const TERMINALS: [char; 5] = ['.', '!', '?', '।', '॥'];
const CLOSERS: [char; 7] = ['"', '\'', '”', '’', ')', ']', '»'];

/// Byte index just past the last sentence end in `text` (the terminal mark
/// plus any closing quotes/brackets), or `None`. A `.` between two digits is
/// a decimal point, not a sentence end.
pub fn last_sentence_end(text: &str) -> Option<usize> {
    let chars: Vec<(usize, char)> = text.char_indices().collect();
    let mut i = chars.len();
    while i > 0 {
        i -= 1;
        let (off, c) = chars[i];
        if !TERMINALS.contains(&c) {
            continue;
        }
        if c == '.' {
            let prev_digit = i > 0 && chars[i - 1].1.is_ascii_digit();
            let next_digit = i + 1 < chars.len() && chars[i + 1].1.is_ascii_digit();
            if prev_digit && next_digit {
                continue;
            }
        }
        let mut end = off + c.len_utf8();
        let mut j = i + 1;
        while j < chars.len() && CLOSERS.contains(&chars[j].1) {
            end = chars[j].0 + chars[j].1.len_utf8();
            j += 1;
        }
        return Some(end);
    }
    None
}

fn word_count(s: &str) -> usize {
    s.split_whitespace().count()
}

/// Tracks how much of the joined finals text has been handed out.
#[derive(Default, Debug)]
pub struct Segmenter {
    /// Exactly the prefix of the joined text handed out so far. Kept as text,
    /// not an offset, so a joined text that no longer starts with it (an
    /// out-of-order final) is detected instead of sliced at a stale offset.
    handed: String,
    chunks: usize,
    /// Set once the handed prefix stopped matching; no further chunks.
    desynced: bool,
    /// The last cut fell inside a run-on, at a comma or a space, so the text
    /// after it continues a sentence.
    cut_mid_sentence: bool,
}

impl Segmenter {
    /// The unhanded remainder of `joined` (the finals joined by single
    /// spaces, as `ws::assemble` does), or the whole text if desynced.
    fn remainder<'a>(&self, joined: &'a str) -> &'a str {
        if self.desynced || !joined.starts_with(&self.handed) {
            return joined;
        }
        joined[self.handed.len()..].trim_start()
    }

    pub fn take_chunk(&mut self, joined: &str) -> Option<String> {
        if self.desynced {
            return None;
        }
        if !joined.starts_with(&self.handed) {
            tracing::warn!(
                handed_bytes = self.handed.len(),
                "finals no longer start with the handed prefix; no more background chunks"
            );
            self.desynced = true;
            return None;
        }
        let rem = self.remainder(joined);
        if word_count(rem) < CHUNK_MIN_WORDS {
            return None;
        }
        let (cut, mid_sentence) = match last_sentence_end(rem) {
            Some(end) if word_count(&rem[..end]) >= CHUNK_MIN_WORDS => (end, false),
            _ if word_count(rem) > MAX_CHUNK_WORDS => {
                // Run-on fallback: last comma, else last space.
                let cut = match rem.rfind(',') {
                    Some(c) if word_count(&rem[..=c]) >= CHUNK_MIN_WORDS => c + 1,
                    _ => rem.rfind(' ').unwrap_or(rem.len()),
                };
                (cut, true)
            }
            _ => return None,
        };
        let chunk = rem[..cut].trim().to_string();
        if chunk.is_empty() {
            return None;
        }
        // The handed prefix is the joined text up to and including the chunk.
        let consumed = joined.len() - rem.len() + cut;
        self.handed = joined[..consumed].to_string();
        self.chunks += 1;
        self.cut_mid_sentence = mid_sentence;
        Some(chunk)
    }

    /// Whether `joined` still extends the text handed out so far, and so
    /// whether the chunks handed out are still a prefix of it.
    ///
    /// Checked against the caller's own `joined` rather than read off the
    /// `desynced` flag alone, because a de-sync can first become visible at
    /// tail time: the flag is only ever set inside `take_chunk`, so a final
    /// that arrives out of order after the last `take_chunk` call is
    /// invisible to the flag but not to this.
    pub fn in_sync(&self, joined: &str) -> bool {
        !self.desynced && joined.starts_with(&self.handed)
    }

    /// The unhanded remainder of `joined` plus the in-flight `partial`, joined
    /// by a single space — what still needs polishing at finish.
    ///
    /// Only a remainder while `in_sync(joined)` holds. When it does not, this
    /// returns the **whole** transcript, and the caller must discard every
    /// chunk it has already been handed — otherwise the already-polished text
    /// is emitted twice. `ws::drain_session` gates the final assembly on
    /// `in_sync`.
    pub fn tail(&self, joined: &str, partial: &str) -> String {
        let rem = self.remainder(joined).trim();
        let partial = partial.trim();
        match (rem.is_empty(), partial.is_empty()) {
            (true, true) => String::new(),
            (false, true) => rem.to_string(),
            (true, false) => partial.to_string(),
            (false, false) => format!("{rem} {partial}"),
        }
    }

    pub fn chunks_taken(&self) -> usize {
        self.chunks
    }

    /// How the text after the last cut meets the text handed out before it:
    /// the start of a new sentence, or, after a run-on was cut at a comma or
    /// a space, the rest of the same one. What the next chunk, or the tail,
    /// is polished with.
    pub fn seam(&self) -> crate::sarvam::chat::Seam {
        if self.cut_mid_sentence {
            crate::sarvam::chat::Seam::MidSentence
        } else {
            crate::sarvam::chat::Seam::NewSentence
        }
    }
}

/// The last `CONTEXT_MAX_CHARS` *characters* of `polished`, cut forward to a
/// word boundary so the window never opens inside a word.
///
/// Counted in characters, not bytes: Devanagari runs about three bytes to the
/// character, so a byte-bounded window would show the model a third of the
/// intended context in Hindi. Walking back over `char_indices` also makes the
/// start a char boundary by construction.
///
/// One documented exception to the word boundary: if the window holds no
/// space at all (a single enormous token), the whole window is returned
/// rather than nothing.
pub fn context_tail(polished: &str) -> String {
    let start = match polished.char_indices().rev().nth(CONTEXT_MAX_CHARS - 1) {
        Some((i, _)) => i,
        // Fewer than CONTEXT_MAX_CHARS characters: all of it is the context.
        None => return polished.to_string(),
    };
    match polished[start..].find(' ') {
        Some(sp) => polished[start + sp + 1..].to_string(),
        None => polished[start..].to_string(),
    }
}

/// What `repair_seam` did to a chunk, for the per-chunk debug line.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SeamRepair {
    pub echoed_words: usize,
    pub capitalised: bool,
}

/// A leading run shorter than this is an ordinary repeated phrase, not an echo.
const ECHO_MIN_WORDS: usize = 5;

fn norm_word(w: &str) -> String {
    w.chars().filter(|c| c.is_alphanumeric()).flat_map(char::to_lowercase).collect()
}

/// Byte spans of whitespace-separated tokens.
fn token_spans(s: &str) -> Vec<(usize, usize)> {
    let mut spans = Vec::new();
    let mut start: Option<usize> = None;
    for (i, c) in s.char_indices() {
        if c.is_whitespace() {
            if let Some(st) = start.take() {
                spans.push((st, i));
            }
        } else if start.is_none() {
            start = Some(i);
        }
    }
    if let Some(st) = start {
        spans.push((st, s.len()));
    }
    spans
}

/// Two text-only repairs at the join between `previous` (already emitted)
/// and `chunk` (about to be appended). Only text identical to
/// what is already emitted can ever be removed.
///
/// `input` is the text the model was *given* for this chunk (the rule-cleaned
/// chunk in the background worker, the tail's rule text at the tail site). It
/// is what separates an echo the model added from a repetition the user
/// actually dictated: if the input already opens with the same run of words,
/// the second copy is the user's own speech and stripping it would silently
/// delete a stretch of the dictation — the worst failure class this project
/// has. Only a run that is in the reply but *not* in the input is an echo.
///
/// `seam` is how the chunk meets `previous`: only at a new sentence does its
/// first letter become a capital.
///
/// Measured over ten 300-word dictations with the current before-cursor rule
/// (`chat::BEFORE_CURSOR_RULE`): without this repair 4 of 45 joins were
/// anomalous; with it, 0 of 41. A change to that rule's wording can move
/// these numbers.
pub fn repair_seam(
    previous: &str,
    chunk: &str,
    input: &str,
    seam: crate::sarvam::chat::Seam,
) -> (String, SeamRepair) {
    let mut out = chunk.trim().to_string();
    let mut report = SeamRepair::default();
    let prev_norm: Vec<String> = previous
        .split_whitespace()
        .map(norm_word)
        .filter(|w| !w.is_empty())
        .collect();
    let input_norm: Vec<String> = input
        .split_whitespace()
        .map(norm_word)
        .filter(|w| !w.is_empty())
        .collect();
    for _ in 0..2 {
        let spans = token_spans(&out);
        let chunk_norm: Vec<String> = spans.iter().map(|&(a, b)| norm_word(&out[a..b])).collect();
        let max_k = prev_norm.len().min(chunk_norm.len());
        let mut found = None;
        // Longest match wins: the whole echoed run goes, not its first five
        // words. `e2e_stress.py::repair_seam` counts down the same way.
        for k in (ECHO_MIN_WORDS..=max_k).rev() {
            if chunk_norm[..k] == prev_norm[prev_norm.len() - k..] {
                found = Some(k);
                break;
            }
        }
        let Some(k) = found else { break };
        // The user dictated the repetition: the words the model was given
        // open with the very run that matched, so the model added nothing and
        // there is nothing to strip. Declined without counting — this is not
        // an echo the repair suppressed, it is ordinary dictation.
        if input_norm.len() >= k && input_norm[..k] == chunk_norm[..k] {
            break;
        }
        let rest_start = spans.get(k).map(|&(a, _)| a).unwrap_or(out.len());
        let rest = out[rest_start..].trim_start_matches(|c: char| !c.is_alphanumeric() && c != '"' && c != '\'');
        // Never strip a chunk to nothing. The echo strip exists to remove a
        // duplicate, not to drop a chunk: if the model returned only what was
        // already written, the safe reading is that the match was a
        // coincidence (or that the guardrail should have caught it), and
        // deleting the whole chunk would silently lose a stretch of the
        // user's dictation — the worst failure class this project has.
        if !rest.chars().any(char::is_alphanumeric) {
            break;
        }
        out = rest.trim_start().to_string();
        report.echoed_words += k;
    }
    // After a cut inside a run-on the chunk carries on the same sentence,
    // whatever the model put at the end of the one before it.
    let prev = previous.trim_end();
    let prev_ends_sentence = seam == crate::sarvam::chat::Seam::NewSentence
        && (prev.is_empty() || last_sentence_end(prev) == Some(prev.len()));
    if prev_ends_sentence {
        if let Some((i, c)) = out.char_indices().find(|(_, c)| c.is_alphabetic()) {
            // "iPhone", "eBay": a word carrying an upper case letter *after*
            // its first alphabetic character is spelled that way on purpose.
            // Upper-casing its first letter would rewrite the word rather
            // than fix a sentence start, so the whole step stands down.
            let word_end = token_spans(&out)
                .into_iter()
                .find(|&(a, b)| a <= i && i < b)
                .map_or(out.len(), |(_, b)| b);
            let camel_case = out[i + c.len_utf8()..word_end].chars().any(char::is_uppercase);
            if c.is_ascii_lowercase() && !camel_case {
                out.replace_range(i..i + c.len_utf8(), &c.to_ascii_uppercase().to_string());
                report.capitalised = true;
            }
        }
    }
    (out, report)
}

/// `polished` without the final period the model gave a chunk that was cut
/// inside a run-on, when `raw`, the chunk as dictated, did not end with one:
/// the sentence goes on in the next chunk. A comma the raw chunk ended with
/// takes the period's place. A question mark, an ellipsis or a period the
/// user's own text had are left alone, and so is an abbreviation's period
/// (`ends_in_abbreviation`), which belongs to the word.
pub fn drop_added_period(polished: &str, raw: &str) -> String {
    let text = polished.trim_end();
    let raw = raw.trim_end();
    if !text.ends_with('.')
        || text.ends_with("..")
        || raw.ends_with('.')
        || ends_in_abbreviation(text)
    {
        return polished.to_string();
    }
    let mut out = text[..text.len() - 1].to_string();
    if raw.ends_with(',') {
        out.push(',');
    }
    out
}

/// Abbreviations whose period the model writes inside a sentence. Kept to the
/// common ones: a word missing here only loses a period at a rare cut.
const ABBREVIATIONS: &[&str] = &[
    "co", "corp", "dr", "etc", "inc", "jr", "ltd", "mr", "mrs", "ms", "prof", "sr", "st", "vs",
];

/// Whether `text`, which ends with a period, ends with an abbreviation that
/// owns that period: letters with another period between them ("p.m.",
/// "U.S."), or one of `ABBREVIATIONS`. A number such as "2.5" is not one.
fn ends_in_abbreviation(text: &str) -> bool {
    let Some(last) = text.split_whitespace().last() else {
        return false;
    };
    let word = last
        .strip_suffix('.')
        .unwrap_or(last)
        .trim_start_matches(|c: char| !c.is_alphanumeric());
    let dotted = word.contains('.') && word.ends_with(char::is_alphabetic);
    dotted || ABBREVIATIONS.contains(&word.to_lowercase().as_str())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChunkOutcome {
    Accepted,
    Rejected,
    Failed,
}

#[derive(Clone, Debug)]
pub struct PolishedChunk {
    pub text: String,
    pub notice: Option<String>,
    pub dict_fixes: u32,
    pub polish_ms: u64,
    pub outcome: ChunkOutcome,
    /// `"\n"` or `"\n\n"` when the user spoke "new line"/"new paragraph" at
    /// the very start of this chunk's *raw* text, else `None`.
    ///
    /// The break cannot be read off `text`: `cleanup::commands::apply` turns
    /// the spoken phrase into a sentinel character, `cleanup::tidy` turns
    /// that into real newlines and then trims the result — and this chunk was
    /// tidied on its own, so a break sitting on its edge was trimmed away.
    /// Carrying it here lets [`assemble_polished_with_tail`] put it back at
    /// the seam, where it belongs.
    pub leading_break: Option<&'static str>,
    /// The same at the end of this chunk's raw text.
    pub trailing_break: Option<&'static str>,
}

/// Chunks then tail, empties skipped, with no break of the tail's own —
/// [`assemble_polished_with_tail`] with `None`. What the callers that build
/// the text before the cursor want, since they pass an empty tail anyway.
pub fn assemble_polished(chunks: &[PolishedChunk], tail: &str) -> String {
    assemble_polished_with_tail(chunks, tail, None)
}

/// Chunks then tail, empties skipped, joined by the paragraph break the seam
/// carries — the previous part's `trailing_break`, else the next part's
/// `leading_break`, else a single space. A paragraph break beats a line
/// break when both sides carry one.
pub fn assemble_polished_with_tail(
    chunks: &[PolishedChunk],
    tail: &str,
    tail_leading_break: Option<&'static str>,
) -> String {
    let mut parts: Vec<(&str, Option<&'static str>, Option<&'static str>)> = chunks
        .iter()
        .map(|c| (c.text.trim(), c.leading_break, c.trailing_break))
        .filter(|(t, _, _)| !t.is_empty())
        .collect();
    let tail = tail.trim();
    if !tail.is_empty() {
        // Nothing follows the tail, so its own trailing break has nowhere to
        // go — and `tidy` would trim it off the end of the document anyway.
        parts.push((tail, tail_leading_break, None));
    }
    let mut out = String::new();
    for (i, &(text, leading, _)) in parts.iter().enumerate() {
        if i > 0 {
            out.push_str(match (parts[i - 1].2, leading) {
                (Some("\n\n"), _) | (_, Some("\n\n")) => "\n\n",
                (Some(b), _) | (_, Some(b)) => b,
                (None, None) => " ",
            });
        }
        out.push_str(text);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(n: usize, sentence_every: usize) -> String {
        // "w1 w2 ... wk." repeated: a sentence end every `sentence_every` words.
        let mut out = String::new();
        for i in 1..=n {
            out.push_str(&format!("w{i}"));
            out.push(if i % sentence_every == 0 { '.' } else { ' ' });
            if i % sentence_every == 0 && i != n {
                out.push(' ');
            }
        }
        out.trim_end().to_string()
    }

    #[test]
    fn a_sentence_end_is_the_last_terminal_mark_plus_closing_quotes() {
        assert_eq!(last_sentence_end("Hello there. How are"), Some(12));
        assert_eq!(last_sentence_end("He said \"go.\" Then"), Some(13));
        assert_eq!(last_sentence_end("क्या हाल है। ठीक"), Some("क्या हाल है।".len()));
        assert_eq!(last_sentence_end("no terminal mark here"), None);
        assert_eq!(last_sentence_end("3.5 percent growth"), None, "a decimal point is not a sentence end");
    }

    #[test]
    fn no_chunk_below_the_minimum_words() {
        let mut s = Segmenter::default();
        assert_eq!(s.take_chunk(&words(49, 10)), None);
        assert_eq!(s.chunks_taken(), 0);
    }

    #[test]
    fn a_chunk_is_cut_at_the_last_sentence_end_at_or_after_the_minimum() {
        let mut s = Segmenter::default();
        let joined = words(63, 10); // sentence ends after w10, w20, ..., w60; tail w61 w62 w63
        let chunk = s.take_chunk(&joined).expect("63 words with a sentence end at 60 is a chunk");
        assert!(chunk.ends_with("w60."), "{chunk}");
        assert_eq!(chunk.split_whitespace().count(), 60);
        assert_eq!(s.tail(&joined, ""), "w61 w62 w63");
        assert_eq!(s.chunks_taken(), 1);
        // Nothing more until 50 new words have accumulated.
        assert_eq!(s.take_chunk(&joined), None);
    }

    #[test]
    fn a_second_chunk_starts_where_the_first_ended() {
        let mut s = Segmenter::default();
        let first = words(60, 10);
        s.take_chunk(&first).unwrap();
        let joined = words(130, 10);
        let second = s.take_chunk(&joined).unwrap();
        assert!(second.starts_with("w61 "), "{second}");
        assert!(second.ends_with("w130."), "{second}");
        assert_eq!(s.tail(&joined, "partial words"), "partial words");
    }

    #[test]
    fn a_run_on_without_sentence_ends_is_cut_at_the_last_comma_past_the_ceiling() {
        let mut s = Segmenter::default();
        let mut joined = words(200, 1000); // no sentence end at all
        joined = joined.replacen("w100 ", "w100, ", 1);
        let chunk = s.take_chunk(&joined).expect("120+ words with no sentence end must still chunk");
        assert!(chunk.ends_with("w100,"), "{chunk}");
    }

    #[test]
    fn the_tail_is_the_unhanded_remainder_plus_the_partial() {
        let mut s = Segmenter::default();
        let joined = words(55, 10);
        s.take_chunk(&joined).unwrap(); // cuts at w50.
        assert_eq!(s.tail(&joined, "and more"), "w51 w52 w53 w54 w55 and more");
        assert_eq!(s.tail(&joined, ""), "w51 w52 w53 w54 w55");
    }

    #[test]
    fn a_joined_text_that_no_longer_starts_with_the_handed_prefix_stops_chunking() {
        let mut s = Segmenter::default();
        s.take_chunk(&words(60, 10)).unwrap();
        // An in-order continuation is still in sync.
        assert!(s.in_sync(&words(70, 10)));
        let rewritten = "completely different text. ".repeat(20);
        // Detectable before take_chunk is ever called again — a de-sync that
        // first appears at tail time still reports.
        assert!(!s.in_sync(&rewritten));
        assert_eq!(s.take_chunk(&rewritten), None);
        assert!(!s.in_sync(&rewritten));
        assert_eq!(s.tail(&rewritten, ""), rewritten.trim());
        // The tail above is the whole transcript, not a remainder, so the
        // caller must discard the chunk it was already handed.
        assert_eq!(s.chunks_taken(), 1);
    }

    /// What comes after a cut: a new sentence when the cut was at a sentence
    /// end, the rest of the same sentence when a run-on was cut at a comma or
    /// a space.
    #[test]
    fn a_run_on_cut_leaves_the_next_text_mid_sentence() {
        use crate::sarvam::chat::Seam;
        let mut s = Segmenter::default();
        assert_eq!(s.seam(), Seam::NewSentence, "nothing handed out yet");
        s.take_chunk(&words(63, 10)).unwrap();
        assert_eq!(s.seam(), Seam::NewSentence);

        let mut s = Segmenter::default();
        s.take_chunk(&words(130, 1000)).unwrap();
        assert_eq!(s.seam(), Seam::MidSentence, "cut at the last space");

        let mut s = Segmenter::default();
        let joined = words(200, 1000).replacen("w100 ", "w100, ", 1);
        s.take_chunk(&joined).unwrap();
        assert_eq!(s.seam(), Seam::MidSentence, "cut at the last comma");
    }

    #[test]
    fn a_run_on_with_no_comma_is_cut_at_the_last_space() {
        let mut s = Segmenter::default();
        let joined = words(130, 1000); // no sentence end, no comma
        let chunk = s.take_chunk(&joined).expect("a 130-word run-on must still chunk");
        assert!(chunk.ends_with("w129"), "{chunk}");
        assert_eq!(s.tail(&joined, ""), "w130");
    }

    #[test]
    fn a_dot_after_a_digit_but_not_before_one_is_a_sentence_end() {
        // Only a dot *between* two digits is a decimal point.
        assert_eq!(last_sentence_end("percent 3."), Some(10));
        assert_eq!(last_sentence_end("we grew 3.5 percent in 2024."), Some(28));
    }

    #[test]
    fn context_is_the_last_chars_at_a_word_boundary() {
        let long = "word ".repeat(300); // 1500 chars
        let ctx = context_tail(&long);
        assert!(ctx.chars().count() <= CONTEXT_MAX_CHARS, "{}", ctx.chars().count());
        assert!(ctx.starts_with("word "), "{ctx:?}");
        assert_eq!(context_tail("short"), "short");
        // Bounded in characters, not bytes: Devanagari runs ~3 bytes to the
        // character, so a byte-bounded window would hold only ~200 of these.
        let hi = "क्या हाल ".repeat(200); // 1800 chars, 4600 bytes
        let ctx = context_tail(&hi);
        assert!(hi.ends_with(&ctx), "the window is a suffix of the text");
        assert!(ctx.chars().count() <= CONTEXT_MAX_CHARS, "{}", ctx.chars().count());
        assert!(ctx.chars().count() >= 580, "only {} chars", ctx.chars().count());
        assert!(
            ctx.starts_with("क्या ") || ctx.starts_with("हाल "),
            "opens at a word, on a char boundary: {ctx:?}"
        );
    }

    /// A `PolishedChunk` carrying no paragraph break, for the assembly tests.
    fn plain(text: &str) -> PolishedChunk {
        PolishedChunk {
            text: text.into(),
            notice: None,
            dict_fixes: 0,
            polish_ms: 1,
            outcome: ChunkOutcome::Accepted,
            leading_break: None,
            trailing_break: None,
        }
    }

    fn broken(text: &str, leading: Option<&'static str>, trailing: Option<&'static str>) -> PolishedChunk {
        PolishedChunk { leading_break: leading, trailing_break: trailing, ..plain(text) }
    }

    #[test]
    fn assembly_joins_chunks_and_tail_with_single_spaces_and_skips_empties() {
        let chunks = vec![
            PolishedChunk { text: "First chunk.".into(), notice: None, dict_fixes: 0, polish_ms: 1, outcome: ChunkOutcome::Accepted, leading_break: None, trailing_break: None },
            PolishedChunk { text: "".into(), notice: None, dict_fixes: 0, polish_ms: 1, outcome: ChunkOutcome::Failed, leading_break: None, trailing_break: None },
            PolishedChunk { text: "Third chunk.".into(), notice: None, dict_fixes: 0, polish_ms: 1, outcome: ChunkOutcome::Accepted, leading_break: None, trailing_break: None },
        ];
        assert_eq!(assemble_polished(&chunks, "The tail."), "First chunk. Third chunk. The tail.");
        assert_eq!(assemble_polished(&chunks, ""), "First chunk. Third chunk.");
        assert_eq!(assemble_polished(&[], "Only tail."), "Only tail.");
    }

    // --- Seam repair -------------------------------------------------------

    /// The repair at a seam where a new sentence begins, the common case.
    fn repair(previous: &str, chunk: &str, input: &str) -> (String, SeamRepair) {
        repair_seam(previous, chunk, input, crate::sarvam::chat::Seam::NewSentence)
    }

    /// After a run-on was cut at a comma or a space, the next chunk carries
    /// on the same sentence, whatever the model put at the end of the last.
    #[test]
    fn a_chunk_that_continues_a_sentence_is_not_capitalised() {
        use crate::sarvam::chat::Seam;
        let (out, r) = repair_seam("We kept going.", "and then we stopped.", "and then we stopped", Seam::MidSentence);
        assert_eq!(out, "and then we stopped.");
        assert!(!r.capitalised);
    }

    /// A chunk cut inside a run-on does not end a sentence, so a period the
    /// model put there goes; a comma the user's text had stays.
    #[test]
    fn a_period_the_model_added_at_a_mid_sentence_cut_is_dropped() {
        assert_eq!(drop_added_period("We kept going and.", "we kept going and"), "We kept going and");
        assert_eq!(drop_added_period("We kept going.", "we kept going,"), "We kept going,");
        assert_eq!(drop_added_period("We kept going,", "we kept going,"), "We kept going,");
        // The user's own period, an ellipsis and a question stay.
        assert_eq!(drop_added_period("Version 2.", "version 2."), "Version 2.");
        assert_eq!(drop_added_period("We kept going...", "we kept going"), "We kept going...");
        assert_eq!(drop_added_period("Did we?", "did we"), "Did we?");
    }

    /// An abbreviation's period is part of the word, not a sentence end the
    /// model added, so a cut right after one leaves it in place.
    #[test]
    fn an_abbreviations_period_at_a_mid_sentence_cut_stays() {
        assert_eq!(
            drop_added_period("We met the CEO of Acme Inc.", "we met the ceo of acme inc"),
            "We met the CEO of Acme Inc."
        );
        assert_eq!(drop_added_period("We meet at 5 p.m.", "we meet at 5 pm"), "We meet at 5 p.m.");
        assert_eq!(drop_added_period("Made in the U.S.", "made in the us"), "Made in the U.S.");
        assert_eq!(drop_added_period("Pens, paper, etc.", "pens paper etc"), "Pens, paper, etc.");
        assert_eq!(drop_added_period("I saw Dr.", "i saw doctor"), "I saw Dr.");
        // A word that only ends like one is still an ordinary word.
        assert_eq!(drop_added_period("We kept the zinc.", "we kept the zinc"), "We kept the zinc");
        assert_eq!(drop_added_period("We paid 2.5.", "we paid 2.5"), "We paid 2.5");
    }

    #[test]
    fn an_echoed_prefix_of_five_or_more_words_is_stripped() {
        let prev = "We agreed to ship the budget review by Friday.";
        let chunk = "the budget review by Friday. Then Priya raised the vendor question.";
        // The model was given only the new words: the run is the model's own
        // addition, so it is an echo.
        let input = "then priya raised the vendor question";
        let (out, r) = repair(prev, chunk, input);
        assert_eq!(out, "Then Priya raised the vendor question.");
        assert_eq!(r.echoed_words, 5);
        assert!(!r.capitalised, "already capitalised");
    }

    #[test]
    fn a_short_overlap_is_not_an_echo() {
        let prev = "We agreed to ship it by Friday.";
        let (out, r) = repair(prev, "by Friday. Then we left.", "by friday then we left");
        assert_eq!(out, "By Friday. Then we left.");
        assert_eq!(r.echoed_words, 0);
        assert!(r.capitalised);
    }

    #[test]
    fn an_echo_is_stripped_at_most_twice() {
        let prev = "one two three four five six.";
        let chunk = "one two three four five six. one two three four five six. Seven eight.";
        let (out, r) = repair(prev, chunk, "seven eight");
        assert_eq!(out, "Seven eight.");
        assert_eq!(r.echoed_words, 12);
    }

    /// The one thing the strip must never do: delete words the user actually
    /// said. The input the model was given opens with the same run, so the
    /// repetition is dictation, not an echo — and the chunk stands as written.
    #[test]
    fn a_repetition_the_user_dictated_is_kept() {
        let prev = "We will ship it by Friday.";
        let input = "we will ship it by friday and that is final";
        let chunk = "We will ship it by Friday and that is final.";
        let (out, r) = repair(prev, chunk, input);
        assert_eq!(out, "We will ship it by Friday and that is final.");
        assert_eq!(r.echoed_words, 0);
    }

    #[test]
    fn the_first_letter_is_capitalised_only_after_a_sentence_end() {
        assert_eq!(repair("Done.", "next thing", "next thing").0, "Next thing");
        assert_eq!(repair("Done!\"", "next thing", "next thing").0, "Next thing");
        assert_eq!(repair("", "next thing", "next thing").0, "Next thing");
        assert_eq!(
            repair("first,", "next thing", "next thing").0,
            "next thing",
            "a comma continues the sentence"
        );
        assert_eq!(repair("ठीक है।", "क्या हाल", "क्या हाल").0, "क्या हाल", "no case in Devanagari");
        assert_eq!(
            repair("Done.", "\"quoted start\"", "quoted start").0,
            "\"Quoted start\"",
            "first alphabetic char, not first char"
        );
    }

    /// "iPhone", "eBay": the sentence-start rule must not rewrite a word that
    /// carries an upper case letter of its own.
    #[test]
    fn a_camel_case_word_keeps_its_own_casing() {
        let (out, r) = repair("Done.", "iPhone sales rose.", "iphone sales rose");
        assert_eq!(out, "iPhone sales rose.");
        assert!(!r.capitalised);
        // An ordinary lower case word at the same place still gets fixed.
        assert!(repair("Done.", "phone sales rose.", "phone sales rose").1.capitalised);
    }

    /// The strip may only ever remove text that is already in the document —
    /// so it must never be the reason a chunk contributes nothing at all. If
    /// everything after the echoed run is punctuation (or there is nothing
    /// after it), the chunk stands exactly as the model wrote it.
    #[test]
    fn an_echo_that_is_the_whole_chunk_is_left_alone() {
        // The comma keeps `previous` mid-sentence, so the casing rule is out
        // of the way and the chunk comes back byte for byte.
        let prev = "We agreed to ship the budget review by Friday,";
        let input = "so then priya raised the vendor question";
        let (out, r) = repair(prev, "the budget review by Friday,", input);
        assert_eq!(out, "the budget review by Friday,");
        assert_eq!(r.echoed_words, 0);
        // Same when only punctuation would survive the strip.
        let (out, r) = repair(prev, "the budget review by Friday, ...", input);
        assert_eq!(out, "the budget review by Friday, ...");
        assert_eq!(r.echoed_words, 0);
    }

    // --- Paragraph breaks across a seam -----------------------------------

    #[test]
    fn a_break_spoken_at_a_seam_survives_the_join() {
        // Spoken at the end of the first chunk: the trim inside that chunk's
        // own `tidy` pass deleted it, so the seam has to carry it.
        let chunks = vec![broken("First chunk.", None, Some("\n\n")), plain("Second chunk.")];
        assert_eq!(assemble_polished(&chunks, ""), "First chunk.\n\nSecond chunk.");
        // Spoken at the start of the second chunk instead.
        let chunks = vec![plain("First chunk."), broken("Second chunk.", Some("\n"), None)];
        assert_eq!(assemble_polished(&chunks, ""), "First chunk.\nSecond chunk.");
        // Neither side: a single space, exactly as before.
        let chunks = vec![plain("First chunk."), plain("Second chunk.")];
        assert_eq!(assemble_polished(&chunks, ""), "First chunk. Second chunk.");
        // Both sides: paragraph beats line.
        let chunks = vec![
            broken("First chunk.", None, Some("\n")),
            broken("Second chunk.", Some("\n\n"), None),
        ];
        assert_eq!(assemble_polished(&chunks, ""), "First chunk.\n\nSecond chunk.");
    }

    #[test]
    fn the_tail_joins_on_its_own_leading_break() {
        let chunks = vec![plain("First chunk.")];
        assert_eq!(
            assemble_polished_with_tail(&chunks, "The tail.", Some("\n\n")),
            "First chunk.\n\nThe tail."
        );
        assert_eq!(assemble_polished_with_tail(&chunks, "The tail.", None), "First chunk. The tail.");
        // The two-argument form is the no-break case.
        assert_eq!(assemble_polished(&chunks, "The tail."), "First chunk. The tail.");
        // The last chunk's own trailing break reaches the tail too.
        let chunks = vec![broken("First chunk.", None, Some("\n"))];
        assert_eq!(assemble_polished(&chunks, "The tail."), "First chunk.\nThe tail.");
    }
}
