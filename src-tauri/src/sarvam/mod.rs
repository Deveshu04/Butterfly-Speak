//! Sarvam AI cloud provider: realtime streaming ASR over WebSocket plus a
//! chat-completions polish pass. All network I/O lives on one long-lived
//! dispatcher task on the tauri (tokio) runtime; the controller talks to it
//! through a `CloudCmd` channel and hears back via `ControlMsg`.

pub mod batch;
// The asynchronous *job* API, which is a different product from `batch`
// despite the name: `batch` is the one-shot 30 s-capped REST rescue for a
// realtime session that produced nothing, `batch_job` is the five-call
// create/upload/start/poll/download lifecycle that file import needs because
// an imported recording is never 30 seconds long.
pub mod batch_job;
pub mod chat;
pub mod codec;
pub mod incremental;
pub mod key;
pub mod net_error;
pub mod translate;
pub mod ws;

use std::sync::{Arc, RwLock};
use std::time::Instant;

/// Sarvam's own realtime socket — the Bring-your-own-key lane's host, and
/// the host the relay itself dials upstream.
pub const REALTIME_WS_URL: &str = "wss://api.sarvam.ai/speech-to-text-realtime/ws";
pub const REALTIME_MODEL: &str = "saaras:v3-realtime";
pub const AUTH_HEADER: &str = "api-subscription-key";

/// The Butterfly Labs relay, which is the only place the Butterfly Labs Sarvam
/// key exists. Overridable by the hidden `cloud.relayUrl` setting — the gate
/// harness and a local `wrangler dev` point at their own — but never shown in
/// the UI.
pub const DEFAULT_RELAY_URL: &str = "https://butterflylabs-relay.butterflylabs.workers.dev";

/// The relay's realtime route. Its query string is the same one Sarvam gets;
/// the relay rebuilds it from an allowlist before it dials upstream.
pub const RELAY_REALTIME_PATH: &str = "/v1/realtime";

/// This week's Cloud allowance is spent. Defined beside the chat error that
/// also carries it (`format::backend::WeeklyChatLimit`), because `fmtbench`
/// compiles `format` and `sarvam::chat` without this module.
pub use crate::format::backend::MSG_CLOUD_QUOTA;

/// The relay closed the socket because this session reached its 30-minute
/// ceiling (close code `4030`, reason `session_limit`). Unlike the weekly
/// limit it clears at once: the next dictation opens a new session.
pub const MSG_CLOUD_SESSION_LIMIT: &str =
    "Cloud sessions end after 30 minutes — start a new dictation";

/// The relay would not take this sign-in — said after one silent refresh has
/// already been tried, so it really does mean "sign in again".
pub const MSG_CLOUD_SIGN_IN: &str = "Sign in again to keep using Cloud";

/// The relay's `429`: too many sockets at once, or the per-user chat rate.
/// Both clear by themselves, which is the difference from [`MSG_CLOUD_QUOTA`]
/// — that one lasts until the week rolls over.
pub const MSG_CLOUD_BUSY: &str = "Cloud is busy — try again in a moment";

/// The relay's `400 bad query` on the realtime route: the query it rebuilds
/// from its allowlist did not survive the shaping. Exactly one part of that
/// query is the user's and big enough to be refused — the dictionary, which
/// travels as `prompt` and is capped at 2,000 characters — so the sentence
/// names it. Deliberately does *not* say "try again": the next attempt would
/// send the very same query.
pub const MSG_CLOUD_BAD_QUERY: &str = "Cloud wouldn't take this dictation — try a shorter dictionary";

/// The Bring-your-own-key lane's missing credential. Here rather than in
/// `ws.rs` so that the three "this lane has no usable credential" sentences
/// sit together: one per lane, and one for the refresh that failed.
pub(crate) const MSG_NO_KEY: &str = "Add your Sarvam API key in Settings → Speech engine";

