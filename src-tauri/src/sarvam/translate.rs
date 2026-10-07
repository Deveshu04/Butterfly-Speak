//! Sarvam text translation (`POST /translate`) — the network half of the
//! translate chord's route (`routes::Route::Translation`).
//!
//! ## Why this is its own endpoint and not another chat completion
//!
//! `sarvam::chat` already talks to a general LLM, so a translation *could* be
//! a prompt ("translate this into Hindi"). Sarvam ships a dedicated
//! translation model instead, and using it removes the whole class of failure
//! a prompt has: no instruction for the transcript to override, no commentary
//! to strip, no end marker to police, no temperature to pick.
//!
//! ## The endpoint's contract
//!
//! From `docs.sarvam.ai/api-reference/text/translate-text`, and confirmed
//! against the live endpoint:
//!
//! - `POST https://api.sarvam.ai/translate`, `api-subscription-key` header,
//!   JSON body `{ input, source_language_code, target_language_code, model }`.
//! - Reply `{ request_id, translated_text, source_language_code }`. A Hindi
//!   sentence came back as English in about 0.8 s, as Odia in about 1.5 s.
//! - Two models, and the cap differs between them: `mayura:v1` takes 1000
//!   characters, `sarvam-translate:v1` takes 2000. Reading one number for
//!   both is the easy mistake here — see [`MAX_INPUT_CHARS`].
//! - Odia is `od-IN` on this endpoint; `or-IN`, the realtime spelling, is a
//!   **400**. That is why [`to_translate_language_code`] exists, and it is
//!   deliberately *not* `batch::to_rest_language_code`.
//! - Source equal to target is a **400** ("Source and target languages must
//!   be different."), not the input handed back. `routes::translate`'s same-language
//!   short-circuit is therefore load-bearing; its comment says so.
//! - An unknown target code is a **400**, with the accepted enum spelled out
//!   in the message.
//!
//! Error bodies are `{"error":{"message","code","request_id"}}`. A bad key is
//! **403** with `invalid_api_key_error` — not 401, which is why
//! [`TranslateError::user_message`] treats the two alike. Nothing in that
//! payload is echoed into a log; see below.
//!
//! ## Privacy
//!
//! The request body *is* the user's dictation and the reply *is* its
//! translation. Nothing in this module logs either one: every `tracing` call
//! below carries a status code, a character count, a language code or a typed
//! failure discriminant, never content. Two branches are easy to get wrong
//! and are called out where they live:
//!
//! - The non-2xx branch does **not** log the response body. Arguing that an
//!   error body holds a JSON error payload rather than content is betting the
//!   user's words on an assumption about someone else's server.
//! - The parse-failure branch does **not** format the `serde_json::Error`.
//!   Its `Display` is a bare line/column only for *syntax* errors; a type
//!   mismatch embeds the offending value, which here is the transcript.
//!   `classify()`/`line()`/`column()` say the useful part and cannot leak.
//!
//! `sarvam::batch` follows the same two rules.

use super::net_error::{classify_io_error, NetFailure};
use std::time::Duration;

pub const TRANSLATE_URL: &str = "https://api.sarvam.ai/translate";

/// The 23-language model (22 scheduled Indian languages + English), formal
/// register only. `mayura:v1` is a deliberate non-goal: it covers 11
/// languages to this one's 23, and the two knobs it adds over this one
/// (`output_script`, `mode`) are transliteration and register controls this
/// app has no setting for. Its auto-detect (`source_language_code: "auto"`)
/// is the one real temptation, and it costs 12 languages — a dictation app
/// for India does not drop Urdu, Assamese and Maithili to save the user
/// naming the language they are speaking.
pub const TRANSLATE_MODEL: &str = "sarvam-translate:v1";

/// `sarvam-translate:v1`'s documented per-request cap. **Characters, not
/// bytes** — `chars().count()`, not `len()`. The distinction is the whole
/// ballgame for this app's languages: 2000 Devanagari characters is roughly
/// 6000 bytes, so a byte check would reject a perfectly legal Hindi
/// dictation at a third of the real limit.
///
/// Not the same number as `mayura:v1`'s 1000 — see the module doc.
pub const MAX_INPUT_CHARS: usize = 2_000;

/// The whole-request budget: connect, send, and read the reply. The user is
/// watching the pill with a finished dictation behind it, so this is not a
/// background job that can take as long as it likes — but it is also the
/// step that makes the translate chord mean anything, so it gets a longer
/// leash than `chat::POLISH_TIMEOUT`'s 6 s, which can fall back to
/// rule-cleaned text and lose almost nothing.
pub const TRANSLATE_TIMEOUT: Duration = Duration::from_secs(10);

/// A finished translation.
///
/// `text` is always **the text to use downstream** — that is the whole point
/// of the type. When the reply matched the input (`unchanged`) it is the
/// caller's own source text, not the model's copy of it, so a caller can
/// paste `text` unconditionally and never has to know which case it is in.
/// See [`translate`] for why an unchanged reply is not an error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Translated {
    pub text: String,
    /// The reply was identical to the input, whitespace aside: the text was
    /// already in the requested language, so nothing needed translating.
    pub unchanged: bool,
}

/// Why a translation didn't happen.
///
/// The network cases reuse `net_error::NetFailure` — the same DNS / refused /
/// timeout / TLS / service distinction the WebSocket path already draws, and
/// the same user-facing prose — rather than inventing a second, less specific
/// vocabulary for the same four causes on a different transport.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TranslateError {
    /// Never reached Sarvam. Carries the classified cause.
    Net(NetFailure),
    /// Over [`MAX_INPUT_CHARS`]. Not chunked: see [`translate`].
    TooLong,
    /// There is no translated text to use — an empty or whitespace-only
    /// reply, a 200 whose body isn't the documented shape, or (short-
    /// circuited before any request) blank input. All three mean the same
    /// thing to every caller: keep the text you already have.
    Empty,
    /// Sarvam answered, and the answer was a refusal.
    Http { status: u16 },
}

