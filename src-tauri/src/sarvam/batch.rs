//! One-shot Sarvam batch STT fallback for a realtime session whose own
//! transcript cannot be used as it stands. `ws::drain_session` tees every
//! `CloudCmd::Audio` chunk it already receives into a local PCM buffer as it
//! streams; if the realtime drain produces an empty transcript, or its socket
//! died and left only a fragment, that buffer gets exactly one REST
//! transcription attempt before the dictation is treated as a failure (or,
//! for a fragment, filed rather than pasted).
//!
//! Privacy rule: the fallback target is Sarvam or nothing. It is never a
//! different provider, and never the on-device model either: the user chose
//! the cloud provider for this dictation, and a silent switch to local
//! inference would be a privacy surprise, not a convenience.
//!
//! Endpoint, field names and the `language_code` enum follow docs.sarvam.ai's
//! speech-to-text REST reference; `to_rest_language_code` and `BATCH_MODEL`
//! hold the parts that differ from realtime.

use super::SessionCfg;
use std::time::Duration;

pub const BATCH_URL: &str = "https://api.sarvam.ai/speech-to-text";

/// The REST endpoint's `model` enum is `saaras:v3` (default) / `saaras:v4`
/// (latest) — no `saarika` variant exists for this endpoint. `v3` is used,
/// matching `REALTIME_MODEL`'s own version and keeping `mode` meaningful:
/// Sarvam's docs note "mode only applicable when using saaras:v3".
pub const BATCH_MODEL: &str = "saaras:v3";

/// One attempt, generous but bounded. This is the *last* chance to salvage
/// a dictation the realtime session already failed to produce — not another
/// interactive round trip the user is watching a live pill for — so it can
/// afford to be longer than any of the realtime budgets, but it still has
/// to resolve before the user gives up and walks away.
pub const BATCH_TIMEOUT: Duration = Duration::from_secs(12);

/// At or below this much captured audio, a realtime session that produced
/// nothing gets no REST rescue: the hold was too short to hold speech worth a
/// second paid round trip, and the user hears "Didn't catch that" at once
/// instead of after it.
///
/// The controller already drops holds under `TAP_MAX` (350 ms) as taps, so
/// this only decides the band just above that. Single words spoken in
/// isolation measured 180-820 ms of voiced audio (median about 420 ms) across
/// 44 English and Hindi samples from Sarvam's and Windows' TTS voices; the
/// shortest were "stop" and "ठीक" at 180-240 ms. A real one-word dictation
/// also carries the pause between pressing and speaking and between
/// finishing and letting go, so it lands well past half a second. A hold of
/// 500 ms or less is mostly a tap that ran long, and gets no call; equality
/// does not qualify.
pub const MIN_FALLBACK_DURATION_MS: u64 = 500;

/// Above this, a fallback attempt is not just unnecessary but guaranteed to
/// fail: Sarvam's REST `speech-to-text` endpoint caps accepted audio at 30 s.
/// Longer audio needs the asynchronous job API (`batch_job`), which this
/// one-shot rescue is not. Skipping the call entirely above this bound —
/// rather than uploading the whole buffer to get a 400 back — matters most
/// for exactly the sessions the fallback exists for: a long hands-free
/// dictation whose realtime session produced nothing.
pub const MAX_FALLBACK_DURATION_MS: u64 = 30_000;

/// Sarvam's REST language vocabulary differs from realtime's in exactly the
/// two places `SarvamSettings::language_code`'s own doc comment already
/// calls out: realtime's `"auto"` sentinel is REST's `"unknown"`, and Odia
/// is `"or-IN"` on realtime but `"od-IN"` on REST. The REST reference's
/// `language_code` enum lists `unknown` for auto-detect and `od-IN`, with no
/// `auto` or `or-IN` entry at all.
pub fn to_rest_language_code(code: &str) -> &str {
    match code {
        "auto" => "unknown",
        "or-IN" => "od-IN",
        other => other,
    }
}

#[derive(serde::Deserialize)]
struct BatchResponse {
    #[serde(default)]
    transcript: String,
}

