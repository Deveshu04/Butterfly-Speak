//! The `GET {base}/models` probe: model discovery, and the one thing behind
//! the Settings screen's "Test connection" button.
//!
//! The probe is a composition: normalize → require a scheme →
//! [`super::is_safe_transport`] → `GET {base}/models`, plus the two
//! things a test button must have: a bounded timeout, and an answer that says
//! which of "I could not reach it", "it refused my credential" and "it
//! answered with an error" actually happened.
//!
//! The model list only suggests ids. [`parse_models`] never fails a
//! response it cannot understand — it returns what it recognised — and
//! nothing here is ever allowed to overwrite the model id the user typed.
//!
//! Nothing in this module logs a key. The credential is read from the slot,
//! travels in an `Authorization` header, and never reaches a `tracing` call,
//! a returned string, or the webview.

use super::{models_url, resolve_base, Invalid};
use futures_util::StreamExt;
use serde_json::Value;
use std::time::Duration;

/// One budget for the whole probe: connecting, the request, and reading the
/// reply body.
///
/// Long enough for the slowest ordinary case, a first request to a remote
/// gateway from a home connection: a DNS lookup, the TCP and TLS handshakes
/// and the request itself are about five round trips, and at 300 to 350 ms
/// per round trip to another continent that is under two seconds with the
/// server's own time added. Four seconds leaves about twice that for a busy
/// link. Short enough that a wrong port or a dead host, which spends the
/// whole budget, still answers while the user is watching the button.
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(4);

/// The most of a reply the probe reads. A local server's model list is
/// small, but a hosted gateway that aggregates many providers lists every
/// model with its description, pricing and parameters, which can run to
/// hundreds of kilobytes or more. The cap only keeps a runaway body from
/// filling memory; [`PROBE_TIMEOUT`] still bounds how long it is read.
const MAX_BODY_BYTES: usize = 16 << 20;

/// How a reply's body failed to arrive whole.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BodyCut {
    /// It was still arriving when [`PROBE_TIMEOUT`] ran out.
    TimedOut,
    /// The connection closed or broke before it was complete.
    Closed,
}

/// One model, as the endpoint described itself.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiscoveredModel {
    pub id: String,
    /// `owned_by`, when the host sends one. Shown as a sublabel; never used
    /// to filter, rank or validate anything.
    pub owned_by: Option<String>,
}

/// A successful probe.
#[derive(Clone, Debug)]
pub struct Probed {
    /// The URL actually requested. Safe to **show**: it is the user's own
    /// input, back on their own screen, and it is the only way "which host
    /// did it try?" is answerable when someone pasted something surprising.
    ///
    /// Not safe to **log** as-is, which is a different question with a
    /// different answer. A pasted URL is where a credential hides when it is
    /// not in the key field — `https://user:pw@host/v1` and `?api_key=…` are
    /// both ordinary ways for a provider's docs to hand someone an endpoint —
    /// so anything that writes a URL to the log writes
    /// `endpoint::loggable_origin(..)` instead. Nothing in this module logs
    /// at all.
    pub url: String,
    pub models: Vec<DiscoveredModel>,
}

/// Why a probe did not come back with a model list. Each arm calls for a
/// different fix, and collapsing any two of them sends the user to the
/// wrong one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProbeError {
    /// Rejected before any packet left the machine — no URL, no scheme, or
    /// cleartext to a public host. Fix the URL.
    Config(Invalid),
    /// 401 or 403. The host is there and speaking; it will not accept what
    /// it was given.
    ///
    /// `sent_key` is whether there was anything to give, because that decides
    /// which of two different instructions the user needs — *add* a key, or
    /// *replace* the one already stored. Telling someone who has a key stored
    /// to add one reads as "the app cannot see my key", and sends them off to
    /// re-enter a credential that was in fact delivered and refused.
    Unauthorized { status: u16, sent_key: bool },
    /// Any other non-2xx. The host is there and speaking, and it has its own
    /// opinion about the request. Fix whatever it named.
    Http { status: u16, summary: String },
    /// No HTTP answer at all: timeout, refused connection, DNS, TLS. Fix the
    /// address, the server, or the network between them.
    Unreachable(String),
    /// The host answered with a status, but the rest of its reply did not
    /// arrive: the time ran out, or the connection closed, part-way through
    /// the body. Fix the server, or whatever sits between it and this app.
    BodyIncomplete { status: u16, cause: BodyCut },
    /// The reply is over [`MAX_BODY_BYTES`]. The URL may well be right; the
    /// list is just more than this app reads, so the model id is typed by hand.
    BodyTooLarge { status: u16 },
}

