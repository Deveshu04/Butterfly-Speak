//! The cloud dispatcher: one long-lived tokio task that runs one realtime STT
//! session per dictation (connect-per-utterance — a persistent socket would
//! fight the server's inactivity timeout and pin one of the plan's concurrent
//! connections for nothing).
//!
//! Command flow per dictation: `Start (Audio*) (Finish | Cancel)`. Replies go
//! back to the controller as `ControlMsg::{FinalResult, CloudError}`. Partial
//! transcripts stay inside this task: they are accumulated into the final
//! text and never surface in the overlay, so a half-decoded phrase can't make
//! the user second-guess what they just said.
//!
//! Recovery: a session whose socket opened and then died — before any finals,
//! or mid-utterance with a fragment in hand (via `ws_dead`, see
//! `drain_session`) — gets one [`batch`] REST fallback attempt on the same
//! audio before it's treated as a real failure, when a rescue is possible at
//! all: on Bring your own key, while the tee still holds the whole utterance
//! (`rescue_left`). When none is, the recording is stopped at once rather
//! than left running into a dead socket. If the fallback can't produce a
//! whole utterance either, a fragment is filed in History and reported rather
//! than pasted (`DrainOutcome::Truncated`). A connect failure gets no
//! fallback: it is classified ([`net_error`]) into a specific user message
//! and, if transient, retried up to `CONNECT_MAX_ATTEMPTS` times within the
//! same dictation before falling back to scheduling a short backoff
//! ([`ConnectBackoff`]) before the *next* utterance's own first attempt; and
//! `FLUSH_WAIT_FLOOR`/`_CEILING` scale the finals-drain wait with how long the
//! utterance actually was.

use super::codec::{self, ClientMsg, ServerMsg};
use super::net_error::NetFailure;
use super::{
    batch, chat, net_error, CloudCmd, Endpointing, Lane, SessionCfg, SharedKey, Transport,
    MSG_CLOUD_BAD_QUERY, MSG_CLOUD_BUSY, MSG_CLOUD_QUOTA, MSG_CLOUD_SESSION_LIMIT,
    MSG_CLOUD_SIGN_IN,
};
use crate::cleanup::CleanupSettings;
use crate::format::timing::StageTimings;
use crate::state::ControlMsg;
use crossbeam_channel::Sender;
use futures_util::stream::SplitSink;
use futures_util::{SinkExt, StreamExt};
use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};
use std::time::Duration;
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::time::Instant;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::protocol::frame::CloseFrame;
use tokio_tungstenite::tungstenite::{Error as WsError, Message};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

type WsStream = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;
type WsSink = SplitSink<WsStream, Message>;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(4);
/// Bound on the wait for `session.begin` when the hotkey was released
/// before the handshake finished — a handshake, not a drain, so utterance
/// duration has no bearing on it and this stays flat.
///
/// Also the *floor* of the scaled wait for finals after the finish frames
/// went out (`scaled_flush_wait`): short utterances see exactly this.
const FLUSH_WAIT_FLOOR: Duration = Duration::from_secs(4);
/// Ceiling on the scaled wait for finals — see `scaled_flush_wait`.
/// `Controller::CLOUD_FINALIZE_TIMEOUT`'s own doc comment sums this in as
/// one of its worst-case terms; changing this means updating that constant
/// too, or a slow but successful long dictation can get discarded at its
/// deadline.
const FLUSH_WAIT_CEILING: Duration = Duration::from_secs(6);
/// One millisecond of extra drain wait per this many milliseconds of utterance
/// duration, added to `FLUSH_WAIT_FLOOR` and clamped at `FLUSH_WAIT_CEILING`
/// (`scaled_flush_wait`). This only covers how much longer a realtime session's
/// own server might still be draining already-streamed audio after the finish
/// frames (`speech_end`/`end`) — a small effect, nothing like a batch job's
/// per-second budget — so a 60 s hands-free utterance reaches
/// `FLUSH_WAIT_CEILING` and anything shorter scales smoothly toward it (see
/// `scaled_flush_wait`'s tests for the worked numbers).
const FLUSH_WAIT_SCALE_DIVISOR: u64 = 30;
/// Fallback exit for a session whose `session.end` never arrives: quiet
/// window after the last final before we stop waiting for more. The finish
/// frames end the session and the drain normally exits on `session.end`
/// ~4 ms after the last final; this window only fires when that frame is
/// lost. Nothing then says every final is in, so the dictation goes out with
/// `msg_end_unconfirmed` as its notice. Long enough for a second final that
/// a slow link delivers well after the first (`session.end` is what normally
/// ends the wait, and the relay forwards it too); the hard deadline still
/// bounds it.
const QUIET_WINDOW: Duration = Duration::from_millis(1_500);
/// A send that can't reach the OS socket buffer within this window means the
/// connection is dead. Without it, one stalled TCP connection parks the
/// dispatcher inside an `.await` where it can't even see a Cancel.
const SEND_TIMEOUT: Duration = Duration::from_secs(5);
/// Best-effort courtesy frame (`end`) for a session Saaras did not end
/// itself. Every exit but one reaches it: the drain's fallback exits (quiet
/// window, hard deadline, and a close or error on the socket)
/// and the `Cancel`/channel-closed arm, which sends its own goodbye before
/// returning. Only a session Saaras ended for us with `session.end` — the
/// normal path — skips it (`session_ended`).
const GOODBYE_TIMEOUT: Duration = Duration::from_secs(1);
/// Ceiling on the warm-up GET. Recording almost always outlasts it; if it
/// doesn't, the polish simply opens its own connection as before.
const WARMUP_TIMEOUT: Duration = Duration::from_secs(3);
/// How long an idle pooled connection to the chat host is kept. reqwest's
/// default is 90 s, shorter than the gap between two dictations more often
/// than not.
const HTTP_POOL_IDLE: Duration = Duration::from_secs(600);
/// How long a pooled connection may sit idle before the first TCP keep-alive
/// probe goes out, so a NAT or load balancer does not silently drop the idle
/// connection the pool still believes in. This is the `SO_KEEPALIVE` idle
/// time — what reqwest's `tcp_keepalive` sets — not the interval between
/// successive probes, which stays at the OS default.
const HTTP_KEEPALIVE: Duration = Duration::from_secs(30);
/// Bound on TCP+TLS connect for every request on the dispatcher's shared
/// client — the polish call, the warm-up, and the batch STT fallback
/// (`batch::transcribe`). Matches `CONNECT_TIMEOUT` on the realtime socket
/// to the same host; measured TLS setup is 120–420 ms, so 4 s is a
/// dead-host bound, not a budget.
const HTTP_CONNECT_TIMEOUT: Duration = Duration::from_secs(4);

/// The wait for finals after the finish frames went out, scaled by how long
/// the utterance actually was. See `FLUSH_WAIT_FLOOR`/`_CEILING`/
/// `_SCALE_DIVISOR` for what each bound means.
fn scaled_flush_wait(duration_ms: u64) -> Duration {
    let scaled_ms = FLUSH_WAIT_FLOOR.as_millis() as u64 + duration_ms / FLUSH_WAIT_SCALE_DIVISOR;
    Duration::from_millis(scaled_ms).min(FLUSH_WAIT_CEILING)
}

/// Whether a batch-fallback attempt is worth making for a realtime session that
/// produced nothing: long enough to be real speech
/// (`batch::MIN_FALLBACK_DURATION_MS`) and short enough that Sarvam's REST
/// endpoint can actually accept it (`batch::MAX_FALLBACK_DURATION_MS`) — see
/// both constants' own doc comments. `has_audio` is a separate check from the
/// duration bounds rather than folded into them: `duration_ms` comes from the
/// controller's own clock (real speech time), `fallback_pcm` from what this
/// dispatcher actually tee'd, and the two can only diverge if something
/// upstream already went wrong — worth keeping as its own, independently-named
/// condition rather than silently passing `0 > MIN_FALLBACK_DURATION_MS`
/// (always false) as the guard for both.
fn should_attempt_batch_fallback(duration_ms: u64, has_audio: bool) -> bool {
    has_audio
        && duration_ms > batch::MIN_FALLBACK_DURATION_MS
        && duration_ms <= batch::MAX_FALLBACK_DURATION_MS
}

/// The rate every stage of the fallback path already agrees on: the
/// controller streams 16 kHz mono f32 (`CloudCmd::Audio`'s doc comment), the
/// realtime URL declares `sample_rate=16000` (`codec::ws_url`), and
/// `batch::transcribe` re-encodes the tee at exactly 16 000 too
/// (`codec::f32_to_wav16(pcm, 16_000)`). Named here only so
/// `MAX_FALLBACK_SAMPLES` below can show its arithmetic instead of hiding a
/// bare literal inside it.
const FALLBACK_SAMPLE_RATE_HZ: usize = 16_000;

/// Hard ceiling on the batch-fallback tee (`fallback_pcm` in `drain_session`):
/// `batch::MAX_FALLBACK_DURATION_MS` worth of samples, and not one more.
///
/// Past this point the buffer can never be *used*: `should_attempt_batch_
/// fallback` above already refuses any utterance longer than
/// `MAX_FALLBACK_DURATION_MS`, because Sarvam's REST endpoint rejects it. So
/// every sample appended beyond this cap is, by construction, never sent
/// anywhere — without it a five-minute hands-free dictation accumulates
/// ~19 MB of `f32` for nothing.
///
/// The cap stops *pushing* rather than dropping the oldest samples. A
/// rolling window would leave the tee holding the last 30 s of a longer
/// dictation, which is not the whole utterance this fallback exists to
/// recover — and which the duration gate would refuse to send regardless —
/// so it would trade one unusable buffer for a more expensive unusable
/// buffer. Keeping the head instead means the buffer is always a genuine
/// prefix of the utterance, which is what it claims to be.
const MAX_FALLBACK_SAMPLES: usize =
    (batch::MAX_FALLBACK_DURATION_MS as usize) * FALLBACK_SAMPLE_RATE_HZ / 1_000;

/// Append `chunk` to the batch-fallback tee, stopping dead at
/// `MAX_FALLBACK_SAMPLES`. Split out of `drain_session`'s `CloudCmd::Audio`
/// arm so the cap is exercisable without a socket.
fn tee_fallback_pcm(buf: &mut Vec<f32>, chunk: &[f32]) {
    let room = MAX_FALLBACK_SAMPLES.saturating_sub(buf.len());
    if room == 0 {
        return;
    }
    buf.extend_from_slice(&chunk[..chunk.len().min(room)]);
}

/// What a finished drain resolves to, once the socket's health, what the
/// realtime session salvaged, and what (if anything) the batch fallback
/// came back with are all known. See `resolve_drain_outcome`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DrainOutcome {
    /// Use whatever the realtime session produced — including nothing at
    /// all, which downstream is either "Didn't catch that" (clean drain) or
    /// the drain's own error (`drain_error`).
    Realtime,
    /// Use the batch fallback's transcript instead: it re-transcribed the
    /// whole utterance from the tee, so it supersedes anything realtime
    /// managed to emit before it died.
    Fallback,
    /// The realtime transcript is a known fragment and there is nothing
    /// whole to replace it with. It is filed in History and reported —
    /// never pasted. See `ControlMsg::CloudTruncated`.
    Truncated,
}

/// Whether the realtime session's own transcript is fit to use as it stands,
/// which is the only case where no batch fallback is even considered.
///
/// Two things disqualify it. It can be empty — a session that yielded nothing.
/// Or the drain can have ended in an error (`drain_error`), which is the case
/// this predicate exists for: when the socket dies mid-utterance, the finals
/// that did arrive stop wherever the death happened, and the user kept talking
/// past that point. Treating those as a complete transcript is a silent
/// truncation: the user gets the first half of their sentence pasted with no
/// notice. "Some finals arrived" is evidence about how far the session got,
/// never evidence that it got all the way.
fn realtime_transcript_stands_alone(errored: bool, has_text: bool) -> bool {
    has_text && !errored
}

/// The drain's terminal decision, as a pure function of the three facts that
/// determine it — no socket, no HTTP client, no clock.
///
/// - `errored`: the drain ended abnormally (`drain_error` is set). That
///   covers a socket that proved dead before `Finish` (`ws_dead`) *and* one
///   that died during the drain itself; both mean the same thing here, that
///   the realtime transcript stops early through no choice of the user's.
/// - `has_realtime_text`: the finals/partial assembled to something.
/// - `has_fallback_text`: the batch fallback ran and returned a non-empty
///   transcript of the whole utterance.
///
/// The fallback wins whenever it has something, because it is the only
/// whole-utterance source in play. Failing that, a fragment from a failed
/// drain is withheld rather than pasted. Everything else is the ordinary
/// realtime path, unchanged — including "errored with nothing at all", where
/// there is no fragment to withhold and `drain_session`'s empty-text branch
/// already reports the failure.
fn resolve_drain_outcome(
    errored: bool,
    has_realtime_text: bool,
    has_fallback_text: bool,
) -> DrainOutcome {
    if has_fallback_text {
        DrainOutcome::Fallback
    } else if errored && has_realtime_text {
        DrainOutcome::Truncated
    } else {
        DrainOutcome::Realtime
    }
}

/// The key the batch rescue would spend, if this dictation has one.
///
/// Bring-your-own-key only, and not a policy choice: the batch rescue is a
/// REST call to Sarvam's own `speech-to-text` route with a Sarvam key, and
/// Cloud mode has neither. The relay is a realtime pipe and a chat proxy —
/// it exposes no batch route to forward this to — so a Cloud dictation never
/// attempts one, whatever its socket did.
fn batch_credential(transport: &Transport) -> Option<&str> {
    match transport {
        Transport::Sarvam { key } => Some(key.as_str()),
        Transport::Relay { .. } => None,
    }
}

/// The host this dictation is actually talking to, as its user knows it.
///
/// A Cloud user has no Sarvam account, no Sarvam key and no Sarvam dashboard
/// to go and look at: naming Sarvam in their error pill would name a company
/// they have never heard of and a fix they could not make. Bring-your-own-key
/// wording is unchanged to the byte — that lane really is talking to Sarvam.
fn host_name(relay: bool) -> &'static str {
    if relay {
        "Butterfly Labs"
    } else {
        "Sarvam"
    }
}

fn msg_unreachable(relay: bool) -> String {
    format!(
        "Couldn't reach {} — check your internet connection",
        host_name(relay)
    )
}

fn msg_lost(relay: bool) -> String {
    format!("Lost the connection to {} — try again", host_name(relay))
}

/// A fatal `ServerMsg::Error` frame, or close code 1011. The relay pipes
/// upstream frames through unchanged, so this reaches a Cloud user exactly as
/// it reaches a Bring-your-own-key one.
fn msg_service_error(relay: bool) -> String {
    format!("{} service error — try again", host_name(relay))
}

/// `session.end` arrived before this dictation asked to finish — the server
/// hung up mid-sentence. Same reasoning as above: it comes down the pipe.
fn msg_session_ended(relay: bool) -> String {
    format!("{} ended the session — try again", host_name(relay))
}

/// A silent fallback would let a dead model (an invalid model id answered
/// with an empty-bodied HTTP 400) go unnoticed. `PolishOutcome::Failed`
/// falls back to the rule-cleaned text — the user's words are never lost —
/// but the failure is reported instead of hidden.
pub(crate) const MSG_POLISH_FAILED: &str = "Formatting failed — used the plain transcript";
/// The incremental path's version of the sentence above, for the one case the
/// single-call wording gets wrong: a *chunk* failed or was rejected in the
/// background while the tail polished cleanly. "Used the plain transcript"
/// would tell the user none of their dictation was formatted, when in fact most
/// of it was — so the partial outcome gets its own sentence.
const MSG_PARTIAL_POLISH: &str = "Part of this dictation was not formatted — the rest was.";
/// The custom endpoint's own version of the sentence above, said when the
/// polish call had no backend at all because that endpoint is unusable. One
/// definition, shared with `asr::custom`, so the two paths cannot drift.
const MSG_CUSTOM_UNAVAILABLE: &str = crate::endpoint::MSG_CUSTOM_UNAVAILABLE;

/// Connect backoff: state that survives across separate `run_session`
/// invocations inside `dispatcher`'s own loop, and doubles as the delay
/// between the *retries* `run_session`'s own connect loop makes within one
/// dictation (`CONNECT_MAX_ATTEMPTS`). The same mechanism serves both, since
/// "wait before the next `connect_async` call" is exactly what each one
/// needs, whether that call is the first attempt of the *next* dictation or
/// the second attempt of *this* one. Scoped to the CONNECT phase only: a
/// failure once a session is already live (mid-drain) never touches this —
/// there is no reconnecting mid-utterance; a mid-drain failure salvages via
/// the batch fallback instead (see `ws_dead` in `drain_session`). This only
/// adds a wait *before* a `connect_async` call, never around one, so no
/// timeout in this file (`CONNECT_TIMEOUT`, `FLUSH_WAIT_FLOOR`/`_CEILING`,
/// `chat::POLISH_TIMEOUT`) is affected.
struct ConnectBackoff {
    consecutive_transient_failures: u32,
    until: Option<Instant>,
}

/// This delay sits between the user pressing the hotkey and the app dialing
/// Sarvam again for a *live* dictation, so it has to stay short enough that a
/// deliberate second press never feels like the app hung: exponential,
/// jittered, and capped at 2 s — far below a background upload retry's
/// numbers (`batch_job::RetryPolicy`).
const BACKOFF_BASE_MS: u64 = 250;
const BACKOFF_FACTOR: u32 = 2;
const BACKOFF_MAX_MS: u64 = 2_000;
const BACKOFF_JITTER_MS: u64 = 200;
/// Caps the exponent so a long failure streak can't overflow — past this
/// many consecutive failures the delay is already pinned at
/// `BACKOFF_MAX_MS` anyway, so further growth would be silent no-ops.
const BACKOFF_MAX_STEPS: u32 = 4;

/// How many total connect attempts one dictation gets before `run_session`
/// gives up and reports a failure. Every attempt happens while the user is
/// watching a live pill (worst case, each one costs its own `CONNECT_TIMEOUT`
/// plus a `ConnectBackoff` wait before it — see `CLOUD_FINALIZE_TIMEOUT`'s own
/// arithmetic in `controller.rs` for the exact budget this has to fit), so 2 —
/// the original attempt plus exactly one retry — rides out a single blip (a
/// momentary ECONNRESET or a Wi-Fi roam) without making a user who is already
/// getting a *classified, specific* error message sit through a third dial on a
/// connection that has failed twice in a row.
const CONNECT_MAX_ATTEMPTS: u32 = 2;

/// Whether `run_session`'s connect loop should try again after `attempt`
/// (1-indexed, the attempt that just failed) rather than give up: only when the
/// failure was classified transient *and* there is still a budgeted attempt
/// left. Pure and deterministic, unlike the loop itself (which also awaits
/// `ConnectBackoff::wait` and a real `connect_async`), so this is the part of
/// the retry a unit test can pin without a mock WebSocket server: that a
/// transient failure is retried, a non-transient one never is, and the attempt
/// cap is a hard stop either way.
fn should_retry_connect(attempt: u32, transient: bool) -> bool {
    transient && attempt < CONNECT_MAX_ATTEMPTS
}

/// What the connect loop does with an HTTP status the upgrade came back with.
#[derive(Debug, PartialEq, Eq)]
enum Verdict {
    /// Refresh the sign-in and dial once more. The relay's first `401` only:
    /// a second one has already survived a fresh token.
    Reauth,
    /// Transient — worth another dial if this dictation has an attempt left.
    /// The sentence is what to say when it does not.
    Retry(String),
    /// A standing failure a few seconds will not fix. This is the sentence.
    Fail(String),
}

/// Which of those three a status means, given the lane and whether this
/// dictation has already spent its one silent re-authentication.
///
/// Extracted from the loop because it is the whole of the decision and none
/// of the I/O: the loop around it awaits a backoff, a refresh and a real
/// `connect_async`, so the table below could otherwise only be checked
/// against a live relay. Its three inputs are exactly what the loop knows.
///
/// 429 means throttling that clears, not a quota that never does, and a 5xx is
/// the service rather than the network — both are worth the short automatic
/// retry; every other status (401/403 auth, a bad parameter) is a standing
/// problem.
fn upgrade_verdict(status: u16, relay: bool, reauthed: bool) -> Verdict {
    if status == 401 && relay && !reauthed {
        return Verdict::Reauth;
    }
    let msg = match status {
        401 if relay => MSG_CLOUD_SIGN_IN.to_string(),
        // Only on the Bring-your-own-key lane: a Cloud user has no Sarvam key
        // field to go and fix, so sending them to Settings → Speech engine would be
        // a dead end. Their 403 (a WAF, say — the relay's own contract has
        // none) falls through to the generic status sentence below.
        401 | 403 if !relay => "Sarvam key rejected — update it in Settings → Speech engine".to_string(),
        // The relay's `400 bad query`, which "try again" cannot fix: the next
        // attempt would send the identical query. See `MSG_CLOUD_BAD_QUERY`.
        400 if relay => MSG_CLOUD_BAD_QUERY.to_string(),
        429 if relay => MSG_CLOUD_BUSY.to_string(),
        429 => "Sarvam rate limit hit — try again in a moment".to_string(),
        _ => format!("{} returned HTTP {status} — try again", host_name(relay)),
    };
    if status == 429 || (500..600).contains(&status) {
        Verdict::Retry(msg)
    } else {
        Verdict::Fail(msg)
    }
}

/// The exponential base for the `attempt`-th consecutive transient failure
/// (1-indexed), before jitter. Pure and deterministic, unlike
/// `ConnectBackoff::record_failure`'s jitter, so it's exactly testable.
fn backoff_base_ms(attempt: u32) -> u64 {
    let step = attempt.min(BACKOFF_MAX_STEPS);
    BACKOFF_BASE_MS
        .saturating_mul(u64::from(BACKOFF_FACTOR.saturating_pow(step.saturating_sub(1))))
        .min(BACKOFF_MAX_MS)
}