/// The API key cache, loaded from the OS credential store at startup. The key
/// deliberately never enters `Settings` (which round-trips through the
/// webview) or the plaintext settings.json.
pub type SharedKey = Arc<RwLock<Option<String>>>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Endpointing {
    /// Push-to-talk: the hotkey release is the authoritative end of speech.
    Manual,
    /// Hands-free: the server segments utterances at pauses.
    Vad,
}

impl Endpointing {
    pub fn as_str(&self) -> &'static str {
        match self {
            Endpointing::Manual => "manual",
            Endpointing::Vad => "vad",
        }
    }
}

/// Which host a dictation talks to — decided at chord-down from the
/// provider, and carried with the session so that flipping the setting
/// mid-sentence cannot move a dictation that is already running.
///
/// The credential is deliberately *not* here. `Controller::begin_recording`
/// is synchronous and must not block the hotkey, and the relay's bearer has
/// to be awaited (`auth::session::access_token`), so the credential is
/// attached in the dispatcher instead — see [`Transport`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum Lane {
    /// Bring your own key: the user's own Sarvam key, straight to Sarvam.
    #[default]
    Byok,
    /// Butterfly Labs Cloud: everything goes through the relay at this base
    /// URL (`https://…`, no trailing slash), which holds the Sarvam key.
    Cloud { relay: String },
}

impl Lane {
    /// The realtime WebSocket URL, before the query string. No credential is
    /// ever in it — Sarvam and the relay both authenticate with a header —
    /// which is why this lives on the lane rather than on [`Transport`].
    pub fn realtime_endpoint(&self) -> String {
        match self {
            Lane::Byok => REALTIME_WS_URL.to_string(),
            Lane::Cloud { relay } => format!("{}{RELAY_REALTIME_PATH}", ws_scheme(relay)),
        }
    }
}

/// An `http(s)` base URL as its WebSocket equivalent, trailing slash gone.
///
/// Anything else is handed back untouched so that
/// `IntoClientRequest::into_client_request` is the one place a malformed URL
/// is rejected — a second opinion here would only produce a different error
/// for the same mistake.
fn ws_scheme(base: &str) -> String {
    let base = base.trim_end_matches('/');
    match base.split_once("://") {
        Some(("https", rest)) => format!("wss://{rest}"),
        Some(("http", rest)) => format!("ws://{rest}"),
        _ => base.to_string(),
    }
}

/// A lane with its credential attached: what one realtime connection and the
/// polish calls of that same dictation authenticate with.
///
/// The split from [`Lane`] is the whole point: in Cloud mode the app holds
/// *only* the user's own Supabase access token. The Sarvam key lives in the
/// relay's Cloudflare secrets and is never sent to, stored by, or known to
/// this app.
#[derive(Clone)]
pub enum Transport {
    Sarvam { key: String },
    Relay { base: String, bearer: String },
}

/// How long a polish waits for this dictation's credential to be looked up
/// again before it goes out with the one the dictation already holds. On the
/// Cloud lane the look-up can be a sign-in refresh, which may take up to
/// `auth::session`'s own token timeout; the polish is on the path to the
/// paste, so it does not wait that long. `controller::CLOUD_FINALIZE_TIMEOUT`
/// and `CUSTOM_FINALIZE_TIMEOUT` both count it.
pub const CREDENTIAL_WAIT: std::time::Duration = std::time::Duration::from_secs(2);

impl Transport {
    /// This dictation's credential, resolved at connect time.
    ///
    /// Called once per dictation and never cached beyond it: the relay's
    /// bearer is an hour-long token that `auth::session::access_token`
    /// refreshes at a five-minute margin, and holding one across dictations
    /// would mean holding a stale one. It is cheap — in-memory unless a
    /// refresh is actually due.
    ///
    /// `Err` is a finished sentence for the user: "no Sarvam key" on the
    /// Bring-your-own-key lane, and on the Cloud lane whatever
    /// `access_token` says, which already distinguishes "your sign-in
    /// expired" from "we could not reach Butterfly Labs".
    pub async fn resolve(lane: &Lane, key: &SharedKey) -> Result<Transport, String> {
        match lane {
            Lane::Byok => key
                .read()
                .expect("key lock")
                .clone()
                .filter(|k| !k.trim().is_empty())
                .map(|key| Transport::Sarvam { key })
                .ok_or_else(|| MSG_NO_KEY.to_string()),
            Lane::Cloud { relay } => Transport::cloud(relay).await,
        }
    }

