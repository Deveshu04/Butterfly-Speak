//! Final-pass recognition thread. Owns the active offline model and the
//! punctuation model, runs the cleanup pipeline, and handles live model
//! switching; nothing here ever leaves this thread, which serializes model
//! use by construction.
//!
//! The on-device polish model (~700 MB) is loaded on the first dictation
//! that needs it and dropped again after `polish::IDLE_UNLOAD` without use,
//! whether or not another dictation comes along to notice. `AsrMsg::Unload`
//! frees it and the recognizer at once, for when the provider leaves Local.

use crate::cleanup::{self, punctuation::Punctuator, CleanupSettings, ModelCaps};
use crate::events;
use crate::state::ControlMsg;
use crossbeam_channel::{Receiver, Sender};
use sherpa_onnx::{OfflineRecognizer, OfflineRecognizerConfig};
use std::sync::{Arc, RwLock};
use std::time::Instant;
use tauri::Emitter;

#[derive(Clone, Debug)]
pub struct ModelSpec {
    pub id: String,
    pub dir: String,
    pub engine: String,
    pub native_punct: bool,
}

pub enum AsrMsg {
    LoadModel(ModelSpec),
    /// The provider is not Local: free the on-device models, if any are
    /// loaded.
    Unload,
    Finalize { req_id: u64, audio: Vec<f32> },
}

pub fn spawn(
    app: tauri::AppHandle,
    initial: Option<ModelSpec>,
    punct_dir: String,
    settings: Arc<RwLock<CleanupSettings>>,
    rx: Receiver<AsrMsg>,
    tx: Sender<ControlMsg>,
) {
    std::thread::Builder::new()
        .name("asr-offline".into())
        .spawn(move || {
            let punct = match Punctuator::load(&punct_dir) {
                Ok(p) => Some(p),
                Err(e) => {
                    tracing::warn!("punctuation model unavailable ({e:#}); raw punctuation only");
                    None
                }
            };

            let mut current: Option<(OfflineRecognizer, ModelCaps)> = None;
            if let Some(spec) = initial {
                current = load_and_announce(&app, spec);
            }

            #[cfg(feature = "polish")]
            let mut polisher: Option<crate::cleanup::polish::Polisher> = None;

            loop {
                // With a polish model loaded, wait no longer than it has left
                // before it counts as idle, so it is freed on time even when
                // no dictation arrives to notice.
                #[cfg(feature = "polish")]
                let msg = {
                    use crate::cleanup::polish::IDLE_UNLOAD;
                    use crossbeam_channel::RecvTimeoutError;
                    let idle_in = polisher
                        .as_ref()
                        .map(|p| IDLE_UNLOAD.saturating_sub(p.last_used.elapsed()));
                    // Only a wait that ran out unloads it: a message that
                    // arrived in time may be the dictation that needs it.
                    match idle_in {
                        Some(wait) => match rx.recv_timeout(wait) {
                            Ok(msg) => msg,
                            Err(RecvTimeoutError::Timeout) => {
                                polisher = None;
                                tracing::info!("polish model unloaded after 10 min idle");
                                continue;
                            }
                            Err(RecvTimeoutError::Disconnected) => break,
                        },
                        None => match rx.recv() {
                            Ok(msg) => msg,
                            Err(_) => break,
                        },
                    }
                };
                #[cfg(not(feature = "polish"))]
                let Ok(msg) = rx.recv() else {
                    break;
                };

                match msg {
                    AsrMsg::LoadModel(spec) => {
                        drop(current.take()); // free the old model before loading the new one
                        current = load_and_announce(&app, spec);
                    }
                    // Sent after every dictation off the local provider, so
                    // most arrive with nothing loaded and say nothing.
                    AsrMsg::Unload => {
                        let had = current.take().is_some();
                        #[cfg(feature = "polish")]
                        let had = polisher.take().is_some() || had;
                        if had {
                            tracing::info!("on-device models unloaded");
                        }
                    }
                    AsrMsg::Finalize { req_id, audio } => {
                        let Some((recognizer, caps)) = current.as_ref() else {
                            let _ = app.emit(
                                events::NOTICE_ERROR,
                                events::NoticePayload {
                                    message: "No speech model installed — open Settings → Speech engine".into(),
                                },
                            );
                            let _ = tx.send(ControlMsg::FinalResult {
                                req_id,
                                text: String::new(),
                                raw: String::new(),
                                fixes: Default::default(),
                                notice: None,
                                cut_short: false,
                            });
                            continue;
                        };
                        let secs = audio.len() as f32 / 16000.0;
                        let rms = (audio.iter().map(|s| s * s).sum::<f32>()
                            / audio.len().max(1) as f32)
                            .sqrt();
                        let t = Instant::now();
                        let stream = recognizer.create_stream();
                        stream.accept_waveform(16000, &audio);
                        recognizer.decode(&stream);
                        let raw = stream
                            .get_result()
                            .map(|r| r.text.trim().to_string())
                            .unwrap_or_default();
                        let cleanup_settings = settings.read().expect("settings lock").clone();
                        #[allow(unused_mut)]
                        let (mut text, dict_fixes) = if raw.is_empty() {
                            (raw.clone(), 0)
                        } else {
                            cleanup::run_pipeline(raw.clone(), *caps, &cleanup_settings, punct.as_ref())
                        };

                        // `mut` is only exercised when the `polish` feature is enabled
                        // (the default); without it nothing ever rejects the format.
                        #[allow(unused_mut)]
                        let mut notice: Option<String> = None;
                        #[cfg(feature = "polish")]
                        {
                            use crate::cleanup::polish::Polisher;
                            if cleanup_settings.level != crate::format::level::CleanupLevel::Off
                                && !text.is_empty()
                            {
                                if polisher.is_none() {
                                    let gguf = crate::settings::models_root().join(
                                        &crate::models::catalog::catalog().ai_polish.file_name,
                                    );
                                    if gguf.exists() {
                                        match Polisher::load(&gguf) {
                                            Ok(p) => polisher = Some(p),
                                            Err(e) => {
                                                tracing::warn!("polish model failed to load: {e:#}")
                                            }
                                        }
                                    }
                                }
                                if let Some(p) = polisher.as_mut() {
                                    let rule_output = text.clone();
                                    // An edited prompt applies on-device too,
                                    // when it fits the model's window.
                                    let prompt = crate::cleanup::polish::local_prompt(
                                        cleanup_settings.level,
                                        cleanup_settings.prompt_rules.as_deref(),
                                    );
                                    let result = p.polish(&rule_output, &prompt);
                                    // The local `Polisher::polish` is synchronous and has no
                                    // failure channel of its own — its `text` is always usable
                                    // (its own policy is to fall back to the input on any
                                    // error) — but it also reports whether generation hit
                                    // the token cap, so the truncation check in `guard::check`
                                    // (the one check that is exact and needs no
                                    // calibration) is a real signal on-device too, not a
                                    // hardcoded `"stop"` placeholder that could never fire.
                                    let synthetic_reply = crate::format::backend::ChatReply {
                                        text: result.text,
                                        finish_reason: if result.truncated { "length" } else { "stop" }.into(),
                                        prompt_tokens: 0,
                                        completion_tokens: 0,
                                        first_token_ms: None,
                                    };
                                    let (resolved, note) = crate::sarvam::ws::resolve_format(
                                        &raw,
                                        &rule_output,
                                        Some(&synthetic_reply),
                                        cleanup_settings.level,
                                    );
                                    text = resolved;
                                    notice = note;
                                }
                            }
                        }
                        // Counts only, never the transcript — the same policy
                        // as `sarvam::ws::resolve_format` and
                        // `sarvam::codec::parse_server`. Printing `raw` and
                        // `text` here would put every word ever dictated on
                        // this machine into the log file. The line carries
                        // what it is read for — how long the utterance was,
                        // how loud, how long decoding took, and whether
                        // cleanup changed the length much.
                        tracing::info!(
                            raw_words = raw.split_whitespace().count(),
                            clean_words = text.split_whitespace().count(),
                            clean_chars = text.chars().count(),
                            "finalize #{req_id}: {secs:.1}s audio (rms {rms:.4}) in {:?}",
                            t.elapsed()
                        );
                        let fixes = crate::state::FixCounts {
                            words_corrected: cleanup::words_changed(&raw, &text),
                            dict_fixes,
                        };
                        let _ = tx.send(ControlMsg::FinalResult {
                            req_id,
                            text,
                            raw,
                            fixes,
                            notice,
                            cut_short: false,
                        });
                    }
                }
            }
        })
        .expect("spawn asr-offline thread");
}

