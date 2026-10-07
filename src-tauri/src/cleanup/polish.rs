//! Optional "AI Polish" stage: Qwen2.5-0.5B-Instruct via llama.cpp formats
//! dictated speech on-device. Entirely offline like everything else.
//! Feature-gated behind `polish`.
//!
//! The system prompt is caller-supplied — the level's on-device prompt
//! ([`local_prompt`]), which is the cloud prompt at Light and Balanced and
//! shorter rules at High, or the user's saved rules when they fit — not a
//! hardcoded
//! grammar-only instruction, so the level the guardrail judges local output
//! against is the level the prompt expresses.
//!
//! Failure policy: any error, empty output, or exceeding the time budget
//! returns the input unchanged — polish must never lose the user's words.

use llama_cpp_2::context::params::LlamaContextParams;
use llama_cpp_2::llama_backend::LlamaBackend;
use llama_cpp_2::llama_batch::LlamaBatch;
use llama_cpp_2::model::params::LlamaModelParams;
use llama_cpp_2::model::{AddBos, LlamaModel};
use llama_cpp_2::sampling::LlamaSampler;
use std::num::NonZeroU32;
use std::path::Path;
use std::time::{Duration, Instant};

/// Hard budget per polish; past this we return the unpolished text.
const TIMEOUT: Duration = Duration::from_secs(4);
/// Long dictations skip polish entirely to keep latency bounded.
const MAX_INPUT_WORDS: usize = 120;
/// Unload the model after this long without use (frees ~700 MB).
pub const IDLE_UNLOAD: Duration = Duration::from_secs(600);

/// Context window for one polish pass. It has to hold the system prompt, the
/// input and the whole reply at once — and the system prompt is not this
/// module's to size: it comes from `CleanupLevel`. When the injection hardening
/// stanza was appended to every level, the High prompt this module received at
/// the time grew from 743 to 1,457 characters, and a 1024-token window left
/// 404-452 tokens of headroom: less than [`MAX_NEW_TOKENS`] asks for, which is
/// a hard decode failure rather than a short reply (see [`generation_budget`]).
/// The test `the_worst_case_high_polish_still_gets_a_usable_budget` checks
/// the shipped on-device prompts against this window, and [`local_prompt`]
/// checks a saved override against it at run time. Qwen2.5-0.5B's KV cache is
/// ~12 KB per token, so doubling this costs ~25 MB.
const N_CTX: u32 = 2048;

/// Ceiling on generated tokens. Must comfortably exceed the token count of
/// the longest accepted input, or the reply is truncated — same reasoning as
/// the cloud path's MAX_OUTPUT_TOKENS (see `sarvam::chat`). MAX_INPUT_WORDS
/// is 120 words, roughly 150-200 Qwen2.5 tokens, and the High prompt
/// explicitly asks for numbered-list expansion, which grows the output past
/// the input's own token count. 256 is too tight: a legitimate long
/// High-level polish hits the cap, comes back `truncated`, and — truncation
/// being always enforced — falls back with a "Formatting was cut short"
/// notice for perfectly good input.
const MAX_NEW_TOKENS: i32 = 512;

pub struct Polisher {
    backend: LlamaBackend,
    model: LlamaModel,
    pub last_used: Instant,
}

/// What a local polish pass produced.
pub struct PolishResult {
    pub text: String,
    /// True when generation hit the `max_new` token cap without reaching a
    /// natural stop (an end-of-generation token) — the on-device analogue of
    /// the cloud path's `finish_reason == "length"`. Wired into
    /// `asr::offline`'s synthetic `ChatReply` so the guardrail's truncation
    /// check — the one check that is exact and needs no calibration — is a real
    /// signal on-device too, not a hardcoded `"stop"` placeholder that can
    /// never fire.
    pub truncated: bool,
}

/// Internal result from one generation pass, before the empty/error
/// fallback policy in `polish` is applied.
struct Generated {
    text: String,
    truncated: bool,
}

/// How many tokens one pass may generate: the [`MAX_NEW_TOKENS`] ceiling,
/// clamped to what is actually left of the `n_ctx` window once the prompt is
/// in it. `None` when the prompt alone fills the window.
///
/// llama.cpp does not clamp this for a caller: a decode past the last KV slot
/// returns `NoKvSlot`, `generate` bails, and `Polisher::polish`'s
/// never-lose-the-user's-words policy turns that into "return the input
/// unchanged". Silently — `asr::offline` builds a synthetic reply with
/// `finish_reason: "stop"` from it, the guardrail accepts it, and the
/// [`PolishResult::truncated`] signal that exists to surface "Formatting was
/// cut short" can never fire, because the hard decode error pre-empts the
/// graceful truncation it describes. Clamping degrades into exactly that
/// truncation instead.
///
/// One slot is held back because `generate` writes its last sampled token at
/// position `prompt_tokens + max_new`, and positions are 0-based.
fn generation_budget(prompt_tokens: usize, n_ctx: u32) -> Option<i32> {
    let room = i64::from(n_ctx) - prompt_tokens as i64 - 1;
    (room > 0).then(|| MAX_NEW_TOKENS.min(room as i32))
}