    /// The same credential, obtained the way a *retry* has to obtain it.
    ///
    /// Only the Cloud lane differs, and only because its credential expires:
    /// the relay has just refused this dictation's bearer, so
    /// [`crate::auth::session::force_refresh`] spends the stored refresh
    /// token instead of handing back the token that was refused. A Sarvam key
    /// is a key — there is nothing to refresh — so the Bring-your-own-key
    /// lane resolves exactly as [`resolve`] does, which also keeps this total
    /// rather than making the caller match on the lane.
    pub async fn resolve_fresh(lane: &Lane, key: &SharedKey) -> Result<Transport, String> {
        match lane {
            Lane::Byok => Transport::resolve(lane, key).await,
            Lane::Cloud { relay } => Transport::cloud_fresh(relay).await,
        }
    }

    /// The relay's transport: the base URL plus this run's access token.
    ///
    /// Separate from [`resolve`] because every caller that is not a dictation
    /// — transform, agent, notes — already knows it is on the Cloud lane by
    /// the time it needs one, and none of them holds the [`SharedKey`] a full
    /// `resolve` would read and then ignore.
    pub async fn cloud(base: &str) -> Result<Transport, String> {
        Ok(Transport::Relay {
            base: base.to_string(),
            bearer: crate::auth::session::access_token().await?,
        })
    }

    /// [`cloud`](Transport::cloud) for the retry after a `401`.
    pub async fn cloud_fresh(base: &str) -> Result<Transport, String> {
        Ok(Transport::Relay {
            base: base.to_string(),
            bearer: crate::auth::session::force_refresh().await?,
        })
    }

    /// The header one upgrade request or one chat call carries, as
    /// (name, value).
    ///
    /// The two lanes are mutually exclusive by construction: Sarvam's own
    /// `api-subscription-key` never travels to the relay (the app has no
    /// Sarvam key in Cloud mode), and the sign-in bearer never travels to
    /// Sarvam.
    pub fn auth_header(&self) -> (&'static str, String) {
        match self {
            Transport::Sarvam { key } => (AUTH_HEADER, key.clone()),
            Transport::Relay { bearer, .. } => ("authorization", format!("Bearer {bearer}")),
        }
    }

    /// Whether this dictation is going through the relay — which is what
    /// makes a `401` a sign-in problem rather than a key problem.
    pub fn is_relay(&self) -> bool {
        matches!(self, Transport::Relay { .. })
    }

    /// Whether two transports would present the same credential.
    ///
    /// The `401` retry's own question, and the reason it is a named method
    /// rather than a derived `PartialEq`: exactly one place in this app has
    /// any business comparing credentials, and a derived one would invite
    /// every other place to.
    pub fn same_credential(&self, other: &Transport) -> bool {
        self.auth_header().1 == other.auth_header().1
    }
}

/// Hand-written for the reason `format::backend::Backend`'s is: this type
/// holds a credential, and the moment to make a `{:?}` leak impossible is
/// before something formats it.
impl std::fmt::Debug for Transport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Transport::Sarvam { .. } => f.write_str("Transport::Sarvam { key: (redacted) }"),
            Transport::Relay { base, .. } => {
                write!(f, "Transport::Relay {{ base: {base}, bearer: (redacted) }}")
            }
        }
    }
}

#[derive(Clone, Debug)]
pub struct SessionCfg {
    pub language_code: String,
    pub stream_type: String,
    pub mode: String,
    pub endpointing: Endpointing,
    /// Terminology hints for the recognizer (the user's dictionary words).
    pub prompt: Option<String>,
    /// Which host this dictation runs against ([`Lane`]).
    pub lane: Lane,
}