fn load_and_announce(
    app: &tauri::AppHandle,
    spec: ModelSpec,
) -> Option<(OfflineRecognizer, ModelCaps)> {
    let t = Instant::now();
    match load_model(&spec) {
        Ok(recognizer) => {
            // Warmup: the first ONNX session run pays graph optimization;
            // spend it now instead of on the first dictation.
            let stream = recognizer.create_stream();
            stream.accept_waveform(16000, &vec![0.0f32; 8000]);
            recognizer.decode(&stream);
            let _ = stream.get_result();
            tracing::info!("model {} loaded+warmed in {:?}", spec.id, t.elapsed());
            Some((
                recognizer,
                ModelCaps {
                    native_punct: spec.native_punct,
                },
            ))
        }
        Err(e) => {
            tracing::error!("failed to load model {}: {e:#}", spec.id);
            let _ = app.emit(
                events::NOTICE_ERROR,
                events::NoticePayload {
                    message: format!("Couldn't load model {}", spec.id),
                },
            );
            None
        }
    }
}

fn load_model(spec: &ModelSpec) -> anyhow::Result<OfflineRecognizer> {
    let dir = &spec.dir;
    let mut cfg = OfflineRecognizerConfig::default();
    cfg.model_config.tokens = Some(format!("{dir}\\tokens.txt"));
    cfg.model_config.num_threads = 4;

    match spec.engine.as_str() {
        "moonshine" => {
            cfg.model_config.moonshine.preprocessor = Some(format!("{dir}\\preprocess.onnx"));
            cfg.model_config.moonshine.encoder = Some(format!("{dir}\\encode.int8.onnx"));
            cfg.model_config.moonshine.uncached_decoder =
                Some(format!("{dir}\\uncached_decode.int8.onnx"));
            cfg.model_config.moonshine.cached_decoder =
                Some(format!("{dir}\\cached_decode.int8.onnx"));
        }
        "nemoCtc" => {
            cfg.model_config.nemo_ctc.model = Some(format!("{dir}\\model.int8.onnx"));
        }
        "nemoTransducer" => {
            cfg.model_config.transducer.encoder = Some(format!("{dir}\\encoder.int8.onnx"));
            cfg.model_config.transducer.decoder = Some(format!("{dir}\\decoder.int8.onnx"));
            cfg.model_config.transducer.joiner = Some(format!("{dir}\\joiner.int8.onnx"));
            cfg.model_config.model_type = Some("nemo_transducer".into());
        }
        other => anyhow::bail!("unknown engine {other}"),
    }

    OfflineRecognizer::create(&cfg)
        .ok_or_else(|| anyhow::anyhow!("OfflineRecognizer::create returned None (bad model files?)"))
}
