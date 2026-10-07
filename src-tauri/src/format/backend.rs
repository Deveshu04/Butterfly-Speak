//! Provider-agnostic chat backend.
//!
//! Sarvam's `/v1/chat/completions` is OpenAI-shaped, so every provider that
//! speaks that dialect is a base URL, a key and a model name. Keeping the
//! transport behind this type is what makes "switch to a faster and cheaper
//! model" a config change rather than a rewrite.

use anyhow::Context;
use serde::Deserialize;
use std::time::Duration;

pub const SARVAM_CHAT_URL: &str = "https://api.sarvam.ai/v1/chat/completions";

/// The cheapest authenticated request on Sarvam's API host, used to warm the
/// polish connection. The server answers `Connection: keep-alive` and the next
/// chat request reuses the socket (connect and TLS time zero).
pub const SARVAM_MODELS_URL: &str = "https://api.sarvam.ai/v1/models";

/// The relay's chat route. Appended to the relay's base URL, so a
/// `Backend`'s `base_url` is a full route on every kind.
pub const RELAY_CHAT_PATH: &str = "/v1/chat/completions";

/// The relay's usage route: the cheapest authenticated request on the relay,
/// and the one the Settings card reads. Used as Cloud mode's warm-up because
/// it is a real `200` that also warms the Worker's JWKS cache, so the polish
/// call that follows pays neither the TLS handshake nor the first key fetch.
pub const RELAY_USAGE_PATH: &str = "/v1/usage";

/// Where a connection warm-up should go for this backend, if anywhere.
///
/// A cold HTTPS connection to Sarvam costs 160–420 ms (once 1,271 ms), and
/// reqwest's pool drops an idle one after 90 s, so without a warm-up the
/// first dictation after any pause pays it. One `GET /v1/models` when
/// recording starts puts a warm connection in the pool before `Finish`.
/// Sarvam and the relay only: a custom endpoint may be a local server that
/// gains nothing or a gateway that logs every request. `Off` never polishes,
/// so it never warms.
pub fn warmup_url(
    backend: &Backend,
    level: crate::format::level::CleanupLevel,
) -> Option<String> {
    if level == crate::format::level::CleanupLevel::Off {
        return None;
    }
    match backend.kind {
        BackendKind::Sarvam => Some(SARVAM_MODELS_URL.to_string()),
        // The relay's own cheapest authenticated route, derived from the
        // chat route this backend already holds so that one base URL is
        // stored rather than two that could disagree.
        BackendKind::Relay => Some(format!(
            "{}{RELAY_USAGE_PATH}",
            backend.base_url.strip_suffix(RELAY_CHAT_PATH)?
        )),
        BackendKind::Custom => None,
    }
}

/// Which host this backend is talking to, and therefore how far it is safe to
/// trust the dialect.
///
/// Sarvam is a host this app has measured: its auth header, its request shape
/// and its empty-bodied 400s are all known quantities, and the requests it
/// receives must not change. A user-supplied OpenAI-compatible host is the
/// opposite — anything from llama.cpp to a corporate gateway — so it gets the
/// conservative treatment: the standard bearer token and nothing else,
/// `max_tokens` rather than its newer name, up to two more attempts without
/// the optional parameters a refusal points at, and `<think>` sections cut
/// from the reply.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum BackendKind {
    #[default]
    Sarvam,
    /// The one custom endpoint slot (`crate::endpoint`).
    Custom,
    /// The Butterfly Labs relay (Cloud mode). Sarvam's dialect — the relay
    /// forwards the body untouched — reached with the user's own sign-in
    /// token instead of a Sarvam key, which this install does not have.
    Relay,
}

#[derive(Clone)]
pub struct Backend {
    pub base_url: String,
    /// Empty means "no credential". Legitimate for a custom endpoint — plenty
    /// of self-hosted servers take no auth — and never for Sarvam.
    pub api_key: String,
    pub model: String,
    pub kind: BackendKind,
}

/// Hand-written so that a key cannot reach a log through a `{:?}` nobody
/// thought about. Nothing formats a `Backend` today; that is exactly when to
/// make it structurally impossible, rather than after the line that does.
///
/// `base_url` is redacted too, and for the same reason it is redacted where
/// the endpoint slot logs it: a pasted URL is where a credential hides when
/// it is not in the key field — `https://user:pw@host/v1` and `?api_key=…`
/// are both ordinary ways to write one.
impl std::fmt::Debug for Backend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Backend")
            .field("base_url", &redacted_url(&self.base_url))
            .field(
                "api_key",
                &if self.api_key.is_empty() {
                    "(none)"
                } else {
                    "(redacted)"
                },
            )
            .field("model", &self.model)
            .field("kind", &self.kind)
            .finish()
    }
}

/// A URL with the two places a secret hides removed: userinfo before the `@`,
/// and everything from the first `?` or `#`. Deliberately a display helper,
/// not a parser — it must never fail, and it must never be mistaken for the
/// security predicate in `crate::endpoint` (which this file, being one
/// `bin/fmtbench.rs` compiles into itself, may not name).
fn redacted_url(url: &str) -> String {
    let (scheme, rest) = match url.split_once("://") {
        Some((s, r)) => (s, r),
        None => ("", url),
    };
    let rest = rest.split(['?', '#']).next().unwrap_or("");
    // The host starts after the last `@` in the authority, if there is one.
    let authority_end = rest.find('/').unwrap_or(rest.len());
    let rest = match rest[..authority_end].rfind('@') {
        Some(at) => &rest[at + 1..],
        None => rest,
    };
    if scheme.is_empty() {
        rest.to_string()
    } else {
        format!("{scheme}://{rest}")
    }
}

/// The `finish_reason` this app writes onto a reply that arrived without its
/// end marker.
///
/// Deliberately *not* `"length"`. Rejecting is the same decision either way
/// (see [`ChatReply::was_truncated`]), but a model that simply won't emit the
/// marker and an API that ran out of `max_tokens` are not the same diagnosis,
/// and folding them made a prompt-compliance problem indistinguishable from a
/// token-budget one in every log — and in `fmtbench --guard-dump`, the
/// artifact `format::guard`'s RETAIN/RATIO thresholds are calibrated from.
pub const MARKER_MISSING: &str = "marker_missing";

#[derive(Clone, Debug)]
pub struct ChatReply {
    pub text: String,
    /// "stop" on a complete answer, "length" when `max_tokens` cut it off,
    /// [`MARKER_MISSING`] when this app rejected it for arriving without its
    /// end marker, and empty for a stream that ended before `data: [DONE]`.
    pub finish_reason: String,
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    /// Milliseconds from the request being sent to the first non-empty
    /// content delta. `Some` only on the streaming (Sarvam) path; `None`
    /// for a custom backend, a hand-built reply, or a stream that produced
    /// no content at all.
    pub first_token_ms: Option<u32>,
}

impl ChatReply {
    /// A truncated reply ends mid-sentence. It is never safe to accept —
    /// whether the API admitted to cutting it off or the missing end
    /// marker is the only evidence.
    pub fn was_truncated(&self) -> bool {
        self.finish_reason == "length" || self.finish_reason == MARKER_MISSING
    }

    /// Strips the per-request marker ([`mint_end_marker`]) from the end of
    /// the reply, proving the model actually reached the end of its own
    /// output rather than being cut off somewhere `finish_reason` doesn't
    /// catch (a mid-stream drop, a proxy truncating the body, or any other
    /// transport failure that still reports "stop"). A reply that does not
    /// end with the marker is folded onto the exact same rejection path as a
    /// `finish_reason == "length"` truncation — `was_truncated()` — rather
    /// than teaching every caller (`format::guard::check` included) a second
    /// truncation signal to check.
    ///
    /// Folded for the *decision*, not for the diagnosis: the reason recorded
    /// is [`MARKER_MISSING`], and it is logged, because "the model ignored an
    /// instruction" and "the reply hit `max_tokens`" call for opposite fixes
    /// and must not read the same in the log.
    ///
    /// Tolerance, measured not guessed: sarvam-105b — a model whose whole job
    /// is punctuating text — ends 33% of live replies with `{marker}.`,
    /// treating the marker as the final word of a sentence (327 of 1000
    /// benchmark cases were thrown away as "truncated" by an exact
    /// `ends_with`, dragging English disfluency F1 from 0.538 to the 0.372
    /// rule floor). So: the marker proves completion wherever it is, as long
    /// as nothing SUBSTANTIVE follows it — a short tail of closing
    /// punctuation and whitespace after the marker is the model punctuating
    /// our sentinel, not content.
    pub fn strip_end_marker(mut self, marker: &str) -> Self {
        if let Some(at) = self.text.rfind(marker) {
            let tail = &self.text[at + marker.len()..];
            let tail_is_noise = tail.chars().count() <= 4
                && tail.chars().all(|c| {
                    c.is_whitespace() || matches!(c, '.' | '!' | '?' | '"' | '\'' | ')' | '।' | '॥')
                });
            if tail_is_noise {
                self.text = self.text[..at].trim_end().to_string();
                return self;
            }
        }
        // Counts only, never the reply itself (`sarvam::codec`'s
        // policy) — but the counts are the whole diagnosis here: a
        // full-length reply that merely skipped the marker looks
        // nothing like one the API cut off.
        tracing::warn!(
            api_finish_reason = %self.finish_reason,
            reply_chars = self.text.chars().count(),
            completion_tokens = self.completion_tokens,
            "reply did not end with its marker; rejecting it as truncated"
        );
        self.finish_reason = MARKER_MISSING.into();
        self
    }
}

/// Symbols an end marker is drawn from: upper-case base-32 without the
/// glyphs that look alike (`0/O`, `1/I/L`), so a marker is never misread in
/// a log. 30 symbols, four positions: 810,000 values.
pub(crate) const MARKER_ALPHABET: &[u8] = b"ABCDEFGHJKMNPQRSTVWXYZ23456789";

/// Mints a fresh, per-request end marker. Unique every call — not a
/// fixed sentinel — so a truncated reply that happens to echo text from its
/// own context can never produce it by accident, and so a marker cannot be
/// anticipated ahead of the request that mints it.
///
/// Short on purpose. The marker's only cost is the output tokens the model
/// spends writing it: a UUID-length marker (`__BS_COMPLETE_<uuid4>__`)
/// tokenises to ~33 pieces for sarvam-105b — roughly 270 ms of decode at its
/// measured ~120 tok/s, on every polish, transform, agent and note call.
/// `<<7K3Q>>` is ~6 tokens, and the model appended it in every live run. Its
/// job is to prove the model reached the end of its own output, not to be
/// unguessable.
pub fn mint_end_marker() -> String {
    let bytes = uuid::Uuid::new_v4().into_bytes();
    let body: String = bytes[..4]
        .iter()
        .map(|b| MARKER_ALPHABET[*b as usize % MARKER_ALPHABET.len()] as char)
        .collect();
    format!("<<{body}>>")
}

/// The instruction that goes with a [`mint_end_marker`] marker: the model
/// ends its text with its own final punctuation, then writes the marker
/// as given on a line of its own, so [`ChatReply::strip_end_marker`] can
/// find it at the end of a complete reply and treat its absence as
/// truncation.
///
/// The shape is measured, not guessed. A marker asked for directly after the
/// text's last character takes the place of the sentence's own full stop:
/// counted against the 200 benchmark inputs on sarvam-105b, the
/// sentence-final punctuation went missing in 53/200 replies with a
/// UUID-length marker and 62/200 with the short `<<XXXX>>` one, and in 75/200
/// when the instruction only asked the model to keep that punctuation. A line
/// of its own for the marker, after the text's own punctuation, brought it
/// down to 23/200, and the wording below keeps that shape.
pub fn end_marker_rule(marker: &str) -> String {
    // The marker is deliberately the LAST thing in this instruction, with
    // nothing after it: when a sentence carried on past the marker, the
    // marker came back with a full stop after it in a third of live replies,
    // since sarvam-105b punctuates whatever it writes. The instruction ends
    // exactly the way the reply should, line break included.
    format!(
        "Finish your text with its own final punctuation. Then write this end \
         marker on a new line by itself, copied exactly, and stop there; the \
         app deletes that line before anyone reads your reply:\n{marker}"
    )
}