/// One REST transcription of `pcm` (16 kHz mono f32), or `None` on any
/// failure — network, non-2xx, or a body that doesn't parse. The caller
/// already has its own error to report (the realtime session's own
/// failure, or "Didn't catch that"), so this is a best-effort rescue, not a
/// second source of user-facing errors: it logs and returns `None` rather
/// than propagating a `Result` for the caller to translate.
///
/// `url` is a parameter rather than the hardcoded `BATCH_URL` so tests can
/// point this at a loopback stub server — the same reason
/// `format::backend::Backend` carries its own `base_url` field. `timeout` is
/// a parameter for the same reason, one level down: a test can shrink it to
/// prove a stalled *body* — not just a stalled connect — is actually bounded
/// (see `a_stalled_response_body_times_out_rather_than_hanging_forever`)
/// without a real dictation ever waiting less than the real `BATCH_TIMEOUT`.
///
/// No transcript content in the failure logs below (`sarvam::codec`'s
/// logging policy applies here too). This endpoint's 2xx body *is* the
/// user's transcript, so the non-2xx branch logs the status code and never
/// the body: what a server puts in an error body is not something to bet
/// the user's words on.
///
/// The parse-failure branch logs `classify()`/`line()`/`column()` — a
/// fieldless enum and two numbers — rather than the error itself:
/// `serde_json::Error`'s `Display` is a bare line/column only for *syntax*
/// errors, but a type mismatch embeds the offending value, so a 2xx carrying
/// a bare JSON string would print the entire transcript into the log.
pub async fn transcribe(
    http: &reqwest::Client,
    url: &str,
    api_key: &str,
    cfg: &SessionCfg,
    pcm: &[f32],
    timeout: Duration,
) -> Option<String> {
    let wav = super::codec::f32_to_wav16(pcm, 16_000);
    let file_part = reqwest::multipart::Part::bytes(wav)
        .file_name("utterance.wav")
        .mime_str("audio/wav")
        .expect("audio/wav is a well-formed mime string");
    let mut form = reqwest::multipart::Form::new()
        .part("file", file_part)
        .text("model", BATCH_MODEL)
        .text("mode", cfg.mode.clone());
    let lang = to_rest_language_code(&cfg.language_code);
    if lang != "unknown" {
        // REST's own default is auto-detect; only send a code when the user
        // actually picked one, mirroring `codec::ws_url`'s don't-send-
        // unless-set style for `prompt`.
        form = form.text("language_code", lang.to_string());
    }

    // `.timeout(timeout)` on the request itself — not a `tokio::time::timeout`
    // wrapped around only `.send()` — matches
    // `format::backend::Backend::complete`'s own call for the same reason:
    // reqwest applies a request-level timeout across the whole request, connect
    // through the response body finishing, so it bounds `resp.text()` below
    // too. Wrapping only `.send()` would leave the body read unbounded against
    // the client this shares (`ws.rs`'s dispatcher `http` client, from
    // `build_http_client`, which bounds only the connect, at 4 s) — a stalled
    // body (a captive portal, a TLS-inspecting proxy, an edge that answers
    // headers then hangs) would then park this call forever, wedging the
    // single-task dispatcher behind it.
    let send = http
        .post(url)
        .header(super::AUTH_HEADER, api_key)
        .multipart(form)
        .timeout(timeout)
        .send();
    let resp = match send.await {
        Ok(r) => r,
        Err(e) if e.is_timeout() => {
            tracing::warn!("batch fallback timed out after {timeout:?}");
            return None;
        }
        Err(e) => {
            tracing::warn!("batch fallback request failed: {e:#}");
            return None;
        }
    };
    let status = resp.status();
    let raw = match resp.text().await {
        Ok(t) => t,
        Err(e) if e.is_timeout() => {
            tracing::warn!("batch fallback body read timed out after {timeout:?}");
            return None;
        }
        Err(e) => {
            tracing::warn!("batch fallback body read failed: {e:#}");
            return None;
        }
    };
    if !status.is_success() {
        // Status only — never `{raw:?}`. See this function's doc comment.
        tracing::warn!("batch fallback returned HTTP {status}");
        return None;
    }
    match serde_json::from_str::<BatchResponse>(&raw) {
        Ok(body) => Some(body.transcript),
        Err(e) => {
            // Never `{e}`: a type mismatch embeds the offending value, and
            // here that value is the transcript.
            tracing::warn!(
                category = ?e.classify(),
                line = e.line(),
                column = e.column(),
                "batch fallback reply was not the documented shape"
            );
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auto_maps_to_the_rest_auto_detect_sentinel() {
        assert_eq!(to_rest_language_code("auto"), "unknown");
    }

    #[test]
    fn odia_maps_to_its_rest_code() {
        assert_eq!(to_rest_language_code("or-IN"), "od-IN");
    }

    #[test]
    fn an_ordinary_language_code_passes_through_unchanged() {
        assert_eq!(to_rest_language_code("hi-IN"), "hi-IN");
        assert_eq!(to_rest_language_code("en-IN"), "en-IN");
    }

    /// REST's own "od-IN" must not be double-translated if it ever reaches
    /// this function already in REST form.
    #[test]
    fn rest_odia_is_left_alone() {
        assert_eq!(to_rest_language_code("od-IN"), "od-IN");
    }

    fn sample_cfg(language_code: &str, mode: &str) -> SessionCfg {
        SessionCfg {
            language_code: language_code.into(),
            stream_type: "balanced".into(),
            mode: mode.into(),
            endpointing: super::super::Endpointing::Manual,
            prompt: None,
            lane: super::super::Lane::Byok,
        }
    }

    /// Reads a raw HTTP/1.1 request off `socket` until the connection
    /// closes, draining the multipart body without parsing it — this test
    /// only needs the server to have consumed the whole request before it
    /// answers. Same non-blocking readable/try_read loop as `format::
    /// backend` and `sarvam::chat`'s own stub-server tests.
    async fn drain_request(socket: &tokio::net::TcpStream) {
        let mut buf = [0u8; 8192];
        loop {
            socket.readable().await.expect("socket readable");
            match socket.try_read(&mut buf) {
                Ok(0) => return,
                Ok(_) => continue,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => return,
                Err(e) => panic!("failed reading stub request: {e}"),
            }
        }
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

    /// A non-2xx status must be treated as a failure (`None`). Sarvam's 400s
    /// can have an empty body, so the status code is the only signal (as in
    /// `format::backend::complete`).
    #[tokio::test]
    async fn a_non_success_status_is_none() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind loopback listener");
        let addr = listener.local_addr().expect("local addr");

        tokio::spawn(async move {
            if let Ok((socket, _)) = listener.accept().await {
                drain_request(&socket).await;
                write_response(
                    &socket,
                    b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .await;
            }
        });

        let http = reqwest::Client::new();
        let cfg = sample_cfg("auto", "transcribe");
        let out = transcribe(&http, &format!("http://{addr}"), "k", &cfg, &[0.0, 0.1, -0.1], BATCH_TIMEOUT)
            .await;
        assert!(out.is_none());
    }

    /// The happy path: a 200 with the documented shape resolves to the
    /// transcript text.
    #[tokio::test]
    async fn a_success_response_yields_the_transcript() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind loopback listener");
        let addr = listener.local_addr().expect("local addr");

        tokio::spawn(async move {
            if let Ok((socket, _)) = listener.accept().await {
                drain_request(&socket).await;
                let body = r#"{"request_id":"x","transcript":"hello there","language_code":"en-IN"}"#;
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                write_response(&socket, response.as_bytes()).await;
            }
        });

        let http = reqwest::Client::new();
        let cfg = sample_cfg("auto", "transcribe");
        let out = transcribe(&http, &format!("http://{addr}"), "k", &cfg, &[0.0, 0.1, -0.1], BATCH_TIMEOUT)
            .await;
        assert_eq!(out, Some("hello there".to_string()));
    }

    /// A 200 whose body isn't the documented `{"transcript": ...}` shape
    /// must not panic and must resolve to `None`.
    #[tokio::test]
    async fn an_unparseable_body_is_none() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind loopback listener");
        let addr = listener.local_addr().expect("local addr");

        tokio::spawn(async move {
            if let Ok((socket, _)) = listener.accept().await {
                drain_request(&socket).await;
                write_response(
                    &socket,
                    b"HTTP/1.1 200 OK\r\nContent-Length: 8\r\nConnection: close\r\n\r\nnot json",
                )
                .await;
            }
        });

        let http = reqwest::Client::new();
        let cfg = sample_cfg("auto", "transcribe");
        let out = transcribe(&http, &format!("http://{addr}"), "k", &cfg, &[0.0], BATCH_TIMEOUT).await;
        assert!(out.is_none());
    }

    /// A response that sends its headers (200, with a `Content-Length`
    /// promising a body) and then never writes another byte must not hang
    /// this call forever — it must resolve to `None` once `timeout` elapses.
    /// Uses a short `timeout` (not the real `BATCH_TIMEOUT`) so this test
    /// finishes in well under a second; the mechanism under test —
    /// `.timeout(timeout)` on the request itself, applied by reqwest across
    /// the whole request including the body read — is what the dispatcher's
    /// real call uses. A `tokio::time::timeout` around only `.send()` hangs
    /// on this scenario, and with it `ws::run_session`'s single dispatcher
    /// task.
    #[tokio::test]
    async fn a_stalled_response_body_times_out_rather_than_hanging_forever() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind loopback listener");
        let addr = listener.local_addr().expect("local addr");

        tokio::spawn(async move {
            if let Ok((socket, _)) = listener.accept().await {
                drain_request(&socket).await;
                // Headers promise a body that never actually arrives — the
                // connection is simply held open, never closed, never
                // written to again.
                write_response(
                    &socket,
                    b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 40\r\n\r\n",
                )
                .await;
                tokio::time::sleep(Duration::from_secs(60)).await;
                drop(socket);
            }
        });

        let http = reqwest::Client::new();
        let cfg = sample_cfg("auto", "transcribe");
        let short_timeout = Duration::from_millis(200);
        let started = tokio::time::Instant::now();
        let out = transcribe(&http, &format!("http://{addr}"), "k", &cfg, &[0.0], short_timeout).await;
        assert!(out.is_none());
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the stalled body must not be able to hang this call — took {:?}",
            started.elapsed()
        );
    }

    /// The parse-failure log must not be able to carry the transcript.
    /// `serde_json::Error`'s `Display` is a bare line/column only for
    /// *syntax* errors; a type mismatch embeds the offending value, and a 2xx
    /// carrying a bare JSON string makes that value the whole transcript.
    /// Both halves are asserted — that `{e}` really would leak, so nobody
    /// "simplifies" the log back to it, and that the fields actually logged
    /// cannot. Same guarantee `sarvam::translate` pins for the same reason.
    #[test]
    fn the_parse_failure_log_fields_cannot_carry_the_body() {
        let secret = "meet me at the clinic at four";
        let body = serde_json::to_string(secret).expect("a JSON string");
        // Matched rather than `expect_err`: that would need `BatchResponse` to
        // be `Debug`, and a `Debug` on a struct whose one field holds the
        // transcript is the same hazard this test exists to close.
        let e = match serde_json::from_str::<BatchResponse>(&body) {
            Ok(_) => panic!("a bare string must not parse as the documented shape"),
            Err(e) => e,
        };

        assert!(
            e.to_string().contains(secret),
            "if this ever stops holding, the hazard changed shape: {e}"
        );

        let logged = format!("{:?} {} {}", e.classify(), e.line(), e.column());
        assert!(
            !logged.contains(secret),
            "the logged fields must not carry body text: {logged}"
        );
    }

    /// The documented shape parses into exactly the transcript text.
    #[test]
    fn the_documented_response_shape_parses() {
        let body: BatchResponse = serde_json::from_str(
            r#"{"request_id":"x","transcript":"hello there","language_code":"en-IN"}"#,
        )
        .unwrap();
        assert_eq!(body.transcript, "hello there");
    }
}