/// Commands from the controller. The controller guarantees the order
/// `Start (Audio* ) (Finish | Cancel)` per dictation; anything else is a
/// stray from a torn-down session and gets ignored.
pub enum CloudCmd {
    Start {
        /// Controller's monotonic session counter, echoed back in
        /// mid-recording `CloudError`s so stale failures are ignorable.
        session: u64,
        cfg: SessionCfg,
    },
    /// 16 kHz mono f32 chunk, same shape the local ASR gets.
    Audio(Vec<f32>),
    Finish {
        req_id: u64,
        /// The instant the controller judged speech to have ended —
        /// stamped in `Controller::finish_recording` *before* it blocks
        /// draining queued audio (`collect_tail`, up to
        /// `TAIL_FLUSH_TIMEOUT_MS`), not when this command happens to be
        /// constructed or received afterwards. `sarvam::ws::drain_session`
        /// uses this as the origin of its `drain_ms` measurement instead
        /// of its own receipt time, so the `collect_tail` wait counts as
        /// latency in the reported number.
        end_of_speech: Instant,
        /// Audio length of the utterance, in milliseconds
        /// (`Controller::pending_duration_ms`, computed the same way from
        /// the same buffer). Two independent uses in `ws::drain_session`:
        /// scaling the finals-drain wait for longer utterances
        /// (`ws::scaled_flush_wait`), and gating the batch fallback on
        /// utterances longer than 2 s, so a fallback is never attempted for
        /// an utterance too short to contain real speech.
        duration_ms: u64,
    },
    Cancel,
}

pub fn spawn(
    ctl_tx: crossbeam_channel::Sender<crate::state::ControlMsg>,
    key: SharedKey,
    cleanup: Arc<RwLock<crate::cleanup::CleanupSettings>>,
) -> tokio::sync::mpsc::UnboundedSender<CloudCmd> {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    tauri::async_runtime::spawn(ws::dispatcher(ctl_tx, key, cleanup, rx));
    tx
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The credential must not be one careless `{:?}` away from a log line.
    /// `Transport` is the type both lanes' secrets pass through — a Sarvam
    /// key on one, the user's own access token on the other — and this app
    /// logs failures with `?transport` shorthand in several places, so the
    /// redaction is pinned rather than left to whoever edits the struct next.
    #[test]
    fn neither_transport_ever_prints_its_credential() {
        let sarvam = Transport::Sarvam {
            key: "sk-live-0123456789".into(),
        };
        let relay = Transport::Relay {
            base: "https://relay.example.workers.dev".into(),
            bearer: "supabase-access-token".into(),
        };
        for printed in [format!("{sarvam:?}"), format!("{sarvam:#?}")] {
            assert!(!printed.contains("sk-live"), "{printed}");
            assert!(printed.contains("redacted"), "{printed}");
        }
        for printed in [format!("{relay:?}"), format!("{relay:#?}")] {
            assert!(!printed.contains("supabase-access-token"), "{printed}");
            assert!(printed.contains("redacted"), "{printed}");
            // The host is diagnostic, not a secret: which relay a failing
            // dictation was dialing is exactly what a bug report needs.
            assert!(printed.contains("relay.example.workers.dev"), "{printed}");
        }
    }

    /// The retry's own question. Two transports match when the header value
    /// they would send matches, and a refreshed bearer is a different one —
    /// which is what tells the connect loop that a second dial is worth
    /// making at all.
    #[test]
    fn a_refreshed_bearer_is_not_the_same_credential() {
        let refused = Transport::Relay {
            base: "https://relay.example.workers.dev".into(),
            bearer: "stale".into(),
        };
        let refreshed = Transport::Relay {
            base: "https://relay.example.workers.dev".into(),
            bearer: "fresh".into(),
        };
        assert!(refused.same_credential(&refused.clone()));
        assert!(!refused.same_credential(&refreshed));
        // Across lanes the header name differs too, so a Sarvam key and a
        // bearer can never be mistaken for one another.
        assert!(!refused.same_credential(&Transport::Sarvam {
            key: "stale".into()
        }));
    }
}