impl ConnectBackoff {
    fn new() -> Self {
        Self {
            consecutive_transient_failures: 0,
            until: None,
        }
    }

    /// A connect attempt just finished; update the schedule. `transient ==
    /// false` (auth rejected, DNS blocked, a corrupted key) clears any
    /// pending backoff instead of extending it — none of those clear on
    /// their own within a few seconds, so delaying the user's next
    /// deliberate retry would not help and would only look like a hang.
    fn record_failure(&mut self, transient: bool) {
        if !transient {
            self.consecutive_transient_failures = 0;
            self.until = None;
            return;
        }
        self.consecutive_transient_failures += 1;
        let base = backoff_base_ms(self.consecutive_transient_failures);
        // `uuid` is already a dependency (per-request end markers,
        // `format::backend::mint_end_marker`) and is a convenient,
        // adequate jitter source — this is anti-thundering-herd padding,
        // not cryptography, so a dedicated `rand` dependency isn't
        // warranted just for it.
        let jitter = (uuid::Uuid::new_v4().as_u128() as u64) % (BACKOFF_JITTER_MS + 1);
        self.until = Some(Instant::now() + Duration::from_millis(base + jitter));
    }

    fn record_success(&mut self) {
        self.consecutive_transient_failures = 0;
        self.until = None;
    }

    /// Wait out any pending backoff — a no-op once it has already elapsed,
    /// or if none was ever scheduled.
    async fn wait(&self) {
        if let Some(until) = self.until {
            tokio::time::sleep_until(until).await;
        }
    }
}

/// The one HTTP client every polish goes through. Built once per dispatcher
/// (per app lifetime) so its connection pool survives between dictations —
/// with the pool told to keep connections long enough to matter. Falls back
/// to the plain client rather than panicking: a TLS backend that cannot be
/// configured is not a reason to refuse to dictate.
fn build_http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .pool_idle_timeout(HTTP_POOL_IDLE)
        .tcp_keepalive(HTTP_KEEPALIVE)
        .connect_timeout(HTTP_CONNECT_TIMEOUT)
        .build()
        .unwrap_or_else(|e| {
            tracing::warn!("http client builder failed ({e}); using defaults");
            reqwest::Client::new()
        })
}

/// Fire-and-forget: one tiny authenticated GET to the polish host so the
/// TLS connection is in the pool before the user releases the key. Result
/// ignored except for a debug line — a failed warm-up costs nothing the
/// polish would not have paid anyway. See `format::backend::warmup_url`.
///
/// Everything, credential resolution included, happens inside the spawned
/// task: on the Cloud lane the credential is an `await` (a token refresh may
/// be due), and the point of this function is that the caller is already on
/// its way to opening the socket.
fn spawn_warmup(
    http: &reqwest::Client,
    key: &SharedKey,
    cleanup: &Arc<RwLock<CleanupSettings>>,
    lane: &Lane,
) {
    let toggles = cleanup.read().expect("cleanup lock").clone();
    let http = http.clone();
    let key = key.clone();
    let lane = lane.clone();
    tokio::spawn(async move {
        let Ok(transport) = Transport::resolve(&lane, &key).await else {
            return;
        };
        let Ok(backend) =
            crate::endpoint::resolve_polish_backend_for(&transport, &toggles.polish_model)
        else {
            return;
        };
        let Some(url) = crate::format::backend::warmup_url(&backend, toggles.level) else {
            return;
        };
        let started = Instant::now();
        match backend
            .authenticate(http.get(&url))
            .timeout(WARMUP_TIMEOUT)
            .send()
            .await
        {
            Ok(resp) => {
                let status = resp.status();
                // Read to the end so the connection goes back to the pool.
                let _ = resp.bytes().await;
                tracing::debug!(
                    status = %status,
                    ms = started.elapsed().as_millis() as u64,
                    "chat connection warm-up"
                );
            }
            Err(e) => tracing::debug!(
                "chat connection warm-up failed: {}",
                crate::format::backend::redact_urls(&format!("{e:#}"))
            ),
        }
    });
}

/// The line or paragraph break a *raw* chunk carries at its very start or
/// end, as `(leading, trailing)`.
///
/// Spoken "new line"/"new paragraph" becomes a sentinel character in
/// `cleanup::commands::apply`, which `cleanup::tidy` later turns into real
/// newlines and then trims. Each chunk is tidied on its own, so a break
/// spoken at a chunk's edge is trimmed off it and then lost for good, since
/// `incremental::assemble_polished_with_tail` would otherwise join with a
/// single space. Reading it off the raw text here lets the seam carry it.
/// A break in the *middle* of a chunk is untouched — `tidy` keeps it.
///
/// `spoken_commands` is the user's own toggle, and must be the same one
/// `cleanup::run_cloud_pipeline` reads: it applies `commands::apply` only
/// when the toggle is on. Reading a break out of raw text the pipeline is
/// going to leave alone would put a newline at the seam for words that stay
/// in the chunk verbatim — a chunk ending "…start a new paragraph." would
/// keep those words *and* get a blank line after them.
fn seam_breaks(raw: &str, spoken_commands: bool) -> (Option<&'static str>, Option<&'static str>) {
    use crate::cleanup::{NEWLINE, PARAGRAPH};
    if !spoken_commands {
        return (None, None);
    }
    let cmd = crate::cleanup::commands::apply(raw.to_string());
    // Paragraph beats line, as `cleanup::tidy` resolves them.
    fn pick(para: bool, nl: bool) -> Option<&'static str> {
        match (para, nl) {
            (true, _) => Some("\n\n"),
            (_, true) => Some("\n"),
            _ => None,
        }
    }
    let head = cmd.trim_start();
    let tail = cmd.trim_end();
    (
        pick(head.starts_with(PARAGRAPH), head.starts_with(NEWLINE)),
        pick(tail.ends_with(PARAGRAPH), tail.ends_with(NEWLINE)),
    )
}

/// Looks this dictation's credential up again, for one polish call.
///
/// The socket connects with the credential of the moment, but a Cloud
/// dictation can outlive it: the bearer is an hour-long token that
/// `auth::session::access_token` refreshes five minutes before it expires,
/// and the relay checks expiry on every chat call. Asking again costs nothing
/// while the token is fresh.
type Credential = Arc<
    dyn Fn() -> futures_util::future::BoxFuture<'static, Result<Transport, String>> + Send + Sync,
>;

/// The backend for one polish call: this dictation's credential, looked up
/// again ([`Credential`]), or the one it connected with when the look-up
/// fails, which is still the best there is.
async fn fresh_backend(
    credential: &Credential,
    connected: &Transport,
    model: &str,
) -> Result<crate::format::backend::Backend, crate::endpoint::Unavailable> {
    fresh_backend_for(&crate::endpoint::slot(), credential, connected, model).await
}

/// [`fresh_backend`] against `slot`.
///
/// The look-up is given [`crate::sarvam::CREDENTIAL_WAIT`]: on the Cloud
/// lane it can be a sign-in refresh, and the tail's polish is on the path to
/// the paste. When the custom endpoint does the polish, this dictation's
/// credential plays no part in the call, so nothing is looked up.
async fn fresh_backend_for(
    slot: &crate::endpoint::CustomSlot,
    credential: &Credential,
    connected: &Transport,
    model: &str,
) -> Result<crate::format::backend::Backend, crate::endpoint::Unavailable> {
    let transport = if crate::endpoint::polishes_itself(slot) {
        connected.clone()
    } else {
        match tokio::time::timeout(crate::sarvam::CREDENTIAL_WAIT, credential()).await {
            Ok(Ok(fresh)) => fresh,
            _ => connected.clone(),
        }
    };
    crate::endpoint::resolve_for(slot, &transport, model)
}

/// One per session, spawned on the first chunk. Polishes chunks strictly in
/// order — each sees the previous chunks' polished text as the text before
/// the cursor, and whether it begins a new sentence there — through the same
/// rules → polish → guardrail path the tail takes, and never blocks the
/// drain loop. A chunk that fails or is rejected yields its rule-cleaned
/// text, exactly like a failed whole dictation does today; nothing is lost.
/// Each chunk's text is then run through `incremental::repair_seam` against
/// everything already assembled. Logs counts only.
fn spawn_chunk_worker(
    http: reqwest::Client,
    credential: Credential,
    connected: Transport,
    toggles: CleanupSettings,
    mut rx: tokio::sync::mpsc::UnboundedReceiver<ChunkJob>,
) -> tokio::task::JoinHandle<Vec<crate::sarvam::incremental::PolishedChunk>> {
    use crate::sarvam::incremental::{
        assemble_polished, context_tail, drop_added_period, repair_seam, ChunkOutcome,
        PolishedChunk,
    };
    tokio::spawn(async move {
        let mut chunks: Vec<PolishedChunk> = Vec::new();
        while let Some(ChunkJob { raw: raw_chunk, seam, ends_mid_sentence }) = rx.recv().await {
            let started = Instant::now();
            // Read off the raw text, before the pipeline that deletes it —
            // and under the same toggle that pipeline reads.
            let (leading_break, trailing_break) =
                seam_breaks(&raw_chunk, toggles.spoken_commands);
            let (rule_text, dict_fixes) =
                crate::cleanup::run_cloud_pipeline(raw_chunk.clone(), &toggles);
            // Everything already assembled: the text before the cursor for
            // this call, and what the seam repair below measures against.
            // `chunks` does not grow in between, so one build serves both.
            let previous = assemble_polished(&chunks, "");
            let context = context_tail(&previous);
            let outcome = match fresh_backend(&credential, &connected, &toggles.polish_model).await
            {
                Ok(backend) => {
                    chat::polish_with_context(
                        &http,
                        &backend,
                        &rule_text,
                        &context,
                        seam,
                        &toggles.dictionary,
                        &toggles.level.prompt(),
                        toggles.prompt_rules.as_deref(),
                    )
                    .await
                }
                // A reason, never a URL — `Unavailable`'s `Debug` is two
                // fieldless enums deep.
                Err(why) => chat::PolishOutcome::Failed(format!("no chat backend ({why:?})").into()),
            };
            let limit_spent = outcome.weekly_limit_spent();
            let (text, notice, kind) = match outcome {
                chat::PolishOutcome::Formatted(reply) => {
                    let (resolved, note) =
                        resolve_format(&raw_chunk, &rule_text, Some(&reply), toggles.level);
                    let kind = if note.is_some() {
                        ChunkOutcome::Rejected
                    } else {
                        ChunkOutcome::Accepted
                    };
                    (resolved, note, kind)
                }
                chat::PolishOutcome::Failed(reason) => {
                    tracing::warn!("background chunk polish failed ({reason}); using rule text");
                    // The weekly limit keeps its own sentence through the
                    // chunk, so the tail's assembly can still name it.
                    let said = if limit_spent {
                        MSG_CLOUD_QUOTA
                    } else {
                        MSG_POLISH_FAILED
                    };
                    // Cloned, not moved: the seam repair below still needs
                    // the model's input to tell an echo from a repetition the
                    // user dictated. One clone on the failure path only.
                    (rule_text.clone(), Some(said.to_string()), ChunkOutcome::Failed)
                }
            };
            // The deterministic half of the seam fix, run against
            // everything already assembled. It can only delete text that is
            // already in the document word for word, add an upper case letter
            // at a new sentence, and take off a final period the dictation
            // did not have — so it is safe after the guardrail, which handles
            // the gross expansions this cannot. `rule_text` is what the model
            // was given, so a repetition the *user* dictated is kept.
            let (text, repair) = repair_seam(&previous, &text, &rule_text, seam);
            let text = if ends_mid_sentence {
                drop_added_period(&text, &raw_chunk)
            } else {
                text
            };
            let chunk = PolishedChunk {
                text,
                notice,
                dict_fixes,
                polish_ms: started.elapsed().as_millis() as u64,
                outcome: kind,
                leading_break,
                trailing_break,
            };
            // Counts and milliseconds only — this file logs no transcript
            // text (see `resolve_format_inner`, `sarvam::codec`).
            tracing::debug!(
                words = raw_chunk.split_whitespace().count(),
                polish_ms = chunk.polish_ms,
                outcome = ?chunk.outcome,
                echoed_words = repair.echoed_words,
                capitalised = repair.capitalised,
                "background chunk polished"
            );
            chunks.push(chunk);
        }
        chunks
    })
}

/// One chunk handed to [`spawn_chunk_worker`].
struct ChunkJob {
    raw: String,
    /// How the chunk meets the text before it.
    seam: chat::Seam,
    /// The chunk was cut inside a run-on, so its sentence goes on in the
    /// next one.
    ends_mid_sentence: bool,
}

/// Whether the background-polished chunks may be assembled in front of the
/// tail, or must be thrown away for one call over the whole text.
///
/// All five conditions are load-bearing:
/// - `chunks_taken > 0`: with nothing handed out, this is the single-call
///   path, byte for byte.
/// - `realtime_text_stands`: the batch fallback replaced the transcript, so
///   the chunks describe text that is not being pasted at all.
/// - `chunks_returned == chunks_taken`: the worker was lost, panicked or ran
///   past its budget, so the middle of the dictation has no polished text.
/// - `in_sync`: a final arrived out of order, so `Segmenter::tail` hands back
///   the *whole* transcript rather than a remainder — assembling chunks in
///   front of that would emit the already-polished text twice.
/// - `level_on`: the user flipped AI Polish to `Off` between the last chunk
///   and the key going up. The polish branch is then skipped entirely and the
///   whole rule-cleaned text is what gets pasted, so the chunks — and the
///   `segments`/`dict_fixes` derived from them — would describe text that
///   never reached the document.
fn use_background_chunks(
    chunks_taken: usize,
    realtime_text_stands: bool,
    chunks_returned: usize,
    in_sync: bool,
    level_on: bool,
) -> bool {
    chunks_taken > 0
        && realtime_text_stands
        && chunks_returned == chunks_taken
        && in_sync
        && level_on
}

pub async fn dispatcher(
    ctl_tx: Sender<ControlMsg>,
    key: SharedKey,
    cleanup: Arc<RwLock<CleanupSettings>>,
    mut rx: UnboundedReceiver<CloudCmd>,
) {
    let http = build_http_client();
    let mut window = crate::format::timing::TimingWindow::default();
    let mut carryover: Option<CloudCmd> = None;
    // Survives across utterances by construction: this loop is the one
    // long-lived task for every dictation session the app ever opens (see this
    // module's doc comment), so a single `ConnectBackoff` here spans
    // utterances.
    let mut backoff = ConnectBackoff::new();
    loop {
        let cmd = match carryover.take() {
            Some(c) => c,
            None => match rx.recv().await {
                Some(c) => c,
                None => return,
            },
        };
        match cmd {
            CloudCmd::Start { session, cfg } => {
                spawn_warmup(&http, &key, &cleanup, &cfg.lane);
                carryover = run_session(
                    &ctl_tx,
                    &key,
                    &cleanup,
                    &http,
                    &mut rx,
                    session,
                    cfg,
                    &mut backoff,
                    &mut window,
                )
                .await;
            }
            // Strays from a torn-down session (late audio, a Cancel racing a
            // completed finalize) are meaningless outside one.
            _ => {}
        }
    }
}

async fn timed_send(ws_tx: &mut WsSink, msg: Message) -> Result<(), WsError> {
    match tokio::time::timeout(SEND_TIMEOUT, ws_tx.send(msg)).await {
        Ok(r) => r,
        Err(_) => Err(WsError::Io(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "websocket send timed out",
        ))),
    }
}

/// Fire-and-forget goodbye (`end`) — never worth blocking on.
async fn send_goodbye(ws_tx: &mut WsSink) {
    let _ = tokio::time::timeout(
        GOODBYE_TIMEOUT,
        ws_tx.send(Message::Text(ClientMsg::End.to_json())),
    )
    .await;
}

/// The two ways an upgrade request can fail before a byte leaves the
/// machine. Kept apart because the sentences the user needs are different:
/// one is "we could not build a URL", the other "that credential is not
/// usable".
#[derive(Debug, PartialEq, Eq)]
enum UpgradeFailure {
    BadUrl,
    BadCredential,
}

/// The realtime upgrade request: the lane's URL and the transport's
/// credential, and nothing else.
///
/// One place decides which header carries which credential, so the two lanes
/// cannot drift: Bring-your-own-key sends Sarvam's own
/// `api-subscription-key` and no bearer; Cloud sends
/// `Authorization: Bearer <the user's Supabase access token>` and never
/// Sarvam's header — the app has no Sarvam key to put in it.
fn upgrade_request(
    cfg: &SessionCfg,
    transport: &Transport,
) -> Result<tokio_tungstenite::tungstenite::handshake::client::Request, UpgradeFailure> {
    let mut request = codec::ws_url(cfg).into_client_request().map_err(|e| {
        // The URL is built from settings and a constant, so this is a
        // configuration error, not a secret: the relay override is the only
        // part a user can supply, and it is logged by `endpoint`'s rules
        // elsewhere. Log the failure, not the URL.
        tracing::error!("bad realtime URL: {e}");
        UpgradeFailure::BadUrl
    })?;
    let (name, value) = transport.auth_header();
    let value: tokio_tungstenite::tungstenite::http::HeaderValue =
        value.parse().map_err(|_| UpgradeFailure::BadCredential)?;
    request.headers_mut().insert(name, value);
    Ok(request)
}

/// Runs one dictation session start-to-finish. Returns a command to carry
/// over to the dispatcher loop (a `Start` that arrived mid-session).
#[allow(clippy::too_many_arguments)]
async fn run_session(
    ctl_tx: &Sender<ControlMsg>,
    key: &SharedKey,
    cleanup: &Arc<RwLock<CleanupSettings>>,
    http: &reqwest::Client,
    rx: &mut UnboundedReceiver<CloudCmd>,
    session: u64,
    cfg: SessionCfg,
    backoff: &mut ConnectBackoff,
    window: &mut crate::format::timing::TimingWindow,
) -> Option<CloudCmd> {
    // Which host every sentence in this function is about. Taken from the
    // lane rather than from the transport because the lane is what was
    // decided at chord-down and cannot change under this dictation, and
    // because the transport is re-resolved mid-loop on a `401`.
    let relay = matches!(cfg.lane, Lane::Cloud { .. });
    // The credential the socket opens with. A signed-out state stops the
    // dictation here, with one notice. The polish calls look the credential
    // up again (`Credential`) and fall back to this one if they cannot.
    let mut transport = match Transport::resolve(&cfg.lane, key).await {
        Ok(transport) => transport,
        Err(message) => return fail_drain(ctl_tx, rx, session, message).await,
    };

    // Classified retry: up to `CONNECT_MAX_ATTEMPTS` real connect attempts
    // for *this* dictation, not just a delay scheduled for the next one.
    // `request` is rebuilt every iteration rather than cloned because
    // `connect_async` consumes it by value. Every failure inside this loop
    // runs through `ConnectBackoff` — `record_failure`'s classification and
    // `backoff.wait()`'s delay are shared between "retry this dictation's
    // own connect" and "wait before the next dictation dials" (see
    // `ConnectBackoff`'s doc comment) — so a non-transient failure (401/403,
    // a corrupted key, DNS blocked) exits on the very first attempt:
    // `continue` only ever follows a `transient` classification *and* a
    // remaining attempt.
    let mut attempt: u32 = 0;
    // The relay gets one silent re-authentication per dictation and no more:
    // a `401` that survives a fresh token is a sign-in the user
    // has to redo, and retrying it in a loop would only spend their battery
    // saying so.
    let mut reauthed = false;
    let ws = loop {
        attempt += 1;
        let request = match upgrade_request(&cfg, &transport) {
            Ok(r) => r,
            Err(UpgradeFailure::BadUrl) => {
                return fail_drain(ctl_tx, rx, session, msg_unreachable(relay)).await;
            }
            Err(UpgradeFailure::BadCredential) => {
                let message = if transport.is_relay() {
                    MSG_CLOUD_SIGN_IN.to_string()
                } else {
                    "Your saved Sarvam key looks corrupted — re-enter it".to_string()
                };
                return fail_drain(ctl_tx, rx, session, message).await;
            }
        };

        // A no-op unless a failure — this dictation's own previous attempt,
        // or the previous dictation's last one — just scheduled a pending
        // delay. See `ConnectBackoff`'s doc comment for why this sits here,
        // before the attempt, rather than wrapped around it.
        backoff.wait().await;

        match tokio::time::timeout(CONNECT_TIMEOUT, tokio_tungstenite::connect_async(request)).await
        {
            Ok(Ok((ws, _resp))) => {
                backoff.record_success();
                break ws;
            }
            Ok(Err(WsError::Http(resp))) => {
                let status = resp.status().as_u16();
                let verdict = upgrade_verdict(status, relay, reauthed);
                let transient = matches!(verdict, Verdict::Retry(_));
                backoff.record_failure(transient);
                match verdict {
                    // Cloud mode's one extra case. A `401` from the relay
                    // means the *sign-in* was not accepted, which a refresh
                    // can fix — the app's Sarvam key is not involved and
                    // cannot be at fault, because there isn't one. One
                    // forced refresh, one retry (which spends an attempt
                    // from the same budget, so this never adds a third
                    // connect), and then the sentence.
                    Verdict::Reauth => {
                        reauthed = true;
                        match Transport::resolve_fresh(&cfg.lane, key).await {
                            // A refresh that came back with the same bearer
                            // has nothing new to present, and dialing again
                            // with it would only collect a second `401` at
                            // the cost of another round trip on a user who
                            // is already waiting. Say it now.
                            Ok(same) if same.same_credential(&transport) => {
                                tracing::info!(
                                    attempt,
                                    "the relay rejected the sign-in and the refresh changed nothing"
                                );
                                return fail_drain(
                                    ctl_tx,
                                    rx,
                                    session,
                                    MSG_CLOUD_SIGN_IN.to_string(),
                                )
                                .await;
                            }
                            Ok(refreshed) => {
                                transport = refreshed;
                                tracing::info!(
                                    attempt,
                                    "the relay rejected the sign-in; retrying once with a fresh token"
                                );
                                continue;
                            }
                            // The refresh itself failed, and its sentence is
                            // the better one: "you are offline" must not be
                            // reported as "sign in again".
                            Err(message) => return fail_drain(ctl_tx, rx, session, message).await,
                        }
                    }
                    Verdict::Retry(msg) => {
                        if should_retry_connect(attempt, true) {
                            tracing::info!(attempt, status, "retrying realtime connect after a transient HTTP failure");
                            continue;
                        }
                        return fail_drain(ctl_tx, rx, session, msg).await;
                    }
                    Verdict::Fail(msg) => return fail_drain(ctl_tx, rx, session, msg).await,
                }
            }
            Ok(Err(e)) => {
                let failure = net_error::classify_ws_error(&e);
                tracing::warn!("realtime connect failed ({failure:?}): {e}");
                let transient = failure.is_retryable();
                backoff.record_failure(transient);
                if should_retry_connect(attempt, transient) {
                    tracing::info!(attempt, ?failure, "retrying realtime connect after a transient failure");
                    continue;
                }
                return fail_drain(
                    ctl_tx,
                    rx,
                    session,
                    failure.user_message_for(host_name(relay)),
                )
                .await;
            }
            Err(_) => {
                // Our own `CONNECT_TIMEOUT` elapsed with nothing to inspect —
                // no status, no inner error — so, like any unclassified
                // network failure, it counts as transient.
                backoff.record_failure(true);
                if should_retry_connect(attempt, true) {
                    tracing::info!(attempt, "retrying realtime connect after a timeout");
                    continue;
                }
                return fail_drain(
                    ctl_tx,
                    rx,
                    session,
                    NetFailure::Timeout.user_message_for(host_name(relay)),
                )
                .await;
            }
        }
    };
    // The polish calls ask for the credential again rather than reuse this
    // one: see `Credential`.
    let credential: Credential = {
        let lane = cfg.lane.clone();
        let key = key.clone();
        Arc::new(move || {
            let (lane, key) = (lane.clone(), key.clone());
            Box::pin(async move { Transport::resolve(&lane, &key).await })
        })
    };
    drain_session(
        ctl_tx,
        cleanup,
        http,
        rx,
        session,
        cfg,
        transport,
        credential,
        ws,
        batch::BATCH_URL,
        window,
    )
    .await
}