impl Backend {
    pub fn sarvam(api_key: &str, model: &str) -> Self {
        Self {
            base_url: SARVAM_CHAT_URL.into(),
            api_key: api_key.to_string(),
            model: model.to_string(),
            kind: BackendKind::Sarvam,
        }
    }

    /// The one custom OpenAI-compatible endpoint. `chat_url` is a full route
    /// (`crate::endpoint::chat_completions_url` builds it); `api_key` is
    /// `None` for a server that takes no auth.
    pub fn custom(chat_url: &str, api_key: Option<String>, model: &str) -> Self {
        Self {
            base_url: chat_url.to_string(),
            api_key: api_key.unwrap_or_default(),
            model: model.to_string(),
            kind: BackendKind::Custom,
        }
    }

    /// Cloud mode's polish backend. `base_url` is the relay's base
    /// (`https://…`, no trailing slash); `bearer` is the user's Supabase
    /// access token, which is the *only* credential this app holds in Cloud
    /// mode — the Sarvam key lives in the relay's Cloudflare secrets.
    ///
    /// The model still travels: the relay allowlists it against the app's
    /// two Sarvam chat ids and forwards the body otherwise untouched, so the
    /// request that reaches Sarvam is the same one Bring-your-own-key sends
    /// and every guardrail and benchmark stays valid.
    pub fn relay(base_url: &str, bearer: &str, model: &str) -> Self {
        Self {
            base_url: format!("{}{RELAY_CHAT_PATH}", base_url.trim_end_matches('/')),
            api_key: bearer.to_string(),
            model: model.to_string(),
            kind: BackendKind::Relay,
        }
    }

    /// Put this backend's credential on a request — the one place that
    /// decides which header carries what, so the warm-up and the chat call
    /// cannot drift apart.
    ///
    /// * Sarvam gets both its own header and a bearer, exactly as it always
    ///   has (each ignores the other, and both have been measured).
    /// * The relay gets the bearer alone: it authenticates the *user*, and
    ///   `api-subscription-key` would be a Sarvam key this app does not have.
    /// * A custom host gets the standard bearer and only when a key was
    ///   entered — never `api-subscription-key`, which is Sarvam's own
    ///   scheme and would hand a third-party host a credential in a header
    ///   the user never agreed to.
    pub fn authenticate(&self, req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match self.kind {
            BackendKind::Sarvam => req
                .header("api-subscription-key", &self.api_key)
                .bearer_auth(&self.api_key),
            BackendKind::Relay => req.bearer_auth(&self.api_key),
            BackendKind::Custom if !self.api_key.is_empty() => req.bearer_auth(&self.api_key),
            BackendKind::Custom => req,
        }
    }

    pub async fn complete(
        &self,
        http: &reqwest::Client,
        system: &str,
        user: &str,
        max_tokens: u32,
        temperature: f32,
        timeout: Duration,
    ) -> anyhow::Result<ChatReply> {
        let mut body = build_request_body(
            &self.model,
            system,
            user,
            max_tokens,
            temperature,
            // Streamed on both Sarvam-dialect lanes: the relay pipes the SSE
            // through untouched, so Cloud mode measures the same
            // first-token time the latency ladder is built on.
            matches!(self.kind, BackendKind::Sarvam | BackendKind::Relay),
        );
        match self.kind {
            BackendKind::Sarvam | BackendKind::Relay => {
                use futures_util::StreamExt;
                let started = std::time::Instant::now();
                let resp = self
                    .authenticate(http.post(&self.base_url))
                    .header("accept", "text/event-stream")
                    .json(&body)
                    // Covers the whole exchange, streamed body included:
                    // reqwest's per-request timeout runs until the body has
                    // finished, so `POLISH_TIMEOUT` still bounds a stream
                    // that stalls mid-reply.
                    .timeout(timeout)
                    .send()
                    .await?;
                let status = resp.status();
                if !status.is_success() {
                    // Sarvam answers 400 with an EMPTY body, so the status code is
                    // the only signal, and `HttpFailure` names it. Never the raw
                    // body, and never the server's message in the error text:
                    // this error becomes `PolishOutcome::Failed`, which `ws.rs`
                    // logs, and a body that echoes the request would carry the
                    // transcript into the log file (see `HttpFailure`). An empty
                    // Sarvam 400 renders as "(empty body)".
                    let raw = resp.text().await?;
                    // The relay's weekly chat cap is its own error — see
                    // `WeeklyChatLimit`. Matched on the relay's own body, so
                    // its per-minute `429` and anything from Sarvam stay the
                    // ordinary failure below.
                    if self.kind == BackendKind::Relay
                        && status.as_u16() == 429
                        && raw.trim() == RELAY_WEEKLY_CHAT_LIMIT
                    {
                        return Err(WeeklyChatLimit.into());
                    }
                    return Err(HttpFailure::new(status, &raw).into());
                }
                // Asking for a stream is not the same as getting one: a
                // buffering proxy, or a host that simply ignores `stream`,
                // answers with the whole JSON body instead. That reply is
                // still perfectly good — it just carries no first-token
                // time — so the content type, not the request, decides how
                // to read it.
                // A media type is case-insensitive and routinely carries
                // parameters (`text/event-stream; charset=utf-8`), so match
                // the prefix, folded (RFC 9110 section 8.3).
                let content_type = resp
                    .headers()
                    .get(reqwest::header::CONTENT_TYPE)
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or_default()
                    .trim_start()
                    .to_ascii_lowercase();
                let streamed = content_type.starts_with("text/event-stream");
                if streamed {
                    let mut acc = SseAccumulator::default();
                    let mut stream = resp.bytes_stream();
                    while let Some(chunk) = stream.next().await {
                        let chunk = chunk.context("reading chat stream")?;
                        acc.feed(&chunk, std::time::Instant::now());
                    }
                    acc.finish(started)
                } else {
                    let raw = resp.text().await?;
                    parse_reply(&raw)
                }
            }
            BackendKind::Custom => {
                let raw = self.send_custom(http, &mut body, timeout).await?;
                let mut reply = parse_reply(&raw)?;
                // A model that reasons out loud can put a `<think>…</think>`
                // section ahead of its answer. Every caller wants the answer
                // alone, so the section is cut here; Sarvam's replies never
                // contain one.
                reply.text = strip_think_tags(&reply.text);
                Ok(reply)
            }
        }
    }

    /// Sends a custom-endpoint request and returns the body of the first
    /// success. A 400 or 422 earns up to two more attempts, each sent
    /// without some of [`DROPPABLE_PARAMS`]; any other failure, or a third
    /// refusal, is returned as it came.
    async fn send_custom(
        &self,
        http: &reqwest::Client,
        body: &mut serde_json::Value,
        timeout: Duration,
    ) -> anyhow::Result<String> {
        // Each flag marks an attempt already spent, so the loop sends the
        // body at most three times, serialising it afresh each time.
        let mut sent_without_effort = false;
        let mut sent_without_named = false;
        loop {
            // Bearer only, and only when there is a key — never Sarvam's own
            // `api-subscription-key` header (see `authenticate`).
            let req = self.authenticate(http.post(&self.base_url));
            let resp = req.json(&*body).timeout(timeout).send().await?;
            let status = resp.status();
            let raw = resp.text().await?;
            if status.is_success() {
                return Ok(raw);
            }

            let code = status.as_u16();
            let refused = code == 400 || code == 422;
            if refused && !sent_without_effort {
                sent_without_effort = true;
                // Some servers refuse a field they do not know without saying
                // which one, and `reasoning_effort` is the newest field this
                // app sends, so it goes first, before the error is read.
                if drop_reasoning_effort(body) {
                    // At warn level so a support log shows it: a server that
                    // needs this pays for two requests on every dictation.
                    tracing::warn!(
                        status = code,
                        dropped = "reasoning_effort",
                        "custom endpoint refused the request; resending without reasoning_effort"
                    );
                    continue;
                }
                // Nothing to drop yet, so this same reply is checked for a
                // parameter it names.
            }
            if refused && !sent_without_named {
                sent_without_named = true;
                let named = drop_params_named_in(body, &raw);
                if !named.is_empty() {
                    tracing::warn!(
                        status = code,
                        dropped = ?named,
                        "custom endpoint named the parameters it refuses; resending without them"
                    );
                    continue;
                }
            }

            return Err(HttpFailure::new(status, &raw).into());
        }
    }
}

/// The parameters [`build_request_body`] adds on its own, beyond the request
/// itself. When a custom endpoint's 400 or 422 mentions one by name, the
/// next attempt goes without it: the reply then loses a preference (a
/// sampling temperature, a request not to reason), never its meaning.
/// `model`, `messages` and `max_tokens` are the request, so none of them is
/// here.
///
/// `max_tokens` keeps that name on this lane and is never renamed to
/// `max_completion_tokens`: a server running an open model on the user's own
/// hardware commonly knows only the older name and answers the newer one with
/// a 400, which dropping a parameter could not fix.
const DROPPABLE_PARAMS: [&str; 2] = ["temperature", "reasoning_effort"];

/// Removes `reasoning_effort` from `body`, and says whether it was there.
fn drop_reasoning_effort(body: &mut serde_json::Value) -> bool {
    body.as_object_mut()
        .is_some_and(|obj| obj.remove("reasoning_effort").is_some())
}

/// Removes every [`DROPPABLE_PARAMS`] entry that `body` carries and
/// `error_body` mentions, all at once, and returns the ones removed. A plain
/// substring test; a parameter the request never carried is never reported.
fn drop_params_named_in(body: &mut serde_json::Value, error_body: &str) -> Vec<&'static str> {
    let Some(obj) = body.as_object_mut() else {
        return Vec::new();
    };
    let named: Vec<&'static str> = DROPPABLE_PARAMS
        .into_iter()
        .filter(|p| obj.contains_key(*p) && error_body.contains(*p))
        .collect();
    for p in &named {
        obj.remove(*p);
    }
    named
}

/// This week's Cloud allowance is spent: the relay closed the realtime
/// socket with close code `4029` (reason `quota`), or answered a chat call
/// with `429 weekly chat limit`. Either one lasts until the week rolls over,
/// so the sentence names the way out rather than "try again". Re-exported as
/// `sarvam::MSG_CLOUD_QUOTA`, which is the name every Cloud caller uses; it
/// lives here so that `fmtbench`, which compiles this module and
/// `sarvam::chat` but not the rest of `sarvam`, still has it.
pub const MSG_CLOUD_QUOTA: &str =
    "Weekly cloud limit reached — switch to Bring your own key or Local";

/// The body of the relay's `429` when this user's chat calls for the week
/// are spent (`relay/src/user_session.ts`). The body is the only thing that
/// tells it from the per-minute `429 rate limited`, which clears by itself.
pub(crate) const RELAY_WEEKLY_CHAT_LIMIT: &str = "weekly chat limit";

