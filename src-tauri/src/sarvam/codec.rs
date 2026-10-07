//! Wire format for the Sarvam realtime STT WebSocket: URL construction,
//! client/server JSON frames, and f32 → base64 PCM16 encoding. Pure functions
//! only — everything here is unit-testable without a network.

use super::SessionCfg;
use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use serde::{Deserialize, Serialize};

/// Frames the client sends. Audio is base64 text inside a JSON frame — the
/// realtime API does not take binary WS frames.
#[derive(Serialize, Debug)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum ClientMsg<'a> {
    AudioInput { audio: &'a str },
    SpeechStart,
    SpeechEnd,
    /// Documented realtime event, kept for the protocol tests. Not sent on
    /// the dictation path: it never produces a `session.end`, and under VAD
    /// it produces nothing at all.
    #[allow(dead_code)]
    Flush,
    End,
    /// Documented keepalive; sessions are short enough not to need it today.
    #[allow(dead_code)]
    Ping,
}

impl ClientMsg<'_> {
    pub fn to_json(&self) -> String {
        serde_json::to_string(self).expect("client frame serializes")
    }
}

/// Frames the server sends. Unknown events parse to `Unknown` so a beta-API
/// addition never kills a session.
#[derive(Deserialize, Debug)]
#[serde(tag = "event")]
pub enum ServerMsg {
    #[serde(rename = "session.begin")]
    SessionBegin,
    #[serde(rename = "vad.speech_start")]
    VadSpeechStart,
    #[serde(rename = "vad.speech_end")]
    VadSpeechEnd,
    #[serde(rename = "transcript.partial")]
    TranscriptPartial { text: String },
    #[serde(rename = "transcript.final")]
    TranscriptFinal {
        #[serde(default)]
        utterance_idx: u64,
        text: String,
    },
    #[serde(rename = "config.updated")]
    ConfigUpdated,
    #[serde(rename = "session.end")]
    SessionEnd,
    #[serde(rename = "pong")]
    Pong,
    #[serde(rename = "error")]
    Error {
        #[serde(default)]
        code: Option<String>,
        #[serde(default)]
        is_fatal: bool,
        #[serde(default)]
        message: String,
    },
    #[serde(other)]
    Unknown,
}

pub fn parse_server(json: &str) -> Option<ServerMsg> {
    match serde_json::from_str(json) {
        Ok(msg) => Some(msg),
        Err(e) => {
            // Deliberately no frame content in the log: unknown frames can
            // carry transcript text, and the file log persists. Not `{e}`
            // either: serde_json's message quotes a string value of the wrong
            // type, which on this socket can be the transcript. Its category
            // and position only, as `sarvam::translate` and `sarvam::batch` do.
            tracing::debug!(
                bytes = json.len(),
                category = ?e.classify(),
                line = e.line(),
                column = e.column(),
                "unparseable server frame"
            );
            None
        }
    }
}

/// 16 kHz mono f32 samples → little-endian PCM16 bytes. Shared by the
/// realtime frame encoder below and `f32_to_wav16` (the batch REST
/// fallback's container format) so the clamp-and-scale logic exists once.
fn f32_to_pcm16_bytes(samples: &[f32]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(samples.len() * 2);
    for &s in samples {
        let v = (s.clamp(-1.0, 1.0) * 32767.0) as i16;
        bytes.extend_from_slice(&v.to_le_bytes());
    }
    bytes
}

/// 16 kHz mono f32 samples → little-endian PCM16 → base64, the shape the
/// `audio_input` frame wants for `encoding=linear16`.
pub fn f32_to_pcm16_b64(samples: &[f32]) -> String {
    B64.encode(f32_to_pcm16_bytes(samples))
}

/// `samples` → a complete little-endian PCM16 WAV file in memory, for the
/// batch REST fallback (`sarvam::batch::transcribe`): the realtime frame
/// format above needs no container (it rides inside a JSON `audio_input`
/// event), but the REST endpoint takes a real audio file as a multipart
/// part. `sample_rate` is a parameter rather than a hardcoded 16000 only so
/// the roundtrip test below can assert the header actually carries what it
/// was given, not just the one value this app happens to always pass.
pub fn f32_to_wav16(samples: &[f32], sample_rate: u32) -> Vec<u8> {
    let pcm = f32_to_pcm16_bytes(samples);
    let data_len = pcm.len() as u32;
    const CHANNELS: u16 = 1;
    const BITS_PER_SAMPLE: u16 = 16;
    let block_align = CHANNELS * BITS_PER_SAMPLE / 8;
    let byte_rate = sample_rate * block_align as u32;

    let mut wav = Vec::with_capacity(44 + pcm.len());
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&(36 + data_len).to_le_bytes());
    wav.extend_from_slice(b"WAVE");
    wav.extend_from_slice(b"fmt ");
    wav.extend_from_slice(&16u32.to_le_bytes()); // fmt chunk size (PCM)
    wav.extend_from_slice(&1u16.to_le_bytes()); // audio format = PCM
    wav.extend_from_slice(&CHANNELS.to_le_bytes());
    wav.extend_from_slice(&sample_rate.to_le_bytes());
    wav.extend_from_slice(&byte_rate.to_le_bytes());
    wav.extend_from_slice(&block_align.to_le_bytes());
    wav.extend_from_slice(&BITS_PER_SAMPLE.to_le_bytes());
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&data_len.to_le_bytes());
    wav.extend_from_slice(&pcm);
    wav
}