/// Everything after the socket is open: stream, drain, resolve, polish,
/// report. Its own function so a test can hand it a socket of its own
/// making — a scripted loopback server — without dialing the relay or
/// resolving a credential, which on the Cloud lane would read the machine's
/// real sign-in. `batch_url` is where the batch rescue goes
/// (`batch::BATCH_URL` outside tests), for the same reason.
#[allow(clippy::too_many_arguments)]
async fn drain_session(
    ctl_tx: &Sender<ControlMsg>,
    cleanup: &Arc<RwLock<CleanupSettings>>,
    http: &reqwest::Client,
    rx: &mut UnboundedReceiver<CloudCmd>,
    session: u64,
    cfg: SessionCfg,
    transport: Transport,
    credential: Credential,
    ws: WsStream,
    batch_url: &str,
    window: &mut crate::format::timing::TimingWindow,
) -> Option<CloudCmd> {
    // Same derivation, and the same reason, as in `run_session`.
    let relay = matches!(cfg.lane, Lane::Cloud { .. });
    let (mut ws_tx, mut ws_rx) = ws.split();

    let mut begun = false;
    let mut pending_audio: Vec<Vec<f32>> = Vec::new();
    let mut finals: BTreeMap<u64, String> = BTreeMap::new();
    // Closed sentences are polished while the user is still speaking.
    let mut segmenter = crate::sarvam::incremental::Segmenter::default();
    let mut chunk_tx: Option<tokio::sync::mpsc::UnboundedSender<ChunkJob>> = None;
    let mut chunk_worker: Option<
        tokio::task::JoinHandle<Vec<crate::sarvam::incremental::PolishedChunk>>,
    > = None;
    let mut partial = String::new();
    let mut finish_req: Option<u64> = None;
    // Deadlines arm once the finish frames are on the wire (or once Finish
    // arrives while we're still waiting for session.begin — that wait must
    // be bounded too).
    let mut hard_deadline: Option<Instant> = None;
    let mut quiet_deadline: Option<Instant> = None;
    // Set when the drain ended abnormally; used instead of "Didn't catch
    // that" when we salvaged nothing.
    let mut drain_error: Option<String> = None;
    // Origin of `drain_ms`: the controller's own end-of-speech instant,
    // carried in on `CloudCmd::Finish` (`end_of_speech`) rather than
    // stamped here at receipt time. `Controller::finish_recording` calls
    // `collect_tail` (blocking up to `TAIL_FLUSH_TIMEOUT_MS` = 250 ms to
    // drain queued audio) *before* it sends `CloudCmd::Finish`, so the
    // receipt time would leave up to 250 ms of real post-release latency out
    // of every session. Latency is measured from end of speech, not from
    // whenever our own controller got round to telling us. Set once,
    // unconditionally, at the top of the `CloudCmd::Finish` arm below
    // (converted from the controller's `std::time::Instant` via
    // `Instant::from_std`), and never reassigned afterwards. Stays `None`
    // until `Finish` is received; every reachable `break` below is
    // downstream of that arm, so `drain_start` is always `Some` by the time
    // `log_timing_sample` runs (the defensive `None` guard inside it is a
    // bug-detector, not a real code path — see its doc comment). Three places
    // end a session *after* `finish_req`/`drain_start` are set without ever
    // reaching `log_timing_sample` (`fail_drain`'s own `Finish` arm, and this
    // loop's `Cancel`/channel-closed and `Start`-carryover arms below); those
    // call `log_timing_skipped` directly, so the skip count covers them.
    let mut drain_start: Option<Instant> = None;
    // Set from `CloudCmd::Finish`'s own `duration_ms` (see its doc comment) the
    // same instant `drain_start` is. Used twice below: scaling the finals-drain
    // wait (`scaled_flush_wait`) and gating the batch-fallback attempt on
    // utterances longer than `batch::MIN_FALLBACK_DURATION_MS`.
    let mut finish_duration_ms: Option<u64> = None;
    // Tee of every audio chunk this session forwards to Sarvam — kept only so a
    // session that ends with nothing usable has something to hand
    // `batch::transcribe`. Capped at `MAX_FALLBACK_SAMPLES` (see
    // `tee_fallback_pcm`): past that the gate would refuse to send it anyway.
    // The speech gate has already run by the time any of this session's
    // commands exist at all (`Controller::finish_recording` only ever sends
    // `CloudCmd::Finish` after `speech_gate::decide` returns `Speech`), so
    // there is no separate silence check to repeat here — a silence-cancelled
    // utterance never reaches this dispatcher in the first place, let alone
    // this buffer.
    let mut fallback_pcm: Vec<f32> = Vec::new();
    // Set the instant the socket proves dead *before* `Finish` arrives (a
    // send timeout, a fatal error frame, an early `session.end`, a close, a
    // stream error/end) — holds the user-facing message that failure would
    // have reported. Once set: stop touching the socket entirely (no more
    // sends, no more polling `ws_rx` — see the `if ws_dead.is_none()` guard
    // below) and just keep tee-ing audio into `fallback_pcm` as normal,
    // exactly as if streaming were still live, because from here the mic and
    // the user are still going and every remaining chunk is still real audio
    // to salvage. `CloudCmd::Finish`'s own arm below is what actually ends the
    // session once it arrives (`ws_dead.take()`), by `break`-ing straight into
    // the unified batch-fallback → rules → polish → guardrail pipeline every
    // other drain-ending path already goes through — never a second injection
    // path.
    //
    // That holds only while a rescue is left (`rescue_left`). When none is,
    // the top of the loop stops the recording at once through
    // `ControlMsg::CloudEnded`, whose `Finish` comes back through the same
    // arm — on Bring your own key always, since the tee then still fits the
    // rescue, and on the relay lane when words are in hand. A relay session
    // with no words ends through `fail_drain`.
    let mut ws_dead: Option<String> = None;
    // Set once `CloudEnded` has gone to the controller, so it goes once.
    let mut stop_asked = false;
    // Set when the drain stopped at a deadline rather than on `session.end`:
    // nothing then says every final is in.
    let mut ended_at_deadline = false;
    // Set when Saaras answers our `end` with `session.end` — the normal
    // exit. Read after the loop to skip the goodbye: the session is
    // already over, and a second `end` on a half-closed socket is at best a
    // wasted write and at worst a `GOODBYE_TIMEOUT` stall.
    let mut session_ended = false;
    // Set when the relay closed the socket with one of its two limit codes
    // (`relay_ended_on_purpose`). The finals that arrived before it are then
    // delivered with the close's own sentence rather than withheld as a
    // truncation — see the outcome below.
    let mut relay_ended = false;

    loop {
        // The socket died before the user finished and nothing can rescue
        // what they say next: stop the recording now rather than let them
        // talk on into it.
        if let Some(msg) = &ws_dead {
            if finish_req.is_none() && !stop_asked && !rescue_left(relay, fallback_pcm.len()) {
                // The relay lane with no words in hand: nothing to deliver
                // and nothing to rescue with.
                if relay && finals.is_empty() && partial.is_empty() {
                    return fail_drain(ctl_tx, rx, session, msg.clone()).await;
                }
                // Words are in hand, or this is Bring your own key, where the
                // batch rescue can still take the whole tee: the controller
                // stops the recording as a released key would, and its
                // `Finish` ends this session in the arm below.
                let _ = ctl_tx.send(ControlMsg::CloudEnded { session });
                stop_asked = true;
            }
        }
        let deadline = match (quiet_deadline, hard_deadline) {
            (Some(q), Some(h)) => Some(q.min(h)),
            (q, h) => q.or(h),
        };
        tokio::select! {
            cmd = rx.recv() => match cmd {
                Some(CloudCmd::Audio(chunk)) => {
                    if finish_req.is_some() {
                        continue; // utterance already closed; drop the stray
                    }
                    // Tee'd before the send/queue below borrows or moves
                    // `chunk` — see `fallback_pcm`'s declaration above.
                    tee_fallback_pcm(&mut fallback_pcm, &chunk);
                    if ws_dead.is_some() {
                        // Already known dead (see `ws_dead`'s declaration) —
                        // the chunk is tee'd above like any other; attempting
                        // another send here would just burn `SEND_TIMEOUT`
                        // per chunk on a socket that cannot possibly answer.
                        continue;
                    }
                    if begun {
                        let frame = ClientMsg::AudioInput { audio: &codec::f32_to_pcm16_b64(&chunk) }.to_json();
                        if timed_send(&mut ws_tx, Message::Text(frame)).await.is_err() {
                            ws_dead = Some(msg_lost(relay));
                        }
                    } else {
                        pending_audio.push(chunk);
                    }
                }
                Some(CloudCmd::Finish { req_id, end_of_speech, duration_ms }) => {
                    finish_req = Some(req_id);
                    // End of speech, per the controller's own clock, not
                    // this arm's receipt time — see `drain_start`'s
                    // declaration above.
                    drain_start = Some(Instant::from_std(end_of_speech));
                    finish_duration_ms = Some(duration_ms);
                    if let Some(msg) = ws_dead.take() {
                        // The socket already proved dead earlier in this same
                        // session — every audio chunk since has gone straight
                        // into `fallback_pcm` with nowhere to send it. Skip
                        // straight to the unified pipeline below (batch
                        // fallback, then rules → polish → guardrail) instead of
                        // wasting `send_finish`'s own `SEND_TIMEOUT` on a
                        // socket that cannot answer.
                        drain_error = Some(msg);
                        break;
                    }
                    if begun {
                        if send_finish(&mut ws_tx, cfg.endpointing).await.is_err() {
                            drain_error = Some(msg_lost(relay));
                            break;
                        }
                        // `end` always brings `session.end` after the last
                        // final, however long that takes on a slow link, so
                        // the drain waits for it up to this bound.
                        hard_deadline = Some(Instant::now() + scaled_flush_wait(duration_ms));
                    } else {
                        // Still waiting for session.begin — bound that wait
                        // so a silent server can't outlive the watchdog.
                        // Flat, not scaled: this is the handshake, not a
                        // drain (see `FLUSH_WAIT_FLOOR`'s doc comment).
                        hard_deadline = Some(Instant::now() + FLUSH_WAIT_FLOOR);
                    }
                }
                Some(CloudCmd::Cancel) | None => {
                    // `finish_req.is_some()` here is not hypothetical: it's
                    // exactly what happens when `ControlMsg::FinalizeTimeout`
                    // (`Controller::handle`) aborts a session that's taking too
                    // long by sending `Cancel` for a session already past
                    // `Finish`. That's a real dictation the user waited on and
                    // never got a result for, so it must be counted rather than
                    // silently vanish — this is the only place this session
                    // ever terminates on this path. `finish_req.is_none()`
                    // means the user cancelled (or the app shut down) before
                    // finishing speaking at all: nothing was ever going to be
                    // measured, so there's nothing to count as skipped either.
                    if finish_req.is_some() {
                        log_timing_skipped("drain aborted (cancelled or shut down)");
                    }
                    // The dictation is being discarded, so anything still
                    // being polished in the background is wasted spend.
                    if let Some(w) = chunk_worker.take() {
                        w.abort();
                    }
                    send_goodbye(&mut ws_tx).await;
                    return None;
                }
                Some(start @ CloudCmd::Start { .. }) => {
                    tracing::warn!("new dictation started while a session was active; dropping the old one");
                    // Defensive, same reasoning as `Cancel` above: the
                    // controller's documented protocol (`Start (Audio*)
                    // (Finish | Cancel)`, see `CloudCmd`'s doc comment) plus
                    // this channel's FIFO delivery should mean a live
                    // `Start` can't overtake a `Finish` already sent for the
                    // session still running here — but if that ever stops
                    // holding, an abandoned mid-drain session must still be
                    // counted, not silently dropped.
                    if finish_req.is_some() {
                        log_timing_skipped("drain abandoned (new dictation started)");
                    }
                    // Same as `Cancel` above: this session's text is never
                    // going anywhere, so its background chunks are waste.
                    if let Some(w) = chunk_worker.take() {
                        w.abort();
                    }
                    return Some(start);
                }
            },
            // Stop polling once the socket is known dead (see `ws_dead`'s
            // declaration) — it has either already errored/closed once (so
            // polling a spent stream again is at best redundant, at worst
            // relies on fusing behaviour this stream doesn't promise) or a
            // send failed while the read half is presumably just as gone;
            // either way there is nothing left to read until `Finish` ends
            // this session from the `rx.recv()` arm above instead.
            frame = ws_rx.next(), if ws_dead.is_none() => match frame {
                Some(Ok(Message::Text(text))) => match codec::parse_server(&text) {
                    Some(ServerMsg::SessionBegin) => {
                        begun = true;
                        let mut ok = true;
                        if cfg.endpointing == Endpointing::Manual {
                            ok &= timed_send(&mut ws_tx, Message::Text(ClientMsg::SpeechStart.to_json())).await.is_ok();
                        }
                        for chunk in pending_audio.drain(..) {
                            let frame = ClientMsg::AudioInput { audio: &codec::f32_to_pcm16_b64(&chunk) }.to_json();
                            ok &= timed_send(&mut ws_tx, Message::Text(frame)).await.is_ok();
                        }
                        if ok && finish_req.is_some() {
                            // `drain_start` was already stamped from the
                            // controller's end-of-speech instant when
                            // `Finish` arrived (see its declaration) — not
                            // reset here just because the finish frame is
                            // only now reaching the wire.
                            ok &= send_finish(&mut ws_tx, cfg.endpointing).await.is_ok();
                            // Fresh drain budget now that the finish frames
                            // are out, scaled the same way the `begun` branch
                            // of the `Finish` arm above does —
                            // `finish_duration_ms` is always `Some` here
                            // (`finish_req.is_some()` is only true once that
                            // arm has already run).
                            hard_deadline =
                                Some(Instant::now() + scaled_flush_wait(finish_duration_ms.unwrap_or(0)));
                        }
                        if !ok {
                            if finish_req.is_some() {
                                drain_error = Some(msg_lost(relay));
                                break;
                            }
                            ws_dead = Some(msg_lost(relay));
                        }
                    }
                    Some(ServerMsg::TranscriptPartial { text }) => {
                        partial = text;
                        // Speech is still being finalized — don't let a
                        // quiet window cut it off; the hard deadline still
                        // bounds the wait.
                        if hard_deadline.is_some() {
                            quiet_deadline = None;
                        }
                    }
                    Some(ServerMsg::TranscriptFinal { utterance_idx, text }) => {
                        finals.insert(utterance_idx, text);
                        partial.clear();
                        // A sentence that closed far enough back is
                        // polished now, while the user is still speaking, so
                        // that only the tail is on the critical path once the
                        // key is released. Never after `Finish`: from there
                        // every remaining final belongs to the tail.
                        if finish_req.is_none() {
                            let toggles = cleanup.read().expect("cleanup lock").clone();
                            if toggles.level != crate::format::level::CleanupLevel::Off {
                                let joined = assemble(&finals, "");
                                // How this chunk meets the one before it,
                                // read before the cut that makes it.
                                let seam = segmenter.seam();
                                if let Some(chunk) = segmenter.take_chunk(&joined) {
                                    if chunk_tx.is_none() {
                                        // Resolved from this session's own
                                        // lane and credential, so the
                                        // background chunks go to the same
                                        // host as the tail polish that
                                        // follows them.
                                        if crate::endpoint::resolve_polish_backend_for(
                                            &transport,
                                            &toggles.polish_model,
                                        )
                                        .is_ok()
                                        {
                                            let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
                                            chunk_worker = Some(spawn_chunk_worker(
                                                http.clone(),
                                                credential.clone(),
                                                transport.clone(),
                                                toggles.clone(),
                                                rx,
                                            ));
                                            chunk_tx = Some(tx);
                                        }
                                    }
                                    match &chunk_tx {
                                        Some(tx) => {
                                            let _ = tx.send(ChunkJob {
                                                raw: chunk,
                                                seam,
                                                ends_mid_sentence: segmenter.seam()
                                                    == chat::Seam::MidSentence,
                                            });
                                        }
                                        // No backend: the chunk stays part of the
                                        // tail (take_chunk already advanced; put
                                        // it back by resetting the segmenter —
                                        // simplest correct move).
                                        None => segmenter = Default::default(),
                                    }
                                }
                            }
                        }
                        if hard_deadline.is_some() {
                            quiet_deadline = Some(Instant::now() + QUIET_WINDOW);
                        }
                    }
                    Some(ServerMsg::Error { code, is_fatal, message }) => {
                        tracing::warn!("sarvam error frame (fatal={is_fatal}, code={code:?}): {message}");
                        if is_fatal {
                            if finish_req.is_some() {
                                drain_error = Some(msg_service_error(relay));
                                break;
                            }
                            ws_dead = Some(msg_service_error(relay));
                        }
                    }
                    Some(ServerMsg::SessionEnd) => {
                        if finish_req.is_some() {
                            session_ended = true;
                            break;
                        }
                        ws_dead = Some(msg_session_ended(relay));
                    }
                    _ => {}
                },
                Some(Ok(Message::Close(frame))) => {
                    relay_ended = relay_ended_on_purpose(frame.as_ref(), relay);
                    let msg = close_message(frame, relay);
                    if finish_req.is_some() {
                        drain_error = Some(msg);
                        break;
                    }
                    ws_dead = Some(msg);
                }
                Some(Ok(_)) => {} // binary/ping/pong — tungstenite answers pings itself
                Some(Err(e)) => {
                    tracing::warn!("realtime stream error: {e}");
                    if finish_req.is_some() {
                        drain_error = Some(msg_lost(relay));
                        break;
                    }
                    ws_dead = Some(msg_lost(relay));
                }
                None => {
                    if finish_req.is_some() {
                        drain_error = Some(msg_lost(relay));
                        break;
                    }
                    ws_dead = Some(msg_lost(relay));
                }
            },
            _ = sleep_until_opt(deadline), if deadline.is_some() => {
                ended_at_deadline = true;
                break;
            }
        }
    }

    // Drain finished. Sample `drain_ms` immediately — before the goodbye
    // write below, which can block up to `GOODBYE_TIMEOUT` on a stalled
    // connection (exactly the condition that produces a slow session).
    // Charging our own courtesy `end` to Sarvam's drain would flatter the
    // number precisely when it matters most.
    let drain_dur = drain_start.map(|s| s.elapsed());

    // End the session politely (no need to wait for the close handshake)
    // unless Saaras already ended it in answer to our finish frames.
    if !session_ended {
        send_goodbye(&mut ws_tx).await;
    }

    // Timed out before the session even began: the server never became
    // usable, which deserves an honest error over "Didn't catch that".
    if !begun && drain_error.is_none() {
        drain_error = Some(msg_unreachable(relay));
    }
    // Stopped at a deadline once the session was running: `session.end`
    // never came, so the last words may still have been on their way.
    let end_unconfirmed = ended_at_deadline && drain_error.is_none();

    let req_id = finish_req.unwrap_or(0);
    let mut raw = assemble(&finals, &partial);

    // Nothing usable from the realtime session — three paths land here, and
    // `realtime_transcript_stands_alone` is the single predicate for all of
    // them: the "fail" path (`drain_error` set: the connection dropped
    // mid-utterance or mid-drain), the "Didn't catch that" path (a clean
    // session that simply produced no finals/partial) — the trigger does not
    // care *why* streaming produced nothing — and a failed drain that DID
    // salvage some finals, because those finals stop wherever the socket
    // died and the batch fallback is the only thing that can re-transcribe
    // the whole utterance.
    //
    // Still exactly one attempt, and still never a second injection path:
    // `raw` is simply replaced below, so every line after it (rules → polish
    // → guardrail) runs exactly as it would for a realtime result, unaware a
    // fallback ever happened.
    let errored = drain_error.is_some();
    let has_realtime_text = !raw.trim().is_empty();
    let mut fallback_text: Option<String> = None;
    let batch_key = batch_credential(&transport);
    if !realtime_transcript_stands_alone(errored, has_realtime_text) {
        let duration_ms = finish_duration_ms.unwrap_or(0);
        if let (Some(api_key), true) = (
            batch_key,
            should_attempt_batch_fallback(duration_ms, !fallback_pcm.is_empty()),
        ) {
            tracing::info!(
                duration_ms,
                pcm_samples = fallback_pcm.len(),
                errored,
                // A word count, never the words — see this file's logging
                // policy (`resolve_format_inner`, `sarvam::codec`).
                realtime_words = raw.split_whitespace().count(),
                "realtime transcript unusable; attempting one batch fallback"
            );
            if let Some(transcript) = batch::transcribe(
                http,
                batch_url,
                api_key,
                &cfg,
                &fallback_pcm,
                batch::BATCH_TIMEOUT,
            )
            .await
            {
                if !transcript.trim().is_empty() {
                    fallback_text = Some(transcript);
                }
            }
        }
    }
    let mut outcome = resolve_drain_outcome(errored, has_realtime_text, fallback_text.is_some());
    // The relay ending a session on purpose is not a dead socket. The finals
    // stop where the relay stopped transcribing, deliberately and for a
    // reason the notice will name, and no rescue could extend them (the
    // relay proxies no batch route) — so withholding them would only take
    // away words the user already spent their allowance on. They are
    // delivered like any other transcript, and the close's sentence goes
    // with them. With no finals at all nothing changes: the notice alone.
    let ended_by_relay = relay_ended.then(|| drain_error.clone()).flatten();
    let cut_short = ended_by_relay.is_some();
    if ended_by_relay.is_some() && outcome == DrainOutcome::Truncated {
        outcome = DrainOutcome::Realtime;
    }
    // Captured before `fallback_text` is consumed below: the background
    // chunks describe the realtime transcript, so a fallback that replaces
    // it invalidates every one of them.
    let fallback_text_was_none = fallback_text.is_none();
    if let Some(transcript) = fallback_text {
        // `resolve_drain_outcome` returns `Fallback` for exactly this
        // condition, so the replacement and the outcome cannot disagree.
        debug_assert_eq!(outcome, DrainOutcome::Fallback);
        raw = transcript;
    }

    let toggles = cleanup.read().expect("cleanup lock").clone();
    let (mut text, dict_fixes) = crate::cleanup::run_cloud_pipeline(raw.clone(), &toggles);
    let mut notice: Option<String> = None;
    // The chat call plus the guardrail check below. Stays `Duration::ZERO`
    // — a real measurement, not a missing one — when formatting is skipped
    // entirely (empty drain, or level Off).
    let mut format_dur = Duration::ZERO;
    // The streaming polish path's time to first token, when one arrived.
    // Stays `None` whenever `format_dur` stays zero, and also when the
    // polish call failed or ran through the non-streaming custom backend —
    // the log line renders it as `ttft_ms=0`.
    let mut ttft: Option<Duration> = None;
    // The dictionary hits of the tail's own rule pass, when only the tail is
    // polished. Stays `0` on the single-call path, where `dict_fixes` above
    // already counted the whole text.
    let mut tail_dict_fixes: u32 = 0;
    // A "new paragraph" spoken right at the last seam: the tail's own `tidy`
    // pass trims it off the tail's front, so the join has to carry it.
    // `None` on the single-call path, which has no seam at all.
    let mut tail_leading_break: Option<&'static str> = None;
    // If chunks were polished in the background AND the realtime transcript
    // is what we are pasting (no batch fallback replaced it, and the
    // segmenter still describes it), only the tail goes through the model
    // now. Otherwise this is the single call over the whole text.
    //
    // Collected here, ahead of the polish branch below, so that a worker that
    // was lost falls back to the whole-text path — and so that the handle is
    // aborted rather than silently detached on the error return inside it.
    let chunks_taken = segmenter.chunks_taken();
    // The finals alone, in the same shape the segmenter was fed while they
    // arrived: what both `in_sync` and `tail` are measured against.
    let joined_finals = assemble(&finals, "");
    let realtime_text_stands = fallback_text_was_none && segmenter.in_sync(&joined_finals);
    drop(chunk_tx.take()); // closes the channel: the worker finishes its queue and returns
    let mut background: Vec<crate::sarvam::incremental::PolishedChunk> = Vec::new();
    // The one part of the background work that is not free: a chunk still in
    // flight when the key goes up delays the tail call. Measured only when
    // this actually waits; `Duration::ZERO` otherwise is a real measurement.
    let mut chunk_wait = Duration::ZERO;
    // AI Polish flipped to `Off` mid-dictation: the chunks are discarded
    // below, so there is nothing to wait for either. Part of this gate and
    // not only of `use_background_chunks`, or the user would sit through up
    // to `POLISH_TIMEOUT + 1 s` for text that is thrown away on arrival.
    let level_on = toggles.level != crate::format::level::CleanupLevel::Off;
    if let Some(mut worker) = chunk_worker.take() {
        if chunks_taken > 0 && realtime_text_stands && level_on {
            let wait_start = Instant::now();
            // `&mut worker`, not `worker`: a `JoinHandle` is `Unpin` and a
            // `Future` by `&mut`, so the handle survives the timeout and the
            // `Err` arm can actually abort the task. Passing it by value gave
            // the handle to `timeout`, which dropped it — and dropping a
            // `JoinHandle` detaches the task rather than cancelling it, so an
            // overrunning worker kept running (and kept calling Sarvam) for
            // the rest of the process's life.
            match tokio::time::timeout(
                chat::POLISH_TIMEOUT + Duration::from_secs(1),
                &mut worker,
            )
            .await
            {
                Ok(Ok(result)) => background = result,
                // Never `{e}`: a `JoinError`'s `Display` can carry the
                // panic payload, and a panic inside the worker can name
                // transcript text. Two booleans say everything this line
                // needs to.
                Ok(Err(e)) => tracing::warn!(
                    panicked = e.is_panic(),
                    cancelled = e.is_cancelled(),
                    "chunk worker did not return a result"
                ),
                Err(_) => {
                    worker.abort();
                    tracing::warn!("chunk worker did not finish within the polish budget");
                }
            }
            chunk_wait = wait_start.elapsed();
        } else {
            worker.abort();
        }
    }
    let use_chunks = use_background_chunks(
        chunks_taken,
        fallback_text_was_none,
        background.len(),
        segmenter.in_sync(&joined_finals),
        level_on,
    );
    if chunks_taken > 0 && !use_chunks {
        // Counts only — never the text (see `resolve_format_inner`).
        tracing::warn!(
            chunks_taken,
            chunks_returned = background.len(),
            "background chunks discarded; polishing the whole text"
        );
        background.clear();
    }
    if text.is_empty() {
        if let Some(msg) = drain_error {
            // Timed before the `ctl_tx` send below, mirroring the normal
            // end-of-function call site: `StageTimings::total_ms` is
            // documented to exclude "the send to the controller".
            // `errored: true` unconditionally — reaching this branch at all
            // means `drain_error` was `Some`, and this sample must not read
            // as a clean one.
            log_timing_sample(
                drain_start,
                drain_dur,
                format_dur,
                None,
                raw.split_whitespace().count(),
                true,
                // Nothing was polished at all on this path, background or
                // otherwise — see `use_chunks` above, which is false here by
                // construction. An empty `text` means either an empty
                // transcript or one that was nothing but spoken commands
                // ("new paragraph", "delete that"), which the rule pipeline
                // consumes; either way the finals never reached the 50 words
                // a chunk needs, so `chunks_taken` is 0.
                0,
                // Zero by the same construction: with no chunks taken, the
                // block above never waited on a worker.
                chunk_wait,
                window,
            );
            let _ = ctl_tx.send(ControlMsg::CloudError {
                req_id,
                session,
                message: msg,
            });
            return None;
        }
    } else if toggles.level != crate::format::level::CleanupLevel::Off {
        let format_start = Instant::now();
        // The resolver can now refuse outright (it stops degrading to this
        // dictation's own host when the custom endpoint is also doing the
        // STT — `endpoint::resolve`). That cannot normally describe a
        // session running *here*: this code path is the realtime socket,
        // already authenticated on this dictation's lane, and that lane is
        // what the polish degrades to. The `Err` arm covers the one race that
        // reaches it — the slot flipped to `use_for_stt` mid-utterance — and
        // costs the user nothing: an unreachable formatter is already a
        // first-class outcome on this path, so it takes the `Failed` road
        // with its own notice.
        let polish_backend = fresh_backend(&credential, &transport, &toggles.polish_model).await;
        // What the model sees now: the whole rule-cleaned text (the single
        // call), or only the tail — everything before it was already polished
        // in the background while the user was speaking. The tail gets its own
        // rule pass, since the whole-text `text` above is not what is going to
        // the model on that path.
        let (model_input_raw, model_input) = if use_chunks {
            let tail_raw = segmenter.tail(&joined_finals, &partial);
            // Off the raw text, before the pipeline that deletes it — and
            // under the same toggle that pipeline reads.
            tail_leading_break = seam_breaks(&tail_raw, toggles.spoken_commands).0;
            let (tail_rules, hits) = crate::cleanup::run_cloud_pipeline(tail_raw.clone(), &toggles);
            tail_dict_fixes = hits;
            (tail_raw, tail_rules)
        } else {
            (raw.clone(), text.clone())
        };
        // The text before the cursor: the chunks already polished, bounded by
        // `incremental::CONTEXT_MAX_CHARS`. `None` on the single-call path,
        // which therefore sends exactly the turns it always has.
        let context = if use_chunks {
            Some(crate::sarvam::incremental::context_tail(
                &crate::sarvam::incremental::assemble_polished(&background, ""),
            ))
        } else {
            None
        };
        let outcome = match (&polish_backend, model_input.is_empty()) {
            // Nothing left to polish: every closed sentence already went
            // through the model in the background and the key was released
            // with no tail behind them.
            (_, true) => None,
            (Ok(backend), false) => Some(match &context {
                Some(ctx) => {
                    chat::polish_with_context(
                        http,
                        backend,
                        &model_input,
                        ctx,
                        segmenter.seam(),
                        &toggles.dictionary,
                        &toggles.level.prompt(),
                        // The rules the user saved on the Prompts page for this level,
                        // if any — resolved into the snapshot at
                        // `From<&Settings>` so this path reads one field
                        // instead of reaching into settings.
                        toggles.prompt_rules.as_deref(),
                    )
                    .await
                }
                None => {
                    chat::polish(
                        http,
                        backend,
                        &model_input,
                        &toggles.dictionary,
                        &toggles.level.prompt(),
                        toggles.prompt_rules.as_deref(),
                    )
                    .await
                }
            }),
            // A reason, never a URL or a transcript — `Unavailable`'s
            // `Debug` is two fieldless enums deep.
            (Err(why), false) => Some(chat::PolishOutcome::Failed(
                format!("no chat backend ({why:?})").into(),
            )),
        };
        // Whatever the model does not replace: the rule-cleaned input it saw.
        let mut tail_text = model_input.clone();
        if let Some(outcome) = outcome {
            // `PolishOutcome::Failed` carries a reason (see `chat::polish`);
            // log it so a dead model shows up instead of hiding as "AI Polish
            // does nothing".
            if let Some(reason) = outcome.reason() {
                tracing::warn!("cloud polish failed ({reason}); using raw text");
            }
            let limit_spent = outcome.weekly_limit_spent();
            match outcome {
                chat::PolishOutcome::Formatted(reply) => {
                    tracing::debug!(
                        prompt_tokens = reply.prompt_tokens,
                        completion_tokens = reply.completion_tokens,
                        "cloud polish token usage"
                    );
                    ttft = reply.first_token_ms.map(|ms| Duration::from_millis(u64::from(ms)));
                    // The guardrail checks the reply against the rule-cleaned
                    // input the model actually saw, not the verbatim
                    // transcript — the rule pipeline already applied spoken
                    // commands, snippets/replacements and tidy before the
                    // model ran, and charging the model for those edits
                    // produces false rejections (see `format::guard::check`'s
                    // doc comment). The raw form is threaded through only so
                    // a rejection's log line can report its word count —
                    // never its content; this file logs no transcript text
                    // (see `resolve_format_inner`).
                    let (resolved, note) =
                        resolve_format(&model_input_raw, &model_input, Some(&reply), toggles.level);
                    tail_text = resolved;
                    notice = note;
                }
                // The rule-cleaned input survives untouched; the failure still
                // needs to reach the user via the same notice path a guardrail
                // rejection uses, or a dead model goes unnoticed.
                chat::PolishOutcome::Failed(_) => {
                    notice = Some(
                        match polish_backend {
                            // The relay refused because the week's chat calls
                            // are spent: nothing about formatting is broken,
                            // and no retry helps until the week rolls over.
                            _ if limit_spent => MSG_CLOUD_QUOTA,
                            // Named, not generic: "formatting failed" would send
                            // this user looking at the model when the fix is the
                            // endpoint's URL.
                            Err(crate::endpoint::Unavailable::CustomEndpoint(_)) => {
                                MSG_CUSTOM_UNAVAILABLE
                            }
                            _ => MSG_POLISH_FAILED,
                        }
                        .to_string(),
                    );
                }
            };
        }
        if use_chunks {
            // The same deterministic seam repair the background chunks get —
            // the last seam is no different from the others.
            // Never on the single-call path below: there is no seam there,
            // and that path stays byte for byte what it has always been.
            let (repaired, repair) = crate::sarvam::incremental::repair_seam(
                &crate::sarvam::incremental::assemble_polished(&background, ""),
                &tail_text,
                // What the model was given for the tail: a run that is in the
                // reply *and* in this is the user's own repetition, not an
                // echo, and must survive the strip.
                &model_input,
                segmenter.seam(),
            );
            tail_text = repaired;
            tracing::debug!(
                echoed_words = repair.echoed_words,
                capitalised = repair.capitalised,
                "tail seam repaired"
            );
            text = crate::sarvam::incremental::assemble_polished_with_tail(
                &background,
                &tail_text,
                tail_leading_break,
            );
            // A chunk's notice (rejection or failure) reaches the user once,
            // the same way the tail's does; the tail's wins if both exist.
            //
            // Not the chunk's own wording, though: every notice on this path
            // is phrased for a single call over the whole dictation ("used
            // the plain transcript"), and here the tail polished fine — only
            // part of the text went unformatted. Saying otherwise would send
            // the user looking for a failure that did not happen.
            if notice.is_none() && background.iter().any(|c| c.notice.is_some()) {
                // A chunk the weekly limit refused says so in its own words:
                // "part of this was not formatted" would hide the reason.
                let limit_spent = background
                    .iter()
                    .any(|c| c.notice.as_deref() == Some(MSG_CLOUD_QUOTA));
                notice = Some(
                    if limit_spent { MSG_CLOUD_QUOTA } else { MSG_PARTIAL_POLISH }.to_string(),
                );
            }
        } else {
            text = tail_text;
        }
        format_dur = format_start.elapsed();
    }
    // Possibly missing words outrank anything the polish had to say: the user
    // has to go and check the end of what was pasted.
    if end_unconfirmed && !text.is_empty() {
        notice = Some(msg_end_unconfirmed(relay));
    }
    // The relay's reason for ending the session outranks anything the polish
    // had to say: it is why the transcript stops where it does, and the only
    // one of the two the user has to act on.
    if let Some(reason) = ended_by_relay {
        notice = Some(reason);
    }
    // `errored` was sampled from `drain_error` before either of the two
    // places that consume it by value (the `if let Some(msg) = drain_error`
    // above, which always returns; the `Truncated` branch below), so it is
    // readable here regardless of which path got here. If it is `true`,
    // `text` must be non-empty (the empty + errored combination always
    // returns above instead) — the drain ended in an error but still
    // salvaged usable text, whether that came from the batch fallback or,
    // on the `Truncated` path, from the finals that arrived before the
    // socket died. Passing it through is what lets an aggregator separate a
    // clean sample from a salvaged one, which `skipped` alone can't do.
    // `format_ms` is the critical path only: with chunks, the tail call plus
    // its guard, with the background calls reported as `segments` beside it.
    let segments = if use_chunks { background.len() as u32 } else { 0 };
    log_timing_sample(
        drain_start,
        drain_dur,
        format_dur,
        ttft,
        raw.split_whitespace().count(),
        errored,
        segments,
        chunk_wait,
        window,
    );
    if outcome == DrainOutcome::Truncated {
        // The drain failed, the fallback could not re-transcribe the whole
        // utterance, and what survived is a fragment that stops wherever the
        // socket died. It has been through the same rules → polish → guardrail
        // pipeline every other transcript goes through, so it is worth keeping
        // — but pasting it would hand the user the first half of their sentence
        // with no notice, which is the silent-truncation class this project
        // treats as its worst failure. File it and say so instead;
        // `CloudTruncated`'s own doc comment covers what the controller does
        // with it.
        //
        // `text` is non-empty here by construction: `has_realtime_text` was
        // true (or `outcome` could not be `Truncated`), and an empty `text`
        // with `drain_error` set already returned above.
        let message = drain_error.unwrap_or_else(|| msg_lost(relay));
        tracing::warn!(
            words = text.split_whitespace().count(),
            "drain failed mid-utterance with no usable fallback; filing the partial \
             transcript in History instead of pasting it"
        );
        let _ = ctl_tx.send(ControlMsg::CloudTruncated { req_id, text, raw, message });
        return None;
    }
    // Built after the `Truncated` branch above: `words_changed` diffs two
    // whole transcripts, and the Insights "fixes" card it feeds only ever
    // describes a dictation that was actually injected.
    // With chunks, the whole-text rule pass above was not what produced the
    // text being pasted: each chunk counted its own hits in the background
    // and the tail counted its own here.
    let dict_fixes = if use_chunks {
        background.iter().map(|c| c.dict_fixes).sum::<u32>() + tail_dict_fixes
    } else {
        dict_fixes
    };
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
        cut_short,
    });
    None
}