impl ProbeError {
    /// The sentence the Settings screen shows.
    pub fn message(&self) -> String {
        match self {
            ProbeError::Config(why) => why.message().to_string(),
            // The code is in the text on purpose: 401 ("no/bad credential")
            // and 403 ("this credential, but not for this") send the user to
            // different settings, and only their server knows which it meant.
            // So does `sent_key` — see the variant's doc. The sentence is
            // shown in the Model and Test connection rows, which both sit
            // under the API key row, hence "above".
            ProbeError::Unauthorized { status, sent_key } => {
                let fix = if *sent_key {
                    "It turned down the key saved here: paste one it accepts into the API \
                     key row above, or check which keys the server lets in."
                } else {
                    "No key went with it: paste the server's key into the API key row \
                     above, or set the server to take requests without one."
                };
                format!("HTTP {status}: the server would not take this request. {fix}")
            }
            ProbeError::Http { status, summary } => {
                format!("The endpoint answered HTTP {status}: {summary}")
            }
            ProbeError::Unreachable(why) => {
                format!("Couldn't reach the endpoint — {why}.")
            }
            ProbeError::BodyIncomplete { status, cause: BodyCut::TimedOut } => format!(
                "The endpoint answered HTTP {status}, but its reply was still arriving after {} \
                 seconds.",
                PROBE_TIMEOUT.as_secs()
            ),
            ProbeError::BodyIncomplete { status, cause: BodyCut::Closed } => format!(
                "The endpoint answered HTTP {status}, but it closed the connection before the \
                 reply was complete."
            ),
            ProbeError::BodyTooLarge { status } => format!(
                "The endpoint answered HTTP {status}, but its model list is larger than {} MiB, \
                 more than this app reads. Type the model id yourself.",
                MAX_BODY_BYTES >> 20
            ),
        }
    }
}

/// The models in a list-models reply, in the server's order.
///
/// The one shape read is OpenAI's, from its API reference for
/// `GET /v1/models`: a top-level `data` array of objects, each with a string
/// `id` and an optional `owned_by`. The servers this slot is aimed at
/// document the same shape on that route: Ollama's "OpenAI compatibility"
/// page (section "/v1/models"), and the llama.cpp server README (section
/// "GET /v1/models: OpenAI-compatible Model Info API"), whose example is
/// `object` plus `data` entries with `id` and `owned_by`. LM Studio's
/// "List Models" page and vLLM's quickstart name the route and show no
/// other shape. None of these documents a bare string as a model or a
/// non-string id, so array elements that are not objects, and ids that are
/// not strings, are skipped rather than converted.
///
/// Never fails: anything else is an empty list. An id is kept exactly as
/// sent, and skipped when blank. `owned_by` is kept only when it is a
/// non-blank string. A repeated id keeps only its first entry, so the
/// dropdown never offers the same choice twice.
pub fn parse_models(payload: &Value) -> Vec<DiscoveredModel> {
    let Some(entries) = payload.get("data").and_then(Value::as_array) else {
        return Vec::new();
    };
    let mut seen = std::collections::HashSet::new();
    let mut models = Vec::with_capacity(entries.len());
    for entry in entries {
        let Some(id) = entry.get("id").and_then(Value::as_str) else {
            continue;
        };
        if id.trim().is_empty() || !seen.insert(id) {
            continue;
        }
        let owned_by = entry
            .get("owned_by")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|owner| !owner.is_empty())
            .map(str::to_string);
        models.push(DiscoveredModel {
            id: id.to_string(),
            owned_by,
        });
    }
    models
}