/// The fewest tokens a polish must be able to generate for its prompt to be
/// usable: room for the longest accepted input to come back whole.
const MIN_USEFUL_BUDGET: i32 = 256;

/// A pessimistic token count for `text` under Qwen2.5's tokenizer: a token
/// per three ASCII characters (it averages about 3.5), and a token for every
/// other character, which covers Indic scripts.
fn estimated_tokens(text: &str) -> usize {
    let ascii = text.chars().filter(char::is_ascii).count();
    let other = text.chars().count() - ascii;
    ascii.div_ceil(3) + other
}

/// Whether `prompt`, with the ChatML template around it and the longest
/// input this module accepts, still leaves [`MIN_USEFUL_BUDGET`] tokens to
/// answer with.
fn leaves_room_to_answer(prompt: &str) -> bool {
    let prompt_tokens = estimated_tokens(prompt) + 32 + MAX_INPUT_WORDS * 2;
    generation_budget(prompt_tokens, N_CTX).is_some_and(|budget| budget >= MIN_USEFUL_BUDGET)
}

/// The system prompt an on-device polish runs with: the user's saved rules
/// for `level` when they fit the window, else the level's shipped on-device
/// rules. A saved High override usually starts from the shipped cloud rules,
/// which alone are larger than the window; used as written it would leave no
/// room to answer, and the polish would return the text untouched.
pub fn local_prompt(level: crate::format::level::CleanupLevel, rules: Option<&str>) -> String {
    let wanted = level.local_prompt_with_rules(rules);
    if rules.is_none() || leaves_room_to_answer(&wanted) {
        return wanted;
    }
    tracing::info!(
        prompt_chars = wanted.chars().count(),
        "the saved prompt is too long for the on-device model; using its shipped rules"
    );
    level.local_prompt_with_rules(None)
}

impl Polisher {
    pub fn load(gguf_path: &Path) -> anyhow::Result<Self> {
        let t = Instant::now();
        let backend = LlamaBackend::init()?;
        let params = LlamaModelParams::default();
        let model = LlamaModel::load_from_file(&backend, gguf_path, &params)?;
        tracing::info!("polish model loaded in {:?}", t.elapsed());
        Ok(Self {
            backend,
            model,
            last_used: Instant::now(),
        })
    }

    /// `prompt` is the system prompt — callers pass the active level's
    /// [`local_prompt`], so Light/Balanced/High actually differ on-device.
    pub fn polish(&mut self, text: &str, prompt: &str) -> PolishResult {
        self.last_used = Instant::now();
        if text.split_whitespace().count() > MAX_INPUT_WORDS {
            tracing::debug!("polish skipped: input longer than {MAX_INPUT_WORDS} words");
            return PolishResult { text: text.to_string(), truncated: false };
        }
        let t = Instant::now();
        match self.generate(text, prompt) {
            Ok(gen) if !gen.text.trim().is_empty() => {
                tracing::info!("polished in {:?}", t.elapsed());
                PolishResult { text: gen.text.trim().to_string(), truncated: gen.truncated }
            }
            Ok(_) => PolishResult { text: text.to_string(), truncated: false },
            Err(e) => {
                tracing::warn!("polish fell back to unpolished text: {e:#}");
                PolishResult { text: text.to_string(), truncated: false }
            }
        }
    }