/// The per-dictation structured line for a dictation that *was* measured
/// (plus, every tenth call, a rolling summary line — see `window` below):
/// the drain ran to completion (cleanly or not) with a real `drain_start`,
/// so a full `StageTimings` is available. Carries `timing_target =
/// crate::format::timing::TARGET` as an ordinary field so an aggregator can
/// filter on that field's value instead of regex-scraping message text.
/// Durations and a word count only — never transcript text (see
/// `sarvam::codec::parse_server`'s logging policy, which this file follows
/// throughout).
///
/// Deliberately *not* emitted via `tracing::info!(target: TARGET, ...)`:
/// that overrides the event's own tracing *metadata* target, which is
/// otherwise the module path (`butterfly_speak_lib::sarvam::ws`, same as
/// every other log line in this file). A crate-scoped `RUST_LOG` such as
/// `butterfly_speak_lib=info` — a natural choice for someone collecting
/// exactly this data, to quiet dependency noise — matches events by
/// metadata-target *prefix* (`tracing_subscriber::EnvFilter`), so overriding
/// it to the bare string `"dictation_timing"` would silently drop every
/// sample under that filter; this crate's own default
/// (`"info,butterfly_speak_lib=debug,tauri_plugin_updater=off"` — see
/// `init_logging` in `lib.rs`) would pass it only via its bare `info`
/// directive. Leaving the metadata target at its default keeps this line
/// under the same filters as its neighbours; the `timing_target` field keeps
/// the aggregator's filter stable and decoupled from this module's path.
///
/// `errored` is `true` when the drain ended via `drain_error` — a mid-drain
/// failure — rather than cleanly. Both of this function's call sites can be
/// reached with `drain_error` having been `Some`: the failure either left
/// nothing usable (the empty-text call site, which always passes `true`) or
/// left salvageable finals/partial that still went through formatting (the
/// end-of-function call site, which passes `drain_error.is_some()`). Either
/// way, without this field the sample is indistinguishable from a clean run
/// with the same shape — `skipped` only tells an aggregator whether
/// duration fields exist at all, not whether a present set of them should
/// be trusted the same as any other.
///
/// `segments` is how many chunks this dictation polished in the background
/// before the key was released. Zero is the single-call path, where
/// `format_ms` covers the whole text; above zero, `format_ms` covers only the
/// tail call plus its guard — the part the user actually waits through — and
/// the background calls are deliberately *not* in it. A reader comparing
/// `format_ms` across samples has to split them on this field or they are
/// comparing two different spans.
///
/// `window` is the dispatcher's rolling window of the last
/// `format::timing::SUMMARY_WINDOW` samples. Every call pushes into it, and
/// every `SUMMARY_EVERY`-th call emits a *second* line — `summary = true`,
/// carrying the same `timing_target` — with the p50/p90/p99 of drain,
/// format and total across that window. An aggregator wanting raw samples
/// must therefore filter on `summary`, not on `timing_target` alone.
/// Counts and milliseconds only, like the sample line above.
///
/// `total` is sampled fresh right here, immediately before the log line
/// goes out — see `StageTimings::total_ms` for exactly what that single
/// span covers and what it still excludes.
///
/// `drain_start`/`drain` take `Option` only as a defensive guard, not
/// because `None` is an expected input: every call site below has already
/// gone through `CloudCmd::Finish`'s arm, which sets both unconditionally
/// (see `drain_start`'s declaration in `drain_session`). If the guard ever
/// actually fires, that is a bookkeeping bug in this file, not a session
/// that genuinely went unmeasured — which is why it does *not* emit
/// `skipped = true`/`timing_target` (that would double-count against the
/// real skip lines `log_timing_skipped` emits below).
fn log_timing_sample(
    drain_start: Option<Instant>,
    drain: Option<Duration>,
    format: Duration,
    ttft: Option<Duration>,
    words: usize,
    errored: bool,
    segments: u32,
    chunk_wait: Duration,
    window: &mut crate::format::timing::TimingWindow,
) {
    let (Some(start), Some(drain)) = (drain_start, drain) else {
        tracing::error!(
            "dictation timing: drain_start/drain unset at a call site that \
             should be unreachable without them — timing bookkeeping bug, \
             not a normal skip; not logged as one"
        );
        return;
    };
    let timings = StageTimings::new(drain, format, start.elapsed())
        .with_ttft(ttft)
        .with_segments(segments)
        .with_chunk_wait(chunk_wait);
    tracing::info!(
        timing_target = crate::format::timing::TARGET,
        skipped = false,
        errored = errored,
        drain_ms = timings.drain_ms,
        format_ms = timings.format_ms,
        ttft_ms = timings.ttft_ms.unwrap_or(0),
        total_ms = timings.total_ms,
        segments = timings.segments,
        chunk_wait_ms = timings.chunk_wait_ms,
        words,
        "dictation timing"
    );
    if let Some(s) = window.push(&timings) {
        tracing::info!(
            timing_target = crate::format::timing::TARGET,
            summary = true,
            n = s.n,
            drain_p50 = s.drain.p50,
            drain_p90 = s.drain.p90,
            drain_p99 = s.drain.p99,
            format_p50 = s.format.p50,
            format_p90 = s.format.p90,
            format_p99 = s.format.p99,
            total_p50 = s.total.p50,
            total_p90 = s.total.p90,
            total_p99 = s.total.p99,
            "dictation timing summary"
        );
    }
}