/// Turns a transport failure into a sentence, without a status code to lean
/// on. The three cases a user can act on are separated; everything else falls
/// through to the deepest cause, which is where rustls and hyper put the
/// actual fault (certificate problems in particular).
///
/// A `reqwest::Error` carries the URL and the failure, never a request
/// header — so this cannot surface the key.
fn transport_reason(e: &reqwest::Error) -> String {
    if e.is_timeout() {
        return format!("no answer within {} seconds", PROBE_TIMEOUT.as_secs());
    }
    if e.is_connect() {
        // `is_connect` covers the whole pre-response phase, TLS included: a
        // certificate a self-hosted server signed itself lands here, not in
        // the fall-through below. The prose has to admit that case rather
        // than send someone to their firewall over a trust-store problem.
        return "the connection was refused, the host could not be resolved, or the TLS \
                handshake failed"
            .into();
    }
    let mut cause: &(dyn std::error::Error + 'static) = e;
    while let Some(next) = cause.source() {
        cause = next;
    }
    cause.to_string()
}

/// Reads `resp`'s body, up to [`MAX_BODY_BYTES`]. A `Content-Length` over the
/// cap is refused before anything is read, and a body without one is
/// abandoned as soon as it passes the cap.
async fn read_body(resp: reqwest::Response) -> Result<String, ProbeError> {
    let status = resp.status().as_u16();
    if resp.content_length().is_some_and(|n| n > MAX_BODY_BYTES as u64) {
        return Err(ProbeError::BodyTooLarge { status });
    }
    let mut body = Vec::new();
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| ProbeError::BodyIncomplete {
            status,
            cause: if e.is_timeout() { BodyCut::TimedOut } else { BodyCut::Closed },
        })?;
        if body.len() + chunk.len() > MAX_BODY_BYTES {
            return Err(ProbeError::BodyTooLarge { status });
        }
        body.extend_from_slice(&chunk);
    }
    Ok(String::from_utf8_lossy(&body).into_owned())
}