/// The realtime URL for this dictation: the lane's host
/// ([`Lane::realtime_endpoint`]) and the same query either host takes.
///
/// Byte-for-byte the same query on both lanes on purpose. The relay rebuilds
/// it from its own allowlist before dialling Sarvam, so anything the app
/// sends outside `language_code`, `stream_type`, `mode`, `endpointing` and
/// `prompt` is replaced server-side with the pinned value — `model`,
/// `encoding` and `sample_rate` included. Sending them anyway keeps one URL
/// builder for both lanes.
pub fn ws_url(cfg: &SessionCfg) -> String {
    let mut url = format!(
        "{}?model={}&language_code={}&stream_type={}&mode={}&endpointing={}&encoding=linear16&sample_rate=16000",
        cfg.lane.realtime_endpoint(),
        super::REALTIME_MODEL,
        cfg.language_code,
        cfg.stream_type,
        cfg.mode,
        cfg.endpointing.as_str(),
    );
    if let Some(prompt) = cfg.prompt.as_deref().filter(|p| !p.trim().is_empty()) {
        url.push_str("&prompt=");
        url.extend(percent_encoding::utf8_percent_encode(
            prompt,
            percent_encoding::NON_ALPHANUMERIC,
        ));
    }
    url
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sarvam::Endpointing;

    #[test]
    fn client_frames_serialize_to_documented_events() {
        assert_eq!(ClientMsg::SpeechStart.to_json(), r#"{"event":"speech_start"}"#);
        assert_eq!(ClientMsg::SpeechEnd.to_json(), r#"{"event":"speech_end"}"#);
        assert_eq!(ClientMsg::Flush.to_json(), r#"{"event":"flush"}"#);
        assert_eq!(ClientMsg::End.to_json(), r#"{"event":"end"}"#);
        assert_eq!(
            ClientMsg::AudioInput { audio: "QUJD" }.to_json(),
            r#"{"event":"audio_input","audio":"QUJD"}"#
        );
    }

    #[test]
    fn parses_documented_server_frames() {
        // Shapes from docs.sarvam.ai realtime STT reference (Aug 2026).
        assert!(matches!(
            parse_server(r#"{"event":"session.begin","session_id":"abc"}"#),
            Some(ServerMsg::SessionBegin)
        ));
        match parse_server(
            r#"{"event":"transcript.partial","utterance_idx":0,"text":"नमस्ते","language":"hi-IN"}"#,
        ) {
            Some(ServerMsg::TranscriptPartial { text }) => {
                assert_eq!(text, "नमस्ते");
            }
            other => panic!("unexpected: {other:?}"),
        }
        match parse_server(
            r#"{"event":"transcript.final","utterance_idx":1,"text":"hello world","language":"en-IN","language_confidence":0.98,"start_s":1.2,"end_s":3.4}"#,
        ) {
            Some(ServerMsg::TranscriptFinal { utterance_idx, text }) => {
                assert_eq!(utterance_idx, 1);
                assert_eq!(text, "hello world");
            }
            other => panic!("unexpected: {other:?}"),
        }
        match parse_server(
            r#"{"event":"error","code":"invalid_audio","is_fatal":true,"message":"bad frame"}"#,
        ) {
            Some(ServerMsg::Error { is_fatal, message, .. }) => {
                assert!(is_fatal);
                assert_eq!(message, "bad frame");
            }
            other => panic!("unexpected: {other:?}"),
        }
        assert!(matches!(
            parse_server(r#"{"event":"session.end","audio_duration_s":4.2}"#),
            Some(ServerMsg::SessionEnd)
        ));
        assert!(matches!(
            parse_server(r#"{"event":"vad.speech_end"}"#),
            Some(ServerMsg::VadSpeechEnd)
        ));
    }

    #[test]
    fn unknown_event_is_tolerated() {
        assert!(matches!(
            parse_server(r#"{"event":"something.new","data":1}"#),
            Some(ServerMsg::Unknown)
        ));
        assert!(parse_server("not json").is_none());
    }

    /// Everything a `tracing` fmt subscriber writes, kept in memory.
    #[derive(Clone, Default)]
    struct CapturedLog(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

    impl std::io::Write for CapturedLog {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().expect("log buffer").extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for CapturedLog {
        type Writer = CapturedLog;
        fn make_writer(&'a self) -> CapturedLog {
            self.clone()
        }
    }

    /// A realtime frame of the wrong shape must not put its words in the log.
    /// serde_json's error quotes a string value that has the wrong type, and
    /// the realtime socket carries the transcripts, so a bare JSON string, or
    /// a string where `utterance_idx` or `is_fatal` should be, would put the
    /// transcript in a file kept for a week. This runs `parse_server` under
    /// the same fmt layer the log file uses, at the level that file gets.
    #[test]
    fn a_misshapen_server_frame_is_logged_without_its_words() {
        let secret = "meet me at the clinic at four";
        let frames = [
            serde_json::to_string(secret).expect("a JSON string"),
            format!(r#"{{"event":"transcript.final","utterance_idx":"{secret}","text":"x"}}"#),
            format!(r#"{{"event":"error","is_fatal":"{secret}","message":"x"}}"#),
        ];
        for frame in &frames {
            // The hazard is real: the error's own text carries the words.
            let e = serde_json::from_str::<ServerMsg>(frame).expect_err("misshapen frame");
            assert!(
                e.to_string().contains(secret),
                "if this ever stops holding, the hazard changed shape: {e}"
            );

            let sink = CapturedLog::default();
            let subscriber = tracing_subscriber::fmt()
                .with_max_level(tracing::Level::TRACE)
                .with_ansi(false)
                .with_writer(sink.clone())
                .finish();
            let parsed = tracing::subscriber::with_default(subscriber, || parse_server(frame));
            assert!(parsed.is_none(), "{frame} must not parse");

            let logged = String::from_utf8(sink.0.lock().expect("log buffer").clone())
                .expect("the log is UTF-8");
            assert!(
                logged.contains("unparseable server frame"),
                "control: the failure is logged at all: {logged:?}"
            );
            assert!(
                !logged.contains(secret),
                "the log line carries the frame's words: {logged}"
            );
        }
    }

    #[test]
    fn pcm16_roundtrip() {
        let encoded = f32_to_pcm16_b64(&[0.0, 1.0, -1.0, 0.5]);
        let bytes = B64.decode(encoded).unwrap();
        let samples: Vec<i16> = bytes
            .chunks_exact(2)
            .map(|c| i16::from_le_bytes([c[0], c[1]]))
            .collect();
        assert_eq!(samples, vec![0, 32767, -32767, 16383]);
        // Out-of-range input clamps instead of wrapping.
        let clipped = f32_to_pcm16_b64(&[2.0, -2.0]);
        let bytes = B64.decode(clipped).unwrap();
        assert_eq!(i16::from_le_bytes([bytes[0], bytes[1]]), 32767);
        assert_eq!(i16::from_le_bytes([bytes[2], bytes[3]]), -32767);
    }

    #[test]
    fn ws_url_carries_all_session_params() {
        let cfg = SessionCfg {
            language_code: "hi-IN".into(),
            stream_type: "balanced".into(),
            mode: "transcribe".into(),
            endpointing: Endpointing::Manual,
            prompt: None,
            lane: crate::sarvam::Lane::Byok,
        };
        let url = ws_url(&cfg);
        assert!(url.starts_with("wss://api.sarvam.ai/speech-to-text-realtime/ws?"));
        for expect in [
            "model=saaras:v3-realtime",
            "language_code=hi-IN",
            "stream_type=balanced",
            "mode=transcribe",
            "endpointing=manual",
            "encoding=linear16",
            "sample_rate=16000",
        ] {
            assert!(url.contains(expect), "missing {expect} in {url}");
        }
        assert!(!url.contains("prompt="), "no prompt param without hints");
    }

    /// Parses just enough of a WAV header back out to prove the writer
    /// produced a well-formed file, without pulling in an audio crate.
    fn parse_wav_header(wav: &[u8]) -> (u32, u16, u16, u32, u32) {
        assert_eq!(&wav[0..4], b"RIFF");
        assert_eq!(&wav[8..12], b"WAVE");
        assert_eq!(&wav[12..16], b"fmt ");
        let channels = u16::from_le_bytes([wav[22], wav[23]]);
        let sample_rate = u32::from_le_bytes([wav[24], wav[25], wav[26], wav[27]]);
        let byte_rate = u32::from_le_bytes([wav[28], wav[29], wav[30], wav[31]]);
        let bits_per_sample = u16::from_le_bytes([wav[34], wav[35]]);
        assert_eq!(&wav[36..40], b"data");
        let data_len = u32::from_le_bytes([wav[40], wav[41], wav[42], wav[43]]);
        (sample_rate, channels, bits_per_sample, byte_rate, data_len)
    }

    #[test]
    fn wav_header_carries_the_requested_format() {
        let samples = vec![0.0f32; 16_000]; // 1 second at 16 kHz
        let wav = f32_to_wav16(&samples, 16_000);
        let (sample_rate, channels, bits, byte_rate, data_len) = parse_wav_header(&wav);
        assert_eq!(sample_rate, 16_000);
        assert_eq!(channels, 1);
        assert_eq!(bits, 16);
        assert_eq!(byte_rate, 32_000); // 16000 * 1 channel * 2 bytes/sample
        assert_eq!(data_len, 32_000); // 16000 samples * 2 bytes each
        assert_eq!(wav.len(), 44 + data_len as usize);
    }

    /// The PCM payload after the 44-byte header must be the exact same
    /// bytes `f32_to_pcm16_b64` would have encoded — one clamp-and-scale
    /// implementation, two containers.
    #[test]
    fn wav_payload_matches_the_realtime_pcm16_encoding() {
        let samples = vec![0.0, 0.5, -0.5, 1.0, -1.0, 2.0, -2.0];
        let wav = f32_to_wav16(&samples, 16_000);
        let pcm_from_wav = &wav[44..];
        let pcm_from_b64 = B64.decode(f32_to_pcm16_b64(&samples)).unwrap();
        assert_eq!(pcm_from_wav, pcm_from_b64.as_slice());
    }

    #[test]
    fn an_empty_buffer_still_produces_a_valid_header() {
        let wav = f32_to_wav16(&[], 16_000);
        let (_, _, _, _, data_len) = parse_wav_header(&wav);
        assert_eq!(data_len, 0);
        assert_eq!(wav.len(), 44);
    }

    #[test]
    fn ws_url_percent_encodes_prompt() {
        let cfg = SessionCfg {
            language_code: "auto".into(),
            stream_type: "balanced".into(),
            mode: "transcribe".into(),
            endpointing: Endpointing::Vad,
            prompt: Some("Deveshu, Sarvam AI".into()),
            lane: crate::sarvam::Lane::Byok,
        };
        let url = ws_url(&cfg);
        assert!(
            url.ends_with("&prompt=Deveshu%2C%20Sarvam%20AI"),
            "unexpected prompt encoding: {url}"
        );
    }

    fn relay_cfg() -> SessionCfg {
        SessionCfg {
            language_code: "hi-IN".into(),
            stream_type: "balanced".into(),
            mode: "transcribe".into(),
            endpointing: Endpointing::Manual,
            prompt: Some("Deveshu, Sarvam AI".into()),
            lane: crate::sarvam::Lane::Cloud {
                relay: "https://butterflylabs-relay.example.workers.dev".into(),
            },
        }
    }

    /// The Cloud lane is the same dictation against a different host: the
    /// query is byte-for-byte what Sarvam gets, and only the origin and path
    /// move. `https` becomes `wss` — a WebSocket URL is what
    /// `into_client_request` will parse.
    #[test]
    fn ws_url_for_the_relay_keeps_the_query_and_moves_the_host() {
        let url = ws_url(&relay_cfg());
        assert!(
            url.starts_with("wss://butterflylabs-relay.example.workers.dev/v1/realtime?"),
            "unexpected relay endpoint: {url}"
        );
        let byok = ws_url(&SessionCfg {
            lane: crate::sarvam::Lane::Byok,
            ..relay_cfg()
        });
        assert_eq!(
            url.split_once('?').expect("a query").1,
            byok.split_once('?').expect("a query").1,
            "both lanes must send the same query"
        );
    }

    /// A local `wrangler dev` is plain HTTP, and the hidden setting is the
    /// only way to reach one. `ws://` keeps that usable without loosening
    /// anything for the shipped default, which is `https`.
    #[test]
    fn a_plain_http_relay_becomes_a_ws_url() {
        let url = ws_url(&SessionCfg {
            lane: crate::sarvam::Lane::Cloud {
                relay: "http://127.0.0.1:8787".into(),
            },
            ..relay_cfg()
        });
        assert!(url.starts_with("ws://127.0.0.1:8787/v1/realtime?"), "{url}");
    }
}