impl TranslateError {
    /// A short line for the overlay pill's error flash. Same tone as
    /// `NetFailure::user_message`, whose strings this delegates to for the
    /// causes they already cover — including 5xx, which is
    /// `ServiceUnavailable` whichever transport saw it.
    pub fn user_message(&self) -> &'static str {
        match self {
            TranslateError::Net(failure) => failure.user_message(),
            TranslateError::TooLong => "Dictation too long to translate",
            TranslateError::Empty => "Translation came back empty",
            TranslateError::Http { status } => match status {
                401 | 403 => "Sarvam rejected the API key — check it in Settings",
                429 => "Too many requests to Sarvam — try again in a moment",
                500..=599 => NetFailure::ServiceUnavailable.user_message(),
                _ => "Sarvam couldn't translate that",
            },
        }
    }
}

impl std::fmt::Display for TranslateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TranslateError::Net(failure) => write!(f, "network failure: {failure:?}"),
            TranslateError::TooLong => write!(f, "input is over {MAX_INPUT_CHARS} characters"),
            TranslateError::Empty => write!(f, "no translated text came back"),
            TranslateError::Http { status } => write!(f, "HTTP {status}"),
        }
    }
}

impl std::error::Error for TranslateError {}

/// Odia is `or-IN` on the realtime socket and `od-IN` on the REST endpoints,
/// this one included — the split `SarvamSettings::language_code` and
/// `TranslationSettings::target_language` both warn about in their own doc
/// comments.
///
/// Deliberately **not** `batch::to_rest_language_code`, despite handling the
/// same quirk: that function also maps `"auto"` to `"unknown"`, which is
/// speech-to-text's auto-detect sentinel and not a value `/translate` accepts
/// at all. Sharing it would silently send a language code this endpoint has
/// never heard of.
///
/// `"auto"` itself passes through untouched, and `sarvam-translate:v1` will
/// reject it with a 400 — the model has no auto-detect (that is `mayura:v1`
/// only). Resolving a concrete source language is the caller's job; this
/// function's job is not to paper over having skipped it.
pub fn to_translate_language_code(code: &str) -> &str {
    match code {
        "or-IN" => "od-IN",
        other => other,
    }
}

/// The exact JSON that goes on the wire. Pure, and separate from the request
/// for the same reason `format::backend::build_request_body` is: the wire
/// shape is the part most likely to drift from the API reference, and it
/// should be assertable without a socket.
///
/// `mode` and `numerals_format` are deliberately absent. `sarvam-translate:v1`
/// supports formal register only (which is also `mode`'s default) and
/// `numerals_format` defaults to `international`; sending a field to restate
/// its own default is a field that can go wrong for nothing. `output_script`
/// and `speaker_gender` are `mayura:v1`-only and would be rejected here.
fn build_request_body(text: &str, source: &str, target: &str) -> serde_json::Value {
    serde_json::json!({
        "input": text,
        "source_language_code": to_translate_language_code(source),
        "target_language_code": to_translate_language_code(target),
        "model": TRANSLATE_MODEL,
    })
}

/// Whether `text` holds nothing but whitespace. The empty string is blank.
///
/// Whitespace here is exactly what `char::is_whitespace` accepts: the Unicode
/// `White_Space` property, so NBSP, ideographic space and the line and
/// paragraph separators count. Format characters do not. U+200C and U+200D
/// decide how an Indic conjunct renders and U+FEFF is not a space either, so
/// a text holding any of them is not blank.
fn is_blank(text: &str) -> bool {
    text.chars().all(char::is_whitespace)
}

/// Whether `a` and `b` carry the same words in the same order, a word being
/// a run of characters between whitespace (as in [`is_blank`]).
///
/// Leading and trailing whitespace, the length of a gap and which whitespace
/// character fills it are ignored. Everything else counts: case, joiners,
/// U+FEFF, and composed versus decomposed forms, since no Unicode
/// normalisation is applied. The two word sequences are walked side by side,
/// so nothing is copied.
fn same_words(a: &str, b: &str) -> bool {
    a.split_whitespace().eq(b.split_whitespace())
}

/// What a parsed 2xx reply means for the caller who sent `input`.
///
/// - A blank reply is [`TranslateError::Empty`]: it must never stand in for
///   the user's words.
/// - A reply with the same words as `input` is unchanged. The result carries
///   `input` itself, byte for byte, rather than the model's respaced copy.
/// - Anything else is a translation, and the reply is returned as received.
///
/// Either way the text is one of the two strings, whole.
fn read_reply(input: &str, reply: String) -> Result<Translated, TranslateError> {
    if is_blank(&reply) {
        return Err(TranslateError::Empty);
    }
    if same_words(input, &reply) {
        return Ok(Translated {
            text: input.to_owned(),
            unchanged: true,
        });
    }
    Ok(Translated {
        text: reply,
        unchanged: false,
    })
}

#[derive(serde::Deserialize)]
struct TranslateResponse {
    /// `#[serde(default)]` so a reply that omits the field entirely lands in
    /// the same `Empty` case as one that sends `""`, rather than a parse
    /// error that would need its own branch to say the same thing.
    #[serde(default)]
    translated_text: String,
}

