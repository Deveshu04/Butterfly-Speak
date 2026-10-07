//! Batch speech-to-text against the user's own OpenAI-compatible endpoint.
//!
//! This is the third transcription path. The other two are streaming Sarvam
//! (`sarvam::ws`) and the on-device model (`asr::offline`); this one has the
//! *shape* of the local path — record the whole utterance, hand it over once,
//! get one transcript back — and the *plumbing* of the cloud path, because it
//! is an HTTP request that has to stay off the controller thread.
//!
//! ## The wire contract
//!
//! Multipart `POST {base}/audio/transcriptions`, fields `file`, `model`,
//! `language` (only when set) and `prompt` (only when the dictionary has
//! entries), reply `{"text": …}` on HTTP 200. Bearer auth, and only when the
//! user entered a key — plenty of self-hosted servers take none. Never
//! `stream`: a custom or self-hosted endpoint is batch multipart, always.
//!
//! Three things this deliberately does *not* do:
//!
//! - **No `api-key` header for Azure.** There is one slot and one auth scheme.
//!   Azure's `api-key` header is a whole second scheme (plus a deployment in
//!   the path and an `api-version` query) that would need its own detection,
//!   its own URL builder and its own test surface; sending a bearer token to
//!   Azure fails cleanly with a 401 the user can read, which is better than a
//!   half-built second scheme nobody can test.
//! - **No size cap.** The recording is bounded by the dictation itself:
//!   16 kHz mono PCM is 32 KB/s, so even a five-minute utterance is under
//!   10 MB. Hosted OpenAI-style APIs cap uploads at 25 MB, about 13 minutes
//!   of that audio; a self-hosted server sets its own limit.
//! - **No `/inference` fallback.** whisper.cpp's own `whisper-server` serves
//!   `POST /inference`, not `/audio/transcriptions`, so pointing this slot at
//!   a bare whisper-server will 404; the self-hosted endpoint this expects is
//!   a wrapper exposing the OpenAI path. Guessing a second path on 404 would
//!   post the user's audio to a route they never named.
//!
//! ## Privacy
//!
//! The request body *is* the user's voice and the reply *is* their words.
//! Nothing here logs either: every `tracing` call carries a duration, a byte
//! or word count, a status code or a typed discriminant. The same rule
//! `sarvam::batch` and `sarvam::codec` follow, on the one path where the
//! audio is a file rather than a stream.

use crate::cleanup::CleanupSettings;
use crate::endpoint::{self, Invalid, SttTarget};
use crate::sarvam::{chat, SharedKey};
use crate::state::ControlMsg;
use crossbeam_channel::Sender;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

/// The shortest transcription budget. Covers connect, the upload of a short
/// utterance, and a cold server loading its model on the first request of the
/// day — which is the case that makes a floor necessary at all.
pub const TIMEOUT_FLOOR: Duration = Duration::from_secs(10);

/// The longest. Past this the user has given up, and a request still in
/// flight is only holding the pill hostage. Also the term
/// `controller::CUSTOM_FINALIZE_TIMEOUT` budgets for.
pub const TIMEOUT_CEILING: Duration = Duration::from_secs(60);

/// How much deadline each second of audio adds on top of the floor, in
/// percent: 150 means one and a half seconds per second of speech.
///
/// Sized for a Whisper-class model running on the user's own CPU, which
/// transcribes at about real time (one second of work per second of
/// audio). The extra half covers a machine that is busy with the user's
/// other work. The floor already pays for connecting and loading the model.
/// At this rate the ceiling is reached at (60 s - 10 s) / 1.5, about 33.3 s
/// of audio.
const DEADLINE_PERCENT_OF_AUDIO: u64 = 150;

/// The dictionary hint's budget, in estimated tokens (see [`half_tokens`]).
///
/// OpenAI's speech-to-text guide says `whisper-1` takes prompts of up to
/// 224 tokens, and that its newer transcription models reject the whole
/// request when the prompt is over their limit. 200 stays under the
/// smaller figure with room for the estimate being a little low.
pub const PROMPT_TOKEN_BUDGET: usize = 200;

/// What goes between two dictionary entries in the hint. The model reads
/// the hint as earlier transcript, and a comma-separated run of names is
/// how a list of names reads in transcribed speech; OpenAI's guide uses the
/// same form in its own vocabulary-prompt example.
const PROMPT_SEPARATOR: &str = ", ";

/// One dictation's worth of work: the whole utterance, and everything about
/// the configuration it is being sent under.
///
/// The slot is a *snapshot*, taken at the finalize seam
/// (`controller::finish_recording`) when the request is built — not at
/// chord-down, where the only thing decided is *which* transcriber runs
/// (`controller::SttPath`). One snapshot, and it is the whole configuration
/// this job may consult: everything downstream of here — the transcription
/// request **and** the polish call that follows it — resolves against this
/// value and never re-reads the live slot. A settings save during a
/// sixty-second transcription must not be able to change where the words go
/// half way through, which is exactly what a second read would allow.
pub struct SttJob {
    pub req_id: u64,
    /// 16 kHz mono f32 — the same buffer the local recognizer would get.
    pub audio: Vec<f32>,
    pub duration_ms: u64,
    pub slot: endpoint::CustomSlot,
    /// The bare language subtag, or `None` for auto-detect. Derived by
    /// [`stt_language`] at the controller so the whole request is decided
    /// from one settings snapshot.
    pub language: Option<String>,
    /// Which host this install talks to when the slot leaves the polish to
    /// it: Sarvam with the user's key, or the Butterfly Labs relay.
    pub lane: crate::sarvam::Lane,
}

/// The transcription deadline for an utterance of `duration_ms`: the floor
/// plus [`DEADLINE_PERCENT_OF_AUDIO`] of the audio's length, held to the
/// ceiling. Saturating throughout, so no length can overflow it.
pub fn scaled_timeout(duration_ms: u64) -> Duration {
    let extra_ms = duration_ms.saturating_mul(DEADLINE_PERCENT_OF_AUDIO) / 100;
    let floor_ms = TIMEOUT_FLOOR.as_millis() as u64;
    Duration::from_millis(floor_ms.saturating_add(extra_ms)).min(TIMEOUT_CEILING)
}