/// A chat call the relay refused because this week's chat calls are spent.
///
/// An error type of its own rather than a status inside a string, so that
/// every caller that turns a failed chat call into a sentence can find it
/// with a downcast (`sarvam::chat::failure_sentence`) instead of parsing
/// text. Its `Display` *is* that sentence: a caller that prints the error
/// as-is (the Prompts page's test) says the right thing too.
#[derive(Debug)]
pub struct WeeklyChatLimit;

impl std::fmt::Display for WeeklyChatLimit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(MSG_CLOUD_QUOTA)
    }
}

impl std::error::Error for WeeklyChatLimit {}

/// The most useful sentence in an error body, for the user's own screen.
///
/// A failed request's body can echo what was sent, and what was sent is the
/// user's transcript. So: pull the server's own diagnostic out of the
/// standard OpenAI / FastAPI error shapes, cap it, and otherwise report only
/// a size. Even the diagnostic can quote the request, so this is shown, not
/// logged: a failed chat call logs [`HttpFailure`]'s text instead.
///
/// `pub(crate)` for `endpoint::probe`, whose Test connection shows it on the
/// Settings screen and logs nothing.
pub(crate) fn error_summary(raw: &str) -> String {
    match server_message(raw) {
        Some(m) => m,
        None => body_size(raw),
    }
}

/// The server's own diagnostic from the standard OpenAI / FastAPI error
/// shapes, capped at 200 characters, or `None` when the body has none.
fn server_message(raw: &str) -> Option<String> {
    const CAP: usize = 200;
    let parsed = serde_json::from_str::<serde_json::Value>(raw).ok()?;
    let mut m = parsed
        .get("error")
        .and_then(|e| e.get("message"))
        .and_then(|m| m.as_str())
        .or_else(|| parsed.get("message").and_then(|m| m.as_str()))
        .or_else(|| parsed.get("detail").and_then(|d| d.as_str()))
        .filter(|s| !s.is_empty())?
        .to_string();
    if m.chars().count() > CAP {
        m = m.chars().take(CAP).collect::<String>() + "…";
    }
    Some(m)
}

/// A body described by its size alone.
fn body_size(raw: &str) -> String {
    if raw.is_empty() {
        "(empty body)".into()
    } else {
        format!("({} bytes, no error message)", raw.len())
    }
}

/// A chat backend answered with a non-2xx status.
///
/// Every caller logs a failed call as `{e:#}`, and the log file is kept
/// whatever the history settings say. A server can build its error message
/// from the request (a template error, a proxy quoting what it refused), and
/// the request is the user's transcript. So this error's `Display` and
/// `Debug` name the status and how long the server's message was, never the
/// message. The message is kept for the user's own screen: [`shown_failure`]
/// hands it to the Prompts page's test, which returns it to the page and does
/// not log it.
pub struct HttpFailure {
    status: reqwest::StatusCode,
    /// The server's own diagnostic, capped (see [`server_message`]).
    message: Option<String>,
    /// [`body_size`] of the body, for when there is no message.
    size: String,
}

impl HttpFailure {
    fn new(status: reqwest::StatusCode, raw: &str) -> Self {
        Self {
            status,
            message: server_message(raw),
            size: body_size(raw),
        }
    }

    /// The failure with the server's message, for the user's own screen only.
    pub fn shown(&self) -> String {
        match &self.message {
            Some(m) => format!("chat backend returned HTTP {}: {m}", self.status),
            None => self.to_string(),
        }
    }
}

impl std::fmt::Display for HttpFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.message {
            Some(m) => write!(
                f,
                "chat backend returned HTTP {} (its {}-character error message is not logged)",
                self.status,
                m.chars().count()
            ),
            None => write!(f, "chat backend returned HTTP {}: {}", self.status, self.size),
        }
    }
}

impl std::fmt::Debug for HttpFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(self, f)
    }
}

impl std::error::Error for HttpFailure {}

/// [`HttpFailure::shown`] for a failed call, when an HTTP status is what
/// failed it. For the user's own screen only; never log it.
pub fn shown_failure(e: &anyhow::Error) -> Option<String> {
    e.chain()
        .find_map(|c| c.downcast_ref::<HttpFailure>())
        .map(HttpFailure::shown)
}

/// [`redacted_url`] applied to every URL inside a sentence.
///
/// The companion to [`error_summary`], for the other half of the same
/// problem. That one keeps a response *body* out of the logs; this one keeps
/// a *request URL* out of them, which matters because the custom endpoint
/// puts a user-pasted host behind this call. The two ordinary
/// ways a provider's docs hand someone a URL are `https://user:pw@host/v1`
/// and `?api_key=…`, and `reqwest`'s own error `Display` embeds the URL it
/// was calling ("error sending request for url (…)"). So `{e:#}` on a
/// transport failure prints whichever of those the user pasted — into the log
/// file, and into `PolishOutcome::Failed`, which `sarvam::ws` logs again.
///
/// The *policy* is `redacted_url`'s, unchanged and used verbatim: one answer
/// to "what may a URL look like in a log", whether it arrives whole (a
/// `Backend`'s `Debug`) or embedded in somebody else's prose. What this adds
/// is only the scanning — finding the spans, which an error chain assembled
/// by three crates can put anywhere in the string.
pub(crate) fn redact_urls(text: &str) -> String {
    fn is_scheme_byte(b: u8) -> bool {
        b.is_ascii_alphanumeric() || matches!(b, b'+' | b'-' | b'.')
    }
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut cursor = 0;
    while let Some(found) = text[cursor..].find("://") {
        let sep = cursor + found;
        // Walk back over the scheme.
        let mut start = sep;
        while start > cursor && is_scheme_byte(bytes[start - 1]) {
            start -= 1;
        }
        let rest = &text[sep + 3..];
        // A URL ends where the sentence resumes: whitespace, or the
        // punctuation that wrapped it.
        let end = rest
            .find(|c: char| c.is_whitespace() || matches!(c, ')' | '"' | '\'' | ',' | '>'))
            .unwrap_or(rest.len());
        if start == sep || end == 0 {
            // "://" with no scheme in front of it, or nothing after it:
            // ordinary prose, and it has to come out unchanged.
            out.push_str(&text[cursor..sep + 3]);
            cursor = sep + 3;
            continue;
        }
        out.push_str(&text[cursor..start]);
        out.push_str(&redacted_url(&text[start..sep + 3 + end]));
        cursor = sep + 3 + end;
    }
    out.push_str(&text[cursor..]);
    out
}

/// Removes `<think>…</think>` sections from a custom endpoint's reply.
///
/// Polish, transforms and the agent all want the answer and nothing else. An
/// opening tag that is never closed means the rest of the text is reasoning;
/// a closing tag with no opening tag means everything before it was.
fn strip_think_tags(text: &str) -> String {
    const OPEN: &str = "<think>";
    const CLOSE: &str = "</think>";
    // ASCII-lowercasing preserves byte length, so offsets found here index
    // the original string at the same char boundaries.
    let lower = text.to_ascii_lowercase();
    let mut out = String::with_capacity(text.len());
    let mut i = 0usize;
    while i < text.len() {
        match lower[i..].find(OPEN) {
            Some(rel) => {
                let start = i + rel;
                out.push_str(&text[i..start]);
                let after_open = start + OPEN.len();
                match lower[after_open..].find(CLOSE) {
                    Some(rel2) => i = after_open + rel2 + CLOSE.len(),
                    None => i = text.len(),
                }
            }
            None => {
                out.push_str(&text[i..]);
                i = text.len();
            }
        }
    }
    if let Some(at) = out.to_ascii_lowercase().rfind(CLOSE) {
        out = out[at + CLOSE.len()..].to_string();
    }
    out.trim().to_string()
}

/// Builds the OpenAI-shaped request body. A caller's `temperature` is a
/// deliberate choice (formatting wants deterministic output, a creative
/// rewrite doesn't) — this must carry it through untouched rather than
/// picking one value for every caller. Split out as a pure function so that
/// choice is testable without a network call. `pub(crate)` so
/// `sarvam::chat`'s tests can assert `polish` and `transform` don't collapse
/// onto the same temperature.
pub(crate) fn build_request_body(
    model: &str,
    system: &str,
    user: &str,
    max_tokens: u32,
    temperature: f32,
    stream: bool,
) -> serde_json::Value {
    let mut body = serde_json::json!({
        "model": model,
        "temperature": temperature,
        "max_tokens": max_tokens,
        // Reasoning tokens bill as completion tokens and a formatting pass
        // needs none. Verified accepted by Sarvam — and load-bearing: with
        // the key omitted (or "low") sarvam-105b thinks until `max_tokens`,
        // 17–18 s measured.
        "reasoning_effort": null,
        "messages": [
            { "role": "system", "content": system },
            { "role": "user", "content": user },
        ],
    });
    if stream {
        // Server-sent events, with the token usage in the final chunk
        // (`stream_options.include_usage` — accepted by Sarvam, measured).
        body["stream"] = serde_json::Value::Bool(true);
        body["stream_options"] = serde_json::json!({ "include_usage": true });
    }
    body
}

#[derive(Deserialize)]
struct Response {
    choices: Vec<Choice>,
    #[serde(default)]
    usage: Usage,
}

#[derive(Deserialize)]
struct Choice {
    message: Message,
    #[serde(default)]
    finish_reason: String,
}

#[derive(Deserialize)]
struct Message {
    content: String,
}

#[derive(Deserialize, Default)]
struct Usage {
    #[serde(default)]
    prompt_tokens: u32,
    #[serde(default)]
    completion_tokens: u32,
}

fn parse_reply(raw: &str) -> anyhow::Result<ChatReply> {
    // Callers log this error. serde_json's own message can quote a string of
    // the reply, so only its category and position are kept.
    let parsed: Response = serde_json::from_str(raw).map_err(|e| {
        anyhow::anyhow!(
            "chat reply was not the expected JSON ({:?} at line {}, column {})",
            e.classify(),
            e.line(),
            e.column()
        )
    })?;
    let choice = parsed
        .choices
        .into_iter()
        .next()
        .context("chat reply contained no choices")?;
    Ok(ChatReply {
        text: choice.message.content,
        finish_reason: choice.finish_reason,
        prompt_tokens: parsed.usage.prompt_tokens,
        completion_tokens: parsed.usage.completion_tokens,
        first_token_ms: None,
    })
}

/// One SSE `data:` chunk of an OpenAI-shaped streaming reply. Every field
/// defaults: a usage-only chunk has no `choices`, a content chunk has no
/// `usage`, and a role-only opener has an empty `content`.
#[derive(Deserialize, Default)]
struct StreamChunk {
    #[serde(default)]
    choices: Vec<StreamChoice>,
    #[serde(default)]
    usage: Option<Usage>,
}

#[derive(Deserialize, Default)]
struct StreamChoice {
    #[serde(default)]
    delta: Delta,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Deserialize, Default)]
struct Delta {
    #[serde(default)]
    content: Option<String>,
}

/// Reassembles an SSE body from arbitrary byte chunks and folds it into a
/// [`ChatReply`]. Lines are only interpreted once their `\n` has arrived —
/// a TCP read boundary lands mid-JSON as often as not.
///
/// A body that stops before `data: [DONE]` still yields whatever text
/// arrived, but with an empty `finish_reason`, whatever the API said on the
/// way: only `[DONE]` shows the stream ended where the API meant it to. A
/// reply cut short then has nothing to vouch for it but its end marker
/// ([`ChatReply::strip_end_marker`]), and no caller can read it as a reply
/// the API completed.
#[derive(Default)]
pub(crate) struct SseAccumulator {
    buf: Vec<u8>,
    text: String,
    finish_reason: Option<String>,
    /// Whether `data: [DONE]` arrived.
    done: bool,
    usage: Option<Usage>,
    first_token_at: Option<std::time::Instant>,
    /// When the most recent [`SseAccumulator::feed`] arrived. A trailing
    /// partial line is only interpreted in [`SseAccumulator::finish`], and
    /// its bytes came in on that last read — stamping it with the request's
    /// own start instant instead would report a first-token time of 0 ms for
    /// a stream whose only content arrived in an unterminated line.
    last_feed_at: Option<std::time::Instant>,
}

