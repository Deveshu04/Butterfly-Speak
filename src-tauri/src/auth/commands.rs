//! The five commands the Cloud card calls, and the deep-link wiring that
//! finishes what the first of them starts.
//!
//! What is *not* here: nothing returns a token, and nothing accepts one.
//! The webview learns whether someone is signed in and, once a token has been
//! spent, which address — the same shape `sarvam_key_status` uses for the
//! Sarvam key, and for the same reason.
//!
//! The log rule from `session` holds here too: no token, no URL, and no
//! `reqwest::Error` rendered whole (its `Display` appends the URL it failed
//! on). The usage lookup below logs nothing at all.

use super::session::{self, CloudStatus};
use crate::events;
use serde::{Deserialize, Serialize};
use std::sync::OnceLock;
use std::time::Duration;
use tauri::{AppHandle, Emitter};

/// Budget for the usage lookup. The same 15 s `session` gives a token call,
/// and for the same reason: long enough to survive a slow network, short
/// enough that the card answers rather than spins.
const USAGE_TIMEOUT: Duration = Duration::from_secs(15);

/// Open the system browser at Google's consent screen.
///
/// Resolves as soon as the browser has been handed the URL — the sign-in
/// itself finishes minutes later, on the deep link, and announces itself with
/// [`events::CLOUD_AUTH_CHANGED`].
#[tauri::command]
pub fn cloud_sign_in(app: AppHandle) -> Result<(), String> {
    session::begin_sign_in(&app)
}

/// Revoke the session and delete the stored refresh token, and tell the card
/// whether Supabase ended its side ([`session::SignOut`]). Never fails: the
/// local half always happens.
#[tauri::command]
pub async fn cloud_sign_out(app: AppHandle) -> session::SignOut {
    let outcome = session::sign_out().await;
    let _ = app.emit(events::CLOUD_AUTH_CHANGED, session::status());
    outcome
}

/// Delete the signed-in user's Cloud account at the relay, and sign out here
/// once it confirms. A failure comes back as a sentence, and the user stays
/// signed in, unless the refresh for a bearer was refused: that has signed
/// them out already. Either way the card hears about a sign-out.
#[tauri::command]
pub async fn cloud_delete_account(app: AppHandle) -> Result<(), String> {
    let result = session::delete_account(&relay_base(&crate::settings::load())).await;
    let status = session::status();
    if !status.signed_in {
        let _ = app.emit(events::CLOUD_AUTH_CHANGED, status);
    }
    result
}

/// Who is signed in.
///
/// Async, and with one network call in it, for the launch case only: a restart
/// wakes up with a refresh token and no name to put on the card, so the first
/// call after a launch spends it. If that refresh is *rejected* the credential
/// is already gone by the time [`session::access_token`] returns, and the
/// second read below correctly says "signed out" — which is the whole reason
/// this re-reads rather than patching the first answer.
#[tauri::command]
pub async fn cloud_status() -> CloudStatus {
    let status = session::status();
    if status.signed_in && status.email.is_none() {
        // A failure here is deliberately not an error: offline is not signed
        // out, and the card has a truthful answer either way.
        let _ = session::access_token().await;
        return session::status();
    }
    status
}

/// This week's dictated words, as the relay counts them.
///
/// The relay answers in snake_case (`GET /v1/usage`); the webview
/// is handed camelCase like every other command payload. One struct carries
/// both rather than two that can drift — hence the alias on the one field
/// whose spelling differs.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CloudUsage {
    /// Monday of the current ISO week, `YYYY-MM-DD`.
    #[serde(alias = "week_start")]
    pub week_start: String,
    /// Words dictated through the relay since that Monday.
    pub words: u32,
    /// The weekly allowance. Read from the answer rather than assumed: the
    /// relay is what enforces it, so it is what gets to say what it is.
    pub limit: u32,
}

/// The Cloud card's one number.
///
/// Fails — rather than answering zero — whenever the count is unknown: no
/// sign-in, no network, or a relay that refused. The card shows a dash for
/// all of them, because "0 words" and "couldn't ask" are not the same
/// sentence and only one of them is true.
#[tauri::command]
pub async fn cloud_usage() -> Result<CloudUsage, String> {
    let bearer = session::access_token().await?;
    usage_at(&relay_base(&crate::settings::load()), &bearer).await
}

/// The relay this install talks to: the same base the dictation socket and
/// the polish backend hang off (`CloudSettings::relay_base`).
fn relay_base(s: &crate::settings::Settings) -> String {
    s.cloud.relay_base()
}