/// The `language` field for a settings language, or `None` for auto-detect.
///
/// The field takes an ISO 639-1 code, which is the primary subtag of the
/// BCP 47 tag the settings store: the part before the first `-`. An `_` is
/// treated as a separator too, so a locale-style `hi_IN` also gives `hi`
/// rather than a value no server accepts. Lower-cased; `None` for a blank
/// value, `auto` in any case, or an empty primary subtag.
///
/// The settings value goes through unchanged otherwise. In particular
/// Sarvam's REST spelling of Odia is never applied here: `or-IN` arrives as
/// `or`, the real code.
pub fn stt_language(code: &str) -> Option<String> {
    let code = code.trim();
    if code.eq_ignore_ascii_case("auto") {
        return None;
    }
    let primary = code.split(['-', '_']).next().unwrap_or_default();
    if primary.is_empty() {
        None
    } else {
        Some(primary.to_ascii_lowercase())
    }
}

/// A cautious estimate of what `text` costs in a transcription model's
/// prompt, counted in half tokens so the arithmetic stays whole: half a
/// token per ASCII character and three tokens per other character. Indic
/// scripts cost several times more tokens per character than Latin script,
/// so one flat rate per character, or a count of bytes, would misjudge one
/// of them.
fn half_tokens(text: &str) -> usize {
    text.chars().map(|c| if c.is_ascii() { 1 } else { 6 }).sum()
}

/// The dictionary as one prompt hint, or `None` when there is nothing to
/// send.
///
/// Entries are trimmed, blank ones skipped, and the rest joined in
/// dictionary order with [`PROMPT_SEPARATOR`]. Packing stops before the
/// first entry that would take the hint over [`PROMPT_TOKEN_BUDGET`], so
/// the hint is always whole entries from the start of the dictionary. An
/// entry too long for the budget on its own therefore ends the hint where it
/// stands, and is never cut. Entries are placed whole, so one that contains
/// a comma of its own is kept intact.
pub fn dictionary_prompt(dictionary: &[String]) -> Option<String> {
    let budget = PROMPT_TOKEN_BUDGET * 2;
    let separator_cost = half_tokens(PROMPT_SEPARATOR);
    let mut hint = String::new();
    let mut spent = 0;
    for entry in dictionary.iter().map(|e| e.trim()).filter(|e| !e.is_empty()) {
        let cost = half_tokens(entry) + if hint.is_empty() { 0 } else { separator_cost };
        if spent + cost > budget {
            break;
        }
        if !hint.is_empty() {
            hint.push_str(PROMPT_SEPARATOR);
        }
        hint.push_str(entry);
        spent += cost;
    }
    (!hint.is_empty()).then_some(hint)
}

/// Why a transcription attempt produced no text. Every variant carries a
/// sentence for the pill: this path replaces the whole of transcription, so
/// a failure here is the dictation's failure, and silence would leave the
/// user with a spinner and no idea which of their two configured hosts is
/// broken.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SttError {
    /// The slot itself is unusable — nothing was sent anywhere.
    NotUsable(Invalid),
    /// No response arrived.
    Unreachable(Reach),
    /// The endpoint rejected the credential (401/403).
    Unauthorized,
    /// The endpoint asked for a slower pace (429).
    RateLimited,
    /// The endpoint answered with a status that is not a transcript.
    Status(u16),
    /// HTTP 200, but not the `{"text": …}` shape this route is defined by.
    BadReply,
}

/// The distinctions `sarvam::net_error` draws, minus the ones that only make
/// sense for a WebSocket upgrade, said about the user's own host instead of
/// about Sarvam. A separate enum rather than a reuse of `NetFailure` because
/// every one of that type's sentences names Sarvam, and telling someone whose
/// endpoint is down to "check your internet connection" would send them to
/// the wrong place entirely.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reach {
    /// DNS never resolved the host.
    Dns,
    /// The OS refused or reset the connection — nothing is listening, or a
    /// firewall said no.
    Refused,
    /// Nothing answered inside this request's own deadline.
    Timeout,
    /// The TLS handshake failed.
    Tls,
    /// A network failure this table has nothing more specific to say about.
    Other,
}

impl SttError {
    /// The pill's sentence. Never contains the URL: a pasted one can carry a
    /// credential in its userinfo or query string (see
    /// `endpoint::loggable_origin`), and this string is shown, logged by the
    /// overlay's event, and filed nowhere the user chose.
    pub fn user_message(&self) -> String {
        match self {
            SttError::NotUsable(why) => why.message().to_string(),
            SttError::Unreachable(Reach::Dns) => {
                "Couldn't look up your endpoint's address — check the URL in Settings".into()
            }
            SttError::Unreachable(Reach::Refused) => {
                "Your transcription endpoint refused the connection — is the server running?".into()
            }
            SttError::Unreachable(Reach::Timeout) => {
                "Your transcription endpoint didn't answer in time — try again".into()
            }
            SttError::Unreachable(Reach::Tls) => {
                "No secure connection to your endpoint — check its certificate, or this PC's clock"
                    .into()
            }
            SttError::Unreachable(Reach::Other) => {
                "Couldn't reach your transcription endpoint — check the URL in Settings".into()
            }
            SttError::Unauthorized => {
                "Your endpoint rejected the API key — re-enter it in Settings".into()
            }
            SttError::RateLimited => "Your endpoint is rate limiting — try again in a moment".into(),
            SttError::Status(code) => {
                format!("Your endpoint answered HTTP {code} — check its URL and model in Settings")
            }
            SttError::BadReply => {
                "Your endpoint didn't return a transcript — it must answer with a text field".into()
            }
        }
    }
}