    fn generate(&mut self, text: &str, prompt: &str) -> anyhow::Result<Generated> {
        let start = Instant::now();
        let full_prompt = format!(
            "<|im_start|>system\n{prompt}<|im_end|>\n<|im_start|>user\n{text}<|im_end|>\n<|im_start|>assistant\n"
        );

        let ctx_params = LlamaContextParams::default().with_n_ctx(NonZeroU32::new(N_CTX));
        let mut ctx = self.model.new_context(&self.backend, ctx_params)?;

        let tokens = self.model.str_to_token(&full_prompt, AddBos::Always)?;
        // Derived from the prompt actually in front of us, not assumed: the
        // system prompt belongs to `CleanupLevel` and has grown once already.
        let max_new = generation_budget(tokens.len(), N_CTX).ok_or_else(|| {
            anyhow::anyhow!(
                "polish prompt is {} tokens, which fills the {N_CTX}-token window",
                tokens.len()
            )
        })?;
        let n_len = tokens.len() as i32 + max_new;
        let mut batch = LlamaBatch::new(N_CTX as usize, 1);
        let last_index = tokens.len() as i32 - 1;
        for (i, token) in (0i32..).zip(tokens.into_iter()) {
            batch.add(token, i, &[0], i == last_index)?;
        }
        ctx.decode(&mut batch)?;

        // Greedy decoding: deterministic and exactly what a formatting task wants
        // (same rationale as the cloud path's `POLISH_TEMPERATURE = 0.0`).
        let mut sampler = LlamaSampler::greedy();
        let mut out = String::new();
        let mut decoder = encoding_rs::UTF_8.new_decoder();
        let mut n_cur = batch.n_tokens();
        // Assume the cap was hit unless the loop below reaches a natural
        // stop (an end-of-generation token) before `n_cur` exceeds `n_len`.
        let mut truncated = true;

        while n_cur <= n_len {
            if start.elapsed() > TIMEOUT {
                anyhow::bail!("polish timed out after {TIMEOUT:?}");
            }
            let token = sampler.sample(&ctx, batch.n_tokens() - 1);
            if self.model.is_eog_token(token) {
                truncated = false;
                break;
            }
            out.push_str(
                &self
                    .model
                    .token_to_piece(token, &mut decoder, false, None)
                    .unwrap_or_default(),
            );
            batch.clear();
            batch.add(token, n_cur, &[0], true)?;
            n_cur += 1;
            ctx.decode(&mut batch)?;
        }
        Ok(Generated { text: out, truncated })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Loading a GGUF and running llama.cpp is not something a unit test can
    /// do, so what is covered here is the arithmetic that decides whether a
    /// generation can finish at all — the only part that is pure.

    /// THE REGRESSION this pins: a prompt that grows (the hardening stanza
    /// took High from 743 to 1457 chars) while `max_new` stays a bare 512
    /// inside a 1024-token window runs out of KV slots mid-generation;
    /// `ctx.decode` returns `NoKvSlot`, and on-device AI Polish silently
    /// returns the input unchanged. The budget must fit, for any prompt.
    #[test]
    fn the_budget_always_fits_inside_the_context_window() {
        for prompt_tokens in [1usize, 100, 620, 1023, 1500, 2046] {
            let Some(max_new) = generation_budget(prompt_tokens, N_CTX) else {
                continue;
            };
            assert!(max_new > 0, "a budget of {max_new} generates nothing");
            assert!(
                prompt_tokens as i32 + max_new < N_CTX as i32,
                "prompt {prompt_tokens} + {max_new} new does not fit in {N_CTX}"
            );
        }
    }

    /// The shape it actually has to survive: the longest system prompt this
    /// module can be handed (High, hardening stanza included) plus the
    /// longest input it accepts. Asserted against the real prompt, so a
    /// future prompt that grows past the window fails here rather than in
    /// `ctx.decode` on a user's machine.
    #[test]
    fn the_worst_case_high_polish_still_gets_a_usable_budget() {
        // The prompt this module is actually handed: `local_prompt_with_rules`,
        // not the cloud prompt, which is larger than this whole window.
        // `leaves_room_to_answer` counts it pessimistically, adds the ChatML
        // template's own tokens and the longest accepted input at 2 tokens a
        // word, and asks for `MIN_USEFUL_BUDGET` tokens to answer with.
        use crate::format::level::CleanupLevel;
        for level in [CleanupLevel::Light, CleanupLevel::Balanced, CleanupLevel::High] {
            assert!(
                leaves_room_to_answer(&level.local_prompt_with_rules(None)),
                "{level:?}'s on-device prompt leaves too little room to answer"
            );
        }
    }

    /// A prompt that fills the window gets no budget at all: a loud error
    /// beats a decode that dies partway through and reads as "polish did
    /// nothing".
    #[test]
    fn a_prompt_that_fills_the_window_has_no_budget() {
        assert_eq!(generation_budget(N_CTX as usize, N_CTX), None);
        assert_eq!(generation_budget(N_CTX as usize + 100, N_CTX), None);
    }

    /// The clamp is a safety floor, not a new policy: with room to spare the
    /// ceiling is still what binds.
    #[test]
    fn the_ceiling_still_binds_when_there_is_room() {
        assert_eq!(generation_budget(100, N_CTX), Some(MAX_NEW_TOKENS));
    }

    /// A saved High override is usually the shipped High rules with an edit:
    /// about 2,100 tokens, more than the whole window. On this device it gives
    /// way to the shipped on-device rules rather than skipping the polish.
    #[test]
    fn a_saved_override_too_long_for_the_window_gives_way_to_the_on_device_rules() {
        use crate::format::level::CleanupLevel;
        let high = CleanupLevel::High;
        let saved = high.default_rules();
        assert!(!leaves_room_to_answer(&high.local_prompt_with_rules(Some(&saved))));
        let prompt = local_prompt(high, Some(&saved));
        assert_eq!(prompt, high.local_prompt_with_rules(None));
        assert!(leaves_room_to_answer(&prompt));

        // An override that fits is used as written.
        let short = "Fix the punctuation only.";
        assert_eq!(local_prompt(high, Some(short)), high.local_prompt_with_rules(Some(short)));
        assert_eq!(local_prompt(high, None), high.local_prompt_with_rules(None));
    }
}