/// Translate `text` from `source` into `target` (Sarvam language codes).
///
/// ### Not chunked
///
/// Input over [`MAX_INPUT_CHARS`] is [`TranslateError::TooLong`], not a
/// split-and-rejoin. Splitting a dictation at a sentence boundary and
/// translating the halves independently loses exactly the context a
/// translation needs (pronoun antecedents, register agreement, a clause that
/// spans the seam), and the docs' "split long input" guidance is a paraphrase
/// with no API contract behind it. The case is reachable: `chat`'s 400-word
/// `MAX_INPUT_WORDS` caps one polish call, not a dictation, which a long one
/// spreads over several, so a few minutes of speech passes 2,000 characters.
/// Refusing honestly beats stitching quietly.
///
/// ### An unchanged reply is a success
///
/// A reply with the same words as `text` comes back as `Ok`, with
/// [`Translated::unchanged`] set and `text` as the caller's own input. This
/// function only reports the flag; whether it earns a notice, a log line or
/// nothing is the caller's decision.
pub async fn translate(
    http: &reqwest::Client,
    api_key: &str,
    text: &str,
    source: &str,
    target: &str,
) -> Result<Translated, TranslateError> {
    translate_at(
        http,
        TRANSLATE_URL,
        api_key,
        text,
        source,
        target,
        TRANSLATE_TIMEOUT,
    )
    .await
}

/// [`translate`] with the endpoint and deadline as parameters, so tests can
/// point it at a loopback stub and shrink the budget — the same reason
/// `batch::transcribe` takes its `url` and `timeout` rather than reading the
/// constants directly.
async fn translate_at(
    http: &reqwest::Client,
    url: &str,
    api_key: &str,
    text: &str,
    source: &str,
    target: &str,
    timeout: Duration,
) -> Result<Translated, TranslateError> {
    let chars = text.chars().count();
    if chars > MAX_INPUT_CHARS {
        tracing::warn!(
            chars,
            cap = MAX_INPUT_CHARS,
            "translation input is over the model's character cap"
        );
        return Err(TranslateError::TooLong);
    }

    // Blank input has nothing to translate, so return `Empty` without
    // spending a request. This runs after the cap, so an over-long run of
    // spaces still reports `TooLong`.
    if is_blank(text) {
        tracing::debug!(chars, "translation input is blank; no request made");
        return Err(TranslateError::Empty);
    }

    if source == "auto" {
        // Not a rejection — pre-resolving the source language belongs to the
        // caller, and quietly substituting one here would be worse than the
        // 400 that follows. But the 400 arrives after a full round trip of
        // the user's finalize budget and says nothing about why, so leave a
        // breadcrumb that names the cause. Language codes only; no content.
        tracing::warn!(
            model = TRANSLATE_MODEL,
            "translate called with source=auto, which this model has no auto-detect for; \
             expect HTTP 400"
        );
    }

    // `.timeout(timeout)` on the request itself rather than a
    // `tokio::time::timeout` around `.send()`, for the reason
    // `batch::transcribe`'s own note spells out: reqwest applies a
    // request-level budget across connect, send *and* the response body, so a
    // server that answers its headers and then goes quiet cannot park the
    // finalize path forever.
    let send = http
        .post(url)
        .header(super::AUTH_HEADER, api_key)
        .json(&build_request_body(text, source, target))
        .timeout(timeout)
        .send();
    let resp = match send.await {
        Ok(r) => r,
        Err(e) => {
            let failure = classify_request_error(&e);
            tracing::warn!(?failure, chars, "translate request failed");
            return Err(TranslateError::Net(failure));
        }
    };

    let status = resp.status().as_u16();
    if !resp.status().is_success() {
        // Status only, never the body: this endpoint's bodies carry the
        // dictation's own words in both directions, and there is no way to
        // tell from here whether an error body echoes the input back.
        tracing::warn!(status, chars, "translate returned a non-success status");
        return Err(TranslateError::Http { status });
    }
    let raw = match resp.text().await {
        Ok(t) => t,
        Err(e) => {
            let failure = classify_request_error(&e);
            tracing::warn!(?failure, "translate body read failed");
            return Err(TranslateError::Net(failure));
        }
    };
    let translated = match serde_json::from_str::<TranslateResponse>(&raw) {
        Ok(body) => body.translated_text,
        Err(e) => {
            // NEVER `{e}` here. `serde_json::Error`'s Display is a bare
            // line/column only for *syntax* errors; a type mismatch embeds
            // the offending value in the message, and the offending value on
            // this endpoint is the user's dictation. A 2xx carrying a bare
            // JSON string produces, verbatim: `invalid type: string "<the
            // whole transcript>", expected struct TranslateResponse at line 1
            // column N`. `classify()` is a fieldless enum and `line()`/
            // `column()` are numbers, so this says exactly as much as the
            // module's logging policy allows and not one character more.
            tracing::warn!(
                category = ?e.classify(),
                line = e.line(),
                column = e.column(),
                "translate reply was not the documented shape"
            );
            return Err(TranslateError::Empty);
        }
    };

    // Counts and the flag only. The result's `text` is either the dictation
    // or its translation, so the `Translated` itself is never formatted.
    let outcome = read_reply(text, translated);
    match &outcome {
        Ok(done) => tracing::debug!(
            chars,
            result_chars = done.text.chars().count(),
            unchanged = done.unchanged,
            "translate reply accepted"
        ),
        Err(_) => tracing::warn!(chars, "translate reply held no text to use"),
    }
    outcome
}