/// One structured line for a dictation the user finished speaking through
/// (`Finish` was received, so `drain_start` was set) but that never reached
/// `log_timing_sample` at all — logged at `info`, the same level as a real
/// sample, specifically so a run collecting the samples this is meant to be the
/// denominator for can see the skip count too; `debug` would be invisible under
/// exactly the filter that makes the numerator visible. No duration fields —
/// there is nothing to report, unlike `log_timing_sample`'s `errored` case,
/// which still has a full, if tainted, `StageTimings`.
///
/// Called from the three places in this file where that actually happens:
/// `fail_drain`'s own `CloudCmd::Finish` arm (a pre-`Finish` failure that a
/// `Finish` later catches up to), and, inside `drain_session`'s main loop, the
/// `Cancel`/channel-closed arm and the `Start`-carryover arm, each only when
/// `finish_req.is_some()` (the user had already finished speaking). `reason` is
/// a short, fixed, per-call-site string (never transcript text or a raw error
/// message) so an aggregator can break the skip count down without needing a
/// fourth thing to filter on beyond `timing_target`.
fn log_timing_skipped(reason: &'static str) {
    tracing::info!(
        timing_target = crate::format::timing::TARGET,
        skipped = true,
        reason = reason,
        "dictation timing not recorded"
    );
}

/// Decide what text to inject. Returns the text plus an optional notice for the
/// overlay — a silent fallback would let a dead model go unnoticed, so an
/// *enforced* rejection is always reported (see `resolve_format_inner` for
/// which rejections that is).
///
/// `pub(crate)`: `asr::offline`'s local polish path shares this exact
/// decision logic (construct a synthetic `ChatReply` — see its call site —
/// and run the same guard rules) rather than duplicating it.
///
/// Thin wrapper around `resolve_format_inner`, supplying the real
/// module-level `guard::REPORT_ONLY` flag. Split out so tests can exercise
/// both the report-only and enforced paths without flipping a global (see
/// `resolve_format_inner`'s own doc comment for the enforcement design).
pub(crate) fn resolve_format(
    raw: &str,
    rule_output: &str,
    reply: Option<&crate::format::backend::ChatReply>,
    level: crate::format::level::CleanupLevel,
) -> (String, Option<String>) {
    resolve_format_inner(raw, rule_output, reply, level, crate::format::guard::REPORT_ONLY)
}

/// Enforcement is tiered, not blanket. `guard::REPORT_ONLY` is `false`, so
/// today every rejection is enforced; the tiers say which rejections stay
/// enforced if it is ever set to `true` for a calibration run, when the
/// guardrail logs what it *would* have rejected except for the checks it
/// trusts without any calibration. Gating every `Reject` on `REPORT_ONLY`
/// would switch those off too; falling back on every `Reject` would make
/// `REPORT_ONLY` dead.
///
/// - **Always enforced, `REPORT_ONLY` or not:**
///   - `RejectReason::Truncated` — exact by construction (`finish_reason`),
///     so it needs no tuning.
///   - An outright empty reply, checked here independently of
///     `guard::check`. `check` accepts a reply with no content words when
///     `rule_output` has none either (e.g. punctuation-only rule output) — a
///     legitimate answer to "nothing to preserve", but it means a genuinely
///     empty model reply could otherwise pass it and inject nothing.
///   - Catastrophic content loss, `retained <= CATASTROPHIC_CONTENT_FLOOR` — a
///     floor far below the calibrated `RETAIN_STRICT`/`RETAIN_LENIENT`,
///     chosen so no legitimate formatting pass can ever reach it. The reply
///     `a_rejected_format_falls_back_to_the_rule_pipeline` (below) rejects,
///     "Sure, I can help with that.", retains 0.0, comfortably inside this
///     tier, so it falls back whatever `REPORT_ONLY` says.
///   - Catastrophic over-expansion, `ratio >= CATASTROPHIC_RATIO_CEILING` —
///     the mirror image of the content-loss floor above. Over-expansion
///     always surfaces as `LengthRatio`, never `ContentLost` (every
///     original word can still be present, so retention stays 1.0), so
///     without this, the "everything else is provisional" tier below would
///     let a fabricated addition reach the user's document silently, even
///     with `REPORT_ONLY` true — see `CATASTROPHIC_RATIO_CEILING`'s doc
///     comment. A reply with words to an input that had none is here too,
///     at an infinite ratio.
/// - **Gated on `REPORT_ONLY`:** everything else `guard::check` rejects —
///   `ContentLost` above the catastrophic floor, and `LengthRatio` below the
///   catastrophic ratio ceiling. With `REPORT_ONLY` set to `true` these would
///   be logged (for calibration) and the model's output still used, silently.
///   The log carries lengths and the reject reason only, never transcript
///   text — see this function's own logging below and
///   `sarvam::codec::parse_server`'s matching policy.
///
/// `raw` plays no part in the guardrail comparison itself (`check` compares
/// against `rule_output` — see its doc comment for why); it is threaded
/// through only so a rejection's log line can report its word count — never
/// its content. This file logs no verbatim transcript text.
fn resolve_format_inner(
    raw: &str,
    rule_output: &str,
    reply: Option<&crate::format::backend::ChatReply>,
    level: crate::format::level::CleanupLevel,
    report_only: bool,
) -> (String, Option<String>) {
    use crate::format::guard::{
        check, RejectReason, Verdict, CATASTROPHIC_CONTENT_FLOOR, CATASTROPHIC_RATIO_CEILING,
    };

    let Some(reply) = reply else {
        return (rule_output.to_string(), None);
    };
    let formatted = reply.text.trim();

    // Independent of `check()` — see this function's doc comment.
    //
    // No transcript content in any log below (see this function's doc
    // comment on `raw`, and `sarvam::codec::parse_server` for the same
    // policy elsewhere in this crate): only word counts, the reject reason,
    // and `report_only`. `RejectReason`'s `Debug` output is numeric-only
    // (a retention fraction or a ratio, or nothing for `Truncated`), so
    // `{reason:?}` never carries transcript text either.
    if formatted.is_empty() {
        tracing::warn!(
            raw_words = raw.split_whitespace().count(),
            rule_output_words = rule_output.split_whitespace().count(),
            "formatting rejected: empty reply; falling back to the rule pipeline"
        );
        return (
            rule_output.to_string(),
            Some(RejectReason::ContentLost { retained: 0.0 }.user_message().to_string()),
        );
    }

    match check(rule_output, formatted, reply, level) {
        Verdict::Accept => (formatted.to_string(), None),
        Verdict::Reject(reason) => {
            let always_enforced = match &reason {
                RejectReason::Truncated => true,
                RejectReason::ContentLost { retained } => *retained <= CATASTROPHIC_CONTENT_FLOOR,
                // Over-expansion never trips `ContentLost` (every original
                // word can still be present), so `LengthRatio` is the only
                // signal that ever sees it — see
                // `CATASTROPHIC_RATIO_CEILING`'s doc comment for why a ratio
                // this large must be enforced unconditionally rather than
                // left in the provisional band below.
                RejectReason::LengthRatio { ratio } => *ratio >= CATASTROPHIC_RATIO_CEILING,
            };
            if always_enforced || !report_only {
                tracing::warn!(
                    raw_words = raw.split_whitespace().count(),
                    rule_output_words = rule_output.split_whitespace().count(),
                    formatted_words = formatted.split_whitespace().count(),
                    report_only = report_only,
                    // `Truncated` alone can't say whether the API cut the
                    // reply off or the model just never wrote its end
                    // marker — opposite fixes, so carry the reason through.
                    finish_reason = %reply.finish_reason,
                    "formatting rejected: {reason:?}; falling back to the rule pipeline"
                );
                (rule_output.to_string(), Some(reason.user_message().to_string()))
            } else {
                // Provisional tier, report-only: log lengths and the reject
                // reason for calibration, but the thresholds
                // aren't trusted yet, so the model's output is still what
                // gets used — silently.
                tracing::info!(
                    raw_words = raw.split_whitespace().count(),
                    rule_output_words = rule_output.split_whitespace().count(),
                    formatted_words = formatted.split_whitespace().count(),
                    report_only = report_only,
                    "formatting flagged but not enforced (report-only): {reason:?}"
                );
                (formatted.to_string(), None)
            }
        }
    }
}

/// How far short of a full tee the recording is stopped: a few capture
/// chunks (`audio::CHUNK_MS` each), for the chunk already on its way when
/// the stop is asked for and the part-chunk the capture flushes at the end.
/// Both still join the utterance, which has to stay within the rescue's
/// limit for the rescue to take it.
const RESCUE_STOP_MARGIN: usize = 3 * FALLBACK_SAMPLE_RATE_HZ / 10;

/// Whether a socket that just proved dead, before the user finished, still
/// leaves a rescue for what they say next: the batch fallback, which needs
/// Bring your own key (`batch_credential`) and the whole utterance in the tee
/// (`MAX_FALLBACK_SAMPLES`). While it does, the user is let finish and the
/// rescue re-transcribes everything tee'd.
///
/// When it does not, every further second of speech is lost, so the drain
/// stops the recording at once. On the relay lane that is every early death:
/// every dictation once the week is spent (the relay accepts the socket and
/// closes it with `4029` before a word), and a socket that drops mid-sentence.
/// On Bring your own key it is a socket that died with the tee within
/// `RESCUE_STOP_MARGIN` of full: stopped then, the utterance still fits, and
/// the rescue takes it whether or not a word had arrived.
fn rescue_left(relay: bool, tee_len: usize) -> bool {
    !relay && tee_len + RESCUE_STOP_MARGIN < MAX_FALLBACK_SAMPLES
}

/// The drain stopped at a deadline without `session.end`: the transcript is
/// pasted, but nothing says every final had arrived.
fn msg_end_unconfirmed(relay: bool) -> String {
    format!(
        "{} didn't confirm the end of this dictation — check the last words",
        host_name(relay)
    )
}

/// A session died before it could produce a result. Tell the controller now
/// (`req_id 0` = mid-recording), then keep answering until the controller
/// tears the dictation down — a `Finish` racing the failure still gets a
/// properly-addressed error, and also logs a `skipped = true` timing line:
/// the user did finish speaking, so this dictation belongs in the timing
/// log's denominator even though nothing could be measured.
async fn fail_drain(
    ctl_tx: &Sender<ControlMsg>,
    rx: &mut UnboundedReceiver<CloudCmd>,
    session: u64,
    message: String,
) -> Option<CloudCmd> {
    let _ = ctl_tx.send(ControlMsg::CloudError {
        req_id: 0,
        session,
        message: message.clone(),
    });
    loop {
        match rx.recv().await {
            Some(CloudCmd::Audio(_)) => continue,
            // `end_of_speech`/`duration_ms` are both irrelevant here — the
            // session already failed before any `Finish` could start a
            // drain to time, so there is no `StageTimings` to report (and,
            // deliberately, no batch-fallback attempt either: `fail_drain`
            // is called from `run_session`'s connect-phase failures, all of
            // which return before the drain loop — and its `fallback_pcm`
            // tee — ever starts, and from exactly one place inside that
            // loop: a relay socket that died before a word arrived. That one
            // gives up nothing: the relay lane has no rescue to give up (see
            // `batch_credential`). A Bring your own key socket that died
            // before a word is not sent here even once the tee is nearly
            // full: it stops the recording through `CloudEnded`, so its
            // `Finish` reaches the batch rescue. Every other failure inside
            // the loop sets `ws_dead`/`drain_error` and `break`s into the
            // unified batch-fallback → rules → polish → guardrail pipeline
            // instead of calling this function, precisely so a socket that dies
            // mid-utterance still gets a fallback attempt on whatever was
            // already tee'd — see `ws_dead`'s own declaration) — but the
            // user *did* finish speaking, which is exactly the population
            // the timing log's denominator needs to include: logged first,
            // the same order as the `CloudError` send below at every other
            // call site in this file.
            Some(CloudCmd::Finish { req_id, .. }) => {
                log_timing_skipped("session failed before end of speech");
                let _ = ctl_tx.send(ControlMsg::CloudError {
                    req_id,
                    session,
                    message,
                });
                return None;
            }
            Some(CloudCmd::Cancel) | None => return None,
            Some(start @ CloudCmd::Start { .. }) => return Some(start),
        }
    }
}

/// The frames that close an utterance, in send order. Push-to-talk ends the
/// utterance itself (`speech_end`) and then ends the session (`end`);
/// hands-free only ends the session. `end` is what makes Saaras flush its
/// pending audio *and* answer with `session.end` — measured with
/// `tools/latency/ws_probe.py`: the final arrives 161–181 ms after these
/// frames and `session.end` 4 ms after the final, with the same text a
/// `flush` produces. `flush` alone never yields `session.end` under manual
/// endpointing and yields nothing at all under VAD, so a drain that relied
/// on it would wait out `QUIET_WINDOW` on every push-to-talk release and the
/// whole `scaled_flush_wait` floor on every hands-free stop.
pub(crate) fn finish_frames(endpointing: Endpointing) -> Vec<ClientMsg<'static>> {
    match endpointing {
        Endpointing::Manual => vec![ClientMsg::SpeechEnd, ClientMsg::End],
        Endpointing::Vad => vec![ClientMsg::End],
    }
}

async fn send_finish(ws_tx: &mut WsSink, endpointing: Endpointing) -> Result<(), WsError> {
    for frame in finish_frames(endpointing) {
        timed_send(ws_tx, Message::Text(frame.to_json())).await?;
    }
    Ok(())
}

fn assemble(finals: &BTreeMap<u64, String>, partial: &str) -> String {
    let mut parts: Vec<&str> = finals
        .values()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .collect();
    let tail = partial.trim();
    if !tail.is_empty() {
        parts.push(tail);
    }
    parts.join(" ")
}

/// Whether this close is the relay ending the session on purpose: the
/// week's allowance is spent (`4029`) or the session reached its 30-minute
/// ceiling (`4030`). Neither is a failure of the transcript that already
/// arrived — the relay stopped transcribing, deliberately, and will not pick
/// up where it left off — so what was said up to that point is delivered
/// with the reason, rather than withheld as a fragment of a dead socket.
///
/// Only on the relay lane: the private close range means nothing on
/// Sarvam's own socket, and Bring your own key's handling stays as it was.
fn relay_ended_on_purpose(frame: Option<&CloseFrame<'_>>, relay: bool) -> bool {
    relay && frame.is_some_and(|f| matches!(u16::from(f.code), 4029 | 4030))
}

fn close_message(frame: Option<CloseFrame<'_>>, relay: bool) -> String {
    let host = host_name(relay);
    let Some(f) = frame else {
        return format!("{host} closed the connection — try again");
    };
    let code: u16 = f.code.into();
    let reason = f.reason.to_lowercase();
    tracing::warn!("realtime close: code={code} reason={}", f.reason);
    match code {
        // The two upstream codes whose Bring-your-own-key sentence names a
        // fix only a key holder has: a Sarvam dashboard, and the key field in
        // Settings. On the Cloud lane the key is Butterfly Labs' own, so
        // either one is a fault at the service and nothing the user can act
        // on — say that instead of sending them somewhere useless.
        1003 if reason.contains("quota") || reason.contains("rate") || reason.contains("limit") => {
            if relay {
                format!("{host} is over its limit right now — try again later")
            } else {
                "Sarvam quota exceeded — check your plan on dashboard.sarvam.ai".into()
            }
        }
        1003 if relay => format!("{host} couldn't authenticate upstream — try again later"),
        1003 => "Sarvam key rejected — update it in Settings → Speech engine".into(),
        1008 => format!("{host} closed an idle connection — try again"),
        // The same sentence the fatal error frame produces, and deliberately
        // so: 1011 and `ServerMsg::Error { is_fatal }` are the same event
        // reaching the app two ways.
        1011 => msg_service_error(relay),
        4000 => format!("{host} rejected the request (bad parameter)"),
        // The relay's own code, in the private range and used by nothing
        // else on this socket: the week's 2,000 words are spent. Matched on
        // the code alone — the reason string is diagnostic, not a contract.
        // Relay-only, both of them: on Sarvam's own socket these private
        // codes are undocumented, and Bring your own key keeps the generic
        // sentence it has always had for them.
        4029 if relay => MSG_CLOUD_QUOTA.into(),
        // The relay's other code: this session reached its 30-minute ceiling.
        4030 if relay => MSG_CLOUD_SESSION_LIMIT.into(),
        _ => format!("{host} closed the connection — try again"),
    }
}