impl SseAccumulator {
    /// Feeds the next chunk of the body. `now` is when the chunk arrived; it
    /// stamps the first non-empty content delta.
    pub(crate) fn feed(&mut self, chunk: &[u8], now: std::time::Instant) {
        self.last_feed_at = Some(now);
        self.buf.extend_from_slice(chunk);
        while let Some(nl) = self.buf.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = self.buf.drain(..=nl).collect();
            self.handle_line(&line, now);
        }
    }

    fn handle_line(&mut self, line: &[u8], now: std::time::Instant) {
        let line = String::from_utf8_lossy(line);
        let Some(data) = line.trim().strip_prefix("data:") else {
            return;
        };
        let data = data.trim();
        if data == "[DONE]" {
            self.done = true;
            return;
        }
        let Ok(chunk) = serde_json::from_str::<StreamChunk>(data) else {
            // Counts only, never the line — it could carry reply text.
            tracing::debug!(bytes = data.len(), "unparseable SSE chunk skipped");
            return;
        };
        if let Some(usage) = chunk.usage {
            self.usage = Some(usage);
        }
        for choice in chunk.choices {
            if let Some(content) = choice.delta.content {
                if !content.is_empty() {
                    self.first_token_at.get_or_insert(now);
                    self.text.push_str(&content);
                }
            }
            if let Some(reason) = choice.finish_reason {
                self.finish_reason = Some(reason);
            }
        }
    }

    /// Folds what arrived into a reply. `started` is when the request was
    /// sent, the origin for `first_token_ms`.
    pub(crate) fn finish(mut self, started: std::time::Instant) -> anyhow::Result<ChatReply> {
        if !self.buf.is_empty() {
            let rest = std::mem::take(&mut self.buf);
            let now = self.last_feed_at.unwrap_or(started);
            self.handle_line(&rest, now);
        }
        let usage = self.usage.unwrap_or_default();
        // A stream closed by `[DONE]` finished on the API's terms, named
        // reason or not. Without it, a `stop` that came first proves nothing
        // about the rest of the body, so no reason is passed on.
        let finish_reason = if self.done {
            self.finish_reason.unwrap_or_else(|| "stop".into())
        } else {
            String::new()
        };
        Ok(ChatReply {
            text: self.text,
            finish_reason,
            prompt_tokens: usage.prompt_tokens,
            completion_tokens: usage.completion_tokens,
            first_token_ms: self
                .first_token_at
                .map(|at| at.saturating_duration_since(started).as_millis() as u32),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The whole point of the abstraction: swapping providers is a base URL.
    #[test]
    fn sarvam_backend_points_at_the_verified_endpoint() {
        let b = Backend::sarvam("k", "sarvam-105b");
        assert_eq!(b.base_url, "https://api.sarvam.ai/v1/chat/completions");
        assert_eq!(b.model, "sarvam-105b");
        assert_eq!(b.kind, BackendKind::Sarvam);
    }

    /// Warming means one tiny authenticated GET to the same host the polish
    /// will hit, so its TLS connection sits in the pool by the time the
    /// user releases the key. Only for Sarvam — a user's local llama.cpp
    /// gains nothing and a corporate gateway might log it — and only when a
    /// polish will actually run.
    #[test]
    fn warmup_targets_sarvam_models_only_when_a_polish_will_run() {
        use crate::format::level::CleanupLevel;
        let sarvam = Backend::sarvam("k", "sarvam-105b");
        assert_eq!(
            warmup_url(&sarvam, CleanupLevel::High).as_deref(),
            Some(SARVAM_MODELS_URL)
        );
        assert_eq!(warmup_url(&sarvam, CleanupLevel::Light).as_deref(), Some(SARVAM_MODELS_URL));
        assert_eq!(warmup_url(&sarvam, CleanupLevel::Off), None);
        let custom = Backend::custom("https://h.example.com/v1/chat/completions", None, "m");
        assert_eq!(warmup_url(&custom, CleanupLevel::High), None);
    }

    /// Cloud mode's backend is the relay's chat route and the user's own
    /// token — never a Sarvam key, which this install does not have.
    #[test]
    fn the_relay_backend_is_the_relays_route_and_the_users_token() {
        let b = Backend::relay("https://relay.example.workers.dev", "tok", "sarvam-105b");
        assert_eq!(
            b.base_url,
            "https://relay.example.workers.dev/v1/chat/completions"
        );
        assert_eq!(b.kind, BackendKind::Relay);
        assert_eq!(b.model, "sarvam-105b");
        // A base with a trailing slash is the obvious way to write one by
        // hand in the hidden setting, and must not produce a double slash.
        assert_eq!(
            Backend::relay("https://relay.example.workers.dev/", "tok", "m").base_url,
            "https://relay.example.workers.dev/v1/chat/completions"
        );
    }

    /// Cloud mode warms the relay, not Sarvam: the app cannot authenticate
    /// against Sarvam at all. `/v1/usage` is a real 200 that also warms the
    /// Worker's JWKS cache.
    #[test]
    fn warmup_targets_the_relays_usage_route_in_cloud_mode() {
        use crate::format::level::CleanupLevel;
        let relay = Backend::relay("https://relay.example.workers.dev", "tok", "sarvam-105b");
        assert_eq!(
            warmup_url(&relay, CleanupLevel::High).as_deref(),
            Some("https://relay.example.workers.dev/v1/usage")
        );
        assert_eq!(warmup_url(&relay, CleanupLevel::Off), None);
    }

    /// The key must not be one forgotten `{:?}` away from a log file. This
    /// pins the *type*, not the call sites, because the call sites are what
    /// keep getting added.
    #[test]
    fn debugging_a_backend_never_prints_its_key() {
        let sarvam = format!("{:?}", Backend::sarvam("sk-live-secret", "sarvam-105b"));
        assert!(!sarvam.contains("sk-live-secret"), "{sarvam}");
        assert!(sarvam.contains("(redacted)"), "{sarvam}");
        assert!(sarvam.contains("sarvam-105b"), "the model is diagnosable");

        let keyless = format!("{:?}", Backend::custom("https://h/v1", None, "m"));
        assert!(keyless.contains("(none)"), "{keyless}");
    }

    /// A pasted URL is the other place a credential hides.
    #[test]
    fn debugging_a_backend_never_prints_a_url_secret() {
        let b = Backend::custom(
            "https://user:pw@h.example.com/v1/chat/completions?api_key=sk-live",
            None,
            "m",
        );
        let shown = format!("{b:?}");
        assert!(!shown.contains("sk-live"), "{shown}");
        assert!(!shown.contains("pw"), "{shown}");
        assert!(shown.contains("https://h.example.com"), "{shown}");
    }

    #[test]
    fn a_redacted_url_keeps_what_identifies_the_host_and_drops_the_rest() {
        assert_eq!(
            redacted_url("https://user:pw@h/v1/chat/completions?k=1#f"),
            "https://h/v1/chat/completions"
        );
        assert_eq!(redacted_url("http://127.0.0.1:1234/v1"), "http://127.0.0.1:1234/v1");
        // `@` after the authority is not userinfo.
        assert_eq!(redacted_url("https://h/v1/models@2"), "https://h/v1/models@2");
        // Never panics on nonsense.
        assert_eq!(redacted_url(""), "");
        assert_eq!(redacted_url("nonsense"), "nonsense");
    }

    #[test]
    fn a_custom_backend_carries_its_own_route_and_an_optional_key() {
        let b = Backend::custom("http://127.0.0.1:1234/v1/chat/completions", None, "qwen3");
        assert_eq!(b.base_url, "http://127.0.0.1:1234/v1/chat/completions");
        assert_eq!(b.api_key, "", "no key is a legitimate custom endpoint");
        assert_eq!(b.model, "qwen3");
        assert_eq!(b.kind, BackendKind::Custom);
    }

    /// Two calls must never mint the same marker — a fixed sentinel could be
    /// echoed back by a truncated reply that merely repeats context it saw
    /// earlier in the conversation.
    #[test]
    fn end_markers_are_unique_per_call() {
        assert_ne!(mint_end_marker(), mint_end_marker());
    }

    /// The marker's whole cost is output tokens: a UUID-length
    /// `__BS_COMPLETE_<uuid4>__` tokenises to ~33 pieces for sarvam-105b —
    /// about 270 ms of decode on every polish. Eight characters from a
    /// 30-symbol alphabet is ~6 tokens, still 810,000 distinct values, and
    /// the model appended it in every live run.
    #[test]
    fn an_end_marker_is_short_delimited_and_unique_per_mint() {
        let m = mint_end_marker();
        assert_eq!(m.chars().count(), 8, "{m}");
        assert!(m.starts_with("<<") && m.ends_with(">>"), "{m}");
        let inner = &m[2..6];
        assert!(
            inner.bytes().all(|b| MARKER_ALPHABET.contains(&b)),
            "{m} uses a character outside the marker alphabet"
        );
        let others: Vec<String> = (0..8).map(|_| mint_end_marker()).collect();
        assert!(
            others.iter().any(|o| o != &m),
            "eight consecutive mints were identical: {m}"
        );
    }

    /// The strip logic does not care about the marker's shape — it finds
    /// the exact string it was given — so the short shape strips the same
    /// way a long one does, trailing punctuation tolerance included.
    #[test]
    fn a_short_marker_strips_like_a_long_one() {
        let m = mint_end_marker();
        let reply = ChatReply {
            text: format!("Hello world. {m}."),
            finish_reason: "stop".into(),
            prompt_tokens: 1,
            completion_tokens: 1,
            first_token_ms: None,
        };
        let stripped = reply.strip_end_marker(&m);
        assert_eq!(stripped.text, "Hello world.");
        assert_eq!(stripped.finish_reason, "stop");
    }

    /// The shape the instruction asks for: the text keeps its own final
    /// punctuation and the marker sits alone on the next line. `rfind` plus
    /// `trim_end` strips it to exactly the text, and `finish_reason` is left
    /// alone.
    #[test]
    fn a_marker_on_its_own_line_strips_to_the_punctuated_text() {
        let reply = ChatReply {
            text: "Hello world.\n<<AB12>>".into(),
            finish_reason: "stop".into(),
            prompt_tokens: 1,
            completion_tokens: 1,
            first_token_ms: None,
        }
        .strip_end_marker("<<AB12>>");
        assert_eq!(reply.text, "Hello world.");
        assert_eq!(reply.finish_reason, "stop");
        assert!(!reply.was_truncated());
    }

    /// The normal case: the model obeyed the instruction and the marker is
    /// stripped cleanly, leaving `finish_reason` untouched.
    #[test]
    fn a_reply_ending_with_its_marker_is_stripped_and_kept() {
        let marker = mint_end_marker();
        let reply = ChatReply {
            text: format!("The meeting is at 3:30 PM.{marker}"),
            finish_reason: "stop".into(),
            prompt_tokens: 10,
            completion_tokens: 5,
            first_token_ms: None,
        }
        .strip_end_marker(&marker);
        assert_eq!(reply.text, "The meeting is at 3:30 PM.");
        assert!(!reply.was_truncated());
    }

    /// The whole point: a reply cut off before it could emit the marker must
    /// be indistinguishable from a `finish_reason == "length"` truncation to
    /// every downstream check, even when the API itself reported "stop".
    #[test]
    fn a_reply_missing_its_marker_is_treated_as_truncated() {
        let marker = mint_end_marker();
        let reply = ChatReply {
            text: "The meeting is at 3:30 P".into(),
            finish_reason: "stop".into(),
            prompt_tokens: 10,
            completion_tokens: 5,
            first_token_ms: None,
        }
        .strip_end_marker(&marker);
        assert!(reply.was_truncated());
    }

    /// Indistinguishable to the *decision*, not to the diagnosis. If a
    /// missing marker overwrote `finish_reason` with "length", a model that
    /// simply ignores the end-marker rule — a prompt-compliance problem —
    /// would be recorded as a `max_tokens` truncation in every log and in
    /// `fmtbench --guard-dump`, the artifact the guardrail's thresholds are
    /// calibrated from.
    #[test]
    fn a_missing_marker_is_recorded_as_its_own_reason_not_as_length() {
        let reply = ChatReply {
            text: "A complete, perfectly formatted sentence.".into(),
            finish_reason: "stop".into(),
            prompt_tokens: 10,
            completion_tokens: 8,
            first_token_ms: None,
        }
        .strip_end_marker(&mint_end_marker());
        assert_eq!(reply.finish_reason, MARKER_MISSING);
        assert_ne!(reply.finish_reason, "length");
        assert!(reply.was_truncated());
    }

    /// And the API's own truncation keeps its own reason — stripping a
    /// marker that *is* present must not relabel it, or the two collapse
    /// again from the other direction.
    #[test]
    fn a_real_length_truncation_keeps_its_own_reason() {
        let marker = mint_end_marker();
        let reply = ChatReply {
            text: format!("Cut off but somehow marked.{marker}"),
            finish_reason: "length".into(),
            prompt_tokens: 10,
            completion_tokens: 2048,
            first_token_ms: None,
        }
        .strip_end_marker(&marker);
        assert_eq!(reply.finish_reason, "length");
        assert!(reply.was_truncated());
    }

    /// Trailing whitespace or a stray newline around the marker (the model
    /// is asked for one newline before it and nothing after, but must not be
    /// trusted blindly) must not defeat detection, and must not leak into
    /// the stripped text.
    #[test]
    fn whitespace_around_the_marker_does_not_defeat_stripping() {
        let marker = mint_end_marker();
        let reply = ChatReply {
            text: format!("Hello there.\n{marker}\n"),
            finish_reason: "stop".into(),
            prompt_tokens: 0,
            completion_tokens: 0,
            first_token_ms: None,
        }
        .strip_end_marker(&marker);
        assert_eq!(reply.text, "Hello there.");
    }

    /// The commonest near-miss: sarvam-105b punctuates the marker as if it were
    /// the last word of a sentence — `{marker}.` in 327 of 1000 live replies. A
    /// short closing-punctuation tail after the marker is the model punctuating
    /// our sentinel, not content, and must be accepted (and fully stripped).
    #[test]
    fn a_period_after_the_marker_is_the_model_punctuating_not_content() {
        let marker = mint_end_marker();
        for tail in [".", ".\n", "!", "?\"", "।"] {
            let reply = ChatReply {
                text: format!("मीटिंग साढ़े तीन बजे है।\n{marker}{tail}"),
                finish_reason: "stop".into(),
                prompt_tokens: 0,
                completion_tokens: 0,
                first_token_ms: None,
            }
            .strip_end_marker(&marker);
            assert_eq!(reply.text, "मीटिंग साढ़े तीन बजे है।", "tail {tail:?}");
            assert!(!reply.was_truncated(), "tail {tail:?}");
        }
    }

    /// The tolerance must stay a tolerance: substantive text after the
    /// marker means the marker did NOT end the reply, and treating it as
    /// complete would hand the guardrail a reply with trailing garbage.
    #[test]
    fn content_after_the_marker_is_still_a_missing_marker() {
        let marker = mint_end_marker();
        let reply = ChatReply {
            text: format!("First half.{marker} And then it kept talking."),
            finish_reason: "stop".into(),
            prompt_tokens: 0,
            completion_tokens: 0,
            first_token_ms: None,
        }
        .strip_end_marker(&marker);
        assert_eq!(reply.finish_reason, MARKER_MISSING);
        assert!(reply.was_truncated());
    }

    /// The instruction must never demonstrate the failure shape it forbids: a
    /// wording that shows "{marker}." mid-sentence gets the period reproduced.
    /// The marker must be the final characters of the instruction, nothing
    /// after it.
    #[test]
    fn the_instruction_ends_with_the_bare_marker() {
        let marker = mint_end_marker();
        let instruction = end_marker_rule(&marker);
        assert!(instruction.ends_with(&marker));
    }

    /// The instruction sentence must actually name the exact marker text, or
    /// the model has nothing concrete to echo back.
    #[test]
    fn the_end_marker_rule_names_the_exact_marker() {
        let marker = mint_end_marker();
        assert!(end_marker_rule(&marker).contains(&marker));
    }

    /// `finish_reason: "length"` means the reply was cut off mid-sentence.
    /// Accepting it silently is how truncated dictations reach the document.
    #[test]
    fn truncation_is_detectable_from_the_reply() {
        let cut = ChatReply {
            text: "Hello there and then the sentence just st".into(),
            finish_reason: "length".into(),
            prompt_tokens: 10,
            completion_tokens: 1024,
            first_token_ms: None,
        };
        assert!(cut.was_truncated());

        let ok = ChatReply {
            text: "Hello there.".into(),
            finish_reason: "stop".into(),
            prompt_tokens: 10,
            completion_tokens: 3,
            first_token_ms: None,
        };
        assert!(!ok.was_truncated());
    }

    /// Sarvam's reply shape, captured from a real 200 response.
    #[test]
    fn parses_a_real_sarvam_response() {
        let raw = r#"{"id":"x","choices":[{"finish_reason":"stop","index":0,
            "message":{"content":"\nThe meeting is at four pm.","role":"assistant"}}],
            "model":"sarvam-105b",
            "usage":{"completion_tokens":9,"prompt_tokens":46,"total_tokens":55}}"#;
        let reply = parse_reply(raw).expect("valid response");
        assert_eq!(reply.text, "\nThe meeting is at four pm.");
        assert_eq!(reply.finish_reason, "stop");
        assert_eq!(reply.prompt_tokens, 46);
        assert_eq!(reply.completion_tokens, 9);
    }

    #[test]
    fn a_reply_with_no_choices_is_an_error() {
        assert!(parse_reply(r#"{"id":"x","choices":[]}"#).is_err());
    }

    /// Callers log this error with `{e:#}`, and serde_json quotes a string it
    /// finds where it expected something else, so a reply of the wrong shape
    /// (here `message` as a plain string) must leave the reply's text out.
    #[test]
    fn a_misshapen_reply_errors_without_quoting_the_reply() {
        let raw = r#"{"choices":[{"message":"zanzibarqat wobblefjord"}]}"#;
        let e = parse_reply(raw).expect_err("the wrong shape is an error");
        let text = format!("{e:#} | {e:?}");
        assert!(!text.contains("zanzibarqat"), "error text quotes the reply: {text}");
        assert!(text.contains("chat reply"), "error text names what failed: {text}");
    }

    /// A host that answers the streaming request with a plain JSON body
    /// anyway (a proxy that buffers, a test stub) still parses — and reports
    /// no first-token time, because nothing measured one.
    #[test]
    fn a_non_streamed_json_body_parses_without_a_first_token_time() {
        let raw = r#"{"id":"x","choices":[{"finish_reason":"stop","index":0,
            "message":{"content":"The meeting is at four pm.","role":"assistant"}}],
            "usage":{"completion_tokens":9,"prompt_tokens":46,"total_tokens":55}}"#;
        let reply = parse_reply(raw).expect("valid response");
        assert_eq!(reply.first_token_ms, None);
    }

    fn t0() -> std::time::Instant {
        std::time::Instant::now()
    }

    const SSE_FIXTURE: &str = "data: {\"id\":\"x\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"\"},\"finish_reason\":null}]}\n\
\n\
data: {\"id\":\"x\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Hello\"},\"finish_reason\":null}]}\n\
\n\
data: {\"id\":\"x\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\" world. <<AB12>>\"},\"finish_reason\":\"stop\"}]}\n\
\n\
data: {\"id\":\"x\",\"choices\":[],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":5,\"total_tokens\":15}}\n\
\n\
data: [DONE]\n\n";

    /// The stream is fed in arbitrary byte chunks — a TCP read boundary
    /// lands mid-line as often as not — and must reassemble lines itself.
    #[test]
    fn an_sse_stream_split_mid_line_yields_text_finish_usage_and_ttft() {
        let started = t0();
        let bytes = SSE_FIXTURE.as_bytes();
        let cut = bytes.iter().position(|&b| b == b'H').unwrap() + 2; // inside "Hello"
        let mut acc = SseAccumulator::default();
        acc.feed(&bytes[..cut], started + Duration::from_millis(150));
        acc.feed(&bytes[cut..], started + Duration::from_millis(400));
        let reply = acc.finish(started).unwrap();
        assert_eq!(reply.text, "Hello world. <<AB12>>");
        assert_eq!(reply.finish_reason, "stop");
        assert_eq!((reply.prompt_tokens, reply.completion_tokens), (10, 5));
        // "Hello" completed in the second chunk, so that chunk's instant is
        // the first-token time.
        assert_eq!(reply.first_token_ms, Some(400));
    }

    /// An empty first delta (the role-only chunk every OpenAI-shaped stream
    /// opens with) is not a token.
    #[test]
    fn an_empty_delta_does_not_stamp_the_first_token() {
        let started = t0();
        let mut acc = SseAccumulator::default();
        acc.feed(
            b"data: {\"choices\":[{\"delta\":{\"role\":\"assistant\",\"content\":\"\"},\"finish_reason\":null}]}\n\n",
            started + Duration::from_millis(100),
        );
        acc.feed(
            b"data: {\"choices\":[{\"delta\":{\"content\":\"x\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n",
            started + Duration::from_millis(300),
        );
        let reply = acc.finish(started).unwrap();
        assert_eq!(reply.first_token_ms, Some(300));
    }

    /// A stream that dies before `[DONE]` claims no finish reason at all, so
    /// nothing downstream can read it as a reply the API completed: the
    /// marker alone decides, and without it the reply is a truncation.
    ///
    /// Cut mid-line, with no trailing newline, because that is how a dropped
    /// connection actually ends — which also makes this the test for
    /// `finish`'s trailing-buffer drain: the one content delta is seen only
    /// there, and its first-token time must be when its bytes arrived, not
    /// the request's own start instant.
    #[test]
    fn a_stream_cut_before_done_is_rejected_by_the_marker_check() {
        let started = t0();
        let mut acc = SseAccumulator::default();
        acc.feed(
            b"data: {\"choices\":[{\"delta\":{\"content\":\"Hello wor\"},\"finish_reason\":null}]}",
            started + Duration::from_millis(250),
        );
        let reply = acc.finish(started).unwrap();
        assert_eq!(reply.text, "Hello wor");
        assert_eq!(reply.finish_reason, "", "a stream cut before [DONE] claims no finish reason");
        assert_eq!(reply.first_token_ms, Some(250), "stamped when the bytes arrived");
        let checked = reply.strip_end_marker("<<AB12>>");
        assert_eq!(checked.finish_reason, MARKER_MISSING);
    }

    /// Only `[DONE]` closes a stream. A `stop` that arrived on a stream cut
    /// before it is not passed on, and a stream that closed with `[DONE]` but
    /// never named a reason finished on its own terms.
    #[test]
    fn only_a_stream_closed_by_done_reports_its_finish_reason() {
        let started = t0();
        let stop = b"data: {\"choices\":[{\"delta\":{\"content\":\"ok\"},\"finish_reason\":\"stop\"}]}\n\n";

        let mut cut = SseAccumulator::default();
        cut.feed(stop, started);
        assert_eq!(cut.finish(started).unwrap().finish_reason, "");

        let mut whole = SseAccumulator::default();
        whole.feed(stop, started);
        whole.feed(b"data: [DONE]\n\n", started);
        assert_eq!(whole.finish(started).unwrap().finish_reason, "stop");

        let mut unnamed = SseAccumulator::default();
        unnamed.feed(
            b"data: {\"choices\":[{\"delta\":{\"content\":\"ok\"},\"finish_reason\":null}]}\n\ndata: [DONE]\n\n",
            started,
        );
        assert_eq!(unnamed.finish(started).unwrap().finish_reason, "stop");

        // `[DONE]` as the final, unterminated line still counts.
        let mut unterminated = SseAccumulator::default();
        unterminated.feed(stop, started);
        unterminated.feed(b"data: [DONE]", started);
        assert_eq!(unterminated.finish(started).unwrap().finish_reason, "stop");
    }

    /// No `usage` chunk (a proxy that strips it, a provider that never sends
    /// it) reports zero tokens, exactly as `parse_reply`'s `#[serde(default)]`
    /// does for a bodiless `usage` — never a failure.
    #[test]
    fn a_stream_without_usage_reports_zero_tokens() {
        let started = t0();
        let mut acc = SseAccumulator::default();
        acc.feed(
            b"data: {\"choices\":[{\"delta\":{\"content\":\"ok <<AB12>>\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n",
            started,
        );
        let reply = acc.finish(started).unwrap();
        assert_eq!((reply.prompt_tokens, reply.completion_tokens), (0, 0));
        assert_eq!(reply.text, "ok <<AB12>>");
    }

    /// Only the Sarvam arm streams; a custom OpenAI-compatible host keeps the
    /// legacy non-streaming shape (and its `<think>` stripping).
    #[test]
    fn the_request_body_streams_only_when_asked() {
        let streamed = build_request_body("m", "s", "u", 10, 0.0, true);
        assert_eq!(streamed["stream"], true);
        assert_eq!(streamed["stream_options"]["include_usage"], true);
        let plain = build_request_body("m", "s", "u", 10, 0.0, false);
        assert!(plain.get("stream").is_none());
        assert!(plain.get("stream_options").is_none());
    }

    /// This is the piece that must carry a caller-supplied temperature
    /// through to the request body instead of flattening it to one value.
    #[test]
    fn the_caller_supplied_temperature_reaches_the_body() {
        let body = build_request_body("sarvam-105b", "sys", "user", 16, 0.2, false);
        assert_eq!(body["temperature"], f64::from(0.2_f32));
    }

    /// Sarvam answers an invalid model id (such as `sarvam-30b`, which does
    /// not exist) with an empty-bodied HTTP 400, so the status code is the
    /// only signal; missing it silently disables formatting.
    /// A trivial TCP listener stands in for the server so this exercises the
    /// real `complete()` status check — not just the JSON parser — without a
    /// network or a mock-server dependency.
    #[tokio::test]
    async fn a_non_success_status_is_an_error_naming_the_status_code() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind loopback listener");
        let addr = listener.local_addr().expect("local addr");

        tokio::spawn(async move {
            if let Ok((socket, _)) = listener.accept().await {
                // Drain the request first. Windows answers a socket close
                // with unread inbound data still queued by sending a RST
                // instead of a graceful FIN, which hyper surfaces as
                // "connection aborted" — masking the very status this test
                // is proving rather than exercising it.
                let mut buf = [0u8; 8192];
                loop {
                    socket.readable().await.expect("socket readable");
                    match socket.try_read(&mut buf) {
                        Ok(0) => break,
                        Ok(_) => continue,
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                        Err(e) => panic!("failed reading stub request: {e}"),
                    }
                }

                // Sarvam's real 400 has an empty body; Content-Length: 0
                // tells the client not to wait for more.
                let response =
                    b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
                loop {
                    socket.writable().await.expect("socket writable");
                    match socket.try_write(response) {
                        Ok(_) => break,
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => continue,
                        Err(e) => panic!("failed writing stub response: {e}"),
                    }
                }
            }
        });

        let mut backend = Backend::sarvam("k", "sarvam-105b");
        backend.base_url = format!("http://{addr}");
        let http = reqwest::Client::new();

        let err = backend
            .complete(&http, "sys", "user", 16, 0.0, Duration::from_secs(2))
            .await
            .expect_err("a non-2xx status must be an Err, not a swallowed empty reply");
        assert!(
            err.to_string().contains("400"),
            "error should name the status code, got: {err}"
        );
    }

    // -----------------------------------------------------------------------
    // The custom endpoint: headers, the request body, and the attempts after a refusal
    // -----------------------------------------------------------------------

    /// A scripted loopback HTTP server. Serves one response per connection in
    /// order and records every request verbatim, so a test can assert on the
    /// bytes that actually went out — the only way to prove "the Sarvam path
    /// is unchanged" rather than assert it.
    ///
    /// Raw sockets rather than a mock-server crate for the same reason the
    /// test above uses them: no new dependency, and `tokio`'s `io-util`
    /// feature is not enabled in this crate.
    struct Stub {
        addr: std::net::SocketAddr,
        seen: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    }

    impl Stub {
        fn requests(&self) -> Vec<String> {
            self.seen.lock().expect("stub lock").clone()
        }

        fn body(&self, nth: usize) -> serde_json::Value {
            let req = self.requests();
            let raw = req.get(nth).unwrap_or_else(|| {
                panic!("stub saw {} request(s), wanted #{nth}", req.len());
            });
            let body = raw
                .split_once("\r\n\r\n")
                .map(|(_, b)| b)
                .expect("request has a body");
            serde_json::from_str(body).expect("request body is JSON")
        }
    }

    fn reason(code: u16) -> &'static str {
        match code {
            200 => "OK",
            400 => "Bad Request",
            422 => "Unprocessable Entity",
            _ => "Error",
        }
    }

    async fn read_request(socket: &tokio::net::TcpStream) -> String {
        let mut buf: Vec<u8> = Vec::new();
        let mut chunk = [0u8; 4096];
        loop {
            socket.readable().await.expect("socket readable");
            match socket.try_read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => {
                    buf.extend_from_slice(&chunk[..n]);
                    if let Some(at) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        let head = String::from_utf8_lossy(&buf[..at]).to_ascii_lowercase();
                        let len = head
                            .lines()
                            .find_map(|l| l.strip_prefix("content-length:"))
                            .and_then(|v| v.trim().parse::<usize>().ok())
                            .unwrap_or(0);
                        if buf.len() >= at + 4 + len {
                            break;
                        }
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => continue,
                Err(e) => panic!("stub read failed: {e}"),
            }
        }
        String::from_utf8_lossy(&buf).into_owned()
    }

    async fn write_all(socket: &tokio::net::TcpStream, mut bytes: &[u8]) {
        while !bytes.is_empty() {
            socket.writable().await.expect("socket writable");
            match socket.try_write(bytes) {
                Ok(n) => bytes = &bytes[n..],
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => continue,
                Err(e) => panic!("stub write failed: {e}"),
            }
        }
    }

    async fn stub(script: Vec<(u16, String)>) -> Stub {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind loopback listener");
        let addr = listener.local_addr().expect("local addr");
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let recorded = seen.clone();

        tokio::spawn(async move {
            for (code, body) in script {
                let Ok((socket, _)) = listener.accept().await else {
                    return;
                };
                let request = read_request(&socket).await;
                recorded.lock().expect("stub lock").push(request);
                // `Connection: close` keeps every attempt on its own
                // connection, so "one response per accept" stays true when
                // a refused request is sent again.
                let response = format!(
                    "HTTP/1.1 {code} {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    reason(code),
                    body.len()
                );
                write_all(&socket, response.as_bytes()).await;
            }
        });

        Stub { addr, seen }
    }

    fn ok_reply(content: &str) -> String {
        serde_json::json!({
            "choices": [{ "finish_reason": "stop", "message": { "content": content } }],
            "usage": { "prompt_tokens": 1, "completion_tokens": 1 }
        })
        .to_string()
    }

    /// A sibling of [`stub`] that answers one request the way a real
    /// streaming endpoint does: `text/event-stream`, chunked, one HTTP chunk
    /// per `parts` entry with a pause between them. Separate from `stub`
    /// rather than a flag on it because every other test here wants the
    /// scripted list of status codes, and none of them wants a body arriving
    /// in pieces.
    ///
    /// The content type is deliberately `TEXT/Event-Stream; charset=utf-8`:
    /// a media type is case-insensitive and routinely carries parameters
    /// (RFC 9110 section 8.3), and `complete` must still recognise it.
    async fn sse_stub(parts: Vec<String>) -> Stub {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind loopback listener");
        let addr = listener.local_addr().expect("local addr");
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let recorded = seen.clone();

        tokio::spawn(async move {
            let Ok((socket, _)) = listener.accept().await else {
                return;
            };
            let request = read_request(&socket).await;
            recorded.lock().expect("stub lock").push(request);
            write_all(
                &socket,
                b"HTTP/1.1 200 OK\r\n\
                  Content-Type: TEXT/Event-Stream; charset=utf-8\r\n\
                  Transfer-Encoding: chunked\r\n\
                  Connection: close\r\n\r\n",
            )
            .await;
            for part in parts {
                write_all(&socket, format!("{:x}\r\n", part.len()).as_bytes()).await;
                write_all(&socket, part.as_bytes()).await;
                write_all(&socket, b"\r\n").await;
                // Long enough that the two halves land in separate reads, so
                // the client really does reassemble a line across chunks.
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            write_all(&socket, b"0\r\n\r\n").await;
        });

        Stub { addr, seen }
    }

    /// The relay's weekly chat limit comes back as its own error, found by
    /// downcast — and only that `429`: the relay's per-minute one clears by
    /// itself, and a `429` from Sarvam is Sarvam's whatever its body says.
    #[tokio::test]
    async fn only_the_relays_weekly_chat_limit_is_the_weekly_limit_error() {
        let complete = |backend: Backend| async move {
            backend
                .complete(
                    &reqwest::Client::new(),
                    "sys",
                    "user",
                    16,
                    0.0,
                    Duration::from_secs(5),
                )
                .await
                .expect_err("a 429 is a failure")
        };

        let s = stub(vec![(429, RELAY_WEEKLY_CHAT_LIMIT.to_string())]).await;
        let err = complete(Backend::relay(&format!("http://{}", s.addr), "tok", "m")).await;
        assert!(err.downcast_ref::<WeeklyChatLimit>().is_some(), "{err:#}");
        assert_eq!(
            crate::sarvam::chat::failure_sentence(&err),
            Some(MSG_CLOUD_QUOTA)
        );
        assert_eq!(format!("{err:#}"), MSG_CLOUD_QUOTA);

        let s = stub(vec![(429, "rate limited".to_string())]).await;
        let err = complete(Backend::relay(&format!("http://{}", s.addr), "tok", "m")).await;
        assert!(err.downcast_ref::<WeeklyChatLimit>().is_none(), "{err:#}");
        assert_eq!(crate::sarvam::chat::failure_sentence(&err), None);

        let s = stub(vec![(429, RELAY_WEEKLY_CHAT_LIMIT.to_string())]).await;
        let mut sarvam = Backend::sarvam("k", "m");
        sarvam.base_url = format!("http://{}", s.addr);
        let err = complete(sarvam).await;
        assert!(err.downcast_ref::<WeeklyChatLimit>().is_none(), "{err:#}");
    }

    /// The anti-leak rule, and the reason `Backend` needed a `kind` at all:
    /// `api-subscription-key` is Sarvam's own scheme. Sending it to a host the
    /// user typed in would hand that host their credential in a header they
    /// never agreed to.
    #[tokio::test]
    async fn a_custom_endpoint_gets_a_bearer_token_and_nothing_else() {
        let s = stub(vec![(200, ok_reply("hi"))]).await;
        let backend = Backend::custom(
            &format!("http://{}/v1/chat/completions", s.addr),
            Some("sk-secret".into()),
            "qwen3",
        );
        backend
            .complete(
                &reqwest::Client::new(),
                "sys",
                "user",
                16,
                0.0,
                Duration::from_secs(5),
            )
            .await
            .expect("a 200 is a reply");

        let req = s.requests().remove(0).to_ascii_lowercase();
        assert!(req.contains("authorization: bearer sk-secret"), "{req}");
        assert!(
            !req.contains("api-subscription-key"),
            "Sarvam's header must never reach a custom host: {req}"
        );
    }

    /// A keyless endpoint is normal, not an error: LM Studio, llama-server and
    /// a plain Ollama all answer without auth.
    #[tokio::test]
    async fn a_keyless_custom_endpoint_sends_no_authorization_header() {
        let s = stub(vec![(200, ok_reply("hi"))]).await;
        let backend = Backend::custom(
            &format!("http://{}/v1/chat/completions", s.addr),
            None,
            "qwen3",
        );
        backend
            .complete(
                &reqwest::Client::new(),
                "sys",
                "user",
                16,
                0.0,
                Duration::from_secs(5),
            )
            .await
            .expect("a 200 is a reply");

        let req = s.requests().remove(0).to_ascii_lowercase();
        assert!(!req.contains("authorization:"), "{req}");
        assert!(!req.contains("api-subscription-key"), "{req}");
    }

    /// The Sarvam request, pinned here in full so nothing changes it by
    /// accident: the streaming keys ask for SSE, and the rest of the body is
    /// exactly the shape Sarvam's chat endpoint receives.
    #[tokio::test]
    async fn the_sarvam_request_is_pinned_in_full() {
        let s = stub(vec![(200, ok_reply("hi"))]).await;
        let mut backend = Backend::sarvam("k", "sarvam-105b");
        backend.base_url = format!("http://{}", s.addr);
        backend
            .complete(
                &reqwest::Client::new(),
                "sys",
                "user",
                16,
                0.0,
                Duration::from_secs(5),
            )
            .await
            .expect("a 200 is a reply");

        let req = s.requests().remove(0).to_ascii_lowercase();
        assert!(req.contains("api-subscription-key: k"), "{req}");
        assert!(req.contains("authorization: bearer k"), "{req}");
        assert_eq!(
            s.body(0),
            serde_json::json!({
                "model": "sarvam-105b",
                "temperature": 0.0,
                "max_tokens": 16,
                "reasoning_effort": null,
                "messages": [
                    { "role": "system", "content": "sys" },
                    { "role": "user", "content": "user" },
                ],
                "stream": true,
                "stream_options": { "include_usage": true },
            })
        );
    }

    /// Cloud mode's wire contract, and the reason `BackendKind` has a third
    /// value: the request body is byte-identical to the one Sarvam
    /// gets — so every guardrail, ratchet and benchmark keeps describing it
    /// — while the only credential on it is the user's own sign-in token.
    /// Sarvam's header must not appear: this app has no Sarvam key in Cloud
    /// mode, and the relay would have nothing to do with one.
    #[tokio::test]
    async fn the_relay_request_is_sarvams_body_with_only_a_bearer() {
        let s = stub(vec![(200, ok_reply("hi"))]).await;
        let backend = Backend::relay(
            &format!("http://{}", s.addr),
            "supabase-access-token",
            "sarvam-105b",
        );
        backend
            .complete(
                &reqwest::Client::new(),
                "sys",
                "user",
                16,
                0.0,
                Duration::from_secs(5),
            )
            .await
            .expect("a 200 is a reply");

        let req = s.requests().remove(0).to_ascii_lowercase();
        assert!(req.contains("authorization: bearer supabase-access-token"), "{req}");
        assert!(
            !req.contains("api-subscription-key"),
            "the app holds no Sarvam key in Cloud mode: {req}"
        );
        assert_eq!(
            s.body(0),
            serde_json::json!({
                "model": "sarvam-105b",
                "temperature": 0.0,
                "max_tokens": 16,
                "reasoning_effort": null,
                "messages": [
                    { "role": "system", "content": "sys" },
                    { "role": "user", "content": "user" },
                ],
                "stream": true,
                "stream_options": { "include_usage": true },
            })
        );
    }

    /// The relay streams the SSE through untouched, so Cloud mode has to
    /// read it exactly as the Sarvam arm does — including the first-token
    /// time the whole latency ladder is measured against.
    #[tokio::test]
    async fn the_relay_arm_reads_a_real_sse_response_end_to_end() {
        let cut = SSE_FIXTURE.find('H').expect("the fixture contains \"Hello\"") + 2;
        let s = sse_stub(vec![
            SSE_FIXTURE[..cut].to_string(),
            SSE_FIXTURE[cut..].to_string(),
        ])
        .await;
        let backend = Backend::relay(&format!("http://{}", s.addr), "tok", "sarvam-105b");
        let reply = backend
            .complete(
                &reqwest::Client::new(),
                "sys",
                "user",
                16,
                0.0,
                Duration::from_secs(5),
            )
            .await
            .expect("a streamed 200 is a reply");

        assert_eq!(reply.text, "Hello world. <<AB12>>");
        assert_eq!(reply.finish_reason, "stop");
        assert!(reply.first_token_ms.is_some());
    }

    /// The streaming arm driven through the real client, not through
    /// `SseAccumulator` directly: reqwest's `bytes_stream`, hyper's chunked
    /// decoding and the content-type branch all sit between the wire and the
    /// reply, and none of the unit tests above exercise any of them. The
    /// fixture is cut two bytes into "Hello" so the line reassembly has to
    /// happen across two real network reads.
    #[tokio::test]
    async fn the_sarvam_arm_reads_a_real_sse_response_end_to_end() {
        let cut = SSE_FIXTURE.find('H').expect("the fixture contains \"Hello\"") + 2;
        let s = sse_stub(vec![
            SSE_FIXTURE[..cut].to_string(),
            SSE_FIXTURE[cut..].to_string(),
        ])
        .await;
        let mut backend = Backend::sarvam("k", "sarvam-105b");
        backend.base_url = format!("http://{}", s.addr);
        let reply = backend
            .complete(
                &reqwest::Client::new(),
                "sys",
                "user",
                16,
                0.0,
                Duration::from_secs(5),
            )
            .await
            .expect("a streamed 200 is a reply");

        assert_eq!(reply.text, "Hello world. <<AB12>>");
        assert_eq!(reply.finish_reason, "stop");
        assert_eq!((reply.prompt_tokens, reply.completion_tokens), (10, 5));
        assert!(
            reply.first_token_ms.is_some(),
            "the streaming arm must measure a first-token time"
        );
        // And the request that produced it asked for a stream.
        assert_eq!(s.body(0)["stream"], true);
    }

    /// A llama.cpp or vLLM server that knows only `max_tokens` refuses
    /// `max_completion_tokens` with a 400, and leaving out an optional
    /// parameter would not help, so the custom body uses the older name.
    #[tokio::test]
    async fn the_custom_request_uses_max_tokens_never_max_completion_tokens() {
        let s = stub(vec![(200, ok_reply("hi"))]).await;
        let backend = Backend::custom(
            &format!("http://{}/v1/chat/completions", s.addr),
            None,
            "qwen3",
        );
        backend
            .complete(
                &reqwest::Client::new(),
                "sys",
                "user",
                16,
                0.3,
                Duration::from_secs(5),
            )
            .await
            .expect("a 200 is a reply");

        let body = s.body(0);
        assert_eq!(body["max_tokens"], 16);
        assert!(body.get("max_completion_tokens").is_none());
        assert_eq!(body["temperature"], f64::from(0.3_f32));
        assert_eq!(body["model"], "qwen3");
    }

    /// A refusal that does not say which field it disliked: the second
    /// attempt goes without `reasoning_effort` and keeps everything else.
    #[tokio::test]
    async fn a_refused_request_goes_again_without_reasoning_effort() {
        let s = stub(vec![
            (400, "{\"error\":{\"message\":\"bad request\"}}".into()),
            (200, ok_reply("hi")),
        ])
        .await;
        let backend = Backend::custom(
            &format!("http://{}/v1/chat/completions", s.addr),
            None,
            "qwen3",
        );
        let reply = backend
            .complete(
                &reqwest::Client::new(),
                "sys",
                "user",
                16,
                0.0,
                Duration::from_secs(5),
            )
            .await
            .expect("the retry succeeds");

        assert_eq!(reply.text, "hi");
        assert_eq!(s.requests().len(), 2);
        assert!(s.body(0).get("reasoning_effort").is_some());
        assert!(s.body(1).get("reasoning_effort").is_none());
        assert_eq!(s.body(1)["max_tokens"], 16, "max_tokens is never dropped");
        assert!(s.body(1).get("messages").is_some(), "never the messages");
    }

    /// When the second refusal names a parameter, the third attempt goes
    /// without it, along with any other droppable one it names.
    #[tokio::test]
    async fn the_third_attempt_leaves_out_what_the_error_names() {
        let s = stub(vec![
            (400, "{\"error\":{\"message\":\"bad request\"}}".into()),
            (422, "{\"detail\":\"unsupported field: temperature\"}".into()),
            (200, ok_reply("hi")),
        ])
        .await;
        let backend = Backend::custom(
            &format!("http://{}/v1/chat/completions", s.addr),
            None,
            "qwen3",
        );
        backend
            .complete(
                &reqwest::Client::new(),
                "sys",
                "user",
                16,
                0.0,
                Duration::from_secs(5),
            )
            .await
            .expect("the third attempt succeeds");

        assert_eq!(s.requests().len(), 3);
        assert!(s.body(2).get("temperature").is_none());
        assert!(s.body(2).get("reasoning_effort").is_none());
        assert_eq!(s.body(2)["max_tokens"], 16);
    }

    /// Three attempts is the most a dictation waits for; after the third
    /// refusal the error goes back to the caller.
    #[tokio::test]
    async fn a_third_refusal_is_returned_as_the_error() {
        let s = stub(vec![
            (400, "{\"error\":{\"message\":\"first\"}}".into()),
            (400, "{\"error\":{\"message\":\"temperature out of range\"}}".into()),
            (400, "{\"error\":{\"message\":\"third\"}}".into()),
        ])
        .await;
        let backend = Backend::custom(
            &format!("http://{}/v1/chat/completions", s.addr),
            None,
            "qwen3",
        );
        let err = backend
            .complete(
                &reqwest::Client::new(),
                "sys",
                "user",
                16,
                0.0,
                Duration::from_secs(5),
            )
            .await
            .expect_err("three refusals are a failure");

        assert_eq!(s.requests().len(), 3, "three attempts at most");
        assert!(err.to_string().contains("400"), "{err}");
    }

    /// Only a 400 or 422 says the request itself was refused. A 500 is a
    /// fault on the server, and sending less would only hide it.
    #[tokio::test]
    async fn a_server_error_is_returned_after_one_attempt() {
        let s = stub(vec![(500, "{\"error\":{\"message\":\"boom\"}}".into())]).await;
        let backend = Backend::custom(
            &format!("http://{}/v1/chat/completions", s.addr),
            None,
            "qwen3",
        );
        let err = backend
            .complete(
                &reqwest::Client::new(),
                "sys",
                "user",
                16,
                0.0,
                Duration::from_secs(5),
            )
            .await
            .expect_err("a 500 is an error");

        assert_eq!(s.requests().len(), 1, "a 5xx is not sent again here");
        assert!(err.to_string().contains("500"), "{err}");
        // The server's message is for the screen, not the log text.
        assert!(!err.to_string().contains("boom"), "{err}");
        assert!(shown_failure(&err).unwrap().contains("boom"), "{err}");
    }

    /// Once `reasoning_effort` is gone, a refusal that mentions no droppable
    /// parameter is final: the same body would only be refused again.
    #[tokio::test]
    async fn a_refusal_that_names_nothing_droppable_is_final() {
        let s = stub(vec![
            (400, "{\"error\":{\"message\":\"model not found\"}}".into()),
            (400, "{\"error\":{\"message\":\"model not found\"}}".into()),
        ])
        .await;
        let backend = Backend::custom(
            &format!("http://{}/v1/chat/completions", s.addr),
            None,
            "qwen3",
        );
        let err = backend
            .complete(
                &reqwest::Client::new(),
                "sys",
                "user",
                16,
                0.0,
                Duration::from_secs(5),
            )
            .await
            .expect_err("nothing left to drop");

        assert_eq!(s.requests().len(), 2);
        assert!(!err.to_string().contains("model not found"), "{err}");
        assert!(shown_failure(&err).unwrap().contains("model not found"), "{err}");
    }

    /// A model that reasons before it answers sends both; only the answer
    /// comes back to the caller.
    #[tokio::test]
    async fn a_custom_reply_comes_back_without_its_think_block() {
        let s = stub(vec![(
            200,
            ok_reply("<think>Let me consider.</think>The meeting is at 3:30 PM."),
        )])
        .await;
        let backend = Backend::custom(
            &format!("http://{}/v1/chat/completions", s.addr),
            None,
            "qwen3",
        );
        let reply = backend
            .complete(
                &reqwest::Client::new(),
                "sys",
                "user",
                16,
                0.0,
                Duration::from_secs(5),
            )
            .await
            .expect("a 200 is a reply");
        assert_eq!(reply.text, "The meeting is at 3:30 PM.");
    }

    // -- the pure pieces -----------------------------------------------------

    /// The shape `reqwest` actually produces, with the two ways a pasted URL
    /// carries a credential. Both halves are asserted — that the raw chain
    /// really would leak, so nobody "simplifies" the log line back to it, and
    /// that what gets logged cannot.
    #[test]
    fn a_redacted_error_chain_keeps_the_host_and_drops_the_secret() {
        let raw = "error sending request for url (https://user:pw@host.example.com:8443/v1/chat/completions?api_key=sk-live-secret): connection closed";
        assert!(raw.contains("sk-live-secret"), "the hazard changed shape");

        let shown = redact_urls(raw);
        assert!(!shown.contains("sk-live-secret"), "{shown}");
        assert!(!shown.contains("user:pw"), "{shown}");
        assert!(
            shown.contains("https://host.example.com:8443/v1/chat/completions"),
            "host, port and route are the diagnosis and all survive: {shown}"
        );
        assert!(
            shown.ends_with(": connection closed"),
            "the error's own words survive: {shown}"
        );
        assert_eq!(
            shown,
            format!(
                "error sending request for url ({}): connection closed",
                redacted_url("https://user:pw@host.example.com:8443/v1/chat/completions?api_key=sk-live-secret")
            ),
            "one policy for what a URL looks like in a log, not two"
        );
    }

    #[test]
    fn redaction_leaves_ordinary_prose_alone() {
        for text in [
            "operation timed out",
            "invalid model id: qwen3:8b",
            "a:// b",
            "",
        ] {
            assert_eq!(redact_urls(text), text, "text {text:?}");
        }
    }

    /// More than one URL in a chain, and a URL with nothing to strip, both
    /// have to come out right — this scans prose, where a source chain
    /// assembled by three crates can put a URL anywhere.
    #[test]
    fn every_url_in_a_chain_is_redacted_and_the_prose_between_them_is_not() {
        assert_eq!(
            redact_urls("first http://127.0.0.1:11434/v1/chat/completions then https://h/x?k=1 done"),
            "first http://127.0.0.1:11434/v1/chat/completions then https://h/x done"
        );
        assert_eq!(redact_urls("http://localhost:1234"), "http://localhost:1234");
        // The wrapping punctuation is the sentence's, not the URL's.
        assert_eq!(
            redact_urls("url (https://u:p@h/v1?k=1), retrying"),
            "url (https://h/v1), retrying"
        );
    }

    #[test]
    fn the_first_drop_takes_reasoning_effort_and_nothing_else() {
        let mut body = build_request_body("m", "s", "u", 16, 0.0, false);
        assert!(drop_reasoning_effort(&mut body));
        assert!(body.get("reasoning_effort").is_none());
        assert!(body.get("temperature").is_some());
        assert!(body.get("max_tokens").is_some());
        assert!(body.get("messages").is_some());
        // A second call finds nothing to drop.
        assert!(!drop_reasoning_effort(&mut body));
    }

    #[test]
    fn only_a_parameter_both_sent_and_named_is_dropped() {
        let mut body = build_request_body("m", "s", "u", 16, 0.0, false);
        // Sent, but the error is about something else: kept.
        assert!(drop_params_named_in(&mut body, "context window exceeded").is_empty());
        assert!(body.get("temperature").is_some());
        // Named in one error, both gone in the same attempt.
        let dropped =
            drop_params_named_in(&mut body, "reasoning_effort and temperature are not allowed");
        assert_eq!(dropped, vec!["temperature", "reasoning_effort"]);
        assert!(body.get("temperature").is_none());
        assert!(body.get("reasoning_effort").is_none());
        // Named again once it is gone: nothing is reported.
        assert!(drop_params_named_in(&mut body, "temperature is not allowed").is_empty());
    }

    /// The list is the builder's optional keys, exactly: a new optional key
    /// has to be added to it on purpose, and a key the builder stops sending
    /// comes off it. What makes the request (`model`, `messages`,
    /// `max_tokens`) is never on it, so a smaller request is still the same
    /// request.
    #[test]
    fn droppable_params_are_the_builders_optional_keys() {
        const THE_REQUEST: [&str; 3] = ["model", "messages", "max_tokens"];
        let body = build_request_body("m", "s", "u", 16, 0.0, false);
        let mut optional: Vec<&str> = body
            .as_object()
            .expect("the body is an object")
            .keys()
            .map(String::as_str)
            .filter(|k| !THE_REQUEST.contains(k))
            .collect();
        optional.sort_unstable();
        let mut listed = DROPPABLE_PARAMS.to_vec();
        listed.sort_unstable();
        assert_eq!(optional, listed);
        for key in THE_REQUEST {
            assert!(body.get(key).is_some(), "{key}");
            assert!(!DROPPABLE_PARAMS.contains(&key), "{key}");
        }
    }

    #[test]
    fn think_blocks_go_and_the_answer_stays() {
        assert_eq!(strip_think_tags("<think>a</think>b"), "b");
        assert_eq!(strip_think_tags("  <think>a</think>\n b "), "b");
        assert_eq!(strip_think_tags("x<think>a</think>y<think>c</think>z"), "xyz");
        assert_eq!(strip_think_tags("<THINK>a</THINK>b"), "b");
        // Unterminated: the tail is all scratch work.
        assert_eq!(strip_think_tags("answer<think>and then"), "answer");
        // Closer with no opener: everything before it was scratch work.
        assert_eq!(strip_think_tags("reasoning</think>answer"), "answer");
        // Nothing to do.
        assert_eq!(strip_think_tags("plain answer"), "plain answer");
        // Multibyte content must survive intact.
        assert_eq!(
            strip_think_tags("<think>सोच</think>मीटिंग साढ़े तीन बजे है।"),
            "मीटिंग साढ़े तीन बजे है।"
        );
    }

    /// The error body can echo the request, and the request is the user's
    /// transcript — which never reaches a log. Only the server's own
    /// diagnostic does.
    #[test]
    fn an_error_summary_is_the_servers_message_not_the_body() {
        assert_eq!(
            error_summary(r#"{"error":{"message":"unknown param 'think'"}}"#),
            "unknown param 'think'"
        );
        assert_eq!(error_summary(r#"{"message":"bad request"}"#), "bad request");
        assert_eq!(error_summary(r#"{"detail":"field required"}"#), "field required");
        assert_eq!(error_summary(""), "(empty body)");
        // A body with no recognisable diagnostic is reported by size alone —
        // it may be the request (and so the transcript) echoed back.
        let echoed = "<html>the transcript echoed back</html>";
        let summary = error_summary(echoed);
        assert!(!summary.contains("transcript"), "{summary}");
        assert_eq!(summary, format!("({} bytes, no error message)", echoed.len()));
        let long = format!(r#"{{"message":"{}"}}"#, "x".repeat(500));
        assert_eq!(error_summary(&long).chars().count(), 201, "capped, with an ellipsis");
    }

    /// Every caller logs a failed call as `{e:#}`, so that text must not
    /// carry the server's own message: a server can build it from the
    /// request, and the request is the user's transcript. It names the status
    /// and how long the message was, and nothing else — on the Sarvam and
    /// relay arm and on the custom arm alike.
    #[tokio::test]
    async fn a_failed_calls_error_text_names_the_status_and_length_only() {
        let message = "cannot parse: zanzibarqat wobblefjord";
        let body = serde_json::json!({ "error": { "message": message } }).to_string();
        let complete = |backend: Backend| async move {
            backend
                .complete(&reqwest::Client::new(), "sys", "user", 16, 0.0, Duration::from_secs(5))
                .await
                .expect_err("a 500 is a failure")
        };

        let s = stub(vec![(500, body.clone())]).await;
        let custom = Backend::custom(&format!("http://{}/v1/chat/completions", s.addr), None, "m");
        let s2 = stub(vec![(500, body.clone())]).await;
        let mut sarvam = Backend::sarvam("k", "m");
        sarvam.base_url = format!("http://{}", s2.addr);

        for err in [complete(custom).await, complete(sarvam).await] {
            let logged = format!("{err:#}");
            assert!(!logged.contains("zanzibarqat"), "the server's message reached the log text: {logged}");
            assert!(logged.contains("500"), "{logged}");
            assert!(logged.contains(&message.chars().count().to_string()), "{logged}");
            let debugged = format!("{err:?}");
            assert!(!debugged.contains("zanzibarqat"), "the server's message reached the Debug text: {debugged}");
            // Kept for the user's own screen.
            assert_eq!(
                shown_failure(&err).as_deref(),
                Some(format!("chat backend returned HTTP 500 Internal Server Error: {message}").as_str())
            );
        }

        // With no message, the size is all there is, in both texts.
        let s = stub(vec![(500, "<html>the transcript echoed back</html>".into())]).await;
        let err = complete(Backend::custom(&format!("http://{}/v1", s.addr), None, "m")).await;
        assert!(!format!("{err:#}").contains("transcript"), "{err:#}");
        assert!(format!("{err:#}").contains("bytes, no error message"), "{err:#}");
        assert_eq!(shown_failure(&err), Some(format!("{err:#}")));
    }
}