/// `reqwest::Error` → the shared `NetFailure` taxonomy.
///
/// The timeout check comes first and on reqwest's own typed predicate: the
/// request-level deadline this module sets fires as a marker type with no
/// `io::Error` underneath it at all, so the chain walk below would file every
/// timeout as `Other`.
fn classify_request_error(e: &reqwest::Error) -> NetFailure {
    if e.is_timeout() {
        return NetFailure::Timeout;
    }
    classify_error_chain(e)
}

/// The first `io::Error` in `err`'s source chain, classified by
/// `net_error::classify_io_error`.
///
/// The walk exists because the socket failure is never the error the caller
/// gets: reqwest wraps hyper wraps hyper-util's connector wraps the actual
/// `io::Error`, and the Winsock DNS codes and rustls-handshake detection
/// `classify_io_error` already knows how to read are buried at the bottom of
/// that stack. Stopping at reqwest's own predicates (`is_connect`,
/// `is_request`) would collapse DNS, refusal and TLS into one bucket, which
/// is exactly the distinction `net_error` exists to preserve.
///
/// Split from [`classify_request_error`] so it can be tested against a
/// hand-built chain. A real refused connection is not a usable fixture: on a
/// Windows machine whose firewall drops the SYN rather than resetting it, a
/// TCP connect to a closed loopback port takes ~2 s and never returns a
/// refusal, so the request deadline wins and the failure arrives as a
/// timeout. A "connection refused" test would be asserting the local firewall
/// policy, not this function.
fn classify_error_chain(err: &(dyn std::error::Error + 'static)) -> NetFailure {
    let mut cur = Some(err);
    while let Some(e) = cur {
        if let Some(io) = e.downcast_ref::<std::io::Error>() {
            return classify_io_error(io);
        }
        cur = e.source();
    }
    NetFailure::Other
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A loopback port nothing listens on, ever. Used only to prove a guard
    /// runs *before* the request: a `TranslateError::Net` coming back where
    /// the guard's own error was expected is how a check that drifted below
    /// the `.send()` announces itself.
    ///
    /// Not usable as a "connection refused" fixture: a connect to a closed
    /// loopback port can take ~2 s and never return a refusal at all. See
    /// [`classify_error_chain`].
    const DEAD_PORT: &str = "http://127.0.0.1:1";

    // --- The wire shape ----------------------------------------------------

    #[test]
    fn the_request_body_is_the_documented_shape() {
        let body = build_request_body("hello there", "en-IN", "hi-IN");
        assert_eq!(body["input"], "hello there");
        assert_eq!(body["source_language_code"], "en-IN");
        assert_eq!(body["target_language_code"], "hi-IN");
        assert_eq!(body["model"], TRANSLATE_MODEL);
    }

    /// `mayura:v1`'s knobs must never appear: `mode` and `numerals_format`
    /// would only restate their own defaults, and `output_script` /
    /// `speaker_gender` are not supported by this model at all.
    #[test]
    fn the_request_body_sends_nothing_beyond_the_four_required_fields() {
        let body = build_request_body("hello", "en-IN", "hi-IN");
        let obj = body.as_object().expect("the body is a JSON object");
        assert_eq!(obj.len(), 4, "unexpected fields: {obj:?}");
        for absent in ["mode", "numerals_format", "output_script", "speaker_gender"] {
            assert!(obj.get(absent).is_none(), "{absent} must not be sent");
        }
    }

    #[test]
    fn the_model_is_the_23_language_one_never_mayura() {
        assert_eq!(TRANSLATE_MODEL, "sarvam-translate:v1");
    }

    // --- Language codes ----------------------------------------------------

    #[test]
    fn odia_maps_to_the_rest_spelling_on_both_ends() {
        assert_eq!(to_translate_language_code("or-IN"), "od-IN");
        let body = build_request_body("x", "or-IN", "or-IN");
        assert_eq!(body["source_language_code"], "od-IN");
        assert_eq!(body["target_language_code"], "od-IN");
    }

    /// Already-REST Odia must not be double-translated into something else.
    #[test]
    fn rest_odia_is_left_alone() {
        assert_eq!(to_translate_language_code("od-IN"), "od-IN");
    }

    #[test]
    fn an_ordinary_language_code_passes_through_unchanged() {
        for code in ["hi-IN", "en-IN", "ta-IN", "ur-IN", "brx-IN"] {
            assert_eq!(to_translate_language_code(code), code);
        }
    }

    /// The one place this must NOT behave like `batch::to_rest_language_code`:
    /// `"unknown"` is speech-to-text's auto-detect sentinel and means nothing
    /// to `/translate`. `"auto"` stays `"auto"` and earns an honest 400 from
    /// a model that has no auto-detect, rather than a code the endpoint has
    /// never heard of.
    #[test]
    fn auto_is_not_rewritten_into_speech_to_texts_sentinel() {
        assert_eq!(to_translate_language_code("auto"), "auto");
        assert_ne!(
            to_translate_language_code("auto"),
            crate::sarvam::batch::to_rest_language_code("auto")
        );
    }

    // --- Blank text, same words, and what a reply means --------------------

    #[test]
    fn same_words_ignores_how_the_gaps_are_spaced() {
        let same = [
            ("hello there", "  hello there\n"),
            ("hello there", "hello      there"),
            ("hello there", "hello\tthere"),
            ("hello there", "hello\r\n\nthere"),
            // NO-BREAK SPACE, THIN SPACE and IDEOGRAPHIC SPACE are all in
            // the White_Space property.
            ("hello there", "hello\u{00A0}there"),
            ("hello there", "\u{2009}hello\u{3000}\u{3000}there\u{2009}"),
            ("नमस्ते दुनिया", "नमस्ते\u{00A0}\u{00A0}दुनिया\n"),
            ("", "   "),
        ];
        for (a, b) in same {
            assert!(same_words(a, b), "{a:?} vs {b:?} should be the same text");
            assert!(same_words(b, a), "{b:?} vs {a:?} should be the same text");
        }
    }

    #[test]
    fn same_words_counts_every_non_whitespace_character() {
        let different = [
            // One letter changed.
            ("hello there", "hallo there"),
            // A word split in two is a different sequence.
            ("to day", "today"),
            // Case is content.
            ("Hello there", "hello there"),
            // ZERO WIDTH NON-JOINER inside a conjunct changes how it renders.
            ("क्\u{200C}ष", "क्ष"),
            // ZERO WIDTH JOINER likewise.
            ("र्\u{200D}य", "र्य"),
            // U+FEFF is a format character, not a space.
            ("hello\u{FEFF} there", "hello there"),
            ("\u{FEFF}", ""),
            // An extra word at the end.
            ("hello there", "hello there friend"),
        ];
        for (a, b) in different {
            assert!(!same_words(a, b), "{a:?} vs {b:?} should differ");
            assert!(!same_words(b, a), "{b:?} vs {a:?} should differ");
        }
    }

    #[test]
    fn blank_means_only_white_space_characters() {
        let blank = [
            "",
            " \t\r\n",
            // NEXT LINE, OGHAM SPACE MARK, EN QUAD, LINE SEPARATOR,
            // PARAGRAPH SEPARATOR, NARROW NO-BREAK SPACE, IDEOGRAPHIC SPACE.
            "\u{0085}\u{1680}\u{2000}\u{2028}\u{2029}\u{202F}\u{3000}",
            " \u{00A0}\n",
        ];
        for text in blank {
            assert!(is_blank(text), "{text:?} should be blank");
        }

        let not_blank = [
            "a",
            // DEVANAGARI VOWEL SIGN I, a combining mark on its own.
            "\u{093F}",
            "\u{200D}",
            "\u{FEFF}",
            "  a  ",
        ];
        for text in not_blank {
            assert!(!is_blank(text), "{text:?} should not be blank");
        }
    }

    #[test]
    fn an_unchanged_reply_hands_back_the_callers_own_spacing() {
        let input = "  hello\u{00A0}\u{00A0}there  \n";
        let out =
            read_reply(input, "hello there".to_string()).expect("an unchanged reply is a success");
        assert_eq!(
            out,
            Translated {
                text: input.to_string(),
                unchanged: true
            }
        );
    }

    #[test]
    fn a_translation_is_returned_exactly_as_received() {
        let reply = "  नमस्ते\u{00A0}दुनिया\n";
        let out = read_reply("hello world", reply.to_string()).expect("a real translation");
        assert_eq!(
            out,
            Translated {
                text: reply.to_string(),
                unchanged: false
            }
        );
    }

    #[test]
    fn a_case_only_difference_is_a_translation() {
        let out = read_reply("hello there", "Hello There".to_string()).expect("a reply with text");
        assert!(!out.unchanged);
        assert_eq!(out.text, "Hello There");
    }

    /// U+FEFF is not whitespace, so a reply of nothing else is not blank.
    /// The model would have to misbehave for this to happen; the rule stays
    /// consistent rather than special-casing it.
    #[test]
    fn a_reply_of_only_a_byte_order_mark_is_not_blank() {
        let out = read_reply("hello", "\u{FEFF}".to_string()).expect("not blank");
        assert_eq!(
            out,
            Translated {
                text: "\u{FEFF}".to_string(),
                unchanged: false
            }
        );
    }

    #[test]
    fn a_blank_reply_is_empty_whatever_the_input() {
        for reply in ["", "   ", "\n\u{2003}\u{00A0}"] {
            for input in ["hello", "   "] {
                assert_eq!(
                    read_reply(input, reply.to_string()),
                    Err(TranslateError::Empty),
                    "reply {reply:?}, input {input:?}"
                );
            }
        }
    }

    /// The cap runs before the blank check, so spaces past the limit are
    /// `TooLong`, not `Empty`. Dead port: a request would come back as `Net`.
    #[tokio::test]
    async fn over_long_whitespace_is_too_long_not_empty() {
        let http = reqwest::Client::new();
        let text = " ".repeat(MAX_INPUT_CHARS + 1);
        let err = translate_at(
            &http,
            DEAD_PORT,
            "k",
            &text,
            "en-IN",
            "hi-IN",
            Duration::from_millis(200),
        )
        .await
        .expect_err("over the cap");
        assert_eq!(err, TranslateError::TooLong);
    }

    // --- Length ------------------------------------------------------------

    #[tokio::test]
    async fn one_character_over_the_cap_is_too_long() {
        let http = reqwest::Client::new();
        let text = "a".repeat(MAX_INPUT_CHARS + 1);
        // Points at a port with nothing on it: proving `TooLong` comes back
        // rather than a connection error is what proves the check happens
        // *before* the request, not after a wasted round trip.
        let err = translate_at(
            &http,
            DEAD_PORT,
            "k",
            &text,
            "en-IN",
            "hi-IN",
            Duration::from_millis(200),
        )
        .await
        .expect_err("2001 characters must be refused");
        assert_eq!(err, TranslateError::TooLong);
    }

    /// The cap is a limit, not a fence one short of it: exactly 2000 must go
    /// through.
    #[tokio::test]
    async fn exactly_the_cap_is_accepted() {
        let (listener, addr) = bind().await;
        let _captured = spawn_stub(listener, json_200(r#"{"translated_text":"ठीक है"}"#));

        let http = reqwest::Client::new();
        let text = "a".repeat(MAX_INPUT_CHARS);
        let out = translate_at(
            &http,
            &format!("http://{addr}"),
            "k",
            &text,
            "en-IN",
            "hi-IN",
            Duration::from_secs(2),
        )
        .await
        .expect("exactly 2000 characters is inside the cap");
        assert!(!out.unchanged);
    }

    /// THE trap this cap invites: Devanagari is three bytes per character, so
    /// 2000 characters is ~6000 bytes. A `len()` check would refuse a legal
    /// Hindi dictation at a third of the real limit — which is most of this
    /// app's users.
    #[tokio::test]
    async fn the_cap_counts_characters_not_bytes() {
        let text = "क".repeat(MAX_INPUT_CHARS);
        assert!(text.len() > MAX_INPUT_CHARS * 2, "the trap must be live");

        let (listener, addr) = bind().await;
        let _captured = spawn_stub(listener, json_200(r#"{"translated_text":"ok"}"#));
        let http = reqwest::Client::new();
        let out = translate_at(
            &http,
            &format!("http://{addr}"),
            "k",
            &text,
            "hi-IN",
            "en-IN",
            Duration::from_secs(2),
        )
        .await;
        assert!(
            out.is_ok(),
            "2000 Devanagari characters must not read as too long: {out:?}"
        );
    }

    // --- Round trips -------------------------------------------------------

    #[tokio::test]
    async fn a_translated_reply_comes_back_with_unchanged_false() {
        let (listener, addr) = bind().await;
        let captured = spawn_stub(
            listener,
            json_200(r#"{"request_id":"x","translated_text":"नमस्ते","source_language_code":"en-IN"}"#),
        );

        let http = reqwest::Client::new();
        let out = translate_at(
            &http,
            &format!("http://{addr}"),
            "k",
            "hello",
            "en-IN",
            "hi-IN",
            Duration::from_secs(2),
        )
        .await
        .expect("a documented 200 must translate");
        assert_eq!(
            out,
            Translated {
                text: "नमस्ते".into(),
                unchanged: false
            }
        );

        // ...and the body that produced it was the documented shape.
        let body = captured.await.expect("the stub captured a request");
        let sent: serde_json::Value = serde_json::from_str(&body).expect("the request body is JSON");
        assert_eq!(sent["input"], "hello");
        assert_eq!(sent["model"], TRANSLATE_MODEL);
    }

    /// The unchanged rule compares the words, not the spacing, so a reply that
    /// differs from the input only in whitespace is still unchanged. `text`
    /// comes back as the caller's own input, not the model's copy — a caller
    /// pastes `text` without ever branching on `unchanged`.
    #[tokio::test]
    async fn a_whitespace_only_difference_still_counts_as_unchanged() {
        let (listener, addr) = bind().await;
        let _captured = spawn_stub(
            listener,
            json_200(r#"{"translated_text":"  hello   there\n"}"#),
        );

        let http = reqwest::Client::new();
        let out = translate_at(
            &http,
            &format!("http://{addr}"),
            "k",
            "hello there",
            "hi-IN",
            "hi-IN",
            Duration::from_secs(2),
        )
        .await
        .expect("an unchanged reply is a success, not an error");
        assert!(out.unchanged, "a respaced copy must be reported as unchanged");
        assert_eq!(
            out.text, "hello there",
            "an unchanged reply keeps the caller's own text, not the model's respacing"
        );
    }

    #[tokio::test]
    async fn an_empty_reply_is_empty() {
        let (listener, addr) = bind().await;
        let _captured = spawn_stub(listener, json_200(r#"{"translated_text":""}"#));

        let http = reqwest::Client::new();
        let err = translate_at(
            &http,
            &format!("http://{addr}"),
            "k",
            "hello",
            "en-IN",
            "hi-IN",
            Duration::from_secs(2),
        )
        .await
        .expect_err("an empty translation must not be returned as text");
        assert_eq!(err, TranslateError::Empty);
    }

    /// A reply made only of whitespace, some of it outside ASCII, arrives
    /// through the real parse step and is still `Empty`: nothing in it can
    /// replace the dictation.
    #[tokio::test]
    async fn a_reply_of_only_spaces_and_breaks_is_empty() {
        let (listener, addr) = bind().await;
        // JSON escapes for NO-BREAK SPACE, EM SPACE and IDEOGRAPHIC SPACE.
        let _captured = spawn_stub(
            listener,
            json_200(r#"{"translated_text":"\u00a0 \n \u2003\t\u3000 "}"#),
        );

        let http = reqwest::Client::new();
        let err = translate_at(
            &http,
            &format!("http://{addr}"),
            "k",
            "hello",
            "en-IN",
            "hi-IN",
            Duration::from_secs(2),
        )
        .await
        .expect_err("a whitespace reply is not a translation");
        assert_eq!(err, TranslateError::Empty);
    }

    /// A 200 whose body isn't the documented shape must not panic, and says
    /// the same thing to the caller as an empty one: there is no translation
    /// to use.
    #[tokio::test]
    async fn an_unparseable_body_is_empty_not_a_panic() {
        let (listener, addr) = bind().await;
        let _captured = spawn_stub(
            listener,
            "HTTP/1.1 200 OK\r\nContent-Length: 8\r\nConnection: close\r\n\r\nnot json".into(),
        );

        let http = reqwest::Client::new();
        let err = translate_at(
            &http,
            &format!("http://{addr}"),
            "k",
            "hello",
            "en-IN",
            "hi-IN",
            Duration::from_secs(2),
        )
        .await
        .expect_err("a body that isn't the documented shape carries no translation");
        assert_eq!(err, TranslateError::Empty);
    }

    /// A 200 whose body is a bare JSON *string* — well-formed JSON, wrong
    /// type — is the shape that would turn a formatted parse error into a
    /// transcript leak. It must resolve to `Empty` like any other
    /// undocumented body.
    #[tokio::test]
    async fn a_bare_json_string_body_is_empty_not_a_panic() {
        let (listener, addr) = bind().await;
        let _captured = spawn_stub(listener, json_200(r#""a bare string, not an object""#));

        let http = reqwest::Client::new();
        let err = translate_at(
            &http,
            &format!("http://{addr}"),
            "k",
            "hello",
            "en-IN",
            "hi-IN",
            Duration::from_secs(2),
        )
        .await
        .expect_err("a bare string is not the documented shape");
        assert_eq!(err, TranslateError::Empty);
    }

    /// THE LEAK, pinned. `serde_json::Error`'s `Display` is a bare
    /// line/column only for *syntax* errors; a type mismatch embeds the
    /// offending value, and on this endpoint the offending value is the
    /// user's dictation. This asserts both halves: that `{e}` really would
    /// carry the transcript (so nobody "simplifies" the log back to it), and
    /// that the three fields actually logged cannot.
    #[test]
    fn the_parse_failure_log_fields_cannot_carry_the_body() {
        let secret = "meet me at the clinic at four";
        let body = serde_json::to_string(secret).expect("a JSON string");
        // Matched rather than `expect_err`: that would need `TranslateResponse`
        // to be `Debug`, and a `Debug` on a struct whose one field holds the
        // transcript is the same hazard this test exists to close.
        let e = match serde_json::from_str::<TranslateResponse>(&body) {
            Ok(_) => panic!("a bare string must not parse as the documented shape"),
            Err(e) => e,
        };

        assert!(
            e.to_string().contains(secret),
            "if this ever stops holding, the hazard changed shape — re-read the log policy: {e}"
        );

        let logged = format!("{:?} {} {}", e.classify(), e.line(), e.column());
        assert!(
            !logged.contains(secret),
            "the logged fields must not carry body text: {logged}"
        );
    }

    /// Blank input never reaches the network: there is nothing to translate,
    /// and the answer ("keep what you have") is already known. Pointed at a
    /// dead port so a request would fail loudly as `Net`.
    #[tokio::test]
    async fn blank_input_short_circuits_without_a_request() {
        let http = reqwest::Client::new();
        for blank in ["", "   ", "\u{00a0}\n"] {
            let err = translate_at(
                &http,
                DEAD_PORT,
                "k",
                blank,
                "en-IN",
                "hi-IN",
                Duration::from_millis(200),
            )
            .await
            .expect_err("blank input has no translation");
            assert_eq!(err, TranslateError::Empty, "input {blank:?}");
        }
    }

    // --- Refusals ----------------------------------------------------------

    #[tokio::test]
    async fn a_4xx_is_reported_with_its_status() {
        let (listener, addr) = bind().await;
        let _captured = spawn_stub(listener, status_only("400 Bad Request"));

        let http = reqwest::Client::new();
        let err = translate_at(
            &http,
            &format!("http://{addr}"),
            "k",
            "hello",
            "auto",
            "hi-IN",
            Duration::from_secs(2),
        )
        .await
        .expect_err("a 400 is a refusal");
        assert_eq!(err, TranslateError::Http { status: 400 });
    }

    #[tokio::test]
    async fn an_unauthorized_key_says_so_rather_than_blaming_the_network() {
        let (listener, addr) = bind().await;
        let _captured = spawn_stub(listener, status_only("401 Unauthorized"));

        let http = reqwest::Client::new();
        let err = translate_at(
            &http,
            &format!("http://{addr}"),
            "bad",
            "hello",
            "en-IN",
            "hi-IN",
            Duration::from_secs(2),
        )
        .await
        .expect_err("a 401 is a refusal");
        assert_eq!(err, TranslateError::Http { status: 401 });
        assert!(err.user_message().contains("key"));
    }

    /// A 5xx is the service, not the network — and says the same thing the
    /// WebSocket path already says for the same cause.
    #[tokio::test]
    async fn a_5xx_borrows_the_shared_service_unavailable_prose() {
        let (listener, addr) = bind().await;
        let _captured = spawn_stub(listener, status_only("503 Service Unavailable"));

        let http = reqwest::Client::new();
        let err = translate_at(
            &http,
            &format!("http://{addr}"),
            "k",
            "hello",
            "en-IN",
            "hi-IN",
            Duration::from_secs(2),
        )
        .await
        .expect_err("a 503 is a refusal");
        assert_eq!(err, TranslateError::Http { status: 503 });
        assert_eq!(
            err.user_message(),
            NetFailure::ServiceUnavailable.user_message()
        );
    }

    // --- Network -----------------------------------------------------------

    /// One `std::error::Error` wrapping another, the way reqwest wraps hyper
    /// wraps the connector wraps the socket failure. Enough to prove the walk
    /// goes all the way down rather than stopping at the first layer.
    #[derive(Debug)]
    struct Layer(Box<dyn std::error::Error + Send + Sync + 'static>);

    impl std::fmt::Display for Layer {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "layer")
        }
    }

    impl std::error::Error for Layer {
        fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
            Some(self.0.as_ref())
        }
    }

    /// A refusal buried two layers down must still come back as `Refused` —
    /// this is the whole reason the classifier digs for an `io::Error`
    /// instead of stopping at reqwest's own typed predicates.
    ///
    /// Asserted against a hand-built chain rather than a real refused
    /// connection: see [`classify_error_chain`]'s note on loopback connects
    /// that never refuse.
    #[test]
    fn a_buried_refusal_is_classified_through_the_shared_taxonomy() {
        let chain = Layer(Box::new(Layer(Box::new(std::io::Error::from(
            std::io::ErrorKind::ConnectionRefused,
        )))));
        assert_eq!(classify_error_chain(&chain), NetFailure::Refused);
    }

    /// The distinction that would be lost by classifying on reqwest's
    /// predicates alone: a DNS failure is a bare OS error with no `ErrorKind`
    /// of its own, and only `net_error`'s Winsock table can tell it from any
    /// other connect failure.
    #[test]
    fn a_buried_dns_failure_keeps_its_own_identity() {
        let chain = Layer(Box::new(std::io::Error::from_raw_os_error(11001)));
        assert_eq!(classify_error_chain(&chain), NetFailure::NameNotResolved);
    }

    /// A chain with no `io::Error` anywhere in it must fall through rather
    /// than misreport whatever the last layer happened to be.
    #[test]
    fn a_chain_with_no_io_error_is_other() {
        let chain = Layer(Box::new(TranslateError::Empty));
        assert_eq!(classify_error_chain(&chain), NetFailure::Other);
    }

    /// Headers, then silence. The budget is on the request itself, so it
    /// bounds the *body* read too — a stalled body must not park the
    /// finalize path forever. Short timeout so the test is fast; the
    /// mechanism is the one the shipped call uses.
    #[tokio::test]
    async fn a_stalled_response_body_times_out_rather_than_hanging_forever() {
        let (listener, addr) = bind().await;
        tokio::spawn(async move {
            if let Ok((socket, _)) = listener.accept().await {
                let _ = read_http_request_body(&socket).await;
                write_raw(
                    &socket,
                    b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 40\r\n\r\n",
                )
                .await;
                tokio::time::sleep(Duration::from_secs(60)).await;
                drop(socket);
            }
        });

        let http = reqwest::Client::new();
        let started = tokio::time::Instant::now();
        let err = translate_at(
            &http,
            &format!("http://{addr}"),
            "k",
            "hello",
            "en-IN",
            "hi-IN",
            Duration::from_millis(200),
        )
        .await
        .expect_err("a stalled body must not hang the call");
        assert_eq!(err, TranslateError::Net(NetFailure::Timeout));
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "took {:?}",
            started.elapsed()
        );
    }

    // --- User-facing prose -------------------------------------------------

    #[test]
    fn every_failure_says_something_of_its_own() {
        let all = [
            TranslateError::Net(NetFailure::NameNotResolved),
            TranslateError::TooLong,
            TranslateError::Empty,
            TranslateError::Http { status: 400 },
        ];
        let messages: std::collections::HashSet<&str> =
            all.iter().map(|e| e.user_message()).collect();
        assert_eq!(messages.len(), all.len());
    }

    // --- Stub server -------------------------------------------------------
    //
    // Same hand-rolled loopback server `sarvam::chat` and `format::backend`
    // use: this crate has no HTTP test-server dependency, and parsing just
    // enough of a request to find `Content-Length` is simpler than adding one.

    async fn bind() -> (tokio::net::TcpListener, std::net::SocketAddr) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind loopback listener");
        let addr = listener.local_addr().expect("local addr");
        (listener, addr)
    }

    /// Serves exactly one request: reads it in full, hands the raw request
    /// body back over the returned channel (so a test can assert what went on
    /// the wire), then writes `response` verbatim.
    fn spawn_stub(
        listener: tokio::net::TcpListener,
        response: String,
    ) -> tokio::sync::oneshot::Receiver<String> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            if let Ok((socket, _)) = listener.accept().await {
                let body = read_http_request_body(&socket).await;
                let _ = tx.send(body);
                write_raw(&socket, response.as_bytes()).await;
            }
        });
        rx
    }

    fn json_200(body: &str) -> String {
        format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        )
    }

    /// Sarvam answers some refusals with an empty body, so the status line is
    /// the whole signal.
    fn status_only(status: &str) -> String {
        format!("HTTP/1.1 {status}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
    }

    async fn read_http_request_body(socket: &tokio::net::TcpStream) -> String {
        let mut buf = Vec::new();
        let mut chunk = [0u8; 8192];
        loop {
            if let Some(header_end) = find_double_crlf(&buf) {
                let headers = String::from_utf8_lossy(&buf[..header_end]);
                let content_length: usize = headers
                    .lines()
                    .find_map(|l| {
                        l.to_lowercase()
                            .starts_with("content-length:")
                            .then(|| l.splitn(2, ':').nth(1).unwrap_or("0").trim().to_string())
                    })
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(0);
                let body_start = header_end + 4;
                if buf.len() >= body_start + content_length {
                    return String::from_utf8_lossy(&buf[body_start..body_start + content_length])
                        .to_string();
                }
            }
            socket.readable().await.expect("socket readable");
            match socket.try_read(&mut chunk) {
                Ok(0) => return String::from_utf8_lossy(&buf).to_string(),
                Ok(n) => buf.extend_from_slice(&chunk[..n]),
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => continue,
                Err(e) => panic!("failed reading stub request: {e}"),
            }
        }
    }

    fn find_double_crlf(buf: &[u8]) -> Option<usize> {
        buf.windows(4).position(|w| w == b"\r\n\r\n")
    }

    async fn write_raw(socket: &tokio::net::TcpStream, bytes: &[u8]) {
        let mut written = 0;
        while written < bytes.len() {
            socket.writable().await.expect("socket writable");
            match socket.try_write(&bytes[written..]) {
                Ok(n) => written += n,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => continue,
                Err(e) => panic!("failed writing stub response: {e}"),
            }
        }
    }
}
