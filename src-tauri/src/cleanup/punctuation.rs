//! Model-based punctuation + casing restoration (7 MB CNN-BiLSTM, English).
//! Applied per sentinel-separated segment, and only to segments that carry no
//! sentence punctuation of their own (Moonshine sometimes punctuates,
//! Parakeet always does — double-punctuating reads badly).

use super::{NEWLINE, PARAGRAPH};
use sherpa_onnx::{OnlinePunctuation, OnlinePunctuationConfig, OnlinePunctuationModelConfig};

pub struct Punctuator {
    inner: OnlinePunctuation,
}

impl Punctuator {
    pub fn load(dir: &str) -> anyhow::Result<Self> {
        let config = OnlinePunctuationConfig {
            model: OnlinePunctuationModelConfig {
                cnn_bilstm: Some(format!("{dir}\\model.int8.onnx")),
                bpe_vocab: Some(format!("{dir}\\bpe.vocab")),
                num_threads: 1,
                debug: false,
                provider: Some("cpu".into()),
            },
        };
        OnlinePunctuation::create(&config)
            .map(|inner| Self { inner })
            .ok_or_else(|| anyhow::anyhow!("failed to create punctuation model from {dir}"))
    }

    pub fn apply(&self, text: String) -> String {
        text.split_inclusive([NEWLINE, PARAGRAPH])
            .map(|piece| {
                let (body, sep) = match piece.char_indices().last() {
                    Some((i, c)) if c == NEWLINE || c == PARAGRAPH => {
                        (&piece[..i], &piece[i..])
                    }
                    _ => (piece, ""),
                };
                let body = body.trim();
                if body.is_empty() || body.contains(['.', '!', '?']) {
                    format!("{body}{sep}")
                } else {
                    let punctuated = self
                        .inner
                        .add_punctuation(body)
                        .unwrap_or_else(|| body.to_string());
                    format!("{punctuated}{sep}")
                }
            })
            .collect::<Vec<_>>()
            .join(" ")
    }
}