/// The lookup itself, with the host and the credential as arguments so it can
/// be tested against a loopback listener.
async fn usage_at(base: &str, bearer: &str) -> Result<CloudUsage, String> {
    let response = http()
        .get(format!(
            "{base}{}",
            crate::format::backend::RELAY_USAGE_PATH
        ))
        .bearer_auth(bearer)
        .send()
        .await
        // Deliberately not `{e}`: reqwest appends the URL it failed on.
        .map_err(|_| "Couldn't reach Butterfly Labs".to_string())?;
    let status = response.status();
    if !status.is_success() {
        return Err(format!(
            "Butterfly Labs answered HTTP {}",
            status.as_u16()
        ));
    }
    response
        .json::<CloudUsage>()
        .await
        .map_err(|_| "Butterfly Labs sent a count this build couldn't read".to_string())
}

fn http() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .timeout(USAGE_TIMEOUT)
            .build()
            .unwrap_or_else(|_| reqwest::Client::new())
    })
}

/// Listen for the browser's hand-back on the `butterflylabs://` scheme.
///
/// Called once from `setup`, and reached by exactly one path: the app was
/// already running, Windows launched a second process for the link, the
/// single-instance plugin killed it and forwarded its argv to the deep-link
/// plugin — that is what the plugin's `deep-link` feature buys — which emits
/// `deep-link://new-url` here.
///
/// A *cold* start does not reach it. The deep-link plugin parses the launch
/// argv inside its own `setup`, which runs before the app's, so a URL that
/// started the process has already been emitted by the time this listener
/// exists and survives only in `get_current()`. Nothing reads it, and nothing
/// should: the PKCE verifier that URL's code has to be paired with died with
/// the process that started the sign-in, so a cold start cannot complete one.
pub fn watch_deep_links(app: &AppHandle) {
    use tauri_plugin_deep_link::DeepLinkExt;

    let handle = app.clone();
    app.deep_link().on_open_url(move |event| {
        for url in event.urls() {
            let Some(code) = session::code_from_callback(url.as_str()) else {
                if session::callback_declined(url.as_str()) {
                    // This *is* the sign-in coming back — the user closed
                    // Google's consent screen, or the provider refused. The
                    // card is showing "Waiting for your browser…" and has to
                    // stop, so the same event a success sends goes out here.
                    // Nothing of what the browser said is read or logged.
                    tracing::debug!("cloud sign-in was declined at the browser");
                    let _ = handle.emit(events::CLOUD_AUTH_CHANGED, session::status());
                    continue;
                }
                // Not a sign-in at all. Said at debug with no URL in it: this
                // scheme is a public doorbell and anything can ring it.
                tracing::debug!("ignoring a deep link that is not a sign-in callback");
                continue;
            };
            let handle = handle.clone();
            tauri::async_runtime::spawn(async move {
                let result = session::complete_sign_in(&code).await;
                if let Err(e) = &result {
                    // The sentence is the user-facing one from `session`; it
                    // never carries a code or a token.
                    tracing::warn!("cloud sign-in failed: {e}");
                }
                // Emitted on failure too: the card was showing "Waiting for
                // your browser…" and has to stop, whichever way this went.
                let _ = handle.emit(events::CLOUD_AUTH_CHANGED, session::status());
                if result.is_ok() {
                    // The browser has focus at this moment; the app is behind
                    // it with a card the user is waiting on.
                    crate::tray::show_main(&handle);
                }
            });
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::{CloudSettings, Settings};

    /// The body the relay answers `GET /v1/usage` with, verbatim.
    const RELAY_BODY: &str = r#"{"week_start":"2026-09-14","words":1240,"limit":2000}"#;

    fn with_relay(relay_url: Option<&str>) -> Settings {
        Settings {
            cloud: CloudSettings {
                relay_url: relay_url.map(str::to_string),
            },
            ..Settings::default()
        }
    }

    /// The card must hang its lookup off exactly the host the dictation
    /// socket and the polish backend use, or a staging run would measure one
    /// relay and display another's count.
    #[test]
    fn the_usage_base_is_normalized_the_way_the_lane_is() {
        assert_eq!(
            relay_base(&with_relay(None)),
            crate::sarvam::DEFAULT_RELAY_URL,
            "no override means the shipped relay"
        );
        assert_eq!(
            relay_base(&with_relay(Some(""))),
            crate::sarvam::DEFAULT_RELAY_URL,
            "an empty string is not a URL"
        );
        assert_eq!(
            relay_base(&with_relay(Some("   "))),
            crate::sarvam::DEFAULT_RELAY_URL,
            "whitespace is not a URL either"
        );
        assert_eq!(
            relay_base(&with_relay(Some("  https://staging.example.workers.dev/  "))),
            "https://staging.example.workers.dev",
            "a hand-edited override is trimmed at both ends, slash included"
        );
        assert_eq!(
            relay_base(&with_relay(Some("http://127.0.0.1:8787"))),
            "http://127.0.0.1:8787",
            "a local wrangler dev is a legal override"
        );
    }

    /// The relay speaks snake_case and the webview is handed camelCase, like
    /// every other command payload. One struct carries both, so a rename on
    /// either side fails here rather than at the card.
    #[test]
    fn the_wire_is_snake_case_and_the_webview_camel_case() {
        let usage: CloudUsage = serde_json::from_str(RELAY_BODY).expect("the relay's own shape");
        assert_eq!(usage.week_start, "2026-09-14");
        assert_eq!(usage.words, 1240);
        assert_eq!(usage.limit, 2000);

        let out = serde_json::to_string(&usage).expect("serialize");
        assert!(out.contains(r#""weekStart":"2026-09-14""#), "{out}");
        assert!(!out.contains("week_start"), "{out}");
    }

    /// A one-shot loopback HTTP server that hands back the request line and
    /// headers it read. Same raw-socket approach as `session`'s stub, and for
    /// the same reason: no new dependency, and `tokio`'s `io-util` is off.
    async fn stub(
        code: u16,
        body: &'static str,
    ) -> (
        std::net::SocketAddr,
        tokio::sync::oneshot::Receiver<String>,
    ) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind loopback listener");
        let addr = listener.local_addr().expect("local addr");
        let (tx, rx) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let Ok((socket, _)) = listener.accept().await else {
                return;
            };
            // Drain first: closing a socket with unread inbound data queued
            // makes Windows send an RST instead of a FIN, which hyper reports
            // as "connection aborted" and masks the status under test.
            let mut buf = [0u8; 8192];
            socket.readable().await.expect("socket readable");
            let read = match socket.try_read(&mut buf) {
                Ok(n) => n,
                Err(_) => 0,
            };
            let _ = tx.send(String::from_utf8_lossy(&buf[..read]).to_string());
            let response = format!(
                "HTTP/1.1 {code} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let mut bytes = response.as_bytes();
            while !bytes.is_empty() {
                socket.writable().await.expect("socket writable");
                match socket.try_write(bytes) {
                    Ok(n) => bytes = &bytes[n..],
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => continue,
                    Err(e) => panic!("stub write failed: {e}"),
                }
            }
        });
        (addr, rx)
    }

    /// The route and the credential, pinned together: the relay authenticates
    /// every route, and a usage call with Sarvam's header on it would be a
    /// token sent to the wrong company.
    #[tokio::test]
    async fn the_lookup_asks_the_usage_route_with_the_bearer() {
        let (addr, request) = stub(200, RELAY_BODY).await;
        let usage = usage_at(&format!("http://{addr}"), "supabase-access-token")
            .await
            .expect("a 200 with the relay's body is a usage answer");
        assert_eq!(usage.words, 1240);

        let sent = request.await.expect("the stub read a request");
        let head = sent.to_lowercase();
        assert!(sent.starts_with("GET /v1/usage "), "{sent}");
        assert!(
            head.contains("authorization: bearer supabase-access-token"),
            "{sent}"
        );
        assert!(
            !head.contains("api-subscription-key"),
            "Sarvam's header has no business on a relay route: {sent}"
        );
    }

    /// Every failure is a sentence, and no sentence may carry the URL or the
    /// token — reqwest's own `Display` appends the URL, which is why the
    /// error is never rendered whole here.
    #[tokio::test]
    async fn a_refusal_is_a_sentence_with_no_url_and_no_token() {
        for code in [401, 429, 500] {
            let (addr, _request) = stub(code, r#"{"error":"nope"}"#).await;
            let base = format!("http://{addr}");
            let message = usage_at(&base, "supabase-access-token")
                .await
                .err()
                .unwrap_or_else(|| panic!("a {code} is not a usage answer"));
            assert!(!message.contains(&base), "{code}: {message}");
            assert!(!message.contains("supabase-access-token"), "{code}: {message}");
            assert!(!message.contains("/v1/usage"), "{code}: {message}");
        }
    }

    /// A host that is not there is not a signed-out user; the card shows a
    /// dash and says nothing.
    #[tokio::test]
    async fn an_unreachable_relay_is_a_transport_failure() {
        // Bound but never listening, so a connect is refused. The socket lives
        // to the end of the test: a port released here could be handed to
        // another test's listener, which would then answer this request.
        let socket = tokio::net::TcpSocket::new_v4().expect("create socket");
        socket
            .bind("127.0.0.1:0".parse().expect("loopback address"))
            .expect("bind loopback socket");
        let addr = socket.local_addr().expect("local addr");
        let base = format!("http://{addr}");
        let message = usage_at(&base, "supabase-access-token")
            .await
            .err()
            .expect("nothing is listening");
        assert!(!message.contains(&base), "{message}");
    }
}