/// Classify a `reqwest` failure by typed discriminants only — never by
/// message text, the rule `sarvam::net_error`'s module doc sets out and for
/// the same reason (locale- and version-dependent strings).
fn classify(err: &reqwest::Error) -> Reach {
    if err.is_timeout() {
        return Reach::Timeout;
    }
    // Walk to the `io::Error` reqwest wraps, then reuse `sarvam::net_error`'s
    // Winsock / rustls table rather than keeping a second copy of it here.
    let mut source: Option<&(dyn std::error::Error + 'static)> = std::error::Error::source(err);
    while let Some(e) = source {
        if let Some(io) = e.downcast_ref::<std::io::Error>() {
            return match crate::sarvam::net_error::classify_io_error(io) {
                crate::sarvam::net_error::NetFailure::NameNotResolved => Reach::Dns,
                crate::sarvam::net_error::NetFailure::Refused => Reach::Refused,
                crate::sarvam::net_error::NetFailure::Timeout => Reach::Timeout,
                crate::sarvam::net_error::NetFailure::Tls => Reach::Tls,
                _ => Reach::Other,
            };
        }
        source = e.source();
    }
    if err.is_connect() {
        Reach::Refused
    } else {
        Reach::Other
    }
}

/// The whole of the documented reply. `text` is **required**: a 200 without
/// it is not this route's contract, and defaulting it to `""` would turn
/// "your endpoint answered something else entirely" into "Didn't catch that"
/// — sending the user to check their microphone about a server
/// misconfiguration. `SttError::BadReply` names the real problem.
#[derive(serde::Deserialize)]
struct TranscriptionResponse {
    text: String,
}

/// One multipart transcription request.
///
/// `target` is a parameter rather than something resolved inside, so a test
/// can point this at a loopback stub — the same seam `sarvam::batch` and
/// `format::backend` use. Returns the transcript verbatim; the caller runs
/// the pipeline over it.
pub async fn transcribe(
    http: &reqwest::Client,
    target: &SttTarget,
    pcm: &[f32],
    language: Option<&str>,
    prompt: Option<&str>,
    timeout: Duration,
) -> Result<String, SttError> {
    // Fields as OpenAI's reference for this route names them. The recorder
    // hands over 16 kHz mono, so the WAV is written at that rate.
    let wav = crate::sarvam::codec::f32_to_wav16(pcm, 16_000);
    let bytes = wav.len();
    let audio = reqwest::multipart::Part::bytes(wav)
        .file_name("dictation.wav")
        .mime_str("audio/wav")
        .expect("audio/wav is a valid media type");
    let mut form = reqwest::multipart::Form::new()
        .part("file", audio)
        .text("model", target.model.clone());
    if let Some(language) = language {
        form = form.text("language", language.to_string());
    }
    if let Some(prompt) = prompt {
        form = form.text("prompt", prompt.to_string());
    }

    // `timeout` runs from connecting until the reply body is read.
    let mut req = http.post(&target.url).timeout(timeout).multipart(form);
    // A blank key is no key: an empty bearer makes some servers refuse a
    // request they would otherwise take.
    if let Some(key) = target.api_key.as_deref().map(str::trim).filter(|k| !k.is_empty()) {
        req = req.bearer_auth(key);
    }

    tracing::info!(
        wav_bytes = bytes,
        model = %target.model,
        has_language = language.is_some(),
        has_prompt = prompt.is_some(),
        "posting one utterance to the custom transcription endpoint"
    );
    let resp = match req.send().await {
        Ok(r) => r,
        Err(e) => {
            let reach = classify(&e);
            // The typed discriminant, never `{e}`: a reqwest error's Display
            // carries the URL, and a pasted URL is where a credential hides.
            tracing::warn!(?reach, "custom transcription request failed");
            return Err(SttError::Unreachable(reach));
        }
    };
    let status = resp.status();
    if !status.is_success() {
        // Status only. A non-2xx body from this route can echo the request —
        // and this request's body is the user's voice.
        tracing::warn!(status = status.as_u16(), "custom transcription returned an error status");
        return Err(match status.as_u16() {
            401 | 403 => SttError::Unauthorized,
            429 => SttError::RateLimited,
            code => SttError::Status(code),
        });
    }
    let raw = match resp.text().await {
        Ok(t) => t,
        Err(e) => {
            let reach = classify(&e);
            tracing::warn!(?reach, "custom transcription body read failed");
            return Err(SttError::Unreachable(reach));
        }
    };
    match serde_json::from_str::<TranscriptionResponse>(&raw) {
        Ok(body) => Ok(body.text),
        Err(e) => {
            // Never `{e}`: `serde_json::Error`'s Display embeds the offending
            // value for a type mismatch, and here that value is the
            // transcript. The same trap `sarvam::batch` pins with a test.
            tracing::warn!(
                category = ?e.classify(),
                line = e.line(),
                column = e.column(),
                "custom transcription reply was not the documented shape"
            );
            Err(SttError::BadReply)
        }
    }
}

/// The HTTP client this path uses, and the one thing that makes it different
/// from every other client in the app: **it does not follow redirects.**
///
/// reqwest's default is up to ten hops. A 307 or 308 preserves the method and
/// the body, so one redirect from a compromised, misconfigured or merely
/// creative endpoint re-POSTs the user's recorded voice to a host they never
/// named — silently, and with nothing in the UI to say where the audio went.
/// Every other request in this app carries text the user is at least sending
/// somewhere deliberately; this one carries the raw recording, so it is the
/// one body worth failing closed for. A redirecting endpoint surfaces as
/// `SttError::Status(307)`, a sentence naming the code — a better answer than
/// a silent success from an unknown host.
fn stt_client() -> reqwest::Client {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        // Only reachable if the TLS backend cannot be initialised, which
        // would have taken the app's other clients with it long before here.
        .unwrap_or_else(|e| {
            tracing::warn!("custom STT client fell back to the default builder: {e}");
            reqwest::Client::new()
        })
}

/// Start the custom-STT worker. One long-lived task on the tauri runtime,
/// exactly like `sarvam::spawn`, so a slow endpoint can never block the
/// controller thread — and so two dictations can never be in flight at once.
pub fn spawn(
    ctl_tx: Sender<ControlMsg>,
    key: SharedKey,
    cleanup: Arc<RwLock<CleanupSettings>>,
) -> UnboundedSender<SttJob> {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    tauri::async_runtime::spawn(dispatcher(ctl_tx, key, cleanup, rx));
    tx
}

async fn dispatcher(
    ctl_tx: Sender<ControlMsg>,
    key: SharedKey,
    cleanup: Arc<RwLock<CleanupSettings>>,
    mut rx: UnboundedReceiver<SttJob>,
) {
    let http = stt_client();
    while let Some(job) = rx.recv().await {
        let lane = job.lane.clone();
        // Awaited only when there is text to polish: on the Cloud lane it can
        // be a sign-in refresh.
        let home = async { crate::sarvam::Transport::resolve(&lane, &key).await.ok() };
        run_job(&ctl_tx, home, &cleanup, &http, job).await;
    }
}

/// Transcribe one utterance, then put it through the same rules → polish →
/// guardrail pipeline every other transcript goes through, and hand the
/// result to the controller as an ordinary `FinalResult`. The route seam,
/// History, the paste and Undo all run downstream of that message and never
/// learn which of the three transcription paths produced it.
///
/// `home` resolves this install's own host on the job's lane — Sarvam with
/// the user's key, or the relay with their sign-in — or `None` when there is
/// neither. It is awaited only when there is text to polish and the slot
/// leaves the polish to that host, and for at most
/// [`crate::sarvam::CREDENTIAL_WAIT`].
async fn run_job(
    ctl_tx: &Sender<ControlMsg>,
    home: impl std::future::Future<Output = Option<crate::sarvam::Transport>>,
    cleanup: &Arc<RwLock<CleanupSettings>>,
    http: &reqwest::Client,
    job: SttJob,
) {
    let SttJob {
        req_id,
        audio,
        duration_ms,
        slot,
        language,
        lane: _,
    } = job;
    let started = Instant::now();
    let toggles = cleanup.read().expect("cleanup lock").clone();

    let target = match endpoint::resolve_stt(&slot) {
        Ok(target) => target,
        Err(why) => return fail(ctl_tx, req_id, &SttError::NotUsable(why)),
    };
    let prompt = dictionary_prompt(&toggles.dictionary);
    let raw = match transcribe(
        http,
        &target,
        &audio,
        language.as_deref(),
        prompt.as_deref(),
        scaled_timeout(duration_ms),
    )
    .await
    {
        Ok(text) => text.trim().to_string(),
        Err(why) => return fail(ctl_tx, req_id, &why),
    };

    // The cloud pipeline, not the local one. A whisper-class model returns
    // punctuated, cased text in whichever of many languages was spoken, which
    // is precisely the shape `run_cloud_pipeline` exists for — the English-only
    // regex stages (fillers, backtrack, ITN) must not touch it, and the
    // punctuation model has nothing to add.
    let (mut text, dict_fixes) = crate::cleanup::run_cloud_pipeline(raw.clone(), &toggles);
    let mut notice: Option<String> = None;
    if !text.is_empty() && toggles.level != crate::format::level::CleanupLevel::Off {
        // Resolved against **this job's own slot**, never the live one.
        // `resolve_polish_backend` would read `endpoint::slot()` fresh, and the
        // gap it reads across is the whole transcription — up to a minute in
        // which the user can open Settings and turn "use for speech-to-text"
        // off. The resolver's fallback rule keys on exactly that flag, so a
        // live read would see it `false`, decide degrading to Sarvam is safe
        // because "Sarvam is already hearing this dictation", and POST Sarvam
        // the transcript of audio it never received. The snapshot is what makes
        // that rule true for the dictation it is actually judging.
        //
        // The host the slot leaves the polish to is this install's own, on
        // its own lane: the relay for a Cloud install, which has no Sarvam
        // key at all.
        //
        // That host is looked up only when it is needed, and for no longer
        // than `CREDENTIAL_WAIT`: on the Cloud lane the look-up can be a
        // sign-in refresh, and a dictation without its own host still pastes,
        // unpolished.
        let polish_backend = if endpoint::polishes_itself(&slot) {
            endpoint::resolve(&slot, None, &toggles.polish_model)
        } else {
            match tokio::time::timeout(crate::sarvam::CREDENTIAL_WAIT, home).await {
                Ok(Some(transport)) => {
                    endpoint::resolve_for(&slot, &transport, &toggles.polish_model)
                }
                _ => endpoint::resolve(&slot, None, &toggles.polish_model),
            }
        };
        let outcome = match &polish_backend {
            Ok(backend) => {
                chat::polish(
                    http,
                    backend,
                    &text,
                    &toggles.dictionary,
                    &toggles.level.prompt(),
                    // The rules the user saved on the Prompts page for this level, if
                    // any — the same field the cloud path reads (`sarvam::ws`).
                    // The snapshot cloned at the top of this job carries it, so
                    // an edited prompt applies on the custom endpoint too.
                    toggles.prompt_rules.as_deref(),
                )
                .await
            }
            // The fallback rule in practice: with the endpoint doing the STT,
            // an unusable polish endpoint does NOT quietly become a Sarvam
            // request. The words still reach the document — they just arrive as
            // dictated.
            Err(why) => chat::PolishOutcome::Failed(format!("no chat backend ({why:?})").into()),
        };
        if let Some(reason) = outcome.reason() {
            tracing::warn!("custom-endpoint polish failed ({reason}); using the rule-cleaned text");
        }
        match outcome {
            chat::PolishOutcome::Formatted(reply) => {
                let (resolved_text, note) =
                    crate::sarvam::ws::resolve_format(&raw, &text, Some(&reply), toggles.level);
                text = resolved_text;
                notice = note;
            }
            chat::PolishOutcome::Failed(_) => {
                notice = Some(
                    match polish_backend {
                        Err(endpoint::Unavailable::CustomEndpoint(_)) => {
                            endpoint::MSG_CUSTOM_UNAVAILABLE
                        }
                        _ => crate::sarvam::ws::MSG_POLISH_FAILED,
                    }
                    .to_string(),
                );
            }
        }
    }

    // Counts and durations only — never the transcript. Same policy as the
    // local finalize log and `sarvam::ws`.
    tracing::info!(
        raw_words = raw.split_whitespace().count(),
        clean_words = text.split_whitespace().count(),
        audio_ms = duration_ms,
        "custom transcription #{req_id} finished in {:?}",
        started.elapsed()
    );
    let fixes = crate::state::FixCounts {
        words_corrected: crate::cleanup::words_changed(&raw, &text),
        dict_fixes,
    };
    let _ = ctl_tx.send(ControlMsg::FinalResult {
        req_id,
        text,
        raw,
        fixes,
        notice,
        cut_short: false,
    });
}

/// Report a failed transcription through the channel the cloud path already
/// uses for exactly this: `session` is `0` because this path has no session
/// counter — nothing here can be raced by a *previous* dictation's socket,
/// since the dispatcher handles one job at a time and `req_id` already
/// identifies which one — and the controller's `Finalizing` arm matches on
/// `req_id` alone.
fn fail(ctl_tx: &Sender<ControlMsg>, req_id: u64, why: &SttError) {
    tracing::warn!(reason = ?why, "custom transcription produced nothing");
    let _ = ctl_tx.send(ControlMsg::CloudError {
        req_id,
        session: 0,
        message: why.user_message(),
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- the timeout ladder -------------------------------------------------

    #[test]
    fn empty_audio_gets_exactly_the_floor() {
        assert_eq!(scaled_timeout(0), TIMEOUT_FLOOR);
    }

    /// Where the rule meets the ceiling, computed from the constants.
    #[test]
    fn the_ceiling_is_reached_exactly_where_the_rule_says() {
        let span_ms = (TIMEOUT_CEILING - TIMEOUT_FLOOR).as_millis() as u64;
        let reach_ms = (span_ms * 100).div_ceil(DEADLINE_PERCENT_OF_AUDIO);
        assert_eq!(scaled_timeout(reach_ms), TIMEOUT_CEILING);
        assert!(scaled_timeout(reach_ms - 1) < TIMEOUT_CEILING);
        assert_eq!(scaled_timeout(u64::MAX), TIMEOUT_CEILING);
    }

    #[test]
    fn a_very_long_utterance_clamps_at_the_ceiling() {
        assert_eq!(scaled_timeout(10 * 60 * 1000), TIMEOUT_CEILING);
    }

    #[test]
    fn scaling_is_monotonic_between_the_floor_and_the_ceiling() {
        let mut previous = Duration::ZERO;
        for ms in (0..=70_000).step_by(1_000) {
            let now = scaled_timeout(ms);
            assert!(now >= previous, "went backwards at {ms} ms");
            assert!(now >= TIMEOUT_FLOOR && now <= TIMEOUT_CEILING, "{now:?} at {ms} ms");
            previous = now;
        }
    }

    // --- the language field -------------------------------------------------

    #[test]
    fn auto_sends_no_language_at_all() {
        assert_eq!(stt_language("auto"), None);
        assert_eq!(stt_language(""), None);
        assert_eq!(stt_language("   "), None);
    }

    #[test]
    fn a_regional_code_is_truncated_at_the_first_dash() {
        assert_eq!(stt_language("hi-IN").as_deref(), Some("hi"));
        assert_eq!(stt_language("en-IN").as_deref(), Some("en"));
        assert_eq!(stt_language("en").as_deref(), Some("en"));
    }

    /// An underscore separates subtags too; case is folded; an empty primary
    /// subtag is no language; `auto` is recognised in any case.
    #[test]
    fn underscores_case_and_empty_subtags() {
        assert_eq!(stt_language("hi_IN").as_deref(), Some("hi"));
        assert_eq!(stt_language("TA-IN").as_deref(), Some("ta"));
        assert_eq!(stt_language(" Mr ").as_deref(), Some("mr"));
        assert_eq!(stt_language("-IN"), None);
        assert_eq!(stt_language("_IN"), None);
        assert_eq!(stt_language("AUTO"), None);
    }

    /// Odia: the settings value is the realtime spelling (`or-IN`), and
    /// truncating it gives `or`, the real ISO-639-1 code — so this path wants
    /// exactly what it already has. `batch::to_rest_language_code`'s `od-IN`
    /// is a Sarvam REST quirk and must never be applied here; `od` is not a
    /// language code.
    #[test]
    fn odia_reaches_a_whisper_endpoint_as_the_iso_code_not_sarvams_rest_spelling() {
        assert_eq!(stt_language("or-IN").as_deref(), Some("or"));
        assert_eq!(
            stt_language(crate::sarvam::batch::to_rest_language_code("or-IN")).as_deref(),
            Some("od"),
            "if this is ever what gets sent, the REST mapping leaked onto this path"
        );
    }

    // --- the dictionary hint ------------------------------------------------

    #[test]
    fn an_empty_dictionary_sends_no_prompt() {
        assert_eq!(dictionary_prompt(&[]), None);
        assert_eq!(dictionary_prompt(&["".into(), "   ".into()]), None);
    }

    fn entries(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    fn hint_tokens(hint: &str) -> usize {
        half_tokens(hint).div_ceil(2)
    }

    #[test]
    fn padded_entries_come_back_trimmed_in_order() {
        assert_eq!(
            dictionary_prompt(&entries(&["  Kubernetes ", "", "\tAnjali Deshmukh"])).as_deref(),
            Some("Kubernetes, Anjali Deshmukh")
        );
    }

    /// A long dictionary is cut at an entry boundary inside the budget,
    /// and an entry with a comma in it is placed whole.
    #[test]
    fn a_long_dictionary_stops_at_a_whole_entry_inside_the_budget() {
        let mut list: Vec<String> = (0..300).map(|i| format!("Term{i:03}")).collect();
        list.insert(3, "Smith, Jones & Co".into());
        let hint = dictionary_prompt(&list).expect("some entries fit");
        assert!(hint_tokens(&hint) <= PROMPT_TOKEN_BUDGET, "{hint}");
        assert!(hint.starts_with("Term000, Term001, Term002, Smith, Jones & Co, Term003"));

        let rest = hint.replacen("Smith, Jones & Co", "SMITH", 1);
        let sent: Vec<&str> = rest.split(PROMPT_SEPARATOR).collect();
        let mut expected: Vec<&str> = list.iter().map(String::as_str).collect();
        expected[3] = "SMITH";
        assert!(sent.len() < expected.len(), "the dictionary was meant to overflow");
        assert_eq!(sent, expected[..sent.len()], "a whole-entry prefix");

        // The next entry really would not have fitted.
        let with_next = format!("{hint}{PROMPT_SEPARATOR}{}", expected[sent.len()]);
        assert!(hint_tokens(&with_next) > PROMPT_TOKEN_BUDGET);
    }

    /// A few dozen ordinary names fit whole.
    #[test]
    fn a_few_dozen_names_fit() {
        let list: Vec<String> = (0..30).map(|i| format!("Priyanka{i:02}")).collect();
        let hint = dictionary_prompt(&list).expect("fits");
        assert_eq!(hint.split(PROMPT_SEPARATOR).count(), 30);
    }

    /// An entry too long for the whole budget is not cut, and does not
    /// panic on multi-byte text: alone it gives no hint, and after smaller
    /// entries the hint stops in front of it.
    #[test]
    fn an_oversized_entry_is_never_cut() {
        let huge = "नमस्ते".repeat(40);
        assert!(hint_tokens(&huge) > PROMPT_TOKEN_BUDGET);
        assert_eq!(dictionary_prompt(&[huge.clone()]), None);
        let hint = dictionary_prompt(&entries(&["दिल्ली", "Pune", &huge, "Kochi"]))
            .expect("the small entries fit");
        assert_eq!(hint, "दिल्ली, Pune");
        assert!(hint_tokens(&hint) <= PROMPT_TOKEN_BUDGET);
    }

    // --- the request --------------------------------------------------------

    /// Reads a raw HTTP/1.1 request off `socket` until the connection stops
    /// producing bytes, returning what arrived so the test can assert on the
    /// headers and the multipart field names. Same non-blocking loop the
    /// `sarvam::batch` and `format::backend` stubs use.
    async fn read_request(socket: &tokio::net::TcpStream) -> String {
        let mut out = Vec::new();
        let mut buf = [0u8; 8192];
        loop {
            // Bounded by silence, not by one `WouldBlock`: a multipart body
            // arrives in several segments, and returning at the first gap
            // would hand the assertions half a request. The client keeps the
            // connection open, so "nothing more for 300 ms" is the end.
            if tokio::time::timeout(Duration::from_millis(300), socket.readable())
                .await
                .is_err()
            {
                break;
            }
            match socket.try_read(&mut buf) {
                Ok(0) => break,
                Ok(n) => out.extend_from_slice(&buf[..n]),
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => continue,
                Err(e) => panic!("failed reading stub request: {e}"),
            }
        }
        String::from_utf8_lossy(&out).into_owned()
    }

    async fn write_response(socket: &tokio::net::TcpStream, response: &[u8]) {
        let mut written = 0;
        while written < response.len() {
            socket.writable().await.expect("socket writable");
            match socket.try_write(&response[written..]) {
                Ok(n) => written += n,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => continue,
                Err(e) => panic!("failed writing stub response: {e}"),
            }
        }
    }

    fn json_response(body: &str) -> String {
        format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        )
    }

    /// Spawns a one-shot stub that answers `response` and hands back what it
    /// received, plus the target pointing at it.
    async fn stub(
        response: String,
        api_key: Option<&str>,
    ) -> (SttTarget, tokio::task::JoinHandle<String>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind loopback listener");
        let addr = listener.local_addr().expect("local addr");
        let handle = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.expect("accept");
            let request = read_request(&socket).await;
            write_response(&socket, response.as_bytes()).await;
            request
        });
        (
            SttTarget {
                url: format!("http://{addr}/v1/audio/transcriptions"),
                model: "whisper-1".into(),
                api_key: api_key.map(str::to_string),
            },
            handle,
        )
    }

    /// The wire contract, asserted on the bytes that actually go out: the
    /// documented fields, a bearer token, and — the load-bearing negative —
    /// no `api-subscription-key`, which is Sarvam's header and must never
    /// reach a host the user pasted in.
    #[tokio::test]
    async fn the_request_carries_the_documented_fields_and_only_a_bearer_token() {
        let (target, handle) = stub(json_response(r#"{"text":"hello there"}"#), Some("sk-abc")).await;
        let http = stt_client();
        let out = transcribe(
            &http,
            &target,
            &[0.0, 0.1, -0.1],
            Some("hi"),
            Some("Sarvam, Devanagari"),
            TIMEOUT_FLOOR,
        )
        .await;
        assert_eq!(out.as_deref(), Ok("hello there"));

        let request = handle.await.expect("stub finished");
        assert!(request.starts_with("POST /v1/audio/transcriptions"), "{request}");
        assert!(request.to_lowercase().contains("authorization: bearer sk-abc"), "{request}");
        assert!(
            !request.to_lowercase().contains("api-subscription-key"),
            "Sarvam's header must never leave for a custom host"
        );
        assert!(request.contains("multipart/form-data"), "{request}");
        for field in [
            r#"name="file""#,
            r#"name="model""#,
            r#"name="language""#,
            r#"name="prompt""#,
        ] {
            assert!(request.contains(field), "missing {field} in {request}");
        }
        assert!(request.contains("filename=\"dictation.wav\""), "{request}");
        assert!(request.contains("RIFF"), "the WAV header itself must be in the body");
        assert!(!request.contains(r#"name="stream""#), "this route is never streamed");
    }

    /// No key entered is the ordinary self-hosted case, and it must send no
    /// Authorization header at all rather than an empty bearer.
    #[tokio::test]
    async fn a_keyless_endpoint_gets_no_authorization_header() {
        let (target, handle) = stub(json_response(r#"{"text":"hi"}"#), None).await;
        let http = stt_client();
        let _ = transcribe(&http, &target, &[0.0], None, None, TIMEOUT_FLOOR).await;
        let request = handle.await.expect("stub finished");
        assert!(!request.to_lowercase().contains("authorization:"), "{request}");
        // ...and the optional fields really are optional.
        assert!(!request.contains(r#"name="language""#), "{request}");
        assert!(!request.contains(r#"name="prompt""#), "{request}");
    }

    #[tokio::test]
    async fn a_401_is_reported_as_a_rejected_key() {
        let (target, _handle) = stub(
            "HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".into(),
            Some("sk-bad"),
        )
        .await;
        let http = stt_client();
        let out = transcribe(&http, &target, &[0.0], None, None, TIMEOUT_FLOOR).await;
        assert_eq!(out, Err(SttError::Unauthorized));
    }

    #[tokio::test]
    async fn an_unexpected_status_carries_its_code_into_the_notice() {
        let (target, _handle) = stub(
            "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".into(),
            None,
        )
        .await;
        let http = stt_client();
        let out = transcribe(&http, &target, &[0.0], None, None, TIMEOUT_FLOOR).await;
        assert_eq!(out, Err(SttError::Status(404)));
        assert!(
            out.unwrap_err().user_message().contains("404"),
            "the status is the one diagnostic worth showing"
        );
    }

    /// A 200 that is not the documented shape must not panic and must not
    /// paste something that isn't a transcript.
    #[tokio::test]
    async fn a_reply_without_a_text_field_is_a_bad_reply() {
        let (target, _handle) = stub(json_response(r#"{"segments":[]}"#), None).await;
        let http = stt_client();
        let out = transcribe(&http, &target, &[0.0], None, None, TIMEOUT_FLOOR).await;
        // A 200 without `text` is not this route's contract. It must not
        // become an empty transcript: the controller renders that as "Didn't
        // catch that", which sends the user to check their microphone about a
        // server that answered the wrong shape.
        assert_eq!(out, Err(SttError::BadReply));

        // ...while a real, empty transcript — `{"text": ""}`, which is what a
        // whisper server returns for silence — still is one.
        let (target, _handle) = stub(json_response(r#"{"text":""}"#), None).await;
        let out = transcribe(&http, &target, &[0.0], None, None, TIMEOUT_FLOOR).await;
        assert_eq!(out.as_deref(), Ok(""), "silence is an answer, not a bad reply");

        let (target, _handle) = stub(
            "HTTP/1.1 200 OK\r\nContent-Length: 8\r\nConnection: close\r\n\r\nnot json".into(),
            None,
        )
        .await;
        let out = transcribe(&http, &target, &[0.0], None, None, TIMEOUT_FLOOR).await;
        assert_eq!(out, Err(SttError::BadReply));
    }

    /// A redirect must not carry the recording anywhere. 307 and 308
    /// preserve the method *and the body*, so a followed hop re-POSTs the
    /// user's voice to a host they never named — the one thing this path
    /// fails closed for (`stt_client`). The redirect target here is a second
    /// live listener, so a regression is caught by that listener accepting a
    /// connection rather than by a hostname assertion that could pass while
    /// the audio still left.
    #[tokio::test]
    async fn a_redirect_is_refused_rather_than_followed_with_the_audio() {
        let elsewhere = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind loopback listener");
        let elsewhere_addr = elsewhere.local_addr().expect("local addr");
        let (target, _handle) = stub(
            format!(
                "HTTP/1.1 307 Temporary Redirect\r\nLocation: http://{elsewhere_addr}/v1/audio/transcriptions\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            ),
            None,
        )
        .await;

        let http = stt_client();
        let out = transcribe(&http, &target, &[0.0, 0.1], None, None, TIMEOUT_FLOOR).await;
        assert_eq!(
            out,
            Err(SttError::Status(307)),
            "a redirect is a configuration answer, not a route to follow"
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(200), elsewhere.accept())
                .await
                .is_err(),
            "the recording must not reach a host the user never named"
        );
    }

    /// Headers, then silence. The request-level timeout has to bound the
    /// body read too, or one stalled endpoint parks the whole dispatcher —
    /// the same guard `sarvam::batch` has on its own path.
    #[tokio::test]
    async fn a_stalled_response_body_times_out_rather_than_hanging_forever() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind loopback listener");
        let addr = listener.local_addr().expect("local addr");
        tokio::spawn(async move {
            if let Ok((socket, _)) = listener.accept().await {
                let _ = read_request(&socket).await;
                write_response(
                    &socket,
                    b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 40\r\n\r\n",
                )
                .await;
                tokio::time::sleep(Duration::from_secs(60)).await;
                drop(socket);
            }
        });
        let target = SttTarget {
            url: format!("http://{addr}/v1/audio/transcriptions"),
            model: "whisper-1".into(),
            api_key: None,
        };
        let http = stt_client();
        let started = tokio::time::Instant::now();
        let out = transcribe(&http, &target, &[0.0], None, None, Duration::from_millis(200)).await;
        assert_eq!(out, Err(SttError::Unreachable(Reach::Timeout)));
        assert!(started.elapsed() < Duration::from_secs(5), "{:?}", started.elapsed());
    }

    /// Nothing listening on the port is the most common misconfiguration
    /// there is (the server is not running), and it must say so rather than
    /// blaming the network.
    #[tokio::test]
    async fn a_dead_port_is_reported_as_a_refused_connection() {
        // Bound but never listening, so a connect is refused. The socket lives
        // to the end of the test: a port released here could be handed to
        // another test's listener, which would then answer this request.
        let socket = tokio::net::TcpSocket::new_v4().expect("create socket");
        socket
            .bind("127.0.0.1:0".parse().expect("loopback address"))
            .expect("bind loopback socket");
        let addr = socket.local_addr().expect("local addr");
        let target = SttTarget {
            url: format!("http://{addr}/v1/audio/transcriptions"),
            model: "whisper-1".into(),
            api_key: None,
        };
        let http = stt_client();
        let out = transcribe(&http, &target, &[0.0], None, None, TIMEOUT_FLOOR).await;
        assert_eq!(out, Err(SttError::Unreachable(Reach::Refused)));
    }


    /// `run_job` resolves its polish backend against the **snapshot** it was
    /// handed, never the live slot.
    ///
    /// The gap a live read spans is the whole transcription — up to a minute
    /// in which the user can open Settings and turn "use for speech-to-text"
    /// off. `endpoint::resolve`'s fallback rule keys on exactly that flag, so a
    /// live read would see it `false`, conclude that degrading to Sarvam is
    /// safe because "Sarvam is already hearing this dictation", and POST
    /// Sarvam the transcript of audio it never received.
    ///
    /// The live slot is left alone: it is one per process, and other tests
    /// read it while they run. In a test process it is off for polish, and
    /// this job has no home host, so a live read would find nothing to polish
    /// with and the snapshot's host would never see the second request.
    ///
    /// The host here is a loopback stub on purpose: a regression must be
    /// caught by *which stub was called*, not by a real request leaving for
    /// a real vendor — a test that leaks when it fails is not a guard.
    #[tokio::test]
    async fn the_polish_backend_is_resolved_against_the_jobs_own_snapshot() {
        let snapshot_host = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind loopback listener");
        let snapshot_addr = snapshot_host.local_addr().expect("local addr");

        // The snapshot's host answers twice: the transcription, then the
        // polish (a 500, so `chat::polish` fails without this test having to
        // fabricate a whole chat reply — only a 400/422 is sent again).
        let served = tokio::spawn(async move {
            let mut request_lines = Vec::new();
            for reply in [
                json_response(r#"{"text":"hello there"}"#),
                "HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    .to_string(),
            ] {
                let (socket, _) = snapshot_host.accept().await.expect("accept");
                let request = read_request(&socket).await;
                write_response(&socket, reply.as_bytes()).await;
                request_lines.push(request.lines().next().unwrap_or_default().to_string());
            }
            request_lines
        });

        let slot = endpoint::CustomSlot {
            base_url: format!("http://{snapshot_addr}"),
            model: "qwen3:8b".into(),
            stt_model: "whisper-1".into(),
            use_for_polish: true,
            use_for_stt: true,
            api_key: None,
        };

        let (ctl_tx, ctl_rx) = crossbeam_channel::unbounded();
        let cleanup = Arc::new(RwLock::new(CleanupSettings::default()));
        let http = stt_client();
        // The snapshot polishes, so this install's own host is never asked
        // for. The look-up records that it was polled and then never
        // finishes, so the check is whether the job asked at all, not how
        // long it took on a busy machine.
        let asked_for_home = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let home = {
            let asked = Arc::clone(&asked_for_home);
            std::future::poll_fn(move |_| {
                asked.store(true, std::sync::atomic::Ordering::SeqCst);
                std::task::Poll::<Option<crate::sarvam::Transport>>::Pending
            })
        };
        // A hang guard only, far above anything the job needs.
        tokio::time::timeout(
            Duration::from_secs(30),
            run_job(
                &ctl_tx,
                home,
                &cleanup,
                &http,
                SttJob {
                    req_id: 7,
                    audio: vec![0.0; 160],
                    duration_ms: 10,
                    slot,
                    language: None,
                    lane: crate::sarvam::Lane::Byok,
                },
            ),
        )
        .await
        .expect("the job hung");
        assert!(
            !asked_for_home.load(std::sync::atomic::Ordering::SeqCst),
            "the job waited on a credential the polish does not use"
        );

        let requests = tokio::time::timeout(Duration::from_secs(10), served)
            .await
            .expect("both requests reached the snapshot's host")
            .expect("stub task");
        assert!(
            requests[0].starts_with("POST /audio/transcriptions"),
            "{requests:?}"
        );
        assert!(
            requests[1].starts_with("POST /v1/chat/completions"),
            "the polish call must go to the host this dictation started on: {requests:?}"
        );

        match ctl_rx.try_recv() {
            Ok(ControlMsg::FinalResult { req_id, notice, .. }) => {
                assert_eq!(req_id, 7);
                assert_eq!(
                    notice.as_deref(),
                    Some(crate::sarvam::ws::MSG_POLISH_FAILED),
                    "the backend resolved and then failed — not 'no backend at all'"
                );
            }
            // Never `{other:?}`: `ControlMsg` deliberately has no `Debug` —
            // half its variants carry a transcript.
            Ok(_) => panic!("expected a FinalResult, got another ControlMsg"),
            Err(e) => panic!("expected a FinalResult, got nothing: {e}"),
        }
    }

    /// A Cloud install holds no Sarvam key. With the endpoint doing only the
    /// speech-to-text, the polish goes to this install's own chat host, the
    /// relay, instead of failing for want of a key.
    #[tokio::test]
    async fn a_cloud_dictation_polishes_through_the_relay_when_the_endpoint_only_transcribes() {
        let stt_host = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind loopback listener");
        let stt_addr = stt_host.local_addr().expect("local addr");
        tokio::spawn(async move {
            let (socket, _) = stt_host.accept().await.expect("accept");
            let _ = read_request(&socket).await;
            write_response(&socket, json_response(r#"{"text":"hello there"}"#).as_bytes()).await;
        });
        let relay = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind loopback listener");
        let relay_addr = relay.local_addr().expect("local addr");
        let chat = tokio::spawn(async move {
            let (socket, _) = relay.accept().await.expect("accept");
            let request = read_request(&socket).await;
            // Answer as a model that followed its instructions: the text,
            // then the end marker the request minted.
            let at = request.rfind("<<").expect("the request carries its marker");
            let end = at + request[at..].find(">>").expect("a closed marker") + 2;
            let body = serde_json::json!({
                "choices": [{
                    "finish_reason": "stop",
                    "message": { "content": format!("Hello there!\n{}", &request[at..end]) }
                }],
                "usage": { "prompt_tokens": 1, "completion_tokens": 1 }
            })
            .to_string();
            write_response(&socket, json_response(&body).as_bytes()).await;
            request.lines().next().unwrap_or_default().to_string()
        });

        let slot = endpoint::CustomSlot {
            base_url: format!("http://{stt_addr}"),
            model: "qwen3:8b".into(),
            stt_model: "whisper-1".into(),
            use_for_polish: false,
            use_for_stt: true,
            api_key: None,
        };
        let relay_base = format!("http://{relay_addr}");
        let home = crate::sarvam::Transport::Relay {
            base: relay_base.clone(),
            bearer: "supabase-access-token".into(),
        };
        let (ctl_tx, ctl_rx) = crossbeam_channel::unbounded();
        let cleanup = Arc::new(RwLock::new(CleanupSettings::default()));
        let http = stt_client();
        run_job(
            &ctl_tx,
            std::future::ready(Some(home)),
            &cleanup,
            &http,
            SttJob {
                req_id: 9,
                audio: vec![0.0; 160],
                duration_ms: 10,
                slot,
                language: None,
                lane: crate::sarvam::Lane::Cloud { relay: relay_base },
            },
        )
        .await;

        match ctl_rx.try_recv() {
            Ok(ControlMsg::FinalResult { text, notice, .. }) => {
                assert!(notice.is_none(), "{notice:?}");
                assert_eq!(text, "Hello there!");
            }
            Ok(_) => panic!("expected a FinalResult, got another ControlMsg"),
            Err(e) => panic!("expected a FinalResult, got nothing: {e}"),
        }
        let line = tokio::time::timeout(Duration::from_secs(5), chat)
            .await
            .expect("the relay's chat route was called")
            .expect("stub task");
        assert!(line.starts_with("POST /v1/chat/completions"), "{line}");
    }

    /// A sign-in refresh that hangs is given up on after `CREDENTIAL_WAIT`:
    /// the words still arrive, unpolished, with the notice that says so.
    #[tokio::test]
    async fn a_sign_in_that_hangs_holds_the_polish_up_no_longer_than_the_credential_wait() {
        let stt_host = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind loopback listener");
        let stt_addr = stt_host.local_addr().expect("local addr");
        tokio::spawn(async move {
            let (socket, _) = stt_host.accept().await.expect("accept");
            let _ = read_request(&socket).await;
            write_response(&socket, json_response(r#"{"text":"hello there"}"#).as_bytes()).await;
        });
        let slot = endpoint::CustomSlot {
            base_url: format!("http://{stt_addr}"),
            model: "qwen3:8b".into(),
            stt_model: "whisper-1".into(),
            use_for_polish: false,
            use_for_stt: true,
            api_key: None,
        };
        let (ctl_tx, ctl_rx) = crossbeam_channel::unbounded();
        let cleanup = Arc::new(RwLock::new(CleanupSettings::default()));
        let http = stt_client();
        let started = Instant::now();
        tokio::time::timeout(
            crate::sarvam::CREDENTIAL_WAIT + Duration::from_secs(3),
            run_job(
                &ctl_tx,
                std::future::pending(),
                &cleanup,
                &http,
                SttJob {
                    req_id: 11,
                    audio: vec![0.0; 160],
                    duration_ms: 10,
                    slot,
                    language: None,
                    lane: crate::sarvam::Lane::Cloud {
                        relay: "https://relay.example".into(),
                    },
                },
            ),
        )
        .await
        .expect("the job gave up on the sign-in");
        assert!(started.elapsed() < crate::sarvam::CREDENTIAL_WAIT + Duration::from_secs(2));
        match ctl_rx.try_recv() {
            Ok(ControlMsg::FinalResult { text, notice, .. }) => {
                assert_eq!(text, "Hello there");
                assert_eq!(notice.as_deref(), Some(crate::sarvam::ws::MSG_POLISH_FAILED));
            }
            Ok(_) => panic!("expected a FinalResult, got another ControlMsg"),
            Err(e) => panic!("expected a FinalResult, got nothing: {e}"),
        }
    }

    // --- the notices --------------------------------------------------------

    #[test]
    fn every_failure_says_something_different_and_names_no_url() {
        let all = [
            SttError::NotUsable(Invalid::NotConfigured),
            SttError::Unreachable(Reach::Dns),
            SttError::Unreachable(Reach::Refused),
            SttError::Unreachable(Reach::Timeout),
            SttError::Unreachable(Reach::Tls),
            SttError::Unreachable(Reach::Other),
            SttError::Unauthorized,
            SttError::RateLimited,
            SttError::Status(500),
            SttError::BadReply,
        ];
        let messages: std::collections::HashSet<String> =
            all.iter().map(|e| e.user_message()).collect();
        assert_eq!(messages.len(), all.len(), "every failure must be distinguishable");
        for message in messages {
            // The endpoint's URL is the one piece of configuration that can
            // hide a credential (userinfo, `?api_key=`), so no notice may
            // carry it — however useful it would look in the pill.
            assert!(!message.contains("://"), "a notice must not carry a URL: {message}");
        }
    }
}
