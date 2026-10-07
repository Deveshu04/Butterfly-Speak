//! Settings schema + persistence: `%APPDATA%\ButterflySpeak\settings.json`.
//! Every field is serde-defaulted so old files survive schema growth.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Bumped when `load()` gains a migration step; the on-disk value tells us
/// which migrations have already run.
pub const SETTINGS_VERSION: u32 = 3;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Settings {
    pub version: u32,
    pub provider: Provider,
    pub hotkey: HotkeySettings,
    pub model: ModelSettings,
    pub sarvam: SarvamSettings,
    pub audio: AudioSettings,
    pub cleanup: CleanupToggles,
    /// Personal vocabulary: names/jargon fed to the recognizer as hints and
    /// to the polish model as preferred spellings.
    pub dictionary: Vec<String>,
    /// Corrections: "wrong → right" find/replace applied to every transcript.
    /// `auto` marks rules learned from the user's edits in the history feed.
    pub replacements: Vec<Replacement>,
    /// Voice snippets: say the trigger phrase, the expansion gets typed.
    pub snippets: Vec<Snippet>,
    /// Output tone: "formal" | "casual" | "veryCasual".
    pub style: String,
    /// Per-app tone overrides matched against the focused process name.
    #[serde(default = "default_style_rules")]
    pub style_rules: Vec<StyleRule>,
    /// Select text anywhere → shortcut → AI rewrite in place.
    #[serde(default = "default_transforms")]
    pub transforms: Vec<Transform>,
    #[serde(default = "default_true")]
    pub transforms_enabled: bool,
    /// App-level global shortcuts (empty string = disabled).
    pub shortcuts: ShortcutSettings,
    pub dictation: DictationSettings,
    pub injection: InjectionSettings,
    pub overlay: OverlaySettings,
    pub app: AppSettings,
    pub history: HistorySettings,
    /// "Translate dictation": what the translate chord turns speech into.
    pub translation: TranslationSettings,
    /// "Voice agent": what the agent chord addresses.
    pub agent: AgentSettings,
    /// Learning corrections from the app the transcript was pasted into.
    pub learn: LearnSettings,
    /// The one custom OpenAI-compatible endpoint slot (`crate::endpoint`).
    pub custom_endpoint: CustomEndpointSettings,
    /// Auto-update policy. `#[serde(default)]` at struct level pulls
    /// `UpdateSettings::default()` for a file written before this existed.
    #[serde(default)]
    pub updates: UpdateSettings,
    /// Notes on disk: the one-way markdown mirror.
    pub notes: NotesSettings,
    /// The Prompts page: the user's rewrite of a shipped prompt's rules half,
    /// per kind. Absent everywhere until they edit one.
    pub prompts: PromptOverrides,
    /// Cloud mode's one hidden knob. `#[serde(default)]` at struct level
    /// covers every settings file written before Cloud mode existed.
    #[serde(default)]
    pub cloud: CloudSettings,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            version: SETTINGS_VERSION,
            provider: Provider::default(),
            hotkey: HotkeySettings::default(),
            model: ModelSettings::default(),
            sarvam: SarvamSettings::default(),
            audio: AudioSettings::default(),
            cleanup: CleanupToggles::default(),
            dictionary: Vec::new(),
            replacements: Vec::new(),
            snippets: Vec::new(),
            style: "formal".into(),
            style_rules: default_style_rules(),
            transforms: default_transforms(),
            transforms_enabled: true,
            shortcuts: ShortcutSettings::default(),
            dictation: DictationSettings::default(),
            injection: InjectionSettings::default(),
            overlay: OverlaySettings::default(),
            app: AppSettings::default(),
            history: HistorySettings::default(),
            translation: TranslationSettings::default(),
            agent: AgentSettings::default(),
            learn: LearnSettings::default(),
            custom_endpoint: CustomEndpointSettings::default(),
            updates: UpdateSettings::default(),
            notes: NotesSettings::default(),
            prompts: PromptOverrides::default(),
            cloud: CloudSettings::default(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct Snippet {
    pub trigger: String,
    pub expansion: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct Replacement {
    pub from: String,
    pub to: String,
    /// Learned automatically from an edit in the history feed.
    pub auto: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct StyleRule {
    /// Case-insensitive substring of the focused process name ("whatsapp").
    pub app: String,
    pub style: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct Transform {
    pub name: String,
    pub prompt: String,
    /// Chord string like "Win+Alt+1"; parsed by hotkeys::parse_binding.
    pub shortcut: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ShortcutSettings {
    /// Paste the last dictation again.
    pub paste_last: String,
    /// Copy the last dictation to the clipboard.
    pub copy_last: String,
    /// Bring up the app on the Notes page. The field keeps the Scratchpad's
    /// name — that page was replaced by Notes, and renaming a serialized
    /// field would silently drop the binding a user already set.
    pub scratchpad: String,
    /// Replace the last injected text with the verbatim transcript.
    pub undo_ai_edit: String,
    // --- dictation-grade chords ------------------------------------------
    // Appended last, and they must stay last: `hotkeys::app_shortcut_bindings`
    // and `hotkeys::route_chord_bindings` are positionally coupled to their
    // own index constants, and a field inserted mid-struct is invisible to
    // the compiler (`hotkeys::tests::shortcut_indices_match_their_own_
    // settings_field` and its route-chord twin are what catch it).
    //
    // These two are not app shortcuts: they start a recording and run the
    // whole push-to-talk / double-tap-hands-free machinery, exactly like
    // `hotkey.binding` does. They only differ in what they stamp on the
    // session (`routes::ChordKind`). Both ship unbound.
    /// Dictate, then translate into `translation.target_language`.
    pub translate_dictation: String,
    /// Dictate a command addressed to the voice agent.
    pub voice_agent: String,
}

impl Default for ShortcutSettings {
    fn default() -> Self {
        Self {
            paste_last: "Alt+Shift+Z".into(),
            copy_last: "Alt+Shift+X".into(),
            scratchpad: String::new(),
            undo_ai_edit: String::new(),
            translate_dictation: String::new(),
            voice_agent: String::new(),
        }
    }
}

fn default_true() -> bool {
    true
}

fn default_style_rules() -> Vec<StyleRule> {
    let casual = ["whatsapp", "telegram", "discord", "signal", "instagram"];
    casual
        .into_iter()
        .map(|app| StyleRule {
            app: app.into(),
            style: "casual".into(),
        })
        .collect()
}

fn default_transforms() -> Vec<Transform> {
    vec![
        Transform {
            name: "Polish".into(),
            prompt: "Rewrite the text to be clearer and more concise. Keep the meaning, tone, language and script. Fix grammar and awkward phrasing.".into(),
            shortcut: "Win+Alt+1".into(),
        },
        Transform {
            name: "Prompt Engineer".into(),
            prompt: "Restructure the text into a well-crafted AI prompt: state the task clearly, add relevant context, and specify the desired output format. Keep the user's intent exactly.".into(),
            shortcut: "Win+Alt+2".into(),
        },
    ]
}

/// Which engine turns speech into text (and powers AI polish).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Provider {
    /// Bring your own key: the user's own Sarvam key, straight to Sarvam.
    /// Serialized as `"sarvam"`, which is what every settings file written
    /// before Cloud mode existed already says — so those files keep meaning
    /// exactly what they meant.
    #[default]
    Sarvam,
    /// On-device sherpa-onnx models (offline).
    Local,
    /// Butterfly Labs Cloud: the same Sarvam models, reached through the
    /// relay, which holds the key and counts the words. This install never
    /// has a Sarvam key at all — it signs in instead (`crate::auth`).
    Cloud,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct SarvamSettings {
    /// Realtime STT language: "auto" or a code like "hi-IN", "en-IN".
    /// Note: realtime uses "or-IN" for Odia where REST uses "od-IN".
    pub language_code: String,
    /// "balanced" (default) or "fast".
    pub stream_type: String,
    /// "transcribe" (default); kept as a free string so "codemix" and
    /// future modes can be tried without a rebuild.
    pub mode: String,
    /// Chat model for the AI-polish pass.
    ///
    /// Must be an id `GET /v1/models` actually returns. It listed exactly
    /// `sarvam-105b` and `sarvam-105b-conversations` when this was written;
    /// the old default `sarvam-30b` does not exist and answers every request
    /// with HTTP 400 and an empty body. Because `chat::polish` swallows all
    /// errors to protect the transcript, that shipped as "AI Polish silently
    /// does nothing" — see `migrate`.
    pub polish_model: String,
}

impl Default for SarvamSettings {
    fn default() -> Self {
        Self {
            language_code: "auto".into(),
            stream_type: "balanced".into(),
            mode: "transcribe".into(),
            polish_model: DEFAULT_POLISH_MODEL.into(),
        }
    }
}

/// The only chat model we ship as a default. Verified against `GET /v1/models`.
pub const DEFAULT_POLISH_MODEL: &str = "sarvam-105b";

/// Cloud mode's settings — one field, and no UI for it.
///
/// There is nothing here for a user to configure: Cloud mode is "sign in and
/// dictate". The override exists for the two places that must not talk to
/// the production relay — the latency gate harness and a local
/// `wrangler dev` — and it is deliberately hidden rather than shown, because
/// a relay URL typed into a settings box is a credential-forwarding hazard,
/// not a feature.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct CloudSettings {
    /// Base URL of the relay (`https://…`), or `None` for
    /// `crate::sarvam::DEFAULT_RELAY_URL`. Hand-edited into settings.json;
    /// read through [`CloudSettings::relay_base`], never directly.
    pub relay_url: Option<String>,
}

impl CloudSettings {
    /// The relay this install talks to: the override when it is usable,
    /// otherwise `crate::sarvam::DEFAULT_RELAY_URL`.
    ///
    /// The relay receives the sign-in bearer token and the user's audio, so
    /// an override counts only when it is `https`, or plain `http` to this
    /// machine (a local `wrangler dev`). Anything else is ignored and the
    /// shipped relay is used. A blank override is not a URL, and a trailing
    /// slash would double up in every route built from the base.
    ///
    /// The one rule for every caller: the dictation socket and the chat
    /// backend (`controller::lane_for`) and the usage and account calls
    /// (`auth::commands`) all hang off this base.
    pub fn relay_base(&self) -> String {
        let Some(url) = self
            .relay_url
            .as_deref()
            .map(str::trim)
            .map(|url| url.trim_end_matches('/'))
            .filter(|url| !url.is_empty())
        else {
            return crate::sarvam::DEFAULT_RELAY_URL.to_string();
        };
        safe_relay(url).unwrap_or_else(|| {
            // Never the URL itself: a hand-edited one can carry a credential.
            tracing::warn!(
                "the relay override is neither https nor http to this machine; \
                 using the shipped relay"
            );
            crate::sarvam::DEFAULT_RELAY_URL.to_string()
        })
    }
}

/// `url` as the URL parser the requests go through writes it, trailing slash
/// removed, when it is `https` or `http` to a loopback host; otherwise
/// `None`. Returning the parser's own text rather than the input means the
/// host this approves is exactly the host every route built from it dials.
fn safe_relay(url: &str) -> Option<String> {
    let parsed = reqwest::Url::parse(url).ok()?;
    let safe = match parsed.scheme() {
        "https" => true,
        "http" => parsed.host_str().is_some_and(|host| {
            host.eq_ignore_ascii_case("localhost")
                || host.parse::<std::net::Ipv4Addr>().is_ok_and(|ip| ip.is_loopback())
                || host
                    .strip_prefix('[')
                    .and_then(|h| h.strip_suffix(']'))
                    .and_then(|h| h.parse::<std::net::Ipv6Addr>().ok())
                    .is_some_and(|ip| ip.is_loopback())
        }),
        _ => false,
    };
    safe.then(|| parsed.as_str().trim_end_matches('/').to_string())
}

/// Chat model ids that no longer exist. Anything here is rewritten to
/// `DEFAULT_POLISH_MODEL` on load, at any settings version — a stale id is
/// dead weight that silently disables polish, not a user preference worth
/// preserving.
const RETIRED_POLISH_MODELS: &[&str] = &["sarvam-30b", "sarvam-m", "sarvam-2b"];

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct HotkeySettings {
    pub binding: String,
    pub double_tap_ms: u64,
    pub min_hold_ms: u64,
}

impl Default for HotkeySettings {
    fn default() -> Self {
        Self {
            binding: "Ctrl+Win".into(),
            double_tap_ms: 350,
            min_hold_ms: 150,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ModelSettings {
    pub selected_id: String,
}

impl Default for ModelSettings {
    fn default() -> Self {
        Self {
            selected_id: default_model_id().into(),
        }
    }
}

/// First-run default scales to the machine: tight-RAM PCs start on the
/// light-tier model so dictation stays smooth with other apps open. Failed
/// detection reads as 0 total RAM and lands on the safe (light) choice.
fn default_model_id() -> &'static str {
    default_model_for(crate::models::ram::ram_info().total)
}

/// [`default_model_id`] for a machine with `total_ram` bytes.
fn default_model_for(total_ram: u64) -> &'static str {
    const GIB: u64 = 1024 * 1024 * 1024;
    let tier = if total_ram < 6 * GIB {
        "light"
    } else {
        "balanced"
    };
    let models = &crate::models::catalog::catalog().models;
    models
        .iter()
        .find(|m| m.tier == tier)
        .or_else(|| models.first())
        .map(|m| m.id.as_str())
        .expect("model catalog is not empty")
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct AudioSettings {
    /// cpal device name; None = system default.
    pub device_name: Option<String>,
    /// Start/stop confirmation tones (`tones`). Default true:
    /// eyes-free push-to-talk needs a non-visual "it's recording" / "it's
    /// done" signal.
    pub cues: bool,
    /// Pause playing GSMTC media sessions for the duration of a recording,
    /// resuming exactly the ones this app paused (`media` module). Off by
    /// default: it stops other apps' playback, which should be the user's
    /// choice.
    pub pause_media: bool,
}

impl Default for AudioSettings {
    fn default() -> Self {
        Self {
            device_name: None,
            cues: true,
            pause_media: false,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct CleanupToggles {
    pub spoken_commands: bool,
    pub fillers: bool,
    pub aggressive_fillers: bool,
    pub backtrack: bool,
    pub punctuation: bool,
    pub itn: bool,
    pub ai_polish: bool,
    /// How aggressively AI formatting may rewrite. Replaces `ai_polish`.
    #[serde(default)]
    pub level: crate::format::level::CleanupLevel,
}

impl Default for CleanupToggles {
    fn default() -> Self {
        Self {
            spoken_commands: true,
            fillers: true,
            aggressive_fillers: false,
            backtrack: true,
            punctuation: true,
            itn: true,
            // Polish is core to the product, and the cloud pass is cheap.
            ai_polish: true,
            level: crate::format::level::CleanupLevel::default(),
        }
    }
}

impl From<&Settings> for crate::cleanup::CleanupSettings {
    fn from(s: &Settings) -> Self {
        let t = &s.cleanup;
        Self {
            spoken_commands: t.spoken_commands,
            fillers: t.fillers,
            aggressive_fillers: t.aggressive_fillers,
            backtrack: t.backtrack,
            punctuation: t.punctuation,
            itn: t.itn,
            level: t.level,
            replacements: s
                .replacements
                .iter()
                .filter(|r| !r.from.trim().is_empty() && !r.to.trim().is_empty())
                .map(|r| (r.from.clone(), r.to.clone()))
                .collect(),
            snippets: s
                .snippets
                .iter()
                .filter(|sn| !sn.trigger.trim().is_empty())
                .map(|sn| (sn.trigger.clone(), sn.expansion.clone()))
                .collect(),
            dictionary: s.dictionary.clone(),
            polish_model: s.sarvam.polish_model.clone(),
            // Resolved here, once, against the level this snapshot carries —
            // so both polish paths (cloud `sarvam::ws`, on-device
            // `asr::offline`) read the same answer, and neither has to know
            // that prompt overrides exist.
            prompt_rules: s.prompts.rules_for(t.level).map(str::to_string),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct DictationSettings {
    /// Append a single trailing space to a dictation result unless it
    /// already ends in whitespace, so typing (or the next dictation) doesn't
    /// run straight into the pasted text. Dictation path only — never
    /// applied to transforms or snippet expansion. See
    /// `controller::smart_space_append`.
    pub smart_space: bool,
}

impl Default for DictationSettings {
    fn default() -> Self {
        Self { smart_space: true }
    }
}

/// The largest `restore_delay_ms` the app will honour, enforced by [`repair`].
///
/// A restore delay exists for one reason: a slow target that has not finished
/// reading the clipboard by the time the app puts the user's own contents
/// back. A second is already generous for that. Past it the setting stops
/// being a delay and becomes a stall — every paste in the app visibly slower,
/// with nothing gained — so there is no value above this worth honouring.
///
/// The clamp is not a tidiness measure. `restore_delay_ms` has no UI control,
/// so the only way it goes out of range is a hand-edited settings file, and
/// [`crate::routes::selection::REPLACE_BUDGET`] spends this term *inside*
/// `controller::CLOUD_FINALIZE_TIMEOUT`. Without a ceiling the finalize sum
/// was conditional on config: a large enough value pushed the replace step
/// past the watchdog, which then fires mid-replace and produces a timeout
/// notice, a timeout History row **and** the paste. Pricing the budget at this
/// maximum is what makes that sum unconditional.
///
/// Only the upper end needs enforcing — the range is `0..=1000` and `u64`
/// closes the lower end by construction. `0` is a legitimate choice: it means
/// "restore the clipboard immediately", which is right for a fast target.
pub(crate) const RESTORE_DELAY_MAX_MS: u64 = 1000;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct InjectionSettings {
    /// Put the clipboard back after a paste. Only text is put back: an image
    /// or a file list on the clipboard is replaced by the dictation and does
    /// not come back, and formatted text comes back as plain text. No
    /// settings row shows this switch; it is changed in the settings file.
    pub restore_clipboard: bool,
    /// Milliseconds between the paste and putting the user's own clipboard
    /// back. Clamped to [`RESTORE_DELAY_MAX_MS`] on load; see that constant
    /// for why the ceiling is load-bearing rather than cosmetic.
    pub restore_delay_ms: u64,
}

impl Default for InjectionSettings {
    fn default() -> Self {
        Self {
            restore_clipboard: true,
            restore_delay_ms: 300,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct OverlaySettings {
    /// Distance in logical px from the bottom of the work area, 0 to
    /// [`OVERLAY_OFFSET_MAX`]: the range the Settings page offers, which
    /// [`repair`] enforces on load and on import.
    pub offset_y: f64,
}

/// The highest pill position the Settings page offers, in logical px.
pub const OVERLAY_OFFSET_MAX: f64 = 400.0;

impl Default for OverlaySettings {
    fn default() -> Self {
        Self { offset_y: 24.0 }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct AppSettings {
    pub launch_at_login: bool,
    pub onboarding_done: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct HistorySettings {
    /// Master switch: `history::Recorder::record` is a write-time no-op
    /// while this is off (nothing gets deleted by turning it off).
    pub enabled: bool,
    /// Days to keep a transcription before the retention sweep purges it.
    /// `0` = forever — the default for local-only data.
    pub keep_days: u32,
}

impl Default for HistorySettings {
    fn default() -> Self {
        Self { enabled: true, keep_days: 0 }
    }
}

/// "Translate dictation" (`shortcuts.translate_dictation`): dictate in one
/// language, paste in another.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct TranslationSettings {
    /// What to translate into, as a Sarvam language code.
    ///
    /// **Blank means translation is not configured**, and the translate chord
    /// skips the translation and pastes the cleaned text with a notice
    /// (`routes::resolve`). That is deliberately the only off switch: a
    /// separate `enabled` flag could disagree with the target and there would
    /// be no honest way to say which one the user meant.
    ///
    /// This shares a code split with `SarvamSettings::language_code`:
    /// the realtime socket spells Odia "or-IN" where the REST endpoints spell
    /// it "od-IN".
    pub target_language: String,
}

impl Default for TranslationSettings {
    fn default() -> Self {
        Self {
            target_language: "hi-IN".into(),
        }
    }
}

/// "Voice agent" (`shortcuts.voice_agent`): a dictation the speaker addresses
/// to the assistant rather than to the document.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct AgentSettings {
    /// What the user calls the agent. Also the wake word, when wake-word
    /// invocation is on. Stored as `name`. Files written by earlier versions
    /// call it `agentName`: [`repair`] moves that key on load and import, and
    /// the alias reads it wherever settings arrive without a repair.
    #[serde(alias = "agentName")]
    pub name: String,
    /// Whether opening an ordinary dictation with the agent's name hands it
    /// to the agent. The scan is `routes::wake`.
    ///
    /// Ships **off**. This is the one route where a dictation changes meaning
    /// without the user doing anything differently: the same gesture, the same
    /// chord, and a name the matcher hears through the usual misspellings and,
    /// on a long name, a misheard letter. The consequence of a false positive
    /// here is not a bad paste, it is *no* paste — a wake invocation the agent
    /// cannot serve types nothing instead of turning the command into prose,
    /// which is deliberate: a command quietly typed as text is the defect
    /// `routes::wake` exists to prevent. Worth opting into; not worth
    /// defaulting to.
    pub wake_word_enabled: bool,
}

impl Default for AgentSettings {
    fn default() -> Self {
        Self {
            name: "Butterfly".into(),
            wake_word_enabled: false,
        }
    }
}

/// Learning from the corrections the user makes in their own text field
/// (`learn::candidates`, and the field monitor that feeds it).
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct LearnSettings {
    /// Whether Butterfly Speak reads back the field it just pasted into, to
    /// notice a word the user corrected by hand.
    ///
    /// Ships **on**, unlike `agent.wake_word_enabled`, and the difference is
    /// deliberate: this cannot change what a dictation says. The monitor only
    /// reads; it never changes any setting of the app it reads. What it
    /// learns is two words rather than a sentence, nothing leaves the
    /// machine, and no single observation changes anything at all — a
    /// correction has to come back in a second paste session before it
    /// becomes a rule. The Settings copy states all of that plainly, because
    /// a feature that reads the focused field has to be legible to be
    /// acceptable, not merely defensible.
    pub field_monitor_enabled: bool,
}

impl Default for LearnSettings {
    fn default() -> Self {
        Self {
            field_monitor_enabled: true,
        }
    }
}

/// The one custom OpenAI-compatible endpoint slot: a base URL, a model name,
/// and which halves of the app may use it.
///
/// **No key field, and there will never be one.** The endpoint's credential
/// lives in the Windows credential store next to the Sarvam one
/// (`sarvam::key::KeySlot::CustomEndpoint`); `Settings` is exported to a
/// user-chosen JSON file by `commands::export_settings`, and a secret in this
/// struct would ride along.
///
/// Also *not* here: the custom endpoint's model has its own field rather than
/// sharing `sarvam.polish_model`, because `repair()` rewrites that one against
/// `RETIRED_POLISH_MODELS` — a list of dead Sarvam ids. A custom endpoint's
/// `qwen3:8b` must never be "repaired" to `sarvam-105b`.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct CustomEndpointSettings {
    /// Exactly as typed. Normalized at use time (`endpoint::trim_pasted_route`)
    /// so a half-entered URL is never rewritten under the cursor. Empty means
    /// the slot is off, and no other address is used in its place.
    pub base_url: String,
    /// The chat model id to send, exactly as typed: `/v1/models` only
    /// suggests ids, and an unlisted one may still work.
    pub model: String,
    /// The transcription model id, for the `/audio/transcriptions` half.
    ///
    /// A second field rather than a second use of `model`, because the two
    /// halves of one OpenAI-compatible host almost never answer to the same
    /// name: the gateway that serves `qwen3:8b` at `/v1/chat/completions`
    /// serves `whisper-large-v3` (or a deployment name, or `Systran/…`) at
    /// `/v1/audio/transcriptions`. Sending the chat id to the transcription
    /// route is a 400 from every server, so `model` is never borrowed for it.
    ///
    /// Empty sends `whisper-1` (`endpoint::DEFAULT_STT_MODEL`), the id
    /// OpenAI's API reference gives its Whisper model, which a server that
    /// copies OpenAI's transcription route while hosting one Whisper model
    /// usually accepts.
    pub stt_model: String,
    /// Route the AI formatting / agent / transform calls here.
    pub use_for_polish: bool,
    /// Route speech-to-text here: the dictation records the whole utterance
    /// and posts it to `{base}/audio/transcriptions` (`asr::custom`).
    pub use_for_stt: bool,
}

/// Its own `Default` impl, not a derive on the fields: the struct-level
/// `#[serde(default)]` above has to pull a *missing* `customEndpoint` object
/// out of this, and the four pinned tests in this file exist because getting
/// that wrong silently ships every upgrading user whatever `bool`/`String`
/// happen to default to. Everything here is off and empty, which is also what
/// those primitives would give — stated explicitly so it stays a decision.
impl Default for CustomEndpointSettings {
    fn default() -> Self {
        Self {
            base_url: String::new(),
            model: String::new(),
            stt_model: String::new(),
            use_for_polish: false,
            use_for_stt: false,
        }
    }
}

/// Auto-update policy. One switch, re-read inside the timer callback that
/// `updater::start` spawns rather than captured when it is scheduled — see
/// that module's doc for why the switch gates the *check* and not the notice.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct UpdateSettings {
    /// Check for a new version 30 s after launch and every 8 h. Ships ON.
    /// Nothing is downloaded or installed without a click either way.
    pub auto_check: bool,
}

/// Its own `Default` impl rather than a derive, for the reason this file pins
/// five times over: the struct-level `#[serde(default)]` on `Settings` pulls a
/// *missing* `updates` object out of this, and a derive would leave it at
/// `bool`'s own `Default` — `false` — which silently stops every install that
/// predates this setting from ever learning about a release.
impl Default for UpdateSettings {
    fn default() -> Self {
        Self { auto_check: true }
    }
}

/// Notes on disk: the one-way markdown mirror (`crate::notes::mirror`).
///
/// Two fields and no default folder, deliberately. A switch that falls back to
/// an app-data folder when the path is blank scatters a copy of every note into
/// a directory the user never chose and may never find. Here the mirror runs
/// only when it has been pointed somewhere: a `None` `mirror_dir` means "no
/// destination", and `mirror_root()` — the one place that decides — answers
/// `None` for it exactly as it does when the switch is off. Files land where
/// the user pointed, or nowhere.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct NotesSettings {
    /// Write a `.md` file per note into [`Self::mirror_dir`]. **Off** by
    /// default: a dictation app does not start copying the user's documents
    /// out of its database because it can.
    pub mirror_enabled: bool,
    /// Where the mirror writes. `None` until the user picks a folder.
    pub mirror_dir: Option<PathBuf>,
}

/// Its own `Default` impl for the same reason `CustomEndpointSettings` has
/// one: the struct-level `#[serde(default)]` above pulls a *missing* `notes`
/// object out of this, and a derive would silently make the answer whatever
/// `bool`/`Option` happen to default to. Off and unset — which is what those
/// primitives would give too, said out loud so it stays a decision.
impl Default for NotesSettings {
    fn default() -> Self {
        Self {
            mirror_enabled: false,
            mirror_dir: None,
        }
    }
}

/// The Prompts page: what the user has written in place of a shipped prompt's
/// **rules half**, one field per editable kind.
///
/// `Option<String>`, not `String`, and that is the whole design. Storing "no
/// customisation" as `""` is a sentinel that works until someone asks whether a
/// stored value is a user's choice or a leftover. `None` means "use whatever
/// default this build ships", so a user who never edited a prompt always runs
/// the current one; `Some(text)` means the user wrote `text` and meant it.
///
/// **Nothing here is the injection-hardening stanza, the agent output rules,
/// the transcript-delimiter rule, or the end-marker rule.**
/// Those are re-appended around whatever is stored here by
/// `CleanupLevel::prompt_with_rules` and `sarvam::chat::build_agent_system`,
/// so an edit cannot move them, break the splice that puts the personal
/// dictionary among the rules, or turn off the truncation check.
///
/// There is no `off` field: `CleanupLevel::Off` never calls a model.
/// There is no transform field either — a Transform's prompt is already the
/// user's own text, in `Settings::transforms`.
///
/// # The save-time guard
///
/// [`PromptOverrides::normalize`] drops any field whose text equals the
/// current shipped default, so saving an unedited prompt is not a
/// customisation. `format::prompt_ratchet` pins every shipped default's
/// hash, so changing one is a deliberate act rather than a surprise.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", default)]
pub struct PromptOverrides {
    /// `CleanupLevel::Light`'s rules.
    pub light: Option<String>,
    /// `CleanupLevel::Balanced`'s rules.
    pub balanced: Option<String>,
    /// `CleanupLevel::High`'s rules.
    pub high: Option<String>,
    /// The voice agent's brief (`sarvam::chat::AGENT_BRIEF`). The agent's
    /// name replaces `sarvam::chat::NAME_PLACEHOLDER` in a custom brief
    /// exactly as in the shipped one. [`repair`] and [`Self::normalize`]
    /// store the token older briefs used (`sarvam::chat::OLD_NAME_PLACEHOLDER`)
    /// as the current one.
    pub agent: Option<String>,
    /// The selection block (`sarvam::chat::AGENT_SELECTION_RULES`),
    /// added to the agent's system turn only when a command is spoken with
    /// text selected. [`repair`] and [`Self::normalize`] give the selection
    /// envelope's former field names, here and in `agent`, their current
    /// ones.
    ///
    /// Stored as `selectionRules`. Files written by earlier versions call it
    /// `selectionEdit`: [`repair`] moves that key on load and import, and the
    /// alias reads it wherever settings arrive without a repair.
    #[serde(alias = "selectionEdit")]
    pub selection_rules: Option<String>,
}

/// Its own `Default` impl rather than a derive, per the trap this file pins
/// four times over: a struct-level `#[serde(default)]` pulls a *missing*
/// `prompts` object out of this. `None` everywhere happens to be what the
/// derive would give, and it is written out anyway so it stays a decision —
/// "no override" is the ship default, and a settings file predating this
/// field must land there rather than anywhere else.
impl Default for PromptOverrides {
    fn default() -> Self {
        Self {
            light: None,
            balanced: None,
            high: None,
            agent: None,
            selection_rules: None,
        }
    }
}

impl NotesSettings {
    /// The directory the mirror writes into, or `None` when it must not write
    /// at all — the switch is off, or no folder has been chosen.
    ///
    /// Every mirror call site goes through this, so "is the mirror on?" has
    /// exactly one answer and the `enabled && dir.is_none()` state cannot be
    /// read as "on" by one caller and "off" by another.
    pub fn mirror_root(&self) -> Option<&Path> {
        if !self.mirror_enabled {
            return None;
        }
        self.mirror_dir.as_deref()
    }
}

impl PromptOverrides {
    /// The rules half in force for a cleanup level: the user's override, or
    /// `None` for "the shipped rules". `Off` never has one.
    pub fn rules_for(&self, level: crate::format::level::CleanupLevel) -> Option<&str> {
        use crate::format::level::CleanupLevel;
        match level {
            CleanupLevel::Off => None,
            CleanupLevel::Light => self.light.as_deref(),
            CleanupLevel::Balanced => self.balanced.as_deref(),
            CleanupLevel::High => self.high.as_deref(),
        }
    }

    /// The current shipped default for each kind, in the same order as the
    /// fields. The Prompts page's "Reset to default", the save-time guard
    /// below and the hash ratchet all read it here, because the three must
    /// agree about what "the default" is.
    pub fn defaults() -> [(EditablePrompt, String); 5] {
        use crate::format::level::CleanupLevel;
        [
            (EditablePrompt::Light, CleanupLevel::Light.default_rules()),
            (EditablePrompt::Balanced, CleanupLevel::Balanced.default_rules()),
            (EditablePrompt::High, CleanupLevel::High.default_rules()),
            (EditablePrompt::Agent, crate::sarvam::chat::AGENT_BRIEF.to_string()),
            (
                EditablePrompt::SelectionRules,
                crate::sarvam::chat::AGENT_SELECTION_RULES.to_string(),
            ),
        ]
    }

    fn field_mut(&mut self, kind: EditablePrompt) -> &mut Option<String> {
        match kind {
            EditablePrompt::Light => &mut self.light,
            EditablePrompt::Balanced => &mut self.balanced,
            EditablePrompt::High => &mut self.high,
            EditablePrompt::Agent => &mut self.agent,
            EditablePrompt::SelectionRules => &mut self.selection_rules,
        }
    }

    /// The save-time guard. Returns whether anything changed.
    ///
    /// Drops an override that is blank, or that matches the current shipped
    /// default once both sides are trimmed. Saving the prompt you were shown
    /// is not a customisation: stored as one, it would hold this install to
    /// today's text after the app ships a better one, with nothing on screen
    /// to say so. Trimming also catches a copy that differs only by a
    /// trailing newline. Each shipped prompt has exactly one text, with no
    /// per-language variants, so comparing against it is the whole check.
    ///
    /// A brief that still marks the agent's name with the older token is
    /// stored with the current one, and an agent or selection override that
    /// names the selection envelope's former fields with their current names.
    ///
    /// Called from `commands::set_settings`, on the way *in*, before the file
    /// is written, so every stored override has already been through it.
    pub fn normalize(&mut self) -> bool {
        let mut changed = false;
        if let Some(text) = self.agent.as_deref().and_then(with_current_name_token) {
            self.agent = Some(text);
            changed = true;
        }
        for field in [&mut self.agent, &mut self.selection_rules] {
            if let Some(text) = field.as_deref().and_then(with_current_envelope_fields) {
                *field = Some(text);
                changed = true;
            }
        }
        for (kind, default) in Self::defaults() {
            let field = self.field_mut(kind);
            let redundant = field
                .as_deref()
                .is_some_and(|text| text.trim().is_empty() || text.trim() == default.trim());
            if redundant {
                *field = None;
                changed = true;
            }
        }
        changed
    }
}

/// The prompts a user can edit. Mirrors `PromptOverrides`' fields; the
/// serde name is what `commands::preview_prompt` / `commands::test_prompt`
/// take from the UI.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum EditablePrompt {
    Light,
    Balanced,
    High,
    Agent,
    SelectionRules,
}

impl EditablePrompt {
    /// The cleanup level this kind's rules belong to, if it is a cleanup
    /// kind at all.
    pub fn level(&self) -> Option<crate::format::level::CleanupLevel> {
        use crate::format::level::CleanupLevel;
        match self {
            EditablePrompt::Light => Some(CleanupLevel::Light),
            EditablePrompt::Balanced => Some(CleanupLevel::Balanced),
            EditablePrompt::High => Some(CleanupLevel::High),
            EditablePrompt::Agent | EditablePrompt::SelectionRules => None,
        }
    }
}

pub fn config_dir() -> PathBuf {
    PathBuf::from(std::env::var("APPDATA").expect("APPDATA not set")).join("ButterflySpeak")
}

fn settings_path() -> PathBuf {
    config_dir().join("settings.json")
}

pub fn load() -> Settings {
    load_from(&settings_path())
}

/// [`load`], from `path`.
///
/// A file that cannot be read as settings is moved aside before the defaults
/// are returned. Left in place it would not survive: the page saves the whole
/// object on the first change the user makes, and that save would write the
/// defaults over it. Aside, it is still there for a later fix or a manual
/// repair.
///
/// That covers a file that cannot be read at all, too: one saved as UTF-16,
/// or one another program holds open. Only a missing file is simply the
/// first run. A leading byte-order mark is not a fault; Windows editors
/// often write one, and it is skipped.
fn load_from(path: &Path) -> Settings {
    let raw = match std::fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Settings::default(),
        Err(e) => {
            tracing::warn!("settings.json unreadable ({e}); using defaults");
            set_aside(path);
            return Settings::default();
        }
    };
    let mut value: serde_json::Value = match serde_json::from_str(raw.trim_start_matches('\u{feff}'))
    {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!("settings.json unreadable ({e}); using defaults");
            set_aside(path);
            return Settings::default();
        }
    };
    // `repair` is deliberately outside the version gate: a file already at
    // the current version can still name a model that has since been retired.
    let migrated = migrate(&mut value) | repair(&mut value);
    match serde_json::from_value::<Settings>(value) {
        Ok(settings) => {
            if migrated {
                if let Err(e) = save_to(path, &settings) {
                    tracing::warn!("couldn't persist migrated settings: {e}");
                }
            }
            settings
        }
        Err(e) => {
            tracing::warn!("settings.json unreadable ({e}); using defaults");
            set_aside(path);
            Settings::default()
        }
    }
}

/// Renames an unreadable settings file to `<name>.unreadable-<unix seconds>`
/// beside it. Logs the new file name only, never the folder: it is under the
/// user's profile.
fn set_aside(path: &Path) {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "settings.json".into());
    let aside = format!("{name}.unreadable-{secs}");
    match std::fs::rename(path, path.with_file_name(&aside)) {
        Ok(()) => tracing::warn!("kept the unreadable settings file as {aside}"),
        Err(e) => tracing::warn!("couldn't move the unreadable settings file aside: {e}"),
    }
}

/// Version-independent repairs for values that have gone stale against the
/// live API. Runs on every load and must stay idempotent.
pub(crate) fn repair(value: &mut serde_json::Value) -> bool {
    let mut changed = false;

    if let Some(sarvam) = value.get_mut("sarvam").and_then(|s| s.as_object_mut()) {
        let retired = sarvam
            .get("polishModel")
            .and_then(|m| m.as_str())
            .is_some_and(|m| RETIRED_POLISH_MODELS.contains(&m));
        if retired {
            tracing::warn!(
                "settings named a retired chat model; switching to {DEFAULT_POLISH_MODEL}. \
                 AI Polish and Transforms could not have worked until now."
            );
            sarvam.insert("polishModel".into(), DEFAULT_POLISH_MODEL.into());
            changed = true;
        }
    }

    // `restore_delay_ms` has no UI control, so an out-of-range value only ever
    // came from a hand-edited file — and `routes::selection::REPLACE_BUDGET`
    // spends this term inside `controller::CLOUD_FINALIZE_TIMEOUT`, pricing it
    // at the maximum below. Clamping here is what makes that budget's sum
    // unconditional instead of "true at the default".
    //
    // The guard is `>`, not `>=`: clamping the boundary itself would rewrite
    // the user's file on every single load, forever. Only the ceiling needs
    // enforcing — the range is `0..=RESTORE_DELAY_MAX_MS` and `u64` closes the
    // floor by construction.
    //
    // A non-integer value (`1500.5`, `-5`) reads as `None` here and is left
    // alone deliberately: it already fails to deserialize into `Settings`, so
    // `load` falls back to defaults, which are in range. Coercing it would be
    // this repair guessing at a different field's problem.
    if let Some(injection) = value.get_mut("injection").and_then(|i| i.as_object_mut()) {
        let over = injection
            .get("restoreDelayMs")
            .and_then(|v| v.as_u64())
            .is_some_and(|ms| ms > RESTORE_DELAY_MAX_MS);
        if over {
            tracing::warn!(
                "clipboard-restore delay above {RESTORE_DELAY_MAX_MS} ms; clamping. \
                 Past a second it stalls every paste without restoring anything sooner."
            );
            injection.insert("restoreDelayMs".into(), RESTORE_DELAY_MAX_MS.into());
            changed = true;
        }
    }

    // The pill position: the Settings page offers 0 to `OVERLAY_OFFSET_MAX`,
    // and a file from elsewhere can hold anything, 2000 being a pill
    // off-screen. Only a value outside the range is rewritten, so an
    // in-range file is never touched on load.
    if let Some(overlay) = value.get_mut("overlay").and_then(|o| o.as_object_mut()) {
        let outside = overlay
            .get("offsetY")
            .and_then(|v| v.as_f64())
            .filter(|y| !(0.0..=OVERLAY_OFFSET_MAX).contains(y));
        if let Some(y) = outside {
            overlay.insert("offsetY".into(), y.clamp(0.0, OVERLAY_OFFSET_MAX).into());
            changed = true;
        }
    }

    // `aiPolish: bool` became a four-level control. Translate once; the old
    // key is left in place harmlessly for rollback.
    if let Some(cleanup) = value.get_mut("cleanup").and_then(|c| c.as_object_mut()) {
        if !cleanup.contains_key("level") {
            let on = cleanup.get("aiPolish").and_then(|v| v.as_bool()).unwrap_or(true);
            cleanup.insert("level".into(), if on { "balanced".into() } else { "off".into() });
            changed = true;
        }
    }

    // The agent's name used to be stored as `agentName`. serde reads that key
    // and `name` as one field and refuses a file that has both, which would
    // set the whole file aside; so the old key moves to `name` when `name` is
    // missing or null and is dropped otherwise.
    if let Some(agent) = value.get_mut("agent").and_then(|a| a.as_object_mut()) {
        if let Some(old) = agent.remove("agentName") {
            settle_older_key(agent, "name", old);
            changed = true;
        }
    }

    // The selection rules used to be stored as `prompts.selectionEdit`, which
    // serde reads as the same field as `selectionRules`, with the same refusal
    // of a file that has both. So the old key moves to `selectionRules` when
    // that is missing or null and is dropped otherwise. This comes before the
    // envelope repair below, which reads the current key.
    if let Some(prompts) = value.get_mut("prompts").and_then(|p| p.as_object_mut()) {
        if let Some(old) = prompts.remove("selectionEdit") {
            settle_older_key(prompts, "selectionRules", old);
            changed = true;
        }
    }

    // A saved agent brief may still mark the name with the older token. The
    // agent fills that one in too, but the stored text is moved to the
    // current token so the Prompts page shows the one its hint names.
    if let Some(brief) = value.pointer_mut("/prompts/agent") {
        let renamed = brief
            .as_str()
            .and_then(with_current_name_token);
        if let Some(text) = renamed {
            *brief = text.into();
            changed = true;
        }
    }

    // A saved agent or selection override may name the selection envelope's
    // fields by their former names. Those fields no longer arrive, so the
    // stored text moves to the names the envelope line describes.
    for pointer in ["/prompts/agent", "/prompts/selectionRules"] {
        if let Some(saved) = value.pointer_mut(pointer) {
            if let Some(text) = saved.as_str().and_then(with_current_envelope_fields) {
                *saved = text.into();
                changed = true;
            }
        }
    }

    changed
}

/// Puts the value of a key that has since been renamed under `current`.
/// A missing or null `current` holds nothing, so the older value fills it;
/// any other value is the newer one and stays, and the older value is dropped.
fn settle_older_key(
    obj: &mut serde_json::Map<String, serde_json::Value>,
    current: &str,
    older: serde_json::Value,
) {
    let slot = obj.entry(current).or_insert(serde_json::Value::Null);
    if slot.is_null() {
        *slot = older;
    }
}

/// `brief` with the older name token replaced by the current one, or `None`
/// when it has no older token to replace.
fn with_current_name_token(brief: &str) -> Option<String> {
    use crate::sarvam::chat::{NAME_PLACEHOLDER, OLD_NAME_PLACEHOLDER};
    brief
        .contains(OLD_NAME_PLACEHOLDER)
        .then(|| brief.replace(OLD_NAME_PLACEHOLDER, NAME_PLACEHOLDER))
}

/// The selection envelope's former field names, each with the name the app
/// sends in its place now.
const RENAMED_ENVELOPE_FIELDS: [(&str, &str); 2] = [
    ("spoken_command", crate::sarvam::chat::REQUEST_FIELD),
    ("selected_text", crate::sarvam::chat::SELECTION_FIELD),
];

/// `text` with the selection envelope's former field names replaced by the
/// current ones, or `None` when it names neither.
fn with_current_envelope_fields(text: &str) -> Option<String> {
    RENAMED_ENVELOPE_FIELDS
        .iter()
        .any(|(old, _)| text.contains(old))
        .then(|| {
            RENAMED_ENVELOPE_FIELDS
                .iter()
                .fold(text.to_string(), |text, (old, new)| text.replace(old, new))
        })
}

/// Import-only: keep this machine's notes mirror, whatever the file says.
///
/// `export_settings` serializes the whole `Settings`, so a mirror folder and
/// its switch travel inside an exported file. A folder named there belongs to
/// another machine, or to whoever wrote the file: taking it would start
/// writing every note into a place this user never chose on this machine
/// (`C:\Users\Public\…` is an existing folder on every Windows install). The
/// folder is chosen here, with the picker, or not at all.
///
/// `cloud` is not taken from a file either; `set_settings` carries it through
/// ([`carry_stored_fields`]).
///
/// **Not** part of [`repair`], and deliberately not on the load path: a
/// vault on a disk that is not mounted yet at login is still the user's
/// choice, and the mirror already logs what it could not write.
pub(crate) fn keep_this_machines_mirror(current: &Settings, imported: &mut Settings) {
    imported.notes.mirror_dir = current.notes.mirror_dir.clone();
    imported.notes.mirror_enabled = current.notes.mirror_enabled;
}

/// What `commands::set_settings` keeps or corrects in a settings object the
/// page sends, before it is saved and becomes the copy every dictation reads.
///
/// - `cloud` comes from the stored settings, never from the page. No screen
///   edits it, so the settings file is its only legitimate writer, and it
///   decides where the sign-in token and the audio go.
/// - `injection.restore_delay_ms` is held to [`RESTORE_DELAY_MAX_MS`], the
///   ceiling [`repair`] enforces on load and on import: the finalize
///   watchdog prices the clipboard restore at that maximum.
pub(crate) fn carry_stored_fields(stored: &Settings, incoming: &mut Settings) {
    incoming.cloud = stored.cloud.clone();
    incoming.injection.restore_delay_ms =
        incoming.injection.restore_delay_ms.min(RESTORE_DELAY_MAX_MS);
}

/// Pre-typed migration pass. Returns true when the file changed and should be
/// rewritten. Each step runs only for a file below the version it belongs to:
///
/// - v* → v2 (the Sarvam-default era): the provider flips to Sarvam and AI
///   polish is switched on, matching the shipped defaults of that version.
/// - v* → v3: a cleanup level left on v2's default, `balanced`, moves to
///   `high`, the level the full post-processor prompt lives on. Any other
///   level was the user's choice and stays.
///
/// Choices made on a build at or above a step's version are the user's and
/// stay untouched.
pub(crate) fn migrate(value: &mut serde_json::Value) -> bool {
    let Some(obj) = value.as_object_mut() else {
        return false;
    };
    let version = obj.get("version").and_then(|v| v.as_u64()).unwrap_or(0);
    if version >= SETTINGS_VERSION as u64 {
        return false;
    }
    // Each step is gated on the version it belongs to, not on the current
    // one: run for any file below v3, the v2 step would force a v2 file's
    // provider back to sarvam, the choice `migration_skips_current_version`
    // protects.
    if version < 2 {
        obj.insert("provider".into(), "sarvam".into());
        if let Some(cleanup) = obj.get_mut("cleanup").and_then(|c| c.as_object_mut()) {
            cleanup.insert("aiPolish".into(), true.into());
        }
    }
    if version < 3 {
        // The shipped prompt became the full post-processor prompt, which
        // lives on High: the only level whose hardening example and
        // word-retention guardrail already promise what it asks for (see
        // `format::level::RULES_POST_PROCESSOR`). `CleanupLevel::default()`
        // moved with it, but that only reaches a file with no level written
        // in it — every install that has run this app has one.
        //
        // So move the installs that were sitting on v2's default and no
        // other. Someone who chose Off, Light or High chose it; a migration
        // that overrode that would be taking a preference away, not
        // correcting a stale one.
        if let Some(cleanup) = obj.get_mut("cleanup").and_then(|c| c.as_object_mut()) {
            if cleanup.get("level").and_then(|l| l.as_str()) == Some("balanced") {
                cleanup.insert("level".into(), "high".into());
            }
        }
    }
    obj.insert("version".into(), SETTINGS_VERSION.into());
    true
}

pub fn save(settings: &Settings) -> anyhow::Result<()> {
    save_to(&settings_path(), settings)
}

fn save_to(path: &Path, settings: &Settings) -> anyhow::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(settings)?)?;
    // std::fs::rename replaces the target on Windows (MOVEFILE_REPLACE_
    // EXISTING), so there is no delete-first crash window.
    std::fs::rename(&tmp, path)?;
    Ok(())
}

pub fn models_root() -> PathBuf {
    PathBuf::from(std::env::var("LOCALAPPDATA").expect("LOCALAPPDATA not set"))
        .join("ButterflySpeak")
        .join("models")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The uninstaller's "Delete app data" (src-tauri/windows/hooks.nsh)
    /// removes `<base>\ButterflySpeak` under %APPDATA% and %LOCALAPPDATA%.
    /// Move any of the settings, models or logs folders and this fails until
    /// the hook follows it.
    #[test]
    fn the_uninstall_hook_removes_the_folders_the_app_writes() {
        const HOOK: &str = include_str!("../windows/hooks.nsh");
        assert!(HOOK.contains("!define BS_DATA_FOLDER \"ButterflySpeak\""));
        assert!(HOOK.contains("!define BS_ROAMING_BASE \"$APPDATA\""));
        assert!(HOOK.contains("!define BS_LOCAL_BASE \"$LOCALAPPDATA\""));
        let roaming = PathBuf::from(std::env::var("APPDATA").unwrap());
        let local = PathBuf::from(std::env::var("LOCALAPPDATA").unwrap());
        assert_eq!(config_dir(), roaming.join("ButterflySpeak"));
        assert!(models_root().starts_with(local.join("ButterflySpeak")));
        assert!(crate::logs_dir().starts_with(local.join("ButterflySpeak")));
    }

    /// A folder of its own under the temp directory, removed on drop.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("bs-settings-{tag}-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        fn entries(&self) -> Vec<String> {
            let mut names: Vec<String> = std::fs::read_dir(&self.0)
                .unwrap()
                .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
                .collect();
            names.sort();
            names
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// An unreadable file is moved aside, not left for the page's first
    /// whole-object save to overwrite with the defaults it was handed.
    #[test]
    fn an_unreadable_settings_file_is_moved_aside_intact() {
        for raw in [
            "{ this is not json",
            r#"{"version":3,"hotkey":{"binding":5}}"#,
        ] {
            let dir = TempDir::new("unreadable");
            let path = dir.0.join("settings.json");
            std::fs::write(&path, raw).unwrap();

            let loaded = load_from(&path);
            assert_eq!(loaded.hotkey.binding, HotkeySettings::default().binding);
            assert!(!path.exists(), "left where the next save would overwrite it: {raw}");
            let names = dir.entries();
            assert_eq!(names.len(), 1, "{names:?}");
            assert!(names[0].starts_with("settings.json.unreadable-"), "{names:?}");
            assert_eq!(std::fs::read_to_string(dir.0.join(&names[0])).unwrap(), raw);
        }
    }

    /// A byte-order mark in front of the JSON is how Windows editors often
    /// save UTF-8; the file loads as written and stays where it is.
    #[test]
    fn a_settings_file_with_a_byte_order_mark_loads() {
        let dir = TempDir::new("bom");
        let path = dir.0.join("settings.json");
        let raw = "\u{feff}{\"version\":3,\"style\":\"casual\"}";
        std::fs::write(&path, raw).unwrap();
        assert_eq!(load_from(&path).style, "casual");
        assert_eq!(dir.entries(), ["settings.json"]);
    }

    /// A file saved as UTF-16 cannot be read as text at all. It is moved
    /// aside intact rather than left for the first save to overwrite.
    #[test]
    fn a_utf16_settings_file_is_moved_aside_intact() {
        let dir = TempDir::new("utf16");
        let path = dir.0.join("settings.json");
        let mut bytes = vec![0xFF, 0xFE];
        for unit in "{\"version\":3,\"style\":\"casual\"}".encode_utf16() {
            bytes.extend_from_slice(&unit.to_le_bytes());
        }
        std::fs::write(&path, &bytes).unwrap();
        assert_eq!(load_from(&path).style, Settings::default().style);
        assert!(!path.exists());
        let names = dir.entries();
        assert_eq!(names.len(), 1, "{names:?}");
        assert!(names[0].starts_with("settings.json.unreadable-"), "{names:?}");
        assert_eq!(std::fs::read(dir.0.join(&names[0])).unwrap(), bytes);
    }

    /// No file at all is a first run: nothing is created or moved.
    #[test]
    fn a_missing_settings_file_is_a_first_run() {
        let dir = TempDir::new("missing");
        assert_eq!(load_from(&dir.0.join("settings.json")).style, Settings::default().style);
        assert!(dir.entries().is_empty());
    }

    /// A readable file stays where it is, and one with nothing to migrate is
    /// not rewritten.
    #[test]
    fn a_readable_settings_file_stays_put() {
        let dir = TempDir::new("readable");
        let path = dir.0.join("settings.json");
        let raw = r#"{"version":3,"style":"casual"}"#;
        std::fs::write(&path, raw).unwrap();
        assert_eq!(load_from(&path).style, "casual");
        assert_eq!(dir.entries(), ["settings.json"]);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), raw);
    }

    /// Mirrors `load()`: both passes, in the same order.
    fn migrated(raw: &str) -> (bool, Settings) {
        let mut value: serde_json::Value = serde_json::from_str(raw).unwrap();
        let changed = migrate(&mut value) | repair(&mut value);
        (changed, serde_json::from_value(value).unwrap())
    }

    #[test]
    fn migration_pre_v2_forces_sarvam_and_polish() {
        let (changed, s) = migrated(
            r#"{"version":1,"provider":"local","cleanup":{"aiPolish":false},"app":{"onboardingDone":true}}"#,
        );
        assert!(changed);
        assert_eq!(s.provider, Provider::Sarvam);
        assert!(s.cleanup.ai_polish);
        assert_eq!(s.version, SETTINGS_VERSION);
    }

    #[test]
    fn migration_v0_defaults_to_sarvam() {
        let (changed, s) = migrated(r#"{"app":{"onboardingDone":true}}"#);
        assert!(changed);
        assert_eq!(s.provider, Provider::Sarvam);
    }

    /// Cloud mode adds a third provider and must take nothing away: a file
    /// that says `sarvam` still means Bring-your-own-key, a file that says
    /// `local` still means on-device, and neither gains a `cloud` block it
    /// never asked for.
    #[test]
    fn the_third_provider_leaves_the_first_two_alone() {
        let (_, byok) = migrated(r#"{"version":3,"provider":"sarvam"}"#);
        assert_eq!(byok.provider, Provider::Sarvam);
        let (_, local) = migrated(r#"{"version":3,"provider":"local"}"#);
        assert_eq!(local.provider, Provider::Local);
        assert!(byok.cloud.relay_url.is_none());
        assert!(local.cloud.relay_url.is_none());
    }

    /// The value the settings file, the History chip and the frontend all
    /// have to agree on.
    #[test]
    fn cloud_serializes_as_cloud() {
        assert_eq!(
            serde_json::to_value(Provider::Cloud).unwrap(),
            serde_json::Value::String("cloud".into())
        );
        let (_, s) = migrated(r#"{"version":3,"provider":"cloud"}"#);
        assert_eq!(s.provider, Provider::Cloud);
    }

    /// The hidden override: no UI writes it, so the only way in is a
    /// hand-edited file — which must survive a round trip so that editing
    /// anything else in Settings does not silently drop it.
    #[test]
    fn the_hidden_relay_override_round_trips() {
        let (_, s) = migrated(
            r#"{"version":3,"provider":"cloud","cloud":{"relayUrl":"http://127.0.0.1:8787"}}"#,
        );
        assert_eq!(s.cloud.relay_url.as_deref(), Some("http://127.0.0.1:8787"));
        let json = serde_json::to_value(&s).unwrap();
        assert_eq!(json["cloud"]["relayUrl"], "http://127.0.0.1:8787");
    }

    /// The base handed to every route is the parser's own text for the URL
    /// that was judged, so the approved host is the host that is dialled.
    #[test]
    fn the_relay_base_is_the_parsed_url_that_was_judged() {
        let base = |url: &str| {
            CloudSettings {
                relay_url: Some(url.into()),
            }
            .relay_base()
        };
        assert_eq!(base("HTTPS://Staging.Example.Workers.DEV/"), "https://staging.example.workers.dev");
        assert_eq!(base("https://relay.example:443/"), "https://relay.example");
        assert_eq!(base(" http://127.0.0.1:8787 "), "http://127.0.0.1:8787");
        assert_eq!(base("http://[::1]:8787/"), "http://[::1]:8787");
        assert_eq!(base("http://evil.example\\@127.0.0.1"), crate::sarvam::DEFAULT_RELAY_URL);
    }

    /// The shipped default was `sarvam-30b`, which `GET /v1/models` does not
    /// list; every polish call answered 400 and fell back to raw text without
    /// telling anyone. Repair has to reach files already at the current
    /// version, which the version-gated migration never would.
    #[test]
    fn retired_polish_model_is_repaired_at_the_current_version() {
        let (changed, s) = migrated(
            r#"{"version":3,"sarvam":{"polishModel":"sarvam-30b","languageCode":"auto"}}"#,
        );
        assert!(changed, "must rewrite the file");
        assert_eq!(s.sarvam.polish_model, DEFAULT_POLISH_MODEL);
        // Untouched neighbours survive.
        assert_eq!(s.sarvam.language_code, "auto");
    }

    /// The pill position the Settings page allows is 0-400 px; an imported
    /// or hand-edited file can hold anything, and 2000 puts the pill
    /// off-screen. Held to the range, with 0 kept as 0.
    #[test]
    fn an_out_of_range_pill_position_is_held_to_the_settings_range() {
        for (raw, want) in [("2000", 400.0), ("-5", 0.0), ("400.5", 400.0)] {
            let mut value: serde_json::Value =
                serde_json::from_str(&format!(r#"{{"version":3,"overlay":{{"offsetY":{raw}}}}}"#))
                    .unwrap();
            assert!(repair(&mut value), "{raw} is outside the range");
            assert!(!repair(&mut value), "second pass must be a no-op");
            let s: Settings = serde_json::from_value(value).unwrap();
            assert_eq!(s.overlay.offset_y, want, "{raw}");
        }
        for raw in ["0", "24", "400"] {
            let mut value: serde_json::Value =
                serde_json::from_str(&format!(r#"{{"version":3,"overlay":{{"offsetY":{raw}}}}}"#))
                    .unwrap();
            assert!(!repair(&mut value), "{raw} is in range and left alone");
        }
    }

    /// `restore_delay_ms` has no UI control, so the only value that is ever
    /// out of range came from a hand-edited file — and
    /// `routes::selection::REPLACE_BUDGET` prices the finalize watchdog at
    /// the maximum, which is only honest if nothing can exceed it.
    ///
    /// The 1000 is written by hand rather than derived from
    /// `RESTORE_DELAY_MAX_MS`, deliberately: the constant *is* the clamp, so
    /// an equation between them could not catch the maximum being moved. The
    /// wall-clock is the thing that has to be re-argued, because
    /// `controller::CLOUD_FINALIZE_TIMEOUT` is derived from it.
    #[test]
    fn an_out_of_range_restore_delay_is_clamped_to_the_documented_maximum() {
        let mut value: serde_json::Value =
            serde_json::from_str(r#"{"version":3,"injection":{"restoreDelayMs":30000}}"#).unwrap();
        assert!(repair(&mut value), "a stall dressed as a restore delay");
        assert!(!repair(&mut value), "second pass must be a no-op");

        let s: Settings = serde_json::from_value(value).unwrap();
        assert_eq!(s.injection.restore_delay_ms, 1000);
        // The settings.rs Default-impl trap: the sibling the file never named
        // comes from `InjectionSettings::default()` (true), not from `bool`'s
        // own `Default` (false), which would silently stop restoring the
        // user's clipboard for anyone whose delay got clamped.
        assert!(s.injection.restore_clipboard);
    }

    /// The is-it-already-there guard, from both ends of the range. The
    /// boundary itself is not a repair — clamping it would rewrite the file
    /// on every load forever.
    #[test]
    fn a_restore_delay_within_range_is_left_exactly_as_it_was() {
        for raw in ["0", "300", "1000"] {
            let mut value: serde_json::Value =
                serde_json::from_str(&format!(r#"{{"version":3,"injection":{{"restoreDelayMs":{raw}}}}}"#))
                    .unwrap();
            assert!(!repair(&mut value), "{raw} ms is inside the range");
            let s: Settings = serde_json::from_value(value).unwrap();
            assert_eq!(s.injection.restore_delay_ms, raw.parse::<u64>().unwrap());
        }

        // And a file with no `injection` object at all is not a repair either:
        // the default is in range by construction.
        let mut bare: serde_json::Value = serde_json::from_str(r#"{"version":3}"#).unwrap();
        assert!(!repair(&mut bare), "an absent object has nothing to clamp");
        assert!(InjectionSettings::default().restore_delay_ms <= 1000);
    }

    #[test]
    fn repair_is_idempotent_and_leaves_valid_models_alone() {
        let mut value: serde_json::Value =
            serde_json::from_str(r#"{"version":3,"sarvam":{"polishModel":"sarvam-105b"}}"#).unwrap();
        assert!(!repair(&mut value), "a live model id is not a repair");

        let mut stale: serde_json::Value =
            serde_json::from_str(r#"{"version":3,"sarvam":{"polishModel":"sarvam-30b"}}"#).unwrap();
        assert!(repair(&mut stale));
        assert!(!repair(&mut stale), "second pass must be a no-op");
    }

    /// `aiPolish: bool` became a four-level control; the old key is still
    /// translated on every load so a file at the current version still
    /// migrates the very first time it's read after the upgrade.
    #[test]
    fn ai_polish_migrates_to_level() {
        let (changed, s) = migrated(r#"{"version":3,"cleanup":{"aiPolish":false}}"#);
        assert!(changed);
        assert_eq!(s.cleanup.level, crate::format::level::CleanupLevel::Off);

        let (changed, s) = migrated(r#"{"version":3,"cleanup":{"aiPolish":true}}"#);
        assert!(changed);
        assert_eq!(s.cleanup.level, crate::format::level::CleanupLevel::Balanced);
    }

    /// `cleanup.get("aiPolish").and_then(...).unwrap_or(true)` covers files
    /// that never had the boolean at all, not just ones where it's `false`.
    #[test]
    fn ai_polish_absent_defaults_to_balanced() {
        let (changed, s) = migrated(r#"{"version":3,"cleanup":{}}"#);
        assert!(changed, "a missing level key still needs writing once");
        assert_eq!(s.cleanup.level, crate::format::level::CleanupLevel::Balanced);
    }

    /// The `!cleanup.contains_key("level")` guard is what stops a stale
    /// `aiPolish` from stomping a deliberate choice on every future load.
    #[test]
    fn level_already_present_is_not_recomputed_from_ai_polish() {
        let (changed, s) =
            migrated(r#"{"version":3,"cleanup":{"aiPolish":false,"level":"high"}}"#);
        assert!(
            !changed,
            "an existing level must not be rewritten from aiPolish"
        );
        assert_eq!(s.cleanup.level, crate::format::level::CleanupLevel::High);
    }

    /// No `cleanup` object at all (e.g. a hand-trimmed or very old file) must
    /// fall through to `CleanupToggles::default()` without panicking.
    #[test]
    fn missing_cleanup_object_falls_through_to_defaults() {
        let (changed, s) = migrated(r#"{"version":3}"#);
        assert!(!changed);
        assert_eq!(s.cleanup.level, crate::format::level::CleanupLevel::default());
    }

    /// A user who deliberately picked the larger conversations model keeps it.
    #[test]
    fn unrecognised_but_live_models_are_preserved() {
        let (changed, s) = migrated(
            r#"{"version":3,"sarvam":{"polishModel":"sarvam-105b-conversations"}}"#,
        );
        assert!(!changed);
        assert_eq!(s.sarvam.polish_model, "sarvam-105b-conversations");
    }

    #[test]
    fn migration_skips_current_version() {
        let (changed, s) = migrated(r#"{"version":3,"provider":"local"}"#);
        assert!(!changed);
        assert_eq!(s.provider, Provider::Local); // v2 choice is the user's
    }

    #[test]
    fn fresh_defaults_are_cloud_first() {
        let s = Settings::default();
        assert_eq!(s.provider, Provider::Sarvam);
        assert_eq!(s.sarvam.language_code, "auto");
        assert_eq!(s.sarvam.stream_type, "balanced");
        assert_eq!(s.sarvam.mode, "transcribe");
        assert_eq!(s.sarvam.polish_model, DEFAULT_POLISH_MODEL);
        assert!(s.cleanup.ai_polish);
        assert_eq!(s.style, "formal");
        assert!(s.dictation.smart_space);
        assert!(s.audio.cues);
        assert!(!s.audio.pause_media);
        assert!(s.history.enabled);
        assert_eq!(s.history.keep_days, 0);
        assert!(s.learn.field_monitor_enabled);
    }

    /// A settings file written before `history` existed must still enable it
    /// — same reasoning as the `audio.cues` test above: the struct-level
    /// `#[serde(default)]` must pull from `HistorySettings::default()`, not
    /// leave the field at `bool`'s own `Default` (`false`), which would
    /// silently disable history for every upgrading user.
    #[test]
    fn history_default_on_for_a_settings_file_predating_the_field() {
        let (_, s) = migrated(r#"{"version":3,"audio":{"deviceName":"Realtek"}}"#);
        assert!(s.history.enabled);
        assert_eq!(s.history.keep_days, 0);
    }

    /// The same trap `history` and `audio.cues` are pinned against, for the
    /// translation and agent structs: the struct-level `#[serde(default)]` has
    /// to pull a missing `translation`/`agent` object from that struct's own
    /// `Default`, not leave `String`/`bool` at theirs — which would ship every
    /// upgrading user a blank target language (every translation quietly
    /// skipped, forever) and a nameless agent.
    #[test]
    fn translation_and_agent_defaults_for_a_settings_file_predating_them() {
        let (_, s) = migrated(r#"{"version":3,"audio":{"deviceName":"Realtek"}}"#);
        assert_eq!(s.translation.target_language, "hi-IN");
        assert_eq!(s.agent.name, "Butterfly");
        assert!(!s.agent.wake_word_enabled);
    }

    /// The same trap once more, for the `learn` object — and it is the worst
    /// case of the three to get wrong: `bool`'s own `Default` is
    /// `false`, so a struct-level `#[serde(default)]` that failed to reach
    /// `LearnSettings::default()` would silently turn correction learning
    /// **off** for every existing user, with the Settings toggle happily
    /// showing "off" as though they had chosen it.
    #[test]
    fn learn_defaults_on_for_a_settings_file_predating_the_field() {
        let (_, s) = migrated(r#"{"version":3,"audio":{"deviceName":"Realtek"}}"#);
        assert!(s.learn.field_monitor_enabled);
    }

    /// And the user's own "off" survives every later load — nothing in
    /// `migrate`/`repair` may quietly re-enable it.
    #[test]
    fn a_users_choice_to_turn_field_monitoring_off_is_preserved() {
        let (changed, s) = migrated(r#"{"version":3,"learn":{"fieldMonitorEnabled":false}}"#);
        assert!(!changed, "reading this file must not rewrite it");
        assert!(!s.learn.field_monitor_enabled);
    }

    /// A settings file written before the updater has no `updates` object.
    /// The struct-level `#[serde(default)]` must pull
    /// `UpdateSettings::default()`, and that default is ON — a missing
    /// preference is not a refusal. `bool`'s own `Default` is `false`, so
    /// getting this wrong silently stops every upgrading install from ever
    /// noticing a release again.
    #[test]
    fn a_settings_file_without_updates_checks_automatically() {
        let (_, s) = migrated(r#"{"version":3,"audio":{"deviceName":"Realtek"}}"#);
        assert!(s.updates.auto_check);
    }

    /// And the user's own "off" survives every later load — nothing in
    /// `migrate`/`repair` may quietly re-enable it.
    #[test]
    fn an_explicit_false_disables_automatic_checks() {
        let (changed, s) = migrated(r#"{"version":3,"updates":{"autoCheck":false}}"#);
        assert!(!changed, "reading this file must not rewrite it");
        assert!(!s.updates.auto_check);
    }

    /// `repair` has no opinion about `learn` and must not grow one: a file
    /// carrying it reads back byte-identical, on the first pass and the
    /// second (the `cleanup.level` insert above is the shape that goes wrong
    /// here if a repair step forgets its "is it already there?" guard).
    #[test]
    fn repair_is_still_idempotent_with_the_learn_object_present() {
        let raw = r#"{"version":3,"sarvam":{"polishModel":"sarvam-105b"},"cleanup":{"level":"balanced"},"learn":{"fieldMonitorEnabled":false}}"#;
        let mut value: serde_json::Value = serde_json::from_str(raw).unwrap();
        assert!(!repair(&mut value), "nothing here is stale");
        assert!(!repair(&mut value), "second pass must be a no-op");
        let s: Settings = serde_json::from_value(value).unwrap();
        assert!(!s.learn.field_monitor_enabled);
    }

    /// A file that names only *part* of a new object still gets the rest from
    /// that object's own `Default` — the field-level half of the same trap.
    #[test]
    fn a_partial_agent_object_keeps_its_sibling_defaults() {
        let (_, s) = migrated(r#"{"version":3,"agent":{"wakeWordEnabled":true}}"#);
        assert!(s.agent.wake_word_enabled, "the user's choice survives");
        assert_eq!(s.agent.name, "Butterfly");
    }

    /// `repair` has no opinion about current translation and agent objects:
    /// a file carrying them reads back byte-identical and never gets
    /// rewritten, on the first pass or the second.
    #[test]
    fn repair_is_still_idempotent_with_the_translation_and_agent_objects_present() {
        let raw = r#"{"version":3,"sarvam":{"polishModel":"sarvam-105b"},"cleanup":{"level":"balanced"},"translation":{"targetLanguage":"ta-IN"},"agent":{"name":"Mitra","wakeWordEnabled":true}}"#;
        let mut value: serde_json::Value = serde_json::from_str(raw).unwrap();
        assert!(!repair(&mut value), "nothing here is stale");
        assert!(!repair(&mut value), "second pass must be a no-op");
        let s: Settings = serde_json::from_value(value).unwrap();
        assert_eq!(s.agent.name, "Mitra");

        // And a file that *does* need a repair still leaves them alone.
        let mut stale: serde_json::Value = serde_json::from_str(
            r#"{"version":3,"sarvam":{"polishModel":"sarvam-30b"},"translation":{"targetLanguage":"ta-IN"}}"#,
        )
        .unwrap();
        assert!(repair(&mut stale));
        assert!(!repair(&mut stale), "second pass must be a no-op");
        let s: Settings = serde_json::from_value(stale).unwrap();
        assert_eq!(s.sarvam.polish_model, DEFAULT_POLISH_MODEL);
        assert_eq!(s.translation.target_language, "ta-IN");
    }

    /// The agent's name is written as `agent.name`, and a file that still
    /// says `agent.agentName` reads into the same field, so nobody's agent
    /// loses its name on upgrade.
    #[test]
    fn the_agent_name_is_saved_as_name_and_its_older_key_still_loads() {
        let (_, old) = migrated(r#"{"version":3,"agent":{"agentName":"Mitra"}}"#);
        assert_eq!(old.agent.name, "Mitra");
        let (_, new) = migrated(r#"{"version":3,"agent":{"name":"Mitra"}}"#);
        assert_eq!(new.agent.name, "Mitra");
        // Without a repair too, as settings the page sends arrive.
        let unrepaired: Settings =
            serde_json::from_str(r#"{"version":3,"agent":{"agentName":"Mitra"}}"#).unwrap();
        assert_eq!(unrepaired.agent.name, "Mitra");

        let saved = serde_json::to_value(&old).unwrap();
        assert_eq!(saved["agent"]["name"], "Mitra");
        assert!(saved["agent"].get("agentName").is_none(), "{saved}");
    }

    /// `repair` moves the older key to `name`, or drops it when `name` already
    /// holds a value. A file holding both would otherwise fail to load
    /// (serde reads them as one field, twice) and be set aside whole.
    #[test]
    fn repair_settles_the_older_agent_name_key_and_a_file_with_both_loads() {
        let mut moved: serde_json::Value =
            serde_json::from_str(r#"{"version":3,"agent":{"agentName":"Mitra","wakeWordEnabled":true}}"#)
                .unwrap();
        assert!(repair(&mut moved), "the older key is stale");
        assert!(!repair(&mut moved), "second pass must be a no-op");
        assert_eq!(moved["agent"]["name"], "Mitra");
        assert!(moved["agent"].get("agentName").is_none(), "{moved}");
        assert_eq!(moved["agent"]["wakeWordEnabled"], true);

        let raw = r#"{"version":3,"agent":{"agentName":"Old","name":"Mitra"}}"#;
        assert!(
            serde_json::from_str::<Settings>(raw).is_err(),
            "both keys at once is what repair exists to prevent"
        );
        let (changed, both) = migrated(raw);
        assert!(changed);
        assert_eq!(both.agent.name, "Mitra", "the current key wins");
    }

    /// A saved agent brief that marks the name with the older token gets the
    /// current one, so the name is still filled in. Only that token changes,
    /// the other kinds are left alone, and a second pass changes nothing.
    #[test]
    fn repair_moves_a_saved_agent_brief_to_the_current_name_token() {
        let raw = r#"{"version":3,"prompts":{"agent":"Hi, {{agentName}} here. Sign off as {{agentName}}.","high":"Keep {{agentName}} as typed."}}"#;
        let mut value: serde_json::Value = serde_json::from_str(raw).unwrap();
        assert!(repair(&mut value), "the old token is stale");
        assert!(!repair(&mut value), "second pass must be a no-op");
        let s: Settings = serde_json::from_value(value).unwrap();
        assert_eq!(
            s.prompts.agent.as_deref(),
            Some("Hi, {{name}} here. Sign off as {{name}}.")
        );
        assert_eq!(s.prompts.high.as_deref(), Some("Keep {{agentName}} as typed."));

        let brief = s.prompts.agent.as_deref().unwrap_or_default();
        let system = crate::sarvam::chat::build_agent_system(
            "Mitra",
            &[],
            "<<T3ST>>",
            false,
            crate::sarvam::chat::AgentPrompts {
                brief: Some(brief),
                selection_rules: None,
            },
        );
        assert!(system.starts_with("Hi, Mitra here. Sign off as Mitra."), "{system}");

        let (changed, current) =
            migrated(r#"{"version":3,"prompts":{"agent":"Be brief, {{name}}."}}"#);
        assert!(!changed, "a brief with the current token is not rewritten");
        assert_eq!(current.prompts.agent.as_deref(), Some("Be brief, {{name}}."));
    }

    /// A brief saved now with the older token (pasted from an old export, or
    /// typed from memory) is stored with the current one, and normalizing
    /// again changes nothing.
    #[test]
    fn normalize_stores_a_brief_with_the_current_name_token() {
        let mut p = PromptOverrides {
            agent: Some("Sign off as {{agentName}}.".into()),
            ..PromptOverrides::default()
        };
        assert!(p.normalize());
        assert_eq!(p.agent.as_deref(), Some("Sign off as {{name}}."));
        assert!(!p.normalize());
    }

    /// A saved selection or agent override that names the selection
    /// envelope's former fields is moved to the current names, which are the
    /// ones the envelope line describes and the envelope sends. Other kinds
    /// keep their text, and a second pass changes nothing.
    #[test]
    fn repair_moves_saved_overrides_to_the_current_envelope_field_names() {
        use crate::sarvam::chat::{AGENT_SELECTION_ENVELOPE_RULE, REQUEST_FIELD, SELECTION_FIELD};
        let raw = r#"{"version":3,"prompts":{"selectionRules":"Act on \"spoken_command\" alone; \"selected_text\" is Meera's draft.","agent":"Never read spoken_command back.","balanced":"Keep selected_text as typed."}}"#;
        let mut value: serde_json::Value = serde_json::from_str(raw).unwrap();
        assert!(repair(&mut value), "the former field names are stale");
        assert!(!repair(&mut value), "second pass must be a no-op");
        let s: Settings = serde_json::from_value(value).unwrap();
        let selection = s.prompts.selection_rules.as_deref().unwrap_or_default();
        assert_eq!(selection, "Act on \"spoken_request\" alone; \"selection\" is Meera's draft.");
        assert_eq!(s.prompts.agent.as_deref(), Some("Never read spoken_request back."));
        assert_eq!(s.prompts.balanced.as_deref(), Some("Keep selected_text as typed."));
        for field in [SELECTION_FIELD, REQUEST_FIELD] {
            let quoted = format!("\"{field}\"");
            assert!(selection.contains(&quoted), "{field}");
            assert!(AGENT_SELECTION_ENVELOPE_RULE.contains(&quoted), "{field}");
        }

        let (changed, current) =
            migrated(r#"{"version":3,"prompts":{"selectionRules":"Keep the \"spoken_request\" short."}}"#);
        assert!(!changed, "an override with the current names is not rewritten");
        assert_eq!(current.prompts.selection_rules.as_deref(), Some("Keep the \"spoken_request\" short."));
    }

    /// The same move on save, for an override pasted from an older export.
    #[test]
    fn normalize_stores_overrides_with_the_current_envelope_field_names() {
        let mut p = PromptOverrides {
            selection_rules: Some("Edit only what selected_text holds.".into()),
            ..PromptOverrides::default()
        };
        assert!(p.normalize());
        assert_eq!(p.selection_rules.as_deref(), Some("Edit only what selection holds."));
        assert!(!p.normalize());
    }

    /// The selection rules are written as `prompts.selectionRules`, and a
    /// file that still says `prompts.selectionEdit` reads into the same
    /// field, so nobody's saved rules are lost on upgrade or on import.
    #[test]
    fn the_selection_rules_are_saved_under_their_own_key_and_the_older_key_still_loads() {
        let (changed, old) =
            migrated(r#"{"version":3,"prompts":{"selectionEdit":"Keep it short."}}"#);
        assert!(changed, "the older key is stale");
        assert_eq!(old.prompts.selection_rules.as_deref(), Some("Keep it short."));
        let (changed, new) =
            migrated(r#"{"version":3,"prompts":{"selectionRules":"Keep it short."}}"#);
        assert!(!changed, "the current key is not rewritten");
        assert_eq!(new.prompts.selection_rules.as_deref(), Some("Keep it short."));
        // Without a repair too, as settings the page sends arrive.
        let unrepaired: Settings =
            serde_json::from_str(r#"{"version":3,"prompts":{"selectionEdit":"Keep it short."}}"#)
                .unwrap();
        assert_eq!(unrepaired.prompts.selection_rules.as_deref(), Some("Keep it short."));

        let saved = serde_json::to_value(&old).unwrap();
        assert_eq!(saved["prompts"]["selectionRules"], "Keep it short.");
        assert!(saved["prompts"].get("selectionEdit").is_none(), "{saved}");
    }

    /// A whole file as an earlier build wrote or exported it: every prompt
    /// key present, the selection rules under the older key. It loads with
    /// the rules and every sibling intact, and comes back out under the
    /// current key.
    #[test]
    fn a_whole_settings_file_from_an_earlier_build_keeps_its_selection_rules() {
        let mut earlier = serde_json::to_value(Settings {
            prompts: PromptOverrides {
                agent: Some("Be brief.".into()),
                ..PromptOverrides::default()
            },
            ..Settings::default()
        })
        .unwrap();
        let prompts = earlier["prompts"].as_object_mut().unwrap();
        prompts.remove("selectionRules").expect("the current key is written");
        prompts.insert("selectionEdit".into(), "Keep it short.".into());

        let (changed, s) = migrated(&earlier.to_string());
        assert!(changed, "the older key is stale");
        assert_eq!(s.prompts.selection_rules.as_deref(), Some("Keep it short."));
        assert_eq!(s.prompts.agent.as_deref(), Some("Be brief."));
        assert!(s.prompts.light.is_none());

        // An earlier file with no selection override still says so, as null.
        let mut untouched = serde_json::to_value(Settings::default()).unwrap();
        let prompts = untouched["prompts"].as_object_mut().unwrap();
        prompts.remove("selectionRules");
        prompts.insert("selectionEdit".into(), serde_json::Value::Null);
        let (changed, s) = migrated(&untouched.to_string());
        assert!(changed, "the older key moves even when it holds nothing");
        assert_eq!(s.prompts, PromptOverrides::default());
    }

    /// `repair` moves the older key to `selectionRules`, or drops it when the
    /// current key already holds a value. A file holding both would otherwise
    /// fail to load (serde reads them as one field, twice) and be set aside
    /// whole. The move comes first, so an older file's rules also get the
    /// envelope's current field names.
    #[test]
    fn repair_settles_the_older_selection_rules_key_and_a_file_with_both_loads() {
        let mut moved: serde_json::Value = serde_json::from_str(
            r#"{"version":3,"prompts":{"selectionEdit":"Keep it short.","agent":"Be brief."}}"#,
        )
        .unwrap();
        assert!(repair(&mut moved), "the older key is stale");
        assert!(!repair(&mut moved), "a second pass changes nothing");
        assert_eq!(moved["prompts"]["selectionRules"], "Keep it short.");
        assert!(moved["prompts"].get("selectionEdit").is_none(), "{moved}");
        assert_eq!(moved["prompts"]["agent"], "Be brief.");

        let raw = r#"{"version":3,"prompts":{"selectionEdit":"Old.","selectionRules":"Current."}}"#;
        assert!(
            serde_json::from_str::<Settings>(raw).is_err(),
            "both keys at once is what repair exists to prevent"
        );
        let (changed, both) = migrated(raw);
        assert!(changed);
        assert_eq!(both.prompts.selection_rules.as_deref(), Some("Current."), "the current key wins");

        let (changed, renamed) = migrated(
            r#"{"version":3,"prompts":{"selectionEdit":"Edit only what selected_text holds."}}"#,
        );
        assert!(changed);
        assert_eq!(
            renamed.prompts.selection_rules.as_deref(),
            Some("Edit only what selection holds.")
        );
    }

    /// A file holding both keys with the current one null keeps the older
    /// key's value: null is "nothing stored", so the older value is the only
    /// one there is. Both renamed keys are settled the same way.
    #[test]
    fn a_null_current_key_takes_the_older_keys_value() {
        let (changed, s) = migrated(r#"{"version":3,"agent":{"agentName":"Mitra","name":null}}"#);
        assert!(changed);
        assert_eq!(s.agent.name, "Mitra");

        let (changed, s) = migrated(
            r#"{"version":3,"prompts":{"selectionEdit":"Keep it short.","selectionRules":null}}"#,
        );
        assert!(changed);
        assert_eq!(s.prompts.selection_rules.as_deref(), Some("Keep it short."));

        let (changed, s) =
            migrated(r#"{"version":3,"prompts":{"selectionEdit":null,"selectionRules":null}}"#);
        assert!(changed);
        assert!(s.prompts.selection_rules.is_none());

        let mut value: serde_json::Value = serde_json::from_str(
            r#"{"version":3,"agent":{"agentName":"Mitra","name":null},"prompts":{"selectionEdit":"Keep it short.","selectionRules":null}}"#,
        )
        .unwrap();
        assert!(repair(&mut value), "both older keys are stale");
        assert!(!repair(&mut value), "a second pass changes nothing");
        assert_eq!(value["agent"]["name"], "Mitra");
        assert_eq!(value["prompts"]["selectionRules"], "Keep it short.");
    }

    /// The translate and agent chords ship unbound: a dictation-grade chord
    /// that fires by default would change what the ordinary hotkey does for
    /// every user.
    #[test]
    fn the_translate_and_agent_chords_ship_unbound() {
        let s = ShortcutSettings::default();
        assert_eq!(s.translate_dictation, "");
        assert_eq!(s.voice_agent, "");
    }

    /// A settings file written before `audio.cues` existed must still turn
    /// cues on — the struct-level `#[serde(default)]` pulls the missing
    /// field from `AudioSettings::default()`, not from `bool`'s own
    /// `Default` (which would silently turn cues off for every upgrading
    /// user).
    #[test]
    fn cues_default_on_for_a_settings_file_predating_the_field() {
        let (_, s) = migrated(r#"{"version":3,"audio":{"deviceName":"Realtek"}}"#);
        assert!(s.audio.cues);
        assert_eq!(s.audio.device_name.as_deref(), Some("Realtek"));
    }

    /// The custom endpoint ships **off**, and "off" has to survive the round
    /// trip through `serde` — an object this struct's own `Default` doesn't
    /// supply would arrive with `use_for_polish` at `bool`'s default, which
    /// happens to be right here but is right by accident, not by decision.
    #[test]
    fn the_custom_endpoint_ships_off_and_empty() {
        let s = Settings::default();
        assert_eq!(s.custom_endpoint.base_url, "");
        assert_eq!(s.custom_endpoint.model, "");
        assert_eq!(s.custom_endpoint.stt_model, "");
        assert!(!s.custom_endpoint.use_for_polish);
        assert!(!s.custom_endpoint.use_for_stt);
    }

    /// The settings.rs trap once more, for the `customEndpoint` object: a file
    /// written before `customEndpoint` existed must read back as a slot that
    /// is off, not as a partially-initialised one.
    #[test]
    fn custom_endpoint_defaults_for_a_settings_file_predating_the_field() {
        let (_, s) = migrated(r#"{"version":3,"audio":{"deviceName":"Realtek"}}"#);
        assert_eq!(s.custom_endpoint.base_url, "");
        assert!(!s.custom_endpoint.use_for_polish);
    }

    /// And the field-level half: naming only the URL keeps the siblings'
    /// defaults, so configuring an endpoint does not silently switch it on.
    #[test]
    fn a_partial_custom_endpoint_object_keeps_its_sibling_defaults() {
        let (_, s) = migrated(
            r#"{"version":3,"customEndpoint":{"baseUrl":"http://localhost:11434/v1"}}"#,
        );
        assert_eq!(s.custom_endpoint.base_url, "http://localhost:11434/v1");
        assert_eq!(s.custom_endpoint.model, "");
        assert!(
            !s.custom_endpoint.use_for_polish,
            "a URL alone must not route dictation anywhere new"
        );
    }

    /// The live trap the custom endpoint designs around: `repair()` rewrites
    /// `sarvam.polishModel` against a list of dead **Sarvam** ids. A custom
    /// endpoint's model has to be out of its reach — including when the same
    /// file does need the Sarvam repair.
    #[test]
    fn repair_never_touches_the_custom_endpoints_model() {
        let raw = r#"{"version":3,"sarvam":{"polishModel":"sarvam-30b"},"customEndpoint":{"baseUrl":"http://localhost:11434/v1","model":"sarvam-30b","useForPolish":true}}"#;
        let mut value: serde_json::Value = serde_json::from_str(raw).unwrap();
        assert!(repair(&mut value), "the Sarvam id is stale and gets fixed");
        assert!(!repair(&mut value), "second pass must be a no-op");
        let s: Settings = serde_json::from_value(value).unwrap();
        assert_eq!(s.sarvam.polish_model, DEFAULT_POLISH_MODEL);
        assert_eq!(
            s.custom_endpoint.model, "sarvam-30b",
            "a custom host's model id is its own, whatever it is named"
        );
        assert!(s.custom_endpoint.use_for_polish);
    }

    /// `repair` has no opinion about `customEndpoint` and must not grow one.
    #[test]
    fn repair_is_still_idempotent_with_the_custom_endpoint_object_present() {
        let raw = r#"{"version":3,"sarvam":{"polishModel":"sarvam-105b"},"cleanup":{"level":"balanced"},"customEndpoint":{"baseUrl":"https://h/v1","model":"m","useForPolish":true,"useForStt":false}}"#;
        let mut value: serde_json::Value = serde_json::from_str(raw).unwrap();
        assert!(!repair(&mut value), "nothing here is stale");
        assert!(!repair(&mut value), "second pass must be a no-op");
        let s: Settings = serde_json::from_value(value).unwrap();
        assert_eq!(s.custom_endpoint.base_url, "https://h/v1");
    }

    /// The tripwire, pinned: settings are exported to a file the user picks,
    /// so no serialized field may ever hold the endpoint's key.
    #[test]
    fn the_custom_endpoints_key_is_not_a_settings_field() {
        let s = Settings {
            custom_endpoint: CustomEndpointSettings {
                base_url: "https://h/v1".into(),
                model: "m".into(),
                stt_model: "whisper-large-v3".into(),
                use_for_polish: true,
                use_for_stt: true,
            },
            ..Settings::default()
        };
        let json = serde_json::to_string(&s).expect("settings serialize");
        let obj: serde_json::Value = serde_json::from_str(&json).unwrap();
        let slot = &obj["customEndpoint"];
        let keys: Vec<&String> = slot.as_object().unwrap().keys().collect();
        assert_eq!(
            keys,
            vec!["baseUrl", "model", "sttModel", "useForPolish", "useForStt"],
            "a key field would ride along with export_settings"
        );
    }

    /// Both ways to start with no mirror choice: no settings file at all (the
    /// shipped default), and a file written before `notes` existed. Each
    /// must read back with the mirror **off** and pointed nowhere.
    #[test]
    fn the_notes_mirror_ships_off_and_unset() {
        let s = Settings::default();
        assert!(!s.notes.mirror_enabled);
        assert_eq!(s.notes.mirror_dir, None);
        assert_eq!(s.notes.mirror_root(), None);

        let (_, old) = migrated(r#"{"version":3,"audio":{"deviceName":"Realtek"}}"#);
        assert!(!old.notes.mirror_enabled);
        assert_eq!(old.notes.mirror_root(), None);
    }

    /// `mirror_root()` is the only place that decides, and it says no to both
    /// halves of the half-configured state: a folder without the switch, and
    /// the switch without a folder. The second is the one that matters — a
    /// fallback folder there would copy every note somewhere the user never
    /// chose.
    #[test]
    fn the_mirror_runs_only_when_it_is_both_on_and_pointed_somewhere() {
        let dir = PathBuf::from(r"D:\somebody\Notes");

        let chosen_but_off = NotesSettings {
            mirror_enabled: false,
            mirror_dir: Some(dir.clone()),
        };
        assert_eq!(chosen_but_off.mirror_root(), None);

        let on_but_nowhere = NotesSettings {
            mirror_enabled: true,
            mirror_dir: None,
        };
        assert_eq!(
            on_but_nowhere.mirror_root(),
            None,
            "no destination is not a licence to invent one"
        );

        let on = NotesSettings {
            mirror_enabled: true,
            mirror_dir: Some(dir.clone()),
        };
        assert_eq!(on.mirror_root(), Some(dir.as_path()));
    }

    /// Naming the folder must not switch the mirror on by itself — the
    /// field-level half of the same trap `custom_endpoint` pins.
    ///
    /// The folder is a real one only so the claim stays about serde's
    /// field-level default rather than about paths; `load`/`repair` keep an
    /// unreachable folder too (see
    /// `repair_keeps_a_mirror_folder_that_is_not_mounted_right_now`).
    #[test]
    fn a_partial_notes_object_keeps_its_sibling_default() {
        let dir = std::env::temp_dir();
        let raw = serde_json::json!({ "version": 2, "notes": { "mirrorDir": dir } }).to_string();
        let (_, s) = migrated(&raw);
        assert_eq!(s.notes.mirror_dir.as_deref(), Some(dir.as_path()));
        assert!(
            !s.notes.mirror_enabled,
            "a folder alone must not start copying notes out of the database"
        );
    }

    // --- Prompt overrides (the Prompts page) -----------------------------

    /// The default-impl trap, for the fifth time. A file predating
    /// `prompts` must land on `PromptOverrides::default()` — every kind
    /// absent, i.e. "resolve the shipped prompt" — not on anything else.
    #[test]
    fn prompt_overrides_default_to_absent_for_a_settings_file_predating_them() {
        let (_, s) = migrated(r#"{"version":3,"audio":{"deviceName":"Realtek"}}"#);
        assert_eq!(s.prompts, PromptOverrides::default());
        assert!(s.prompts.light.is_none());
        assert!(s.prompts.balanced.is_none());
        assert!(s.prompts.high.is_none());
        assert!(s.prompts.agent.is_none());
        assert!(s.prompts.selection_rules.is_none());
    }

    /// The field-level half of the same trap: a file naming one kind keeps
    /// the others absent rather than blanking them.
    #[test]
    fn a_partial_prompts_object_keeps_its_sibling_defaults() {
        let (_, s) = migrated(r#"{"version":3,"prompts":{"balanced":"Just punctuate."}}"#);
        assert_eq!(s.prompts.balanced.as_deref(), Some("Just punctuate."));
        assert!(s.prompts.light.is_none(), "an unedited kind must stay absent");
        assert!(s.prompts.agent.is_none());
    }

    /// THE SAVE-TIME GUARD. Saving the prompt you were shown is not a
    /// customisation, and storing it would pin this install to today's text.
    /// `None` instead of `""`, and trimmed on both sides so a trailing
    /// newline out of a textarea is not a customisation either.
    #[test]
    fn saving_an_unedited_default_stores_no_override() {
        for (kind, default) in PromptOverrides::defaults() {
            let mut p = PromptOverrides::default();
            *p.field_mut(kind) = Some(default.clone());
            assert!(p.normalize(), "{kind:?}: the guard must fire");
            assert_eq!(
                p,
                PromptOverrides::default(),
                "{kind:?}: the unedited default must not be stored"
            );

            // ...and the same text with a textarea's trailing newline.
            let mut p = PromptOverrides::default();
            *p.field_mut(kind) = Some(format!("{default}\n"));
            assert!(p.normalize(), "{kind:?}: whitespace-only drift is not an edit");
            assert!(p.field_mut(kind).is_none());
        }
    }

    /// The guard compares the trimmed texts exactly. Whitespace around the
    /// default is not an edit, but a change of case inside it is: a user who
    /// capitalises one word keeps their version.
    #[test]
    fn padding_is_trimmed_away_but_a_recased_word_is_kept() {
        let default = crate::format::level::CleanupLevel::Balanced.default_rules();

        let mut p = PromptOverrides {
            balanced: Some(format!("  \n{default}\r\n\t ")),
            ..PromptOverrides::default()
        };
        assert!(p.normalize(), "padding the default with whitespace is not an edit");
        assert!(p.balanced.is_none());

        let mut recased_one = false;
        let recased = default
            .split(' ')
            .map(|word| {
                let plain = word.len() >= 4 && word.bytes().all(|b| b.is_ascii_lowercase());
                if plain && !recased_one {
                    recased_one = true;
                    word.to_ascii_uppercase()
                } else {
                    word.to_string()
                }
            })
            .collect::<Vec<_>>()
            .join(" ");
        assert!(recased_one, "the default has a lowercase word to capitalise");
        assert!(recased != default && recased.eq_ignore_ascii_case(&default));

        let mut p = PromptOverrides {
            balanced: Some(recased.clone()),
            ..PromptOverrides::default()
        };
        assert!(!p.normalize(), "a change of case is an edit");
        assert_eq!(p.balanced, Some(recased));
    }

    /// Blank is absence, not an empty rules text. A stored `""` would send
    /// the model nothing but the hardening stanza.
    #[test]
    fn a_blank_override_is_stored_as_absent() {
        let mut p = PromptOverrides {
            high: Some("   \n\t ".into()),
            ..PromptOverrides::default()
        };
        assert!(p.normalize());
        assert!(p.high.is_none());
    }

    /// `normalize` runs on every save; a second pass must find nothing to do
    /// — the same idempotence `repair` is held to.
    #[test]
    fn normalize_is_idempotent() {
        let mut p = PromptOverrides {
            light: Some(crate::format::level::CleanupLevel::Light.default_rules()),
            balanced: Some("Just punctuate.".into()),
            ..PromptOverrides::default()
        };
        assert!(p.normalize());
        assert!(!p.normalize(), "second pass must be a no-op");
        assert!(p.light.is_none());
        assert_eq!(p.balanced.as_deref(), Some("Just punctuate."));
    }

    /// The resolver the dictation path reads. `Off` never has one — it makes
    /// no model call, so there is no prompt to override.
    #[test]
    fn rules_for_answers_per_level_and_never_for_off() {
        use crate::format::level::CleanupLevel;
        let p = PromptOverrides {
            light: Some("L".into()),
            balanced: Some("B".into()),
            high: Some("H".into()),
            agent: Some("A".into()),
            selection_rules: Some("S".into()),
        };
        assert_eq!(p.rules_for(CleanupLevel::Light), Some("L"));
        assert_eq!(p.rules_for(CleanupLevel::Balanced), Some("B"));
        assert_eq!(p.rules_for(CleanupLevel::High), Some("H"));
        assert_eq!(p.rules_for(CleanupLevel::Off), None);
    }

    /// The override reaches the per-dictation snapshot both polish paths
    /// read, resolved against the level that snapshot carries — a Balanced
    /// override must not leak into a High dictation.
    #[test]
    fn the_snapshot_carries_the_override_for_its_own_level() {
        let mut s = Settings {
            prompts: PromptOverrides {
                balanced: Some("Just punctuate.".into()),
                ..PromptOverrides::default()
            },
            ..Settings::default()
        };
        s.cleanup.level = crate::format::level::CleanupLevel::Balanced;
        let snapshot: crate::cleanup::CleanupSettings = (&s).into();
        assert_eq!(snapshot.prompt_rules.as_deref(), Some("Just punctuate."));

        s.cleanup.level = crate::format::level::CleanupLevel::High;
        let snapshot: crate::cleanup::CleanupSettings = (&s).into();
        assert_eq!(
            snapshot.prompt_rules, None,
            "another level's override must not apply"
        );
    }

    /// `repair` finds nothing to do with ordinary overrides present. They are
    /// the user's own text; the only changes a load makes to one are the
    /// name token in a saved agent brief, the selection envelope's former
    /// field names and the selection rules' older key (all tested above).
    #[test]
    fn repair_is_still_idempotent_with_the_prompt_overrides_present() {
        let raw = r#"{"version":3,"sarvam":{"polishModel":"sarvam-105b"},"cleanup":{"level":"high"},"prompts":{"high":"Just punctuate.","agent":"Be terse."}}"#;
        let mut value: serde_json::Value = serde_json::from_str(raw).unwrap();
        assert!(!repair(&mut value), "nothing here is stale");
        assert!(!repair(&mut value), "second pass must be a no-op");
        let s: Settings = serde_json::from_value(value).unwrap();
        assert_eq!(s.prompts.high.as_deref(), Some("Just punctuate."));
        assert_eq!(s.prompts.agent.as_deref(), Some("Be terse."));
    }

    /// A stored override that happens to equal a shipped default stays as it
    /// is through `load()`: only a save runs `PromptOverrides::normalize`, so
    /// reading the file never rewrites a prompt in it.
    #[test]
    fn loading_leaves_a_default_equal_override_alone() {
        let default = crate::format::level::CleanupLevel::Balanced.default_rules();
        let raw = serde_json::json!({
            "version": 3,
            "prompts": { "balanced": default },
        })
        .to_string();
        let (changed, s) = migrated(&raw);
        assert!(!changed, "reading settings must not rewrite a stored prompt");
        assert_eq!(s.prompts.balanced.as_deref(), Some(default.as_str()));
    }

    /// Prompts are the user's own words but they are not secrets, so unlike
    /// the endpoint key they DO belong in an export — pinned so the field's
    /// serde names cannot drift away from the UI that writes them.
    #[test]
    fn prompt_overrides_serialize_under_their_camel_case_names() {
        let s = Settings {
            prompts: PromptOverrides {
                selection_rules: Some("S".into()),
                ..PromptOverrides::default()
            },
            ..Settings::default()
        };
        let obj: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
        assert_eq!(obj["prompts"]["selectionRules"], "S");
        assert!(obj["prompts"]["light"].is_null());
    }

    /// The load path keeps a mirror folder it cannot see right now.
    ///
    /// `load()` persists whatever `migrate | repair` changed, so anything
    /// `repair` clears is cleared in `settings.json` *permanently*. A vault on
    /// a BitLocker-locked disk, a VeraCrypt volume or a mapped network drive
    /// is routinely not mounted yet when `launch_at_login` starts the app —
    /// so a `repair` that ran `is_dir()` would erase the folder and the switch
    /// on every boot, and the user would re-pick them on every boot. The
    /// mirror's own contract (`mirror::Counts::log`) already covers an absent
    /// destination: one line per save counting what could not be written.
    #[test]
    fn repair_keeps_a_mirror_folder_that_is_not_mounted_right_now() {
        let gone = std::env::temp_dir().join("bs-settings-repair-no-such-folder-2f8c1a");
        let mut value = serde_json::json!({
            "version": 2,
            "notes": { "mirrorEnabled": true, "mirrorDir": gone },
        });
        assert!(!repair(&mut value), "an unmounted drive is not a stale value");
        let s: Settings = serde_json::from_value(value).unwrap();
        assert_eq!(s.notes.mirror_dir.as_deref(), Some(gone.as_path()));
        assert!(s.notes.mirror_enabled, "the switch must survive the drive being away");
        assert_eq!(s.notes.mirror_root(), Some(gone.as_path()));
    }

    /// An imported file never names the notes folder: a folder written there
    /// belongs to another machine or to whoever wrote the file, and one that
    /// exists everywhere (`C:\Users\Public\Documents`) would start receiving
    /// every note. The folder and the switch stay as this machine has them.
    #[test]
    fn import_keeps_this_machines_notes_mirror() {
        let here = std::env::temp_dir();
        let mut current = Settings::default();
        current.notes.mirror_dir = Some(here.join("my-vault"));
        current.notes.mirror_enabled = false;

        let mut imported: Settings = serde_json::from_value(serde_json::json!({
            "version": 3,
            "style": "casual",
            "notes": { "mirrorEnabled": true, "mirrorDir": "C:\\Users\\Public\\Documents" },
        }))
        .unwrap();
        keep_this_machines_mirror(&current, &mut imported);
        assert_eq!(imported.notes.mirror_dir, Some(here.join("my-vault")));
        assert!(!imported.notes.mirror_enabled);
        assert_eq!(imported.style, "casual", "the rest of the file is taken");

        let mut into_nothing: Settings = serde_json::from_value(serde_json::json!({
            "notes": { "mirrorEnabled": true, "mirrorDir": "C:\\Users\\Public\\Documents" },
        }))
        .unwrap();
        keep_this_machines_mirror(&Settings::default(), &mut into_nothing);
        assert_eq!(into_nothing.notes.mirror_dir, None);
        assert!(into_nothing.notes.mirror_root().is_none());
    }

    /// No screen edits `cloud`, so a page write (or an import, which goes
    /// through the same door) can never move the relay the sign-in token and
    /// the audio go to.
    #[test]
    fn a_page_write_keeps_the_stored_relay() {
        let stored = Settings {
            cloud: CloudSettings {
                relay_url: Some("http://127.0.0.1:8787".into()),
            },
            ..Settings::default()
        };
        let mut incoming = Settings {
            cloud: CloudSettings {
                relay_url: Some("https://attacker.example".into()),
            },
            ..Settings::default()
        };
        carry_stored_fields(&stored, &mut incoming);
        assert_eq!(incoming.cloud.relay_url.as_deref(), Some("http://127.0.0.1:8787"));

        let mut cleared = Settings::default();
        carry_stored_fields(&Settings::default(), &mut cleared);
        assert!(cleared.cloud.relay_url.is_none());
    }

    /// The ceiling `repair` enforces on load holds for a page write too: the
    /// finalize watchdog prices the clipboard restore at it.
    #[test]
    fn a_page_write_cannot_raise_the_restore_delay_past_its_ceiling() {
        let mut incoming = Settings::default();
        incoming.injection.restore_delay_ms = 30_000;
        carry_stored_fields(&Settings::default(), &mut incoming);
        assert_eq!(incoming.injection.restore_delay_ms, RESTORE_DELAY_MAX_MS);
        assert!(incoming.injection.restore_clipboard);

        let mut in_range = Settings::default();
        in_range.injection.restore_delay_ms = 450;
        carry_stored_fields(&Settings::default(), &mut in_range);
        assert_eq!(in_range.injection.restore_delay_ms, 450);
    }

    #[test]
    fn default_model_scales_with_ram() {
        const GIB: u64 = 1024 * 1024 * 1024;
        assert_eq!(default_model_for(4 * GIB), "moonshine-tiny-en");
        assert_eq!(default_model_for(8 * GIB), "moonshine-base-en");
        assert_eq!(default_model_for(16 * GIB), "moonshine-base-en");
        assert_eq!(default_model_for(0), "moonshine-tiny-en", "failed detection");
    }
}