async fn sleep_until_opt(deadline: Option<Instant>) {
    match deadline {
        Some(d) => tokio::time::sleep_until(d).await,
        None => std::future::pending().await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- Duration-scaled flush wait ---------------------------------------

    #[test]
    fn a_short_utterance_gets_exactly_the_floor() {
        assert_eq!(scaled_flush_wait(0), FLUSH_WAIT_FLOOR);
        assert_eq!(scaled_flush_wait(5_000), FLUSH_WAIT_FLOOR + Duration::from_millis(166));
    }

    /// A 60 s hands-free utterance is exactly the point the worked-numbers
    /// doc comment on `FLUSH_WAIT_SCALE_DIVISOR` describes: floor (4 s) +
    /// 60000/30 ms (2 s) = the ceiling, exactly.
    #[test]
    fn a_sixty_second_utterance_lands_exactly_on_the_ceiling() {
        assert_eq!(scaled_flush_wait(60_000), FLUSH_WAIT_CEILING);
    }

    /// Anything longer must clamp, never exceed the ceiling —
    /// `Controller::CLOUD_FINALIZE_TIMEOUT`'s own sum depends on this bound
    /// actually holding.
    #[test]
    fn a_very_long_utterance_clamps_at_the_ceiling() {
        assert_eq!(scaled_flush_wait(600_000), FLUSH_WAIT_CEILING);
        assert_eq!(scaled_flush_wait(u64::MAX), FLUSH_WAIT_CEILING);
    }

    #[test]
    fn scaling_is_monotonic_between_the_floor_and_the_ceiling() {
        let a = scaled_flush_wait(1_000);
        let b = scaled_flush_wait(30_000);
        let c = scaled_flush_wait(60_000);
        assert!(a <= b);
        assert!(b <= c);
    }

    // --- Batch-fallback gate ----------------------------------------------

    #[test]
    fn too_short_a_duration_does_not_attempt_the_fallback() {
        assert!(!should_attempt_batch_fallback(batch::MIN_FALLBACK_DURATION_MS, true));
        assert!(!should_attempt_batch_fallback(0, true));
    }

    #[test]
    fn no_tee_d_audio_does_not_attempt_the_fallback_even_if_duration_qualifies() {
        assert!(!should_attempt_batch_fallback(10_000, false));
    }

    #[test]
    fn a_qualifying_duration_with_audio_attempts_the_fallback() {
        assert!(should_attempt_batch_fallback(10_000, true));
    }

    /// Sarvam's REST `speech-to-text` endpoint caps accepted audio at 30 s
    /// (`batch::MAX_FALLBACK_DURATION_MS`) — a long hands-free dictation
    /// whose realtime session produced nothing must not spend the drain's
    /// own budget on a REST call that is guaranteed to 400.
    #[test]
    fn a_duration_past_the_rest_endpoints_cap_does_not_attempt_the_fallback() {
        assert!(should_attempt_batch_fallback(batch::MAX_FALLBACK_DURATION_MS, true));
        assert!(!should_attempt_batch_fallback(batch::MAX_FALLBACK_DURATION_MS + 1, true));
        assert!(!should_attempt_batch_fallback(180_000, true)); // a 3-minute hands-free session
    }

    // --- The tee's own cap ------------------------------------------------

    /// `MAX_FALLBACK_SAMPLES` has to be exactly the gate's own ceiling
    /// expressed in samples, or the tee and the gate disagree about what
    /// "usable" means. 30 s of 16 kHz mono = 480 000 samples.
    #[test]
    fn the_tee_cap_is_the_gates_own_ceiling_in_samples() {
        assert_eq!(MAX_FALLBACK_SAMPLES, 480_000);
        assert_eq!(
            MAX_FALLBACK_SAMPLES as u64,
            batch::MAX_FALLBACK_DURATION_MS * FALLBACK_SAMPLE_RATE_HZ as u64 / 1_000
        );
    }

    #[test]
    fn the_tee_accumulates_normally_below_the_cap() {
        let mut buf = Vec::new();
        tee_fallback_pcm(&mut buf, &[0.25; 1_600]);
        tee_fallback_pcm(&mut buf, &[0.5; 1_600]);
        assert_eq!(buf.len(), 3_200);
        assert_eq!(buf[0], 0.25);
        assert_eq!(buf[3_199], 0.5);
    }

    /// A long hands-free dictation must not keep growing a buffer the
    /// fallback gate (`should_attempt_batch_fallback`) is already guaranteed
    /// to refuse — ~19 MB of f32 for a five-minute session, held for the
    /// whole utterance and never sent anywhere.
    #[test]
    fn the_tee_stops_growing_at_the_cap() {
        let mut buf = Vec::new();
        // Ten minutes of audio in one-second chunks.
        for _ in 0..600 {
            tee_fallback_pcm(&mut buf, &[0.1; FALLBACK_SAMPLE_RATE_HZ]);
        }
        assert_eq!(buf.len(), MAX_FALLBACK_SAMPLES);
    }

    /// A chunk that straddles the cap is taken up to the boundary and no
    /// further — the buffer must land exactly on the cap, never past it.
    #[test]
    fn a_chunk_that_straddles_the_cap_is_truncated_to_the_boundary() {
        let mut buf = vec![0.0; MAX_FALLBACK_SAMPLES - 10];
        tee_fallback_pcm(&mut buf, &[1.0; 100]);
        assert_eq!(buf.len(), MAX_FALLBACK_SAMPLES);
        assert_eq!(buf[MAX_FALLBACK_SAMPLES - 1], 1.0);
        tee_fallback_pcm(&mut buf, &[1.0; 100]);
        assert_eq!(buf.len(), MAX_FALLBACK_SAMPLES, "a full tee must not grow again");
    }

    /// Oldest-first, not newest-first: the samples kept are the *start* of
    /// the utterance. A rolling window would leave the tee holding the last
    /// 30 s of a longer dictation — not the whole utterance the fallback
    /// exists to recover, and one the duration gate would refuse to send
    /// anyway. See `MAX_FALLBACK_SAMPLES`' doc comment.
    #[test]
    fn the_tee_keeps_the_start_of_the_utterance_not_a_rolling_window() {
        let mut buf = Vec::new();
        tee_fallback_pcm(&mut buf, &vec![0.7; MAX_FALLBACK_SAMPLES]);
        tee_fallback_pcm(&mut buf, &[0.9; 1_000]);
        assert_eq!(buf[0], 0.7);
        assert_eq!(buf[MAX_FALLBACK_SAMPLES - 1], 0.7);
    }

    // --- Drain outcome ----------------------------------------------------

    /// Row 1 of the decision table: a healthy socket that produced a
    /// transcript needs nothing else — no fallback attempt, no notice.
    #[test]
    fn a_healthy_session_with_a_transcript_stands_alone() {
        assert!(realtime_transcript_stands_alone(false, true));
        assert_eq!(
            resolve_drain_outcome(false, true, false),
            DrainOutcome::Realtime
        );
    }

    /// A healthy session that simply heard nothing is still worth one batch
    /// attempt — "stands alone" is about having something to stand on.
    #[test]
    fn a_healthy_session_with_no_transcript_does_not_stand_alone() {
        assert!(!realtime_transcript_stands_alone(false, false));
    }

    /// A socket that died mid-utterance leaves finals that stop wherever the
    /// death happened. Those finals must never be treated as a complete
    /// transcript, however many of them arrived.
    #[test]
    fn a_dead_socket_never_lets_partial_finals_stand_alone() {
        assert!(!realtime_transcript_stands_alone(true, true));
    }

    /// Row 2: dead socket, nothing salvaged, the fallback rescued the
    /// utterance.
    #[test]
    fn a_dead_socket_with_no_finals_uses_a_successful_fallback() {
        assert_eq!(
            resolve_drain_outcome(true, false, true),
            DrainOutcome::Fallback
        );
    }

    /// Row 2, the other half: nothing salvaged and no fallback either. There
    /// is no fragment to withhold, so this stays the plain realtime path —
    /// `drain_session`'s empty-text-plus-`drain_error` branch reports the
    /// failure.
    #[test]
    fn a_dead_socket_with_nothing_at_all_stays_on_the_realtime_path() {
        assert_eq!(
            resolve_drain_outcome(true, false, false),
            DrainOutcome::Realtime
        );
    }

    /// Row 3: dead socket, partial finals, and a usable fallback. The
    /// fallback re-transcribes the WHOLE utterance from the tee, so it wins
    /// over the fragment — never a merge, never the finals.
    #[test]
    fn a_dead_socket_prefers_the_whole_utterance_fallback_over_partial_finals() {
        assert_eq!(
            resolve_drain_outcome(true, true, true),
            DrainOutcome::Fallback
        );
    }

    /// Row 4: dead socket, partial finals, no usable fallback. Pasting the
    /// fragment would hand the user half a sentence with no notice — the
    /// silent-truncation class this project treats as its worst failure —
    /// so it is withheld.
    #[test]
    fn a_dead_socket_with_partial_finals_and_no_fallback_is_truncated() {
        assert_eq!(
            resolve_drain_outcome(true, true, false),
            DrainOutcome::Truncated
        );
    }

    /// The two halves of the decision have to agree: whenever the realtime
    /// transcript stands alone there is nothing for a fallback to contribute,
    /// so the outcome is always `Realtime`. Guards against the pair drifting
    /// apart later.
    #[test]
    fn standing_alone_always_resolves_to_the_realtime_outcome() {
        for errored in [false, true] {
            for has_text in [false, true] {
                if realtime_transcript_stands_alone(errored, has_text) {
                    assert_eq!(
                        resolve_drain_outcome(errored, has_text, false),
                        DrainOutcome::Realtime,
                        "errored={errored} has_text={has_text}"
                    );
                }
            }
        }
    }

    // --- Connect backoff --------------------------------------------------

    #[test]
    fn the_first_failure_uses_the_base_delay() {
        assert_eq!(backoff_base_ms(1), BACKOFF_BASE_MS);
    }

    #[test]
    fn the_delay_grows_exponentially_then_clamps() {
        assert_eq!(backoff_base_ms(2), BACKOFF_BASE_MS * u64::from(BACKOFF_FACTOR));
        assert_eq!(
            backoff_base_ms(3),
            BACKOFF_BASE_MS * u64::from(BACKOFF_FACTOR) * u64::from(BACKOFF_FACTOR)
        );
        // By `BACKOFF_MAX_STEPS` the exponential would already have
        // overtaken `BACKOFF_MAX_MS`; every step past it must stay pinned,
        // not keep growing (or wrap on overflow for a very long streak).
        assert_eq!(backoff_base_ms(BACKOFF_MAX_STEPS), BACKOFF_MAX_MS);
        assert_eq!(backoff_base_ms(BACKOFF_MAX_STEPS + 10), BACKOFF_MAX_MS);
        assert_eq!(backoff_base_ms(1_000_000), BACKOFF_MAX_MS);
    }

    #[test]
    fn a_non_transient_failure_clears_any_pending_backoff() {
        let mut b = ConnectBackoff::new();
        b.record_failure(true);
        assert!(b.until.is_some());
        b.record_failure(false);
        assert!(b.until.is_none());
        assert_eq!(b.consecutive_transient_failures, 0);
    }

    #[test]
    fn a_success_clears_any_pending_backoff() {
        let mut b = ConnectBackoff::new();
        b.record_failure(true);
        b.record_failure(true);
        assert!(b.consecutive_transient_failures >= 2);
        b.record_success();
        assert!(b.until.is_none());
        assert_eq!(b.consecutive_transient_failures, 0);
    }

    /// The scheduled delay must land within `[base, base + jitter]` — the
    /// jitter source is random, so this checks the bound, not an exact
    /// value.
    #[test]
    fn a_transient_failure_schedules_a_delay_within_the_jittered_bound() {
        let mut b = ConnectBackoff::new();
        let before = Instant::now();
        b.record_failure(true);
        let until = b.until.expect("a transient failure must schedule a delay");
        let scheduled = until.saturating_duration_since(before);
        let base = Duration::from_millis(backoff_base_ms(1));
        assert!(scheduled >= base, "scheduled {scheduled:?} must be at least the base {base:?}");
        assert!(
            scheduled <= base + Duration::from_millis(BACKOFF_JITTER_MS) + Duration::from_millis(50),
            "scheduled {scheduled:?} must not exceed base + jitter (with a little slack for test timing)"
        );
    }

    // --- Classified connect retry -----------------------------------------

    /// A transient failure on the first attempt, with a budgeted attempt
    /// still remaining, must be retried within the same dictation.
    #[test]
    fn a_transient_failure_with_a_remaining_attempt_is_retried() {
        assert!(should_retry_connect(1, true));
    }

    /// A non-transient failure (401/403, DNS blocked, a corrupted key) must
    /// never be retried, on any attempt — a few seconds of waiting was never
    /// going to fix it, so retrying would only make the user wait longer for
    /// the identical failure.
    #[test]
    fn a_non_transient_failure_is_never_retried() {
        assert!(!should_retry_connect(1, false));
        assert!(!should_retry_connect(0, false));
    }

    /// The attempt cap is a hard stop even for a transient failure: once
    /// `CONNECT_MAX_ATTEMPTS` attempts have already been made, there is no
    /// budgeted attempt left to retry into.
    #[test]
    fn a_transient_failure_stops_retrying_once_the_attempt_cap_is_reached() {
        assert!(!should_retry_connect(CONNECT_MAX_ATTEMPTS, true));
        assert!(!should_retry_connect(CONNECT_MAX_ATTEMPTS + 1, true));
    }

    /// Rejected formatting must fall back to the rule-cleaned text, never to
    /// nothing and never to the model's output.
    #[test]
    fn a_rejected_format_falls_back_to_the_rule_pipeline() {
        let rule_output = "The meeting is at 3:30 PM.";
        let model_output = "Sure, I can help with that.";
        let reply = crate::format::backend::ChatReply {
            text: model_output.into(),
            finish_reason: "stop".into(),
            prompt_tokens: 0,
            completion_tokens: 0,
            first_token_ms: None,
        };
        let (text, notice) = resolve_format(
            "um so the meeting is at three thirty pm",
            rule_output,
            Some(&reply),
            crate::format::level::CleanupLevel::Balanced,
        );
        assert_eq!(text, rule_output);
        assert!(notice.is_some(), "the user must be told");
    }

    #[test]
    fn an_accepted_format_is_used_and_says_nothing() {
        let reply = crate::format::backend::ChatReply {
            text: "The meeting is at 3:30 PM.".into(),
            finish_reason: "stop".into(),
            prompt_tokens: 0,
            completion_tokens: 0,
            first_token_ms: None,
        };
        let (text, notice) = resolve_format(
            "um so the meeting is at three thirty pm",
            "the meeting is at 3:30 pm",
            Some(&reply),
            crate::format::level::CleanupLevel::Balanced,
        );
        assert_eq!(text, "The meeting is at 3:30 PM.");
        assert!(notice.is_none());
    }

    /// Polish never ran (e.g. `level == Off`) or was skipped — nothing to
    /// validate, and nothing to tell the user about.
    #[test]
    fn no_reply_falls_back_to_the_rule_pipeline_without_a_notice() {
        let (text, notice) = resolve_format(
            "raw transcript",
            "rule-cleaned text",
            None,
            crate::format::level::CleanupLevel::Light,
        );
        assert_eq!(text, "rule-cleaned text");
        assert!(notice.is_none());
    }

    /// End-to-end at Light, through the public `resolve_format` wrapper
    /// (report_only = the real `guard::REPORT_ONLY`): the guardrail compares
    /// against the rule output, not the raw transcript, so an output that only
    /// reproduces what the rule pipeline's own ITN already did is accepted, not
    /// rejected — Light's own prompt requires exactly this ("write numbers,
    /// dates, times... the way people type them").
    #[test]
    fn a_light_level_format_that_only_normalizes_is_accepted() {
        let reply = crate::format::backend::ChatReply {
            text: "The meeting is at 3:30 PM.".into(),
            finish_reason: "stop".into(),
            prompt_tokens: 0,
            completion_tokens: 0,
            first_token_ms: None,
        };
        let (text, notice) = resolve_format(
            "the meeting is at three thirty pm",
            "The meeting is at 3:30 PM.", // already ITN'd by the rule pipeline
            Some(&reply),
            crate::format::level::CleanupLevel::Light,
        );
        assert_eq!(text, "The meeting is at 3:30 PM.");
        assert!(notice.is_none());
    }

    /// `Truncated` is exact by construction and needs no calibration — it
    /// must be enforced even while `report_only` is true.
    #[test]
    fn a_truncated_reply_is_enforced_even_while_report_only() {
        let rule_output = "Tell them the release ships on Thursday and the docs follow.";
        let reply = crate::format::backend::ChatReply {
            text: "Tell them the release ships on Thursday and the do".into(),
            finish_reason: "length".into(),
            prompt_tokens: 0,
            completion_tokens: 0,
            first_token_ms: None,
        };
        let (text, notice) = resolve_format_inner(
            "tell them the release ships on thursday and the docs follow",
            rule_output,
            Some(&reply),
            crate::format::level::CleanupLevel::Balanced,
            true, // report_only
        );
        assert_eq!(text, rule_output);
        assert!(notice.is_some());
    }

    /// `guard::check` short-circuits to `Accept` when `rule_output` has no
    /// content words of its own (e.g. punctuation-only rule output) —
    /// without an independent empty-reply check, a genuinely empty model
    /// reply would ride that shortcut straight through and inject nothing.
    /// Must be caught here regardless of `check`, and regardless of
    /// `report_only`.
    #[test]
    fn an_empty_reply_is_enforced_even_when_check_would_short_circuit_to_accept() {
        let rule_output = "..."; // content_words("...") is empty: no alphanumerics
        let reply = crate::format::backend::ChatReply {
            text: "   ".into(), // trims to empty
            finish_reason: "stop".into(),
            prompt_tokens: 0,
            completion_tokens: 0,
            first_token_ms: None,
        };
        let (text, notice) = resolve_format_inner(
            "...",
            rule_output,
            Some(&reply),
            crate::format::level::CleanupLevel::Balanced,
            true, // report_only
        );
        assert_eq!(text, rule_output);
        assert!(notice.is_some());
    }

    /// The provisional tier (`ContentLost` above the catastrophic floor): while
    /// `report_only` is true this must be silent AND must still use the model's
    /// output — this is what lets a calibration run see real pass/fail counts
    /// before enforcement can affect a user's document. Light floor is
    /// `RETAIN_STRICT`; dropping "tomorrow"/"evening" lands retention at 7/9 ≈
    /// 0.778 with two novel losses — well clear of the catastrophic floor
    /// (0.15), so this must land in the provisional tier, not the
    /// always-enforced one.
    #[test]
    fn a_marginal_rejection_is_silent_and_uses_the_model_output_while_report_only() {
        let rule_output = "the meeting is at 3:30 pm tomorrow evening";
        let reply = crate::format::backend::ChatReply {
            text: "The meeting is at 3:30 PM.".into(),
            finish_reason: "stop".into(),
            prompt_tokens: 0,
            completion_tokens: 0,
            first_token_ms: None,
        };
        let (text, notice) = resolve_format_inner(
            "the meeting is at three thirty pm tomorrow evening",
            rule_output,
            Some(&reply),
            crate::format::level::CleanupLevel::Light,
            true, // report_only
        );
        assert_eq!(
            text, "The meeting is at 3:30 PM.",
            "the model's output must still be used"
        );
        assert!(notice.is_none(), "a provisional, uncalibrated rejection must be silent");
    }

    /// The same marginal rejection, once calibration turns enforcement on
    /// (`report_only = false`): now it must actually fall back and notify —
    /// this is what makes the flag's two states observably different, and
    /// what flipping it does once the bounds are calibrated against rule
    /// output.
    #[test]
    fn the_same_marginal_rejection_is_enforced_once_report_only_is_false() {
        let rule_output = "the meeting is at 3:30 pm tomorrow evening";
        let reply = crate::format::backend::ChatReply {
            text: "The meeting is at 3:30 PM.".into(),
            finish_reason: "stop".into(),
            prompt_tokens: 0,
            completion_tokens: 0,
            first_token_ms: None,
        };
        let (text, notice) = resolve_format_inner(
            "the meeting is at three thirty pm tomorrow evening",
            rule_output,
            Some(&reply),
            crate::format::level::CleanupLevel::Light,
            false, // report_only
        );
        assert_eq!(text, rule_output);
        assert!(notice.is_some());
    }

    /// Over-expansion can never trip `ContentLost` (every original word can
    /// still be present, so retention stays 1.0), so if `LengthRatio` sat
    /// entirely inside the `REPORT_ONLY`-gated tier a wildly padded reply
    /// would land in the provisional band and reach the user's document with
    /// no warning, even with `REPORT_ONLY` true. Goes through the public
    /// `resolve_format` wrapper with the real `guard::REPORT_ONLY`.
    #[test]
    fn a_wildly_expanded_reply_is_enforced_even_while_report_only() {
        let rule_output = "Lunch at noon.";
        let reply = crate::format::backend::ChatReply {
            text: "Lunch at noon. I have also taken the liberty of booking a table \
                   for four people at the Italian restaurant on the corner, and \
                   added a calendar invitation for everyone on the team."
                .into(),
            finish_reason: "stop".into(),
            prompt_tokens: 0,
            completion_tokens: 0,
            first_token_ms: None,
        };
        let (text, notice) = resolve_format(
            "lunch at noon",
            rule_output,
            Some(&reply),
            crate::format::level::CleanupLevel::High,
        );
        assert_eq!(
            text, rule_output,
            "an 11x expansion must fall back to the rule pipeline, never inject the fabricated addition"
        );
        assert!(notice.is_some(), "an 11x expansion must be enforced, not silently accepted");
    }

    /// The other side of `CATASTROPHIC_RATIO_CEILING`: a mild expansion
    /// (ratio ~2.33, above `MAX_RATIO` but well below the 3.0 ceiling) must
    /// stay in the provisional tier — it might be legitimate High-level list
    /// expansion, not yet trustworthy enough to enforce without calibration.
    /// Without this, the ceiling above could overcorrect into enforcing
    /// every `LengthRatio` rejection regardless of size, which
    /// would defeat `REPORT_ONLY` for ordinary formatting.
    #[test]
    fn a_mild_expansion_stays_silent_under_report_only() {
        let rule_output = "Lunch at noon."; // 3 content words
        let reply = crate::format::backend::ChatReply {
            // 7 content words / 3 = ratio ~2.33: rejected on LengthRatio
            // (retention is 1.0 — "lunch", "at" and "noon" all survive), but
            // short of the 3.0 catastrophic ceiling.
            text: "Lunch at noon today with the team.".into(),
            finish_reason: "stop".into(),
            prompt_tokens: 0,
            completion_tokens: 0,
            first_token_ms: None,
        };
        let (text, notice) = resolve_format_inner(
            "lunch at noon",
            rule_output,
            Some(&reply),
            crate::format::level::CleanupLevel::High,
            true, // report_only
        );
        assert_eq!(
            text, "Lunch at noon today with the team.",
            "a mild, uncalibrated expansion must still use the model's output"
        );
        assert!(notice.is_none(), "a mild expansion under the ceiling must stay silent");
    }

    // --- Chunks are used only when the realtime text is what ships --------

    #[test]
    fn chunks_are_discarded_when_the_batch_fallback_replaced_the_transcript() {
        assert!(!use_background_chunks(3, false, 3, true, true));
    }

    #[test]
    fn chunks_are_discarded_when_the_worker_returned_fewer_than_were_handed_out() {
        assert!(!use_background_chunks(3, true, 2, true, true));
    }

    /// `Segmenter::in_sync` false means the tail it hands back is the *whole*
    /// transcript, not a remainder — assembling chunks in front of it would
    /// emit the already-polished text twice.
    #[test]
    fn chunks_are_discarded_when_the_segmenter_is_out_of_sync() {
        assert!(!use_background_chunks(3, true, 3, false, true));
    }

    /// The level flipped to `Off` between the last chunk and the key going
    /// up: the polish branch is skipped entirely, so the whole rule-cleaned
    /// text is what gets pasted and the chunks describe text that is not.
    #[test]
    fn chunks_are_discarded_when_the_level_flipped_to_off_before_finish() {
        assert!(!use_background_chunks(3, true, 3, true, false));
    }

    #[test]
    fn chunks_are_used_when_all_of_them_came_back_and_the_realtime_text_stands() {
        assert!(use_background_chunks(3, true, 3, true, true));
        assert!(
            !use_background_chunks(0, true, 0, true, true),
            "no chunks means the single-call path"
        );
    }

    /// Spoken "new paragraph" at a chunk boundary: per-chunk `tidy` trims the
    /// break off the chunk's own edge, so the seam has to carry it.
    #[test]
    fn a_break_spoken_at_the_edge_of_a_raw_chunk_is_reported() {
        assert_eq!(seam_breaks("new paragraph so then we moved on", true), (Some("\n\n"), None));
        assert_eq!(seam_breaks("new line so then we moved on", true), (Some("\n"), None));
        assert_eq!(seam_breaks("so then we moved on new paragraph", true), (None, Some("\n\n")));
        assert_eq!(seam_breaks("so then we moved on, new line", true), (None, Some("\n")));
        assert_eq!(seam_breaks("so then we moved on", true), (None, None));
        // A break in the middle is the chunk's own business — `tidy` keeps it.
        assert_eq!(seam_breaks("so then new paragraph we moved on", true), (None, None));
    }

    /// With the user's spoken-commands toggle off, `run_cloud_pipeline` never
    /// runs `commands::apply`, so "new paragraph" stays in the chunk as three
    /// ordinary words. A seam break here would paste those words *and* a
    /// blank line after them.
    #[test]
    fn no_seam_break_is_read_when_spoken_commands_are_off() {
        assert_eq!(seam_breaks("so then we moved on, new paragraph", false), (None, None));
        assert_eq!(seam_breaks("new line so then we moved on", false), (None, None));
        // The same inputs with the toggle on do report one — this test is
        // about the toggle, not about the phrases failing to match.
        assert_eq!(
            seam_breaks("so then we moved on, new paragraph", true),
            (None, Some("\n\n"))
        );
    }

    // --- Finish frames ----------------------------------------------------

    /// Push-to-talk: the client owns the utterance boundary, so `speech_end`
    /// closes it and `end` makes Saaras flush *and* send `session.end`
    /// (measured: final +161–181 ms, session.end +4 ms after it).
    #[test]
    fn a_manual_finish_sends_speech_end_then_end() {
        let frames: Vec<String> = finish_frames(Endpointing::Manual)
            .iter()
            .map(|f| f.to_json())
            .collect();
        assert_eq!(frames, vec![r#"{"event":"speech_end"}"#, r#"{"event":"end"}"#]);
    }

    /// Hands-free: `flush` is a no-op on saaras:v3-realtime under VAD
    /// (measured — the last final only ever arrived after `end`), so the
    /// stop sends `end` and nothing else.
    #[test]
    fn a_vad_finish_sends_only_end() {
        let frames: Vec<String> = finish_frames(Endpointing::Vad)
            .iter()
            .map(|f| f.to_json())
            .collect();
        assert_eq!(frames, vec![r#"{"event":"end"}"#]);
    }

    /// Nothing on the dictation path sends `flush`: it never produces a
    /// `session.end`, so the drain would wait on `QUIET_WINDOW`
    /// (push-to-talk) or the whole `scaled_flush_wait` floor (hands-free).
    #[test]
    fn no_finish_ever_sends_flush() {
        for ep in [Endpointing::Manual, Endpointing::Vad] {
            assert!(finish_frames(ep)
                .iter()
                .all(|f| f.to_json() != r#"{"event":"flush"}"#));
        }
    }

    // --- The two transports (Cloud mode) ----------------------------------

    const RELAY: &str = "https://butterflylabs-relay.example.workers.dev";

    fn cfg_on(lane: crate::sarvam::Lane) -> SessionCfg {
        SessionCfg {
            language_code: "auto".into(),
            stream_type: "balanced".into(),
            mode: "transcribe".into(),
            endpointing: Endpointing::Manual,
            prompt: None,
            lane,
        }
    }

    /// Byte-for-byte what Sarvam has always received: its own header, and no
    /// bearer that a proxy in the middle could pick up.
    #[test]
    fn the_byok_upgrade_carries_sarvams_own_header() {
        let request = upgrade_request(
            &cfg_on(crate::sarvam::Lane::Byok),
            &Transport::Sarvam { key: "sk-live".into() },
        )
        .expect("a request");
        assert_eq!(
            request.headers().get("api-subscription-key").map(|v| v.to_str().unwrap()),
            Some("sk-live")
        );
        assert!(request.headers().get("authorization").is_none());
        assert_eq!(request.uri().host(), Some("api.sarvam.ai"));
    }

    /// The anti-leak rule for Cloud mode: the app holds no Sarvam key at
    /// all, so the only credential on the wire is the user's own sign-in
    /// token — and Sarvam's header must never appear, even empty.
    #[test]
    fn the_cloud_upgrade_carries_the_bearer_and_never_sarvams_header() {
        let request = upgrade_request(
            &cfg_on(crate::sarvam::Lane::Cloud { relay: RELAY.into() }),
            &Transport::Relay {
                base: RELAY.into(),
                bearer: "supabase-access-token".into(),
            },
        )
        .expect("a request");
        assert_eq!(
            request.headers().get("authorization").map(|v| v.to_str().unwrap()),
            Some("Bearer supabase-access-token")
        );
        assert!(
            request.headers().get("api-subscription-key").is_none(),
            "Sarvam's header must never reach the relay"
        );
        assert_eq!(
            request.uri().host(),
            Some("butterflylabs-relay.example.workers.dev")
        );
        assert_eq!(request.uri().path(), "/v1/realtime");
    }

    /// A credential with a newline in it is not a header value. Refusing it
    /// here is what keeps a corrupted credential store from turning into a
    /// request-splitting attempt.
    #[test]
    fn a_credential_that_is_not_a_legal_header_value_is_refused() {
        assert_eq!(
            upgrade_request(
                &cfg_on(crate::sarvam::Lane::Byok),
                &Transport::Sarvam { key: "bad\nkey".into() }
            )
            .expect_err("not a header value"),
            UpgradeFailure::BadCredential
        );
    }

    /// The one close code Cloud mode adds. The relay sends it with reason
    /// `quota` when the week's 2,000 words are spent, and the app has to say
    /// something the user can act on rather than "try again".
    #[test]
    fn the_relays_quota_close_names_the_weekly_limit() {
        use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
        let msg = close_message(
            Some(CloseFrame {
                code: CloseCode::Library(4029),
                reason: "quota".into(),
            }),
            true,
        );
        assert_eq!(msg, MSG_CLOUD_QUOTA);
    }

    /// Every other close code keeps the sentence it had.
    #[test]
    fn the_other_close_codes_are_unchanged() {
        use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
        let msg = close_message(
            Some(CloseFrame {
                code: CloseCode::Library(4000),
                reason: "".into(),
            }),
            false,
        );
        assert_eq!(msg, "Sarvam rejected the request (bad parameter)");
        // The relay's weekly-limit code means nothing on Sarvam's socket, so
        // Bring your own key keeps the generic sentence for it.
        let msg = close_message(
            Some(CloseFrame {
                code: CloseCode::Library(4029),
                reason: "quota".into(),
            }),
            false,
        );
        assert_eq!(msg, "Sarvam closed the connection — try again");
    }

    /// The relay's second close code: a session that reached its 30-minute
    /// ceiling. The fix is a new dictation, not a retry of this one.
    #[test]
    fn the_relays_session_limit_close_says_so() {
        use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
        let msg = close_message(
            Some(CloseFrame {
                code: CloseCode::Library(4030),
                reason: "session_limit".into(),
            }),
            true,
        );
        assert_eq!(msg, MSG_CLOUD_SESSION_LIMIT);
    }

    /// Exactly the relay's two limit codes, and only on the relay lane.
    #[test]
    fn only_the_relays_limit_closes_are_deliberate_endings() {
        use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
        let frame = |code| CloseFrame {
            code: CloseCode::Library(code),
            reason: "".into(),
        };
        assert!(relay_ended_on_purpose(Some(&frame(4029)), true));
        assert!(relay_ended_on_purpose(Some(&frame(4030)), true));
        assert!(!relay_ended_on_purpose(Some(&frame(4000)), true));
        assert!(!relay_ended_on_purpose(Some(&frame(4029)), false));
        assert!(!relay_ended_on_purpose(Some(&frame(4030)), false));
        assert!(!relay_ended_on_purpose(None, true));
    }

    /// The batch rescue is Sarvam REST with a Sarvam key; the relay proxies
    /// no such route and Cloud holds no such key, so a Cloud dictation never
    /// has anything to attempt one with.
    #[test]
    fn only_bring_your_own_key_has_a_batch_credential() {
        assert_eq!(
            batch_credential(&Transport::Sarvam { key: "sk".into() }),
            Some("sk")
        );
        assert_eq!(
            batch_credential(&Transport::Relay {
                base: "https://relay.example.workers.dev".into(),
                bearer: "supabase-access-token".into(),
            }),
            None
        );
    }

    // --- A Cloud session the relay ends on purpose -------------------------
    //
    // Driven end to end through `drain_session`: a scripted loopback socket
    // stands in for the relay's realtime route and a loopback listener for
    // its chat route. No credential is resolved and nothing leaves the
    // machine.

    /// One thing the scripted relay does, in order.
    enum Step {
        /// Send this text frame.
        Send(&'static str),
        /// Close with this code and reason.
        Close(u16, &'static str),
        /// Wait until the app sends a text frame containing this.
        WaitFor(&'static str),
        /// Drop the connection with no close frame, the way a dead socket
        /// ends.
        Drop,
        /// Say nothing for this long, the way a slow link does.
        Pause(Duration),
    }

    const BEGIN: &str = r#"{"event":"session.begin","session_id":"s"}"#;
    const FATAL: &str =
        r#"{"event":"error","code":"internal","is_fatal":true,"message":"upstream failed"}"#;
    const FINAL_0: &str =
        r#"{"event":"transcript.final","utterance_idx":0,"text":"Hello there."}"#;
    const FINAL_1: &str =
        r#"{"event":"transcript.final","utterance_idx":1,"text":"How are you?"}"#;
    const SESSION_END: &str = r#"{"event":"session.end"}"#;
    /// What `FINAL_0` and `FINAL_1` assemble to.
    const SAID: &str = "Hello there. How are you?";

    /// The app's end of a loopback WebSocket whose server runs `script`.
    async fn scripted_socket(script: Vec<Step>) -> WsStream {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind loopback listener");
        let addr = listener.local_addr().expect("local addr");
        tokio::spawn(async move {
            let Ok((tcp, _)) = listener.accept().await else {
                return;
            };
            let Ok(mut ws) = tokio_tungstenite::accept_async(tcp).await else {
                return;
            };
            for step in script {
                match step {
                    Step::Send(text) => {
                        if ws.send(Message::Text(text.into())).await.is_err() {
                            return;
                        }
                    }
                    Step::Close(code, reason) => {
                        use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
                        let _ = ws
                            .send(Message::Close(Some(CloseFrame {
                                code: CloseCode::Library(code),
                                reason: reason.into(),
                            })))
                            .await;
                    }
                    Step::WaitFor(needle) => loop {
                        match ws.next().await {
                            Some(Ok(Message::Text(text))) if text.contains(needle) => break,
                            Some(Ok(_)) => continue,
                            _ => return,
                        }
                    },
                    Step::Drop => return,
                    Step::Pause(quiet) => tokio::time::sleep(quiet).await,
                }
            }
            // Hold the socket until the app lets go of it.
            while let Some(Ok(_)) = ws.next().await {}
        });
        let (client, _) = tokio_tungstenite::connect_async(format!("ws://{addr}"))
            .await
            .expect("loopback websocket");
        client
    }

    /// Head and body of one HTTP request, read to its `Content-Length`.
    async fn read_http_request(socket: &tokio::net::TcpStream) -> String {
        let mut buf = Vec::new();
        let mut chunk = [0u8; 8192];
        // The whole request's length, once its head is in. Found once: a
        // batch rescue's body is a megabyte of audio, too much to rescan on
        // every read.
        let mut whole: Option<usize> = None;
        loop {
            if socket.readable().await.is_err() {
                break;
            }
            match socket.try_read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => buf.extend_from_slice(&chunk[..n]),
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => continue,
                Err(_) => break,
            }
            if whole.is_none() {
                if let Some(head_end) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                    let head = String::from_utf8_lossy(&buf[..head_end]);
                    let length = head
                        .lines()
                        .find_map(|line| {
                            let (name, value) = line.split_once(':')?;
                            if name.trim().eq_ignore_ascii_case("content-length") {
                                value.trim().parse::<usize>().ok()
                            } else {
                                None
                            }
                        })
                        .unwrap_or(0);
                    whole = Some(head_end + 4 + length);
                }
            }
            if whole.is_some_and(|whole| buf.len() >= whole) {
                break;
            }
        }
        String::from_utf8_lossy(&buf).into_owned()
    }

    /// A one-request chat route on loopback, returned as the relay base URL
    /// the polish call is built from. A `200` answers as a model that
    /// followed its instructions would: `reply`, then the end marker
    /// the request minted, read back out of the request. Any other status
    /// answers with `reply` as the body, the way the relay's refusals do.
    async fn chat_route(status: u16, reply: &'static str) -> String {
        chat_route_by(move |_| status, reply).await
    }

    /// [`chat_route`] that answers `200` only to a request carrying
    /// `bearer`, and the relay's `401` to any other.
    async fn chat_route_for_bearer(bearer: &'static str, reply: &'static str) -> String {
        let signed = format!("authorization: bearer {bearer}").to_lowercase();
        chat_route_by(
            move |request| {
                if request.to_lowercase().contains(&signed) {
                    200
                } else {
                    401
                }
            },
            reply,
        )
        .await
    }

    /// [`chat_route`] with the status chosen from the request it receives.
    async fn chat_route_by(
        status_for: impl FnOnce(&str) -> u16 + Send + 'static,
        reply: &'static str,
    ) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind loopback listener");
        let addr = listener.local_addr().expect("local addr");
        tokio::spawn(async move {
            let Ok((socket, _)) = listener.accept().await else {
                return;
            };
            let request = read_http_request(&socket).await;
            let status = status_for(&request);
            let body = if status == 200 {
                let at = request.rfind("<<").expect("the request carries its marker");
                let end = at + request[at..].find(">>").expect("a closed marker") + 2;
                serde_json::json!({
                    "choices": [{
                        "finish_reason": "stop",
                        "message": { "content": format!("{reply}\n{}", &request[at..end]) }
                    }],
                    "usage": { "prompt_tokens": 1, "completion_tokens": 1 }
                })
                .to_string()
            } else {
                reply.to_string()
            };
            let response = format!(
                "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let mut bytes = response.as_bytes();
            while !bytes.is_empty() {
                if socket.writable().await.is_err() {
                    return;
                }
                match socket.try_write(bytes) {
                    Ok(n) => bytes = &bytes[n..],
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => continue,
                    Err(_) => return,
                }
            }
        });
        format!("http://{addr}")
    }

    /// A relay base with nothing listening behind it: the socket is bound but
    /// never listens, so every polish call is refused at connect. Keep the
    /// socket for the whole test: a port released early could be handed to
    /// another test's listener, which would then answer.
    fn unreachable_chat() -> (tokio::net::TcpSocket, String) {
        let socket = tokio::net::TcpSocket::new_v4().expect("create socket");
        socket
            .bind("127.0.0.1:0".parse().expect("loopback address"))
            .expect("bind loopback socket");
        let addr = socket.local_addr().expect("local addr");
        (socket, format!("http://{addr}"))
    }

    fn kind(msg: &ControlMsg) -> &'static str {
        match msg {
            ControlMsg::FinalResult { .. } => "FinalResult",
            ControlMsg::CloudError { .. } => "CloudError",
            ControlMsg::CloudTruncated { .. } => "CloudTruncated",
            ControlMsg::CloudEnded { .. } => "CloudEnded",
            _ => "another message",
        }
    }

    fn kinds(heard: &[ControlMsg]) -> Vec<&'static str> {
        heard.iter().map(kind).collect()
    }

    /// When the user lets go of the key in [`dictation`].
    enum Release {
        /// After this long, whatever the relay has done by then.
        After(Duration),
        /// Never on their own: they are still talking when the relay acts.
        Held,
    }

    /// Everything the controller hears from one Cloud dictation against the
    /// scripted relay, with its chat route at `chat`, in order.
    ///
    /// Plays the controller's side the way `Controller::handle` does: a
    /// mid-recording `CloudError` (`req_id == 0`) cancels the recording, and
    /// a `CloudEnded` finishes it as a released key would. Stops listening at
    /// the dictation's last word — a result, a truncation, a finalize-time
    /// error, or the cancel it sent — and gives up (cancelling) after ten
    /// seconds of silence, so a drain that never tells the controller
    /// anything fails the test instead of hanging it.
    async fn dictation(script: Vec<Step>, chat: String, release: Release) -> Vec<ControlMsg> {
        const BEARER: &str = "supabase-access-token";
        dictation_with(script, chat, release, BEARER, BEARER).await
    }

    /// [`dictation`], connected with the bearer `connected` while a fresh
    /// look-up of the credential would now give `fresh`.
    async fn dictation_with(
        script: Vec<Step>,
        chat: String,
        release: Release,
        connected: &'static str,
        fresh: &'static str,
    ) -> Vec<ControlMsg> {
        let ws = scripted_socket(script).await;
        let (ctl_tx, ctl_rx) = crossbeam_channel::unbounded();
        let (cmd_tx, mut cmd_rx) = tokio::sync::mpsc::unbounded_channel();
        let cleanup = Arc::new(RwLock::new(CleanupSettings::default()));
        let http = reqwest::Client::new();
        let mut window = crate::format::timing::TimingWindow::default();
        let transport = Transport::Relay {
            base: chat.clone(),
            bearer: connected.into(),
        };
        let now = Transport::Relay {
            base: chat.clone(),
            bearer: fresh.into(),
        };
        let credential: Credential = Arc::new(move || {
            let now = now.clone();
            Box::pin(async move { Ok(now) })
        });
        let drain = drain_session(
            &ctl_tx,
            &cleanup,
            &http,
            &mut cmd_rx,
            1,
            cfg_on(Lane::Cloud { relay: chat }),
            transport,
            credential,
            ws,
            batch::BATCH_URL,
            &mut window,
        );
        let controller = async {
            let finish = || {
                let _ = cmd_tx.send(CloudCmd::Finish {
                    req_id: 7,
                    end_of_speech: std::time::Instant::now(),
                    duration_ms: 1_500,
                });
            };
            let started = tokio::time::Instant::now();
            let mut released = false;
            let mut heard = Vec::new();
            loop {
                if let Ok(msg) = ctl_rx.try_recv() {
                    let last = match &msg {
                        ControlMsg::CloudError { req_id: 0, .. } => {
                            let _ = cmd_tx.send(CloudCmd::Cancel);
                            true
                        }
                        ControlMsg::CloudEnded { .. } => {
                            if !released {
                                released = true;
                                finish();
                            }
                            false
                        }
                        _ => true,
                    };
                    heard.push(msg);
                    if last {
                        break;
                    }
                    continue;
                }
                if let Release::After(after) = release {
                    if !released && started.elapsed() >= after {
                        released = true;
                        finish();
                    }
                }
                if started.elapsed() > Duration::from_secs(10) {
                    let _ = cmd_tx.send(CloudCmd::Cancel);
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            heard
        };
        let (carryover, heard) = tokio::join!(drain, controller);
        assert!(carryover.is_none(), "no new dictation was started");
        heard
    }

    /// The relay's 30-minute ceiling, reached while the user is still
    /// talking. The controller is told at once — it must not wait for a key
    /// release that, hands-free, may be minutes away — and what was
    /// transcribed up to then is delivered, polished since the chat route
    /// still answers, with the reason it stopped. Filing it in History
    /// unpasted would treat a deliberate, documented ending as a dead socket.
    #[tokio::test]
    async fn a_session_limit_close_ends_the_recording_and_delivers_the_finals_polished() {
        let chat = chat_route(200, "Hello there! How are you?").await;
        let heard = dictation(
            vec![
                Step::Send(BEGIN),
                Step::Send(FINAL_0),
                Step::Send(FINAL_1),
                Step::Close(4030, "session_limit"),
            ],
            chat,
            Release::Held,
        )
        .await;
        assert_eq!(kinds(&heard), ["CloudEnded", "FinalResult"]);
        let ControlMsg::CloudEnded { session } = &heard[0] else { unreachable!() };
        assert_eq!(*session, 1, "the end names the session it ended");
        let ControlMsg::FinalResult { text, raw, notice, cut_short, req_id, .. } = &heard[1] else {
            unreachable!()
        };
        assert_eq!(*req_id, 7);
        assert_eq!(raw, SAID);
        assert_eq!(text, "Hello there! How are you?", "the polished text is pasted");
        assert_eq!(notice.as_deref(), Some(MSG_CLOUD_SESSION_LIMIT));
        assert!(cut_short, "History has to be able to say this was cut short");
    }

    /// The weekly limit, reached mid-dictation, with the chat route down as
    /// well. Still ended at once and still delivered — as the rule-cleaned
    /// transcript — and the notice is the limit rather than "formatting
    /// failed": the limit is what the user has to act on. No batch rescue is
    /// tried (nothing would answer it on this lane); the finals are the whole
    /// answer.
    #[tokio::test]
    async fn a_quota_close_ends_the_recording_and_delivers_the_finals_unpolished() {
        let (_socket, chat) = unreachable_chat();
        let heard = dictation(
            vec![
                Step::Send(BEGIN),
                Step::Send(FINAL_0),
                Step::Send(FINAL_1),
                Step::Close(4029, "quota"),
            ],
            chat,
            Release::Held,
        )
        .await;
        assert_eq!(kinds(&heard), ["CloudEnded", "FinalResult"]);
        let ControlMsg::FinalResult { text, raw, notice, cut_short, .. } = &heard[1] else {
            unreachable!()
        };
        assert_eq!(raw, SAID);
        assert_eq!(
            *text,
            crate::cleanup::run_cloud_pipeline(SAID.to_string(), &CleanupSettings::default()).0,
            "the rule-cleaned transcript stands in for the polish"
        );
        assert_eq!(notice.as_deref(), Some(MSG_CLOUD_QUOTA));
        assert!(cut_short);
    }

    /// The weekly limit at connect — every dictation once the week is spent:
    /// the relay accepts the socket and closes it at once, before a word. No
    /// rescue exists on this lane, so recording on would only lose whatever
    /// the user says next. The controller is told while the key is still
    /// down (`req_id == 0` is its "stop the recording now"), and nothing
    /// else follows.
    #[tokio::test]
    async fn a_quota_close_before_any_words_stops_the_recording_at_once() {
        let (_socket, chat) = unreachable_chat();
        let heard = dictation(
            vec![Step::Close(4029, "quota")],
            chat,
            Release::Held,
        )
        .await;
        assert_eq!(kinds(&heard), ["CloudError"]);
        let ControlMsg::CloudError { req_id, session, message } = &heard[0] else {
            unreachable!()
        };
        assert_eq!((*req_id, *session), (0, 1), "a mid-recording error for this session");
        assert_eq!(message, MSG_CLOUD_QUOTA);
    }

    /// The same reasoning for a socket that simply dies before any words on
    /// the relay lane: nothing to keep and nothing to rescue it with, so the
    /// user hears now rather than at the release.
    #[tokio::test]
    async fn a_relay_socket_that_dies_before_any_words_stops_the_recording_at_once() {
        let (_socket, chat) = unreachable_chat();
        let heard = dictation(
            vec![Step::Send(BEGIN), Step::Drop],
            chat,
            Release::Held,
        )
        .await;
        assert_eq!(kinds(&heard), ["CloudError"]);
        let ControlMsg::CloudError { req_id, message, .. } = &heard[0] else { unreachable!() };
        assert_eq!(*req_id, 0);
        assert_eq!(*message, msg_lost(true));
    }

    /// A relay socket that dies after some words: nothing can rescue what the
    /// user says next, so the recording stops now, and the words already
    /// transcribed are filed rather than pasted as if they were everything.
    #[tokio::test]
    async fn a_relay_socket_that_dies_after_words_stops_the_recording_at_once() {
        let (_socket, chat) = unreachable_chat();
        let heard = dictation(
            vec![Step::Send(BEGIN), Step::Send(FINAL_0), Step::Drop],
            chat,
            Release::Held,
        )
        .await;
        assert_eq!(kinds(&heard), ["CloudEnded", "CloudTruncated"]);
        let ControlMsg::CloudTruncated { raw, message, .. } = &heard[1] else { unreachable!() };
        assert_eq!(raw, "Hello there.");
        assert_eq!(*message, msg_lost(true));
    }

    /// The same for a fatal error frame and for the server ending the session
    /// early: with words in hand the recording stops and they are filed, and
    /// with none it stops with the error.
    #[tokio::test]
    async fn a_fatal_error_or_an_early_end_on_the_relay_stops_the_recording_at_once() {
        let (_socket, chat) = unreachable_chat();
        let heard = dictation(vec![Step::Send(BEGIN), Step::Send(FATAL)], chat, Release::Held).await;
        assert_eq!(kinds(&heard), ["CloudError"]);
        let ControlMsg::CloudError { req_id, message, .. } = &heard[0] else { unreachable!() };
        assert_eq!(*req_id, 0);
        assert_eq!(*message, msg_service_error(true));

        let (_socket, chat) = unreachable_chat();
        let heard = dictation(
            vec![Step::Send(BEGIN), Step::Send(FINAL_0), Step::Send(SESSION_END)],
            chat,
            Release::Held,
        )
        .await;
        assert_eq!(kinds(&heard), ["CloudEnded", "CloudTruncated"]);
        let ControlMsg::CloudTruncated { message, .. } = &heard[1] else { unreachable!() };
        assert_eq!(*message, msg_session_ended(true));
    }

    /// Only a session with a rescue still possible keeps recording once its
    /// socket has died: Bring your own key, while the tee has room for the
    /// batch rescue to take the whole utterance.
    #[test]
    fn only_a_session_with_a_rescue_left_records_on_after_its_socket_dies() {
        assert!(rescue_left(false, 0));
        assert!(rescue_left(false, MAX_FALLBACK_SAMPLES / 2));
        assert!(!rescue_left(false, MAX_FALLBACK_SAMPLES), "the tee is full");
        assert!(!rescue_left(true, 0), "the relay lane has no rescue");
    }

    /// The stop has to land while the utterance still fits the rescue. The
    /// audio already on its way when the stop is asked for still joins the
    /// utterance, so waiting for a full tee leaves a dictation just over the
    /// rescue's limit, which the rescue refuses.
    #[test]
    fn the_recording_stops_before_the_tee_is_full() {
        let one_chunk = FALLBACK_SAMPLE_RATE_HZ / 10;
        assert!(!rescue_left(false, MAX_FALLBACK_SAMPLES - one_chunk));
        assert!(rescue_left(false, MAX_FALLBACK_SAMPLES - RESCUE_STOP_MARGIN - 1));
        assert!(!rescue_left(false, MAX_FALLBACK_SAMPLES - RESCUE_STOP_MARGIN));
    }

    /// A one-request batch route on loopback that answers with `transcript`
    /// in Sarvam's documented shape, returned as its URL. The request's
    /// size, head and body, is sent back on the receiver once it is read.
    async fn batch_route(
        transcript: &'static str,
    ) -> (String, tokio::sync::oneshot::Receiver<usize>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind loopback listener");
        let addr = listener.local_addr().expect("local addr");
        let (size_tx, size_rx) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let Ok((socket, _)) = listener.accept().await else {
                return;
            };
            let request = read_http_request(&socket).await;
            let _ = size_tx.send(request.len());
            let body = serde_json::json!({ "request_id": "r", "transcript": transcript }).to_string();
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let mut bytes = response.as_bytes();
            while !bytes.is_empty() {
                if socket.writable().await.is_err() {
                    return;
                }
                match socket.try_write(bytes) {
                    Ok(n) => bytes = &bytes[n..],
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => continue,
                    Err(_) => return,
                }
            }
        });
        (format!("http://{addr}"), size_rx)
    }

    /// Bring your own key, hands-free: the socket dies before a word and the
    /// user talks on. When the tee nears the rescue's limit the recording is
    /// stopped as a released key would stop it, and the batch rescue
    /// transcribes everything said. Failing it there instead, as the relay
    /// lane must, would throw away up to thirty seconds of speech the rescue
    /// can still take.
    #[tokio::test]
    async fn a_byok_socket_that_dies_before_any_words_is_rescued_when_the_tee_fills() {
        const KEY: &str = "sk-test";
        const RESCUED: &str = "Everything I said while the socket was down.";
        let (batch_url, request_size) = batch_route(RESCUED).await;
        let ws = scripted_socket(vec![Step::Send(BEGIN), Step::Drop]).await;
        let (ctl_tx, ctl_rx) = crossbeam_channel::unbounded();
        let (cmd_tx, mut cmd_rx) = tokio::sync::mpsc::unbounded_channel();
        // AI Polish off: nothing in this dictation may reach a real host.
        let cleanup = Arc::new(RwLock::new(CleanupSettings {
            level: crate::format::level::CleanupLevel::Off,
            ..CleanupSettings::default()
        }));
        let http = reqwest::Client::new();
        let mut window = crate::format::timing::TimingWindow::default();
        let credential: Credential =
            Arc::new(|| Box::pin(async { Ok(Transport::Sarvam { key: KEY.into() }) }));
        let drain = drain_session(
            &ctl_tx,
            &cleanup,
            &http,
            &mut cmd_rx,
            1,
            cfg_on(crate::sarvam::Lane::Byok),
            Transport::Sarvam { key: KEY.into() },
            credential,
            ws,
            &batch_url,
            &mut window,
        );
        let controller = async {
            // Let the socket die first, then talk on: 100 ms chunks, well
            // past the rescue's limit.
            tokio::time::sleep(Duration::from_millis(200)).await;
            let chunk = vec![0.1_f32; FALLBACK_SAMPLE_RATE_HZ / 10];
            for _ in 0..MAX_FALLBACK_SAMPLES / chunk.len() + 20 {
                let _ = cmd_tx.send(CloudCmd::Audio(chunk.clone()));
            }
            let started = tokio::time::Instant::now();
            let mut heard = Vec::new();
            loop {
                if let Ok(msg) = ctl_rx.try_recv() {
                    let last = match &msg {
                        ControlMsg::CloudEnded { .. } => {
                            let _ = cmd_tx.send(CloudCmd::Finish {
                                req_id: 7,
                                end_of_speech: std::time::Instant::now(),
                                duration_ms: 29_800,
                            });
                            false
                        }
                        ControlMsg::CloudError { req_id: 0, .. } => {
                            let _ = cmd_tx.send(CloudCmd::Cancel);
                            true
                        }
                        _ => true,
                    };
                    heard.push(msg);
                    if last {
                        break;
                    }
                    continue;
                }
                if started.elapsed() > Duration::from_secs(10) {
                    let _ = cmd_tx.send(CloudCmd::Cancel);
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            heard
        };
        let (carryover, heard) = tokio::join!(drain, controller);
        assert!(carryover.is_none(), "no new dictation was started");
        assert_eq!(kinds(&heard), ["CloudEnded", "FinalResult"]);
        let ControlMsg::FinalResult { raw, req_id, .. } = &heard[1] else { unreachable!() };
        assert_eq!(*req_id, 7);
        assert_eq!(raw, RESCUED, "the batch rescue's transcript is the dictation");
        // The rescue was sent nearly the whole tee: 16-bit samples, so about
        // two bytes for each one tee'd.
        let size = request_size.await.expect("the rescue reached the batch route");
        assert!(
            size > 2 * (MAX_FALLBACK_SAMPLES - RESCUE_STOP_MARGIN),
            "the rescue carried the speech said after the socket died: {size} bytes"
        );
    }

    /// After `end`, the last final can take more than a moment on a slow
    /// link. `session.end` always follows it, so the drain waits for that.
    #[tokio::test]
    async fn a_final_that_arrives_late_after_end_is_still_in_the_transcript() {
        let chat = chat_route(200, "Hello there! How are you?").await;
        let heard = dictation(
            vec![
                Step::Send(BEGIN),
                Step::Send(FINAL_0),
                Step::WaitFor(r#""event":"end""#),
                Step::Pause(Duration::from_millis(1_200)),
                Step::Send(FINAL_1),
                Step::Send(SESSION_END),
            ],
            chat,
            Release::After(Duration::from_millis(300)),
        )
        .await;
        assert_eq!(kinds(&heard), ["FinalResult"]);
        let ControlMsg::FinalResult { raw, notice, .. } = &heard[0] else { unreachable!() };
        assert_eq!(raw, SAID);
        assert_eq!(*notice, None);
    }

    /// Both finals can come after `end`, the second well after the first on
    /// a slow link. The drain waits for `session.end` rather than cutting the
    /// second one off.
    #[tokio::test]
    async fn a_second_final_that_arrives_late_after_end_is_still_in_the_transcript() {
        let chat = chat_route(200, "Hello there! How are you?").await;
        let heard = dictation(
            vec![
                Step::Send(BEGIN),
                Step::WaitFor(r#""event":"end""#),
                Step::Send(FINAL_0),
                Step::Pause(Duration::from_millis(800)),
                Step::Send(FINAL_1),
                Step::Send(SESSION_END),
            ],
            chat,
            Release::After(Duration::from_millis(300)),
        )
        .await;
        assert_eq!(kinds(&heard), ["FinalResult"]);
        let ControlMsg::FinalResult { raw, notice, .. } = &heard[0] else { unreachable!() };
        assert_eq!(raw, SAID);
        assert_eq!(*notice, None);
    }

    /// A drain that ends without `session.end` cannot know it has every final.
    /// What arrived is pasted, and the user is told the end may be missing.
    #[tokio::test]
    async fn a_drain_that_ends_without_session_end_says_the_end_may_be_missing() {
        let chat = chat_route(200, "Hello there! How are you?").await;
        let heard = dictation(
            vec![
                Step::Send(BEGIN),
                Step::Send(FINAL_0),
                Step::WaitFor(r#""event":"end""#),
                Step::Send(FINAL_1),
            ],
            chat,
            Release::After(Duration::from_millis(300)),
        )
        .await;
        assert_eq!(kinds(&heard), ["FinalResult"]);
        let ControlMsg::FinalResult { raw, notice, .. } = &heard[0] else { unreachable!() };
        assert_eq!(raw, SAID);
        assert_eq!(notice.as_deref(), Some(msg_end_unconfirmed(true).as_str()));
    }

    /// A Cloud dictation's bearer is looked up again for each polish call, so
    /// a dictation that outlives the token it connected with still polishes.
    #[tokio::test]
    async fn the_polish_asks_for_a_fresh_bearer_instead_of_reusing_the_connect_one() {
        let chat = chat_route_for_bearer("fresh-token", "Hello there!").await;
        let heard = dictation_with(
            vec![
                Step::Send(BEGIN),
                Step::Send(FINAL_0),
                Step::WaitFor(r#""event":"end""#),
                Step::Send(SESSION_END),
            ],
            chat,
            Release::After(Duration::from_millis(300)),
            "stale-token",
            "fresh-token",
        )
        .await;
        assert_eq!(kinds(&heard), ["FinalResult"]);
        let ControlMsg::FinalResult { text, notice, .. } = &heard[0] else { unreachable!() };
        assert_eq!(text, "Hello there!");
        assert_eq!(*notice, None);
    }

    /// A sign-in refresh that hangs must not hold the polish up: past
    /// `CREDENTIAL_WAIT` the polish goes out with the credential the session
    /// connected with.
    #[tokio::test]
    async fn a_credential_look_up_that_hangs_falls_back_to_the_connected_one() {
        let connected = Transport::Relay {
            base: RELAY.into(),
            bearer: "connected-token".into(),
        };
        let credential: Credential = Arc::new(|| Box::pin(std::future::pending()));
        let started = Instant::now();
        let backend = tokio::time::timeout(
            crate::sarvam::CREDENTIAL_WAIT + Duration::from_secs(2),
            fresh_backend_for(&crate::endpoint::CustomSlot::default(), &credential, &connected, "sarvam-m"),
        )
        .await
        .expect("the look-up is given up on")
        .expect("the relay answers the polish");
        assert_eq!(backend.api_key, "connected-token");
        assert!(started.elapsed() < crate::sarvam::CREDENTIAL_WAIT + Duration::from_secs(1));
    }

    /// When the custom endpoint does the polish, this dictation's own
    /// credential plays no part in the call, so it is not looked up at all.
    #[tokio::test]
    async fn a_polish_on_the_custom_endpoint_looks_up_no_credential() {
        let slot = crate::endpoint::CustomSlot {
            base_url: "http://127.0.0.1:9".into(),
            model: "qwen3:8b".into(),
            use_for_polish: true,
            ..Default::default()
        };
        let asked = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let credential: Credential = {
            let asked = asked.clone();
            Arc::new(move || {
                asked.store(true, std::sync::atomic::Ordering::SeqCst);
                Box::pin(std::future::pending())
            })
        };
        let connected = Transport::Relay {
            base: RELAY.into(),
            bearer: "connected-token".into(),
        };
        let backend = tokio::time::timeout(
            Duration::from_secs(1),
            fresh_backend_for(&slot, &credential, &connected, "sarvam-m"),
        )
        .await
        .expect("nothing is waited on")
        .expect("the custom endpoint answers the polish");
        assert_eq!(backend.kind, crate::format::backend::BackendKind::Custom);
        assert!(!asked.load(std::sync::atomic::Ordering::SeqCst));
    }

    /// A session that ended normally, whose polish the relay refused because
    /// the week's chat calls are spent. The transcript is pasted unpolished,
    /// and the notice says why in the words the quota close uses — not
    /// "formatting failed", which reads as a fault worth retrying. Not cut
    /// short: every word the user said is there.
    #[tokio::test]
    async fn a_spent_weekly_chat_limit_is_named_instead_of_formatting_failed() {
        let chat = chat_route(429, crate::format::backend::RELAY_WEEKLY_CHAT_LIMIT).await;
        let heard = dictation(
            vec![
                Step::Send(BEGIN),
                Step::Send(FINAL_0),
                Step::WaitFor(r#""event":"end""#),
                Step::Send(SESSION_END),
            ],
            chat,
            Release::After(Duration::from_millis(300)),
        )
        .await;
        assert_eq!(kinds(&heard), ["FinalResult"]);
        let ControlMsg::FinalResult { raw, notice, cut_short, .. } = &heard[0] else {
            unreachable!()
        };
        assert_eq!(raw, "Hello there.");
        assert_eq!(notice.as_deref(), Some(MSG_CLOUD_QUOTA));
        assert!(!cut_short);
    }

    // --- What each lane is told when the upgrade fails --------------------

    /// The decision table the connect loop runs on, for both lanes. Every row
    /// is a failure a user can actually meet, and the split that matters most
    /// is the first two: one `401` is a token that can be refreshed, and the
    /// second one — the same request with a *new* token — is a sign-in the
    /// user has to redo themselves.
    #[test]
    fn the_upgrade_table_answers_each_status_for_each_lane() {
        let relay = true;
        let byok = false;
        // The relay's first 401: refresh and dial again.
        assert_eq!(upgrade_verdict(401, relay, false), Verdict::Reauth);
        // Its second: the fresh token was refused too.
        assert_eq!(
            upgrade_verdict(401, relay, true),
            Verdict::Fail(MSG_CLOUD_SIGN_IN.to_string())
        );
        // Bring your own key has nothing to refresh, so a 401 is the key —
        // and this sentence is exactly the one that lane has always shown.
        assert_eq!(
            upgrade_verdict(401, byok, false),
            Verdict::Fail("Sarvam key rejected — update it in Settings → Speech engine".to_string())
        );
        assert_eq!(
            upgrade_verdict(403, byok, false),
            Verdict::Fail("Sarvam key rejected — update it in Settings → Speech engine".to_string())
        );
        // A relay 403 is not about a key the user holds: no key field, no
        // Settings → Speech engine, so it falls through to the generic sentence.
        assert_eq!(
            upgrade_verdict(403, relay, false),
            Verdict::Fail("Butterfly Labs returned HTTP 403 — try again".to_string())
        );
        // Transient on both lanes, with the sentence to use once the attempt
        // budget is spent.
        assert_eq!(
            upgrade_verdict(503, byok, false),
            Verdict::Retry("Sarvam returned HTTP 503 — try again".to_string())
        );
        assert_eq!(
            upgrade_verdict(503, relay, false),
            Verdict::Retry("Butterfly Labs returned HTTP 503 — try again".to_string())
        );
        assert_eq!(
            upgrade_verdict(429, byok, false),
            Verdict::Retry("Sarvam rate limit hit — try again in a moment".to_string())
        );
        assert_eq!(
            upgrade_verdict(429, relay, false),
            Verdict::Retry(MSG_CLOUD_BUSY.to_string())
        );
        // The relay's `400 bad query`: the one client-controlled part of that
        // query big enough to be refused is the dictionary, and "try again"
        // is advice that cannot work — the next attempt sends the same query.
        assert_eq!(
            upgrade_verdict(400, relay, false),
            Verdict::Fail(MSG_CLOUD_BAD_QUERY.to_string())
        );
        assert_eq!(
            upgrade_verdict(400, byok, false),
            Verdict::Fail("Sarvam returned HTTP 400 — try again".to_string())
        );
    }

    /// A Cloud user has no Sarvam account, so no sentence they can be shown may
    /// name one. The Bring-your-own-key column of the same table is asserted
    /// above, byte for byte, so this cannot be satisfied by blanding both
    /// lanes.
    #[test]
    fn a_cloud_dictation_is_never_told_about_sarvam() {
        use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
        let mut sentences = vec![
            msg_lost(true),
            msg_unreachable(true),
            close_message(None, true),
            // The two the relay pipes through from upstream untouched: a
            // fatal error frame, and a `session.end` before this dictation
            // asked to finish.
            msg_service_error(true),
            msg_session_ended(true),
        ];
        // The commonest failure of all — no socket at any level. A Cloud user
        // on a train, behind a captive portal or inside a corporate proxy
        // meets one of these six, not an HTTP status.
        for failure in [
            NetFailure::NameNotResolved,
            NetFailure::Refused,
            NetFailure::Timeout,
            NetFailure::Tls,
            NetFailure::ServiceUnavailable,
            NetFailure::Other,
        ] {
            sentences.push(failure.user_message_for(host_name(true)));
        }
        // The relay's own limits, said on their own and through the close
        // codes that carry them below.
        sentences.push(MSG_CLOUD_QUOTA.to_string());
        sentences.push(MSG_CLOUD_SESSION_LIMIT.to_string());
        for code in [1003u16, 1008, 1011, 4000, 4029, 4030, 1006] {
            for reason in ["", "quota exceeded"] {
                sentences.push(close_message(
                    Some(CloseFrame {
                        code: CloseCode::Library(code),
                        reason: reason.into(),
                    }),
                    true,
                ));
            }
        }
        for status in [400u16, 401, 403, 429, 500, 503] {
            for reauthed in [false, true] {
                match upgrade_verdict(status, true, reauthed) {
                    Verdict::Reauth => {}
                    Verdict::Retry(m) | Verdict::Fail(m) => sentences.push(m),
                }
            }
        }
        for sentence in sentences {
            assert!(
                !sentence.contains("Sarvam"),
                "a Cloud user was told about Sarvam: {sentence}"
            );
        }
    }

    /// And the other half of that promise: Bring your own key still says
    /// exactly what it has always said.
    #[test]
    fn the_byok_sentences_are_unchanged() {
        assert_eq!(
            msg_unreachable(false),
            "Couldn't reach Sarvam — check your internet connection"
        );
        assert_eq!(msg_lost(false), "Lost the connection to Sarvam — try again");
        assert_eq!(
            close_message(None, false),
            "Sarvam closed the connection — try again"
        );
        assert_eq!(msg_service_error(false), "Sarvam service error — try again");
        // The relay's session-limit code means nothing on Sarvam's socket.
        assert_eq!(
            close_message(
                Some(CloseFrame {
                    code: tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode::Library(4030),
                    reason: "".into(),
                }),
                false
            ),
            "Sarvam closed the connection — try again"
        );
        assert_eq!(
            msg_session_ended(false),
            "Sarvam ended the session — try again"
        );
        assert_eq!(
            NetFailure::Other.user_message_for(host_name(false)),
            NetFailure::Other.user_message(),
            "the offline sentences are `net_error`'s own, unchanged"
        );
    }
}