/// `GET {base}/models` against the endpoint a user configured.
///
/// `configured_url` is what they typed (a draft, before it is saved — that is
/// the point of a test button); `api_key` comes from the credential store and
/// is sent as a bearer token only when non-empty, because plenty of
/// self-hosted servers take no auth and sending an empty `Authorization`
/// header to one is how a working endpoint starts 401ing.
pub async fn probe(
    http: &reqwest::Client,
    configured_url: &str,
    api_key: Option<&str>,
) -> Result<Probed, ProbeError> {
    // Pre-flight first, and with no network call behind it: an unusable URL
    // is a sentence we can already write, and "cleartext to a public host" in
    // particular must never be attempted just to report on it.
    let base = resolve_base(configured_url).map_err(ProbeError::Config)?;
    let url = models_url(&base);

    let mut req = http.get(&url).timeout(PROBE_TIMEOUT);
    let sent_key = match api_key.map(str::trim).filter(|k| !k.is_empty()) {
        Some(key) => {
            req = req.bearer_auth(key);
            true
        }
        None => false,
    };

    let resp = match req.send().await {
        Ok(resp) => resp,
        Err(e) => return Err(ProbeError::Unreachable(transport_reason(&e))),
    };
    let status = resp.status();
    let code = status.as_u16();
    // A refused credential is the whole answer; its body is not needed.
    if code == 401 || code == 403 {
        return Err(ProbeError::Unauthorized {
            status: code,
            sent_key,
        });
    }
    // Any other body is read before the status is judged, so an error body
    // can be summarised. A body that never finishes, or is too large to be a
    // model list, is an answer of its own: an empty "success" would tell the
    // user a broken endpoint works.
    let raw = read_body(resp).await?;
    if !status.is_success() {
        return Err(ProbeError::Http {
            status: code,
            // Same summariser the chat path uses: the server's own message,
            // capped — never the raw body, which on some hosts echoes the
            // request, and request content is never logged.
            summary: crate::format::backend::error_summary(&raw),
        });
    }

    let payload = serde_json::from_str::<Value>(&raw).unwrap_or(Value::Null);
    Ok(Probed {
        url,
        models: parse_models(&payload),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    // -- parse_models --------------------------------------------------------

    #[test]
    fn the_openai_shape_parses() {
        let payload = serde_json::json!({
            "object": "list",
            "data": [
                { "id": "qwen3:8b", "owned_by": "llamacpp" },
                { "id": "gemma3:4b" },
            ]
        });
        assert_eq!(
            parse_models(&payload),
            vec![
                DiscoveredModel {
                    id: "qwen3:8b".into(),
                    owned_by: Some("llamacpp".into())
                },
                DiscoveredModel {
                    id: "gemma3:4b".into(),
                    owned_by: None
                },
            ]
        );
    }

    /// No shape beyond OpenAI's is accepted (see `parse_models`), so a list
    /// under any other container key reads as nothing to suggest.
    #[test]
    fn a_list_under_another_key_is_not_read() {
        for payload in [
            serde_json::json!({ "models": [{ "id": "llama3.2:3b" }] }),
            serde_json::json!({ "models": [{ "name": "llama3.2:3b" }] }),
            serde_json::json!({ "result": [{ "id": "llama3.2:3b" }] }),
        ] {
            assert!(parse_models(&payload).is_empty(), "payload {payload}");
        }
    }

    /// A non-string id is skipped, not converted: the documented shape
    /// has string ids only. Its string-id neighbours still come through.
    #[test]
    fn a_non_string_id_is_skipped() {
        let payload = serde_json::json!({
            "data": [{ "id": 7 }, { "id": "phi4-mini" }, { "id": true }, { "id": ["x"] }]
        });
        let ids: Vec<String> = parse_models(&payload).into_iter().map(|m| m.id).collect();
        assert_eq!(ids, ["phi4-mini"]);
    }

    /// Bare strings in `data` are not models: the documented entries are
    /// objects. The object entries around them keep their order.
    #[test]
    fn bare_strings_in_the_list_are_skipped() {
        let payload = serde_json::json!({
            "data": ["mistral-small", { "id": "b-model" }, "c-model", { "id": "a-model" }]
        });
        let ids: Vec<String> = parse_models(&payload).into_iter().map(|m| m.id).collect();
        assert_eq!(ids, ["b-model", "a-model"]);
    }

    /// The server's order is kept, a repeated id keeps its first entry, an
    /// id is never trimmed or rewritten, and a blank `owned_by` is dropped.
    #[test]
    fn order_is_kept_and_repeats_collapse_to_the_first() {
        let payload = serde_json::json!({
            "data": [
                { "id": "zeta", "owned_by": "  " },
                { "id": "Alpha ", "owned_by": "org-a" },
                { "id": "zeta", "owned_by": "org-z" },
                { "id": "   " },
                { "id": "beta", "owned_by": 3 },
            ]
        });
        assert_eq!(
            parse_models(&payload),
            vec![
                DiscoveredModel { id: "zeta".into(), owned_by: None },
                DiscoveredModel { id: "Alpha ".into(), owned_by: Some("org-a".into()) },
                DiscoveredModel { id: "beta".into(), owned_by: None },
            ]
        );
    }

    /// A 200 whose body is not JSON is still a success, with nothing to
    /// suggest; the panel shows its "listed no models" note.
    #[tokio::test]
    async fn a_reply_that_is_not_json_is_an_empty_success() {
        let addr = stub(200, "<html>hello</html>").await;
        let probed = probe(&reqwest::Client::new(), &format!("http://{addr}"), None)
            .await
            .expect("a 200 is a success even when the list cannot be read");
        assert!(probed.models.is_empty());
    }

    /// Discovery is a hint. A payload this parser cannot read is "nothing to
    /// suggest", never an error and never a reason to touch the user's id.
    #[test]
    fn an_unreadable_payload_is_an_empty_list_not_a_failure() {
        for payload in [
            serde_json::json!({}),
            serde_json::json!(null),
            serde_json::json!([1, 2, 3]),
            serde_json::json!({ "data": "not an array" }),
            serde_json::json!({ "data": [{ "id": "" }, { "id": null }, { "id": {} }, {}] }),
        ] {
            assert!(parse_models(&payload).is_empty(), "payload {payload}");
        }
    }

    // -- the probe -----------------------------------------------------------

    /// A one-shot loopback HTTP server, same raw-socket approach as
    /// `format::backend`'s stub and for the same reason: no new dependency,
    /// and `tokio`'s `io-util` feature is off in this crate.
    async fn stub(code: u16, body: &'static str) -> std::net::SocketAddr {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind loopback listener");
        let addr = listener.local_addr().expect("local addr");
        tokio::spawn(async move {
            let Ok((socket, _)) = listener.accept().await else {
                return;
            };
            // Drain the request first: closing a socket with unread inbound
            // data queued makes Windows send an RST instead of a FIN, which
            // hyper reports as "connection aborted" — masking the status the
            // test is about.
            let mut buf = [0u8; 8192];
            socket.readable().await.expect("socket readable");
            match socket.try_read(&mut buf) {
                Ok(_) | Err(_) => {}
            }
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
        addr
    }

    #[tokio::test]
    async fn a_reachable_endpoint_returns_its_models_and_the_url_it_answered_on() {
        let addr = stub(200, r#"{"data":[{"id":"qwen3:8b"}]}"#).await;
        let probed = probe(
            &reqwest::Client::new(),
            &format!("http://{addr}"),
            Some("sk-secret"),
        )
        .await
        .expect("a 200 with a model list is a success");
        assert_eq!(probed.url, format!("http://{addr}/v1/models"));
        assert_eq!(probed.models[0].id, "qwen3:8b");
    }

    /// 401/403 is its own answer: the host is there, it just won't take what
    /// it was given. Reporting it as "unreachable" sends the user to their
    /// firewall instead of their key.
    ///
    /// And *which* key instruction is its own answer again. Telling someone
    /// with a key stored that none went with the request reads as "the app
    /// cannot see my key", and sends them to re-enter a credential that was
    /// delivered and refused — so the sentence turns on whether one was
    /// actually sent, and the two must not collapse.
    #[tokio::test]
    async fn an_unauthorized_endpoint_says_whether_to_add_or_replace_the_key() {
        for code in [401u16, 403] {
            for key in [None, Some("sk-stored")] {
                let addr = stub(code, r#"{"error":{"message":"nope"}}"#).await;
                let err = probe(&reqwest::Client::new(), &format!("http://{addr}"), key)
                    .await
                    .expect_err("401/403 is a failure");
                assert_eq!(
                    err,
                    ProbeError::Unauthorized {
                        status: code,
                        sent_key: key.is_some()
                    },
                    "code {code}, key {key:?}"
                );

                let msg = err.message();
                assert!(msg.contains(&code.to_string()), "{msg}");
                assert!(msg.contains("API key row above"), "{msg}");
                if key.is_some() {
                    assert!(msg.contains("turned down the key saved here"), "{msg}");
                    assert!(
                        !msg.contains("No key went"),
                        "a stored key must not be described as missing: {msg}"
                    );
                } else {
                    assert!(msg.contains("No key went with it"), "{msg}");
                    assert!(
                        !msg.contains("saved here"),
                        "there is no saved key to blame: {msg}"
                    );
                }
            }
        }
    }

    /// Any other non-2xx carries the server's own diagnostic, summarised —
    /// not the raw body, which some hosts fill with an echo of the request.
    #[tokio::test]
    async fn another_http_status_carries_the_servers_own_message() {
        let addr = stub(500, r#"{"error":{"message":"model store offline"}}"#).await;
        let err = probe(&reqwest::Client::new(), &format!("http://{addr}"), None)
            .await
            .expect_err("500 is a failure");
        assert_eq!(
            err,
            ProbeError::Http {
                status: 500,
                summary: "model store offline".into()
            }
        );
        assert!(err.message().contains("HTTP 500"), "{}", err.message());
    }

    /// The pre-flight is not decoration: cleartext to a public host must be
    /// refused without a packet, or the "test" is itself the leak.
    #[tokio::test]
    async fn an_unusable_url_is_refused_before_anything_is_sent() {
        for (url, want) in [
            ("", Invalid::NotConfigured),
            ("localhost:11434", Invalid::NotAUrl),
            ("http://api.example.com/v1", Invalid::InsecureHttp),
        ] {
            let Err(err) = probe(&reqwest::Client::new(), url, None).await else {
                panic!("url {url:?} must be refused before anything is sent");
            };
            assert_eq!(err, ProbeError::Config(want), "url {url:?}");
        }
    }

    /// A loopback server that drains the request and writes `response` as it
    /// is. Then it holds the connection open without another byte when
    /// `hold` is set, and closes it otherwise. A client that stops reading
    /// part-way just ends the write.
    async fn raw_stub(response: Vec<u8>, hold: bool) -> std::net::SocketAddr {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind loopback listener");
        let addr = listener.local_addr().expect("local addr");
        tokio::spawn(async move {
            let Ok((socket, _)) = listener.accept().await else {
                return;
            };
            let mut buf = [0u8; 8192];
            socket.readable().await.expect("socket readable");
            let _ = socket.try_read(&mut buf);
            let mut bytes = &response[..];
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
            if hold {
                std::future::pending::<()>().await;
            }
            drop(socket);
        });
        addr
    }

    const PARTIAL_LIST: &[u8] =
        b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 100\r\n\r\n{\"data\":[";

    /// A 200 whose body starts and never finishes is not a working endpoint
    /// with no models: the test says the reply did not arrive, and why.
    #[tokio::test]
    async fn a_body_that_never_finishes_is_not_a_success() {
        let addr = raw_stub(PARTIAL_LIST.to_vec(), true).await;
        let err = probe(&reqwest::Client::new(), &format!("http://{addr}"), None)
            .await
            .expect_err("a body that never arrived passed the test");
        assert_eq!(err, ProbeError::BodyIncomplete { status: 200, cause: BodyCut::TimedOut });
        assert_eq!(
            err.message(),
            "The endpoint answered HTTP 200, but its reply was still arriving after 4 seconds."
        );
    }

    /// The same when the connection closes part-way through the body.
    #[tokio::test]
    async fn a_body_cut_off_by_a_closed_connection_is_not_a_success() {
        let addr = raw_stub(PARTIAL_LIST.to_vec(), false).await;
        let err = probe(&reqwest::Client::new(), &format!("http://{addr}"), None)
            .await
            .expect_err("a body cut off part-way passed the test");
        assert_eq!(err, ProbeError::BodyIncomplete { status: 200, cause: BodyCut::Closed });
        assert!(err.message().contains("closed the connection"), "{}", err.message());
    }

    /// A reply that declares more than the cap is refused on its header,
    /// before a byte of it is read.
    #[tokio::test]
    async fn a_reply_declaring_more_than_the_cap_is_refused_unread() {
        let head = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
            MAX_BODY_BYTES + 1
        );
        let addr = raw_stub(head.into_bytes(), true).await;
        let started = Instant::now();
        let err = probe(&reqwest::Client::new(), &format!("http://{addr}"), None)
            .await
            .expect_err("an oversized reply passed the test");
        assert_eq!(err, ProbeError::BodyTooLarge { status: 200 });
        assert!(started.elapsed() < PROBE_TIMEOUT, "the body was waited for");
        let message = err.message();
        assert!(message.contains("16 MiB") && message.contains("model list is larger"), "{message}");
        assert!(!message.contains("URL"), "a list too large is not the URL's fault: {message}");
    }

    /// A gateway's list, with long descriptions per model, runs well past
    /// a megabyte and is still a model list.
    #[tokio::test]
    async fn a_multi_megabyte_model_list_is_read() {
        let body = serde_json::json!({
            "data": [{ "id": "vendor/model-a", "description": "x".repeat(3 << 20) }]
        })
        .to_string();
        let mut response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
            body.len()
        )
        .into_bytes();
        response.extend_from_slice(body.as_bytes());
        let addr = raw_stub(response, false).await;
        let probed = probe(&reqwest::Client::new(), &format!("http://{addr}"), None)
            .await
            .expect("a 3 MiB list is a success");
        assert_eq!(probed.models[0].id, "vendor/model-a");
    }

    /// A reply with no length is read only up to the cap.
    #[tokio::test]
    async fn a_chunked_reply_past_the_cap_is_refused() {
        let mut response =
            b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\n\r\n".to_vec();
        let piece = vec![b' '; 64 * 1024];
        for _ in 0..(MAX_BODY_BYTES / piece.len() + 1) {
            response.extend_from_slice(format!("{:x}\r\n", piece.len()).as_bytes());
            response.extend_from_slice(&piece);
            response.extend_from_slice(b"\r\n");
        }
        response.extend_from_slice(b"0\r\n\r\n");
        let addr = raw_stub(response, true).await;
        let err = probe(&reqwest::Client::new(), &format!("http://{addr}"), None)
            .await
            .expect_err("an oversized reply passed the test");
        assert_eq!(err, ProbeError::BodyTooLarge { status: 200 });
    }

    /// The timeout is the difference between a test button and a hang. A
    /// listener that accepts and then says nothing is exactly the case a
    /// wrong-port paste produces.
    #[tokio::test]
    async fn a_silent_host_times_out_inside_the_probe_budget() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind loopback listener");
        let addr = listener.local_addr().expect("local addr");
        // Accept and hold: never answer, never close.
        tokio::spawn(async move {
            let held = listener.accept().await;
            std::future::pending::<()>().await;
            drop(held);
        });

        let started = Instant::now();
        let err = probe(&reqwest::Client::new(), &format!("http://{addr}"), None)
            .await
            .expect_err("a host that never answers is a failure");
        let elapsed = started.elapsed();

        assert!(
            matches!(err, ProbeError::Unreachable(_)),
            "expected a transport failure, got {err:?}"
        );
        assert!(
            elapsed < PROBE_TIMEOUT * 3,
            "probe took {elapsed:?}; the {PROBE_TIMEOUT:?} budget is not being enforced"
        );
    }
}
