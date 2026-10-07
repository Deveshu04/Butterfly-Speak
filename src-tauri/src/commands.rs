//! Tauri command surface: everything the settings/onboarding UI can do.
//!
//! Not quite everything. The five Cloud commands (`cloud_sign_in`,
//! `cloud_sign_out`, `cloud_status`, `cloud_usage`, `cloud_delete_account`)
//! live in `auth::commands`, beside the session state they are a shell over,
//! and the three updater commands (`update_status`, `update_check`,
//! `update_install`) live in `updater`; `generate_handler!` names each from
//! its own module. Re-exporting them here takes more than the obvious line,
//! which is worth knowing: `#[tauri::command]` also emits a hidden
//! `__cmd__<name>` macro that `generate_handler!` reaches for through the
//! same path, so a `pub use` of the function alone leaves that behind. Naming
//! the hidden macro too (`pub use auth::commands::{cloud_sign_in,
//! __cmd__cloud_sign_in}`) does compile; pointing `generate_handler!` at the
//! module the commands live in is simply plainer.

use crate::asr::offline::{AsrMsg, ModelSpec};
use crate::audio::AudioCmd;
use crate::cleanup::CleanupSettings;
use crate::history;
use crate::hotkeys::{parse_binding, Binding};
use crate::models::{self, catalog, downloader, ram, DownloadRegistry, ModelStatus};
use crate::settings::{self, Settings};
use crossbeam_channel::Sender;
use serde::Serialize;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use tauri::{AppHandle, Manager, State};
use tauri_plugin_autostart::ManagerExt;

pub struct Backend {
    pub settings: Arc<RwLock<Settings>>,
    pub cleanup: Arc<RwLock<CleanupSettings>>,
    pub asr_tx: Sender<AsrMsg>,
    pub audio_cmd: Sender<AudioCmd>,
    pub binding: Arc<RwLock<Binding>>,
    pub transform_bindings: Arc<RwLock<Vec<Binding>>>,
    pub app_shortcut_bindings: Arc<RwLock<Vec<Binding>>>,
    /// The translate / voice-agent dictation chords (`hotkeys::ROUTE_CHORD_*`).
    pub route_chord_bindings: Arc<RwLock<Vec<Binding>>>,
    pub capture: Arc<AtomicBool>,
    pub paused: Arc<AtomicBool>,
    /// True while a dictation is in any state but Idle. Written only by the
    /// controller thread (`controller::Controller::set_state`); read by the
    /// updater, which must not hand the installer the bytes — and with them
    /// this process's life — while a recording, a finalize or an injection is
    /// in flight.
    pub dictation_busy: Arc<AtomicBool>,
    /// True while a transform runs (`transforms::spawn`'s guard clears it).
    /// Read by the updater beside `dictation_busy`: a transform has copied
    /// the user's selection and is about to paste over it, and ending the
    /// process in between would leave the clipboard holding it.
    pub transform_busy: Arc<AtomicBool>,
    pub downloads: DownloadRegistry,
    /// Sarvam API key cache (backed by the Windows credential store). Never
    /// part of `Settings` — the webview only ever sees presence + a mask.
    pub sarvam_key: crate::sarvam::SharedKey,
    /// Handle to the history DB thread.
    pub history: history::Recorder,
    /// The controller's channel, so History's delete and clear reach the
    /// last dictation it keeps in memory.
    pub control: crossbeam_channel::Sender<crate::state::ControlMsg>,
    /// The auto-learn frequency guard. `undo_learned_correction` below reads
    /// it; the field monitor calls `observe` on the controller's clone.
    pub learn: crate::learn::candidates::Guard,
    /// The audio/video import queue. Shared by the picker command, the
    /// drag-drop handler in `lib.rs`, and the Cancel button — one queue, so a
    /// cancel always reaches the run that is actually draining.
    pub import: crate::import::ImportQueue,
}

pub fn spec_for(entry: &catalog::ModelEntry) -> ModelSpec {
    ModelSpec {
        id: entry.id.clone(),
        dir: downloader::model_dir(&entry.dir_name)
            .to_string_lossy()
            .into_owned(),
        engine: entry.engine.clone(),
        native_punct: entry.native_punct,
    }
}

/// What a settings write means for the on-device model in memory.
#[derive(Debug, PartialEq, Eq)]
enum ModelChange {
    None,
    Load,
    Unload,
}

/// The on-device model is held only while the local provider is selected:
/// loaded when a write turns Local on or picks another model under it, and
/// dropped when a write moves the provider away (about 1.35 GB for the
/// largest model).
///
/// Not dropped while `dictating`: a local recording started before the
/// switch still needs the model to be transcribed. The controller drops it
/// when that dictation ends (`controller::unloads_at_idle`).
fn model_change(old: &Settings, new: &Settings, dictating: bool) -> ModelChange {
    use crate::settings::Provider;
    let was_local = old.provider == Provider::Local;
    match (was_local, new.provider == Provider::Local) {
        (false, true) => ModelChange::Load,
        (true, true) if old.model.selected_id != new.model.selected_id => ModelChange::Load,
        (true, false) if !dictating => ModelChange::Unload,
        _ => ModelChange::None,
    }
}

#[tauri::command]
pub fn get_settings(app: AppHandle, backend: State<Backend>) -> Result<Settings, String> {
    Ok(get_settings_inner(app, backend))
}

fn get_settings_inner(app: AppHandle, backend: State<Backend>) -> Settings {
    // The persisted value is only ever this app's own last write; it can't
    // see a later disable from Task Manager / Settings > Startup Apps.
    // Overwrite it with the TRUE registry state on every read, so the
    // toggle always shows reality rather than a stale intent — see
    // `autostart::true_state`. This has to land back in `backend.settings`,
    // not just the clone handed to the frontend: `set_settings` diffs the
    // *next* save against that cache (commands.rs's own `old.app.
    // launch_at_login != settings.app.launch_at_login` check), and the
    // frontend always posts a whole-object read-modify-write. A read-only
    // overwrite here would leave the cache holding a stale intent forever,
    // so the very next unrelated save (cleanup level, mic, hotkey — anything)
    // would see a manufactured mismatch against the freshly-read truth and
    // fire an autolaunch.enable()/disable() nobody asked for.
    //
    // The whole UI is gated on this call succeeding, so never panic on a
    // poisoned lock here: a panic would fail every future call and leave the
    // window blank for the rest of the session — recover the guard instead.
    let mut settings = backend
        .settings
        .write()
        .unwrap_or_else(|e| e.into_inner());
    settings.app.launch_at_login = crate::autostart::true_state(&app.package_info().name);
    settings.clone()
}

#[tauri::command]
pub fn set_settings(
    app: AppHandle,
    backend: State<Backend>,
    mut settings: Settings,
) -> Result<(), String> {
    let old = backend.settings.read().expect("settings lock").clone();
    // Before anything reads `settings`: the fields a page never writes, and
    // the ceiling a page must not exceed. See `carry_stored_fields`.
    settings::carry_stored_fields(&old, &mut settings);

    if old.app.launch_at_login != settings.app.launch_at_login {
        let autolaunch = app.autolaunch();
        let result = if settings.app.launch_at_login {
            autolaunch.enable()
        } else {
            autolaunch.disable()
        };
        if let Err(e) = result {
            tracing::warn!("autostart change failed: {e}");
        }
    }
    // Whatever the caller asked for, persist what actually happened: the
    // plugin's enable()/disable() can silently no-op under a policy that
    // blocks writing `Run`, or leave a stale `StartupApproved` disable in
    // place from outside this app entirely. Re-reading the truth here (not
    // just trusting the request), before the file write below, is what
    // keeps the persisted setting from drifting from the registry the
    // moment either one changes without the other — see
    // `autostart::true_state`.
    settings.app.launch_at_login = crate::autostart::true_state(&app.package_info().name);

    // The Prompts page's save-time guard, on the way IN — before the file is
    // written and before this becomes the in-memory copy every dictation
    // reads. A prompt that equals the shipped default is not a
    // customisation; stored as one, it would keep this install on today's
    // text after an update ships a better one, with nothing to show for it.
    // Here rather than in the UI because this is the one door: an import, a
    // hand-edited file round-tripped through the settings screen, and a
    // future caller all pass through it. See `PromptOverrides::normalize`.
    settings.prompts.normalize();

    settings::save(&settings).map_err(|e| e.to_string())?;

    *backend.cleanup.write().expect("cleanup lock") = (&settings).into();
    // The settings half of the custom endpoint slot only; the key half is
    // owned by the credential store and untouched by a settings write.
    crate::endpoint::set_config(&settings.custom_endpoint);
    *backend.binding.write().expect("binding lock") = parse_binding(&settings.hotkey.binding);
    if old.hotkey.binding != settings.hotkey.binding {
        crate::tray::refresh_tooltip(
            &app,
            backend.paused.load(Ordering::Relaxed),
            &settings.hotkey.binding,
        );
    }
    *backend.transform_bindings.write().expect("transforms lock") =
        crate::hotkeys::transform_bindings(&settings);
    *backend
        .app_shortcut_bindings
        .write()
        .expect("shortcuts lock") = crate::hotkeys::app_shortcut_bindings(&settings);
    *backend.route_chord_bindings.write().expect("route lock") =
        crate::hotkeys::route_chord_bindings(&settings);

    if old.audio.device_name != settings.audio.device_name {
        let _ = backend
            .audio_cmd
            .send(AudioCmd::SetDevice(settings.audio.device_name.clone()));
    }
    // The on-device model is in memory only while the local provider is
    // selected: it is loaded when Local is chosen (or its model changes while
    // Local is on) and unloaded when the provider moves away. Nothing
    // downloads here — downloads happen only from the explicit Download
    // buttons in the app.
    match model_change(&old, &settings, backend.dictation_busy.load(Ordering::SeqCst)) {
        ModelChange::Load => {
            if let Some(entry) = catalog::entry(&settings.model.selected_id) {
                if downloader::dir_installed(&entry.dir_name) {
                    let _ = backend.asr_tx.send(AsrMsg::LoadModel(spec_for(entry)));
                }
            }
        }
        ModelChange::Unload => {
            let _ = backend.asr_tx.send(AsrMsg::Unload);
        }
        ModelChange::None => {}
    }

    // Unconditional, like the other side-effect syncs above — the DB thread
    // itself decides whether retention actually changed (history module doc).
    backend.history.set_retention(history::RetentionCfg {
        enabled: settings.history.enabled,
        keep_days: settings.history.keep_days,
    });

    *backend.settings.write().expect("settings lock") = settings;
    Ok(())
}

#[tauri::command]
pub fn list_models(backend: State<Backend>) -> Vec<ModelStatus> {
    let selected = backend
        .settings
        .read()
        .expect("settings lock")
        .model
        .selected_id
        .clone();
    models::list(&selected, &backend.downloads)
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PolishStatus {
    /// False when this binary was built without the `polish` cargo feature —
    /// the on-device polish model can't run, so the UI must not offer it.
    pub available: bool,
    pub installed: bool,
    pub downloading: bool,
    pub disk_bytes: u64,
    pub est_ram_bytes: u64,
    pub verdict: ram::RamVerdict,
}

#[tauri::command]
pub fn polish_status(backend: State<Backend>) -> PolishStatus {
    let p = &catalog::catalog().ai_polish;
    PolishStatus {
        available: cfg!(feature = "polish"),
        installed: downloader::file_installed(&p.file_name, p.disk_bytes),
        downloading: backend.downloads.is_downloading("aiPolish"),
        disk_bytes: p.disk_bytes,
        est_ram_bytes: p.est_ram_bytes,
        verdict: ram::verdict(p.est_ram_bytes),
    }
}

#[tauri::command]
pub fn download_model(app: AppHandle, backend: State<Backend>, id: String) -> Result<(), String> {
    let job = if id == "aiPolish" {
        if !cfg!(feature = "polish") {
            return Err("This build doesn't include the on-device polish engine".into());
        }
        let p = &catalog::catalog().ai_polish;
        downloader::Job {
            id: "aiPolish".into(),
            url: p.url.clone(),
            sha256: p.sha256.clone(),
            total_bytes: p.disk_bytes,
            kind: downloader::JobKind::File {
                file_name: p.file_name.clone(),
            },
        }
    } else {
        let entry = catalog::entry(&id).ok_or("unknown model id")?;
        models::job_for(entry)
    };

    let Some(cancel) = backend.downloads.begin(&job.id) else {
        return Err("already downloading".into());
    };
    let job_id = job.id.clone();
    let app2 = app.clone();
    tauri::async_runtime::spawn(async move {
        downloader::run(app2.clone(), job, cancel).await;
        let backend = app2.state::<Backend>();
        backend.downloads.finish(&job_id);
        // If this was the selected (but previously missing) model, load it
        // now — on the local provider only, which is the one that uses it.
        let (selected, local) = {
            let s = backend.settings.read().expect("settings lock");
            (
                s.model.selected_id.clone(),
                s.provider == crate::settings::Provider::Local,
            )
        };
        if local && selected == job_id {
            if let Some(entry) = catalog::entry(&job_id) {
                if downloader::dir_installed(&entry.dir_name) {
                    let _ = backend.asr_tx.send(AsrMsg::LoadModel(spec_for(entry)));
                }
            }
        }
    });
    Ok(())
}

#[tauri::command]
pub fn cancel_download(backend: State<Backend>, id: String) {
    backend.downloads.cancel(&id);
}

#[tauri::command]
pub fn delete_model(backend: State<Backend>, id: String) -> Result<(), String> {
    let selected = backend
        .settings
        .read()
        .expect("settings lock")
        .model
        .selected_id
        .clone();
    if id == selected {
        return Err("Can't delete the active model — select another one first".into());
    }
    let entry = catalog::entry(&id).ok_or("unknown model id")?;
    downloader::delete_dir(&entry.dir_name).map_err(|e| e.to_string())
}

#[tauri::command]
pub fn select_model(app: AppHandle, backend: State<Backend>, id: String) -> Result<(), String> {
    let entry = catalog::entry(&id).ok_or("unknown model id")?;
    if !downloader::dir_installed(&entry.dir_name) {
        return Err("Model is not downloaded yet".into());
    }
    let mut settings = backend.settings.read().expect("settings lock").clone();
    settings.model.selected_id = id;
    set_settings(app, backend, settings)
}

#[tauri::command]
pub fn list_mics() -> Vec<String> {
    crate::audio::list_input_devices()
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SystemInfo {
    pub total_ram_bytes: u64,
    pub available_ram_bytes: u64,
    pub app_version: String,
}

#[tauri::command]
pub fn system_info() -> SystemInfo {
    let info = ram::ram_info();
    SystemInfo {
        total_ram_bytes: info.total,
        available_ram_bytes: info.available,
        app_version: env!("CARGO_PKG_VERSION").into(),
    }
}

#[tauri::command]
pub fn set_meter(backend: State<Backend>, enabled: bool) {
    let _ = backend.audio_cmd.send(AudioCmd::Meter(enabled));
}

#[tauri::command]
pub fn hotkey_capture(backend: State<Backend>, active: bool) {
    backend.capture.store(active, Ordering::Relaxed);
}

/// Turn the shortcut recorder off when the page that turned it on can no
/// longer end it: the main window was closed to the tray, or its page is
/// loading again. Left on, every chord the user pressed would go to the
/// recorder instead of dictating.
///
/// A page still recording is told it was cancelled (the same empty `done` an
/// Escape sends), so a dialog hidden with the window does not wait on, and
/// save, whatever chord comes next.
pub fn end_hotkey_capture(app: &AppHandle) {
    use tauri::Emitter;
    let Some(backend) = app.try_state::<Backend>() else {
        return;
    };
    if backend.capture.swap(false, Ordering::Relaxed) {
        let _ = app.emit(
            crate::events::HOTKEY_CAPTURE,
            crate::events::HotkeyCapturePayload::done(String::new()),
        );
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SarvamKeyStatus {
    pub present: bool,
    /// "••••" + last 4 characters, for the settings UI.
    pub masked: Option<String>,
}

/// **The one mask shape for every key this app shows.**
///
/// The Settings screen shows two keys, Sarvam's and the custom endpoint's,
/// and both use this shape rather than, say, `first3…last4`, for three
/// reasons:
///
/// 1. It is already on users' screens. Re-rendering the Sarvam key that every
///    existing install has looked at since launch buys nothing.
/// 2. The last four characters are the part that answers the question a mask
///    is for — *is this the key I think it is?* The first three answer a
///    different question, "which vendor issued it", and on this screen the
///    endpoint URL sits two rows above the key and answers it better.
/// 3. It discloses less of a secret, and bullets read as "something is hidden
///    here" where a leading `sk-…` reads as a value that got truncated.
///
/// A key of eight characters or fewer shows nothing at all, because four of
/// eight is half the secret. No real Sarvam key is that short, so this only
/// stops the rule being accidentally right.
fn mask_key(key: &str) -> String {
    let chars: Vec<char> = key.chars().collect();
    if chars.len() <= 8 {
        return "••••••••".into();
    }
    // Counts characters, not bytes: byte-slicing a multibyte key would panic
    // on a character boundary.
    let tail: String = chars[chars.len() - 4..].iter().collect();
    format!("••••{tail}")
}

#[tauri::command]
pub fn sarvam_key_status(backend: State<Backend>) -> SarvamKeyStatus {
    let key = backend.sarvam_key.read().expect("key lock");
    SarvamKeyStatus {
        present: key.is_some(),
        masked: key.as_deref().map(mask_key),
    }
}

/// Store (or, with an empty string, remove) the Sarvam API key without
/// validating it. The onboarding/settings flow normally goes through
/// `validate_sarvam_key`, which stores on success.
#[tauri::command]
pub fn set_sarvam_key(backend: State<Backend>, key: String) -> Result<(), String> {
    let key = key.trim().to_string();
    let slot = crate::sarvam::key::KeySlot::Sarvam;
    if key.is_empty() {
        crate::sarvam::key::delete(slot).map_err(|e| e.to_string())?;
        *backend.sarvam_key.write().expect("key lock") = None;
    } else {
        crate::sarvam::key::store(slot, &key).map_err(|e| e.to_string())?;
        *backend.sarvam_key.write().expect("key lock") = Some(key);
    }
    Ok(())
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CustomEndpointKeyStatus {
    pub present: bool,
    /// The same `••••` + last four the Sarvam key uses — see [`mask_key`] for
    /// why the two shapes were collapsed onto this one.
    pub masked: Option<String>,
}

/// Presence + a mask for the custom endpoint's key. Never the key.
#[tauri::command]
pub fn custom_endpoint_key_status() -> CustomEndpointKeyStatus {
    let key = crate::endpoint::slot().api_key;
    CustomEndpointKeyStatus {
        present: key.is_some(),
        masked: key.as_deref().map(mask_key),
    }
}

/// Store (or, with an empty string, remove) the custom endpoint's key.
///
/// There is no validation step and no probe: an OpenAI-compatible host may
/// legitimately need no key at all, so "no key" is a configuration, not an
/// error. A wrong key surfaces as the server's own 401 at request time.
#[tauri::command]
pub fn set_custom_endpoint_key(key: String) -> Result<(), String> {
    let key = key.trim().to_string();
    let slot = crate::sarvam::key::KeySlot::CustomEndpoint;
    if key.is_empty() {
        crate::sarvam::key::delete(slot).map_err(|e| e.to_string())?;
        crate::endpoint::set_key(None);
    } else {
        crate::sarvam::key::store(slot, &key).map_err(|e| e.to_string())?;
        crate::endpoint::set_key(Some(key));
    }
    Ok(())
}

/// The settings-screen pre-flight for a pasted endpoint URL: normalize it, or
/// say why it cannot be used. Pure — no network call, nothing stored.
///
/// `Ok` carries **the chat route that would actually be requested**, not the
/// bare base. The base is not what gets requested, and the Settings panel
/// cannot turn one into the other by string concatenation. `/v1` would be
/// appended twice to a base that already ends in it, and a base carrying a
/// query (`https://h/v1?api-version=…`, an ordinary Azure/gateway paste)
/// would come out as `…?api-version=…/chat/completions`. Both transforms
/// live in `endpoint::chat_completions_url`; this hands the UI its output.
///
/// `Err` carries the sentence to display, and is also what the panel's
/// "Falling back to Sarvam: …" line is derived from.
#[tauri::command]
pub fn check_custom_endpoint(url: String) -> Result<String, String> {
    crate::endpoint::resolve_base(&url)
        .map(|base| crate::endpoint::chat_completions_url(&base))
        .map_err(|why| why.message().to_string())
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EndpointModel {
    pub id: String,
    /// The host's own `owned_by`, shown as a sublabel. Never used to filter
    /// or validate: `/models` only suggests ids, and an unlisted one may
    /// still work.
    pub owned_by: Option<String>,
}

impl From<crate::endpoint::probe::DiscoveredModel> for EndpointModel {
    fn from(m: crate::endpoint::probe::DiscoveredModel) -> Self {
        Self {
            id: m.id,
            owned_by: m.owned_by,
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EndpointProbeResult {
    /// The URL that was actually requested — configuration, not a credential,
    /// and the only way a user can tell that "https://host" became
    /// "https://host/v1/models".
    pub url: String,
    pub models: Vec<EndpointModel>,
}

/// One `GET {base}/models` on behalf of the Settings screen.
///
/// The key is read from the credential store here rather than accepted as an
/// argument: the webview has never held it and must not start, and a probe
/// has to use the same credential the real request will. It goes only to the
/// saved endpoint's address (`endpoint::key_for_probe`).
async fn run_probe(url: &str) -> Result<crate::endpoint::probe::Probed, String> {
    let key = crate::endpoint::key_for_probe(&crate::endpoint::slot(), url);
    crate::endpoint::probe::probe(&reqwest::Client::new(), url, key.as_deref())
        .await
        .map_err(|e| e.message())
}

/// **Test connection.** Pre-flight the URL, then ask the endpoint for its
/// model list with a four-second budget, and report which of "unusable URL",
/// "couldn't reach it", "it refused the credential" and "it answered with an
/// error" actually happened.
///
/// Nothing is stored and nothing is logged: a test is a question, not a
/// configuration change.
#[tauri::command]
pub async fn endpoint_test_connection(url: String) -> Result<EndpointProbeResult, String> {
    let probed = run_probe(&url).await?;
    Ok(EndpointProbeResult {
        url: probed.url,
        models: probed.models.into_iter().map(EndpointModel::from).collect(),
    })
}

/// The same probe, for the model-discovery dropdown. A host that answers with
/// a payload this app cannot read yields an empty list, not an error — the
/// user's own model id is valid whatever `/models` says, and wiping it
/// because a server spoke a dialect we don't parse would be the worst
/// possible reading of a *hint*.
#[tauri::command]
pub async fn endpoint_list_models(url: String) -> Result<Vec<EndpointModel>, String> {
    Ok(run_probe(&url)
        .await?
        .models
        .into_iter()
        .map(EndpointModel::from)
        .collect())
}

// --- The Prompts page ------------------------------------------------------
//
// Three commands, none of which writes a prompt anywhere. Saving an override
// is an ordinary `set_settings` write of `settings.prompts`, normalized on the
// way in by `PromptOverrides::normalize`; the two below only read and run.
//
// A draft is never written into the real settings store to test it: that
// would leave the draft persisted if the process died mid-request, and a
// dictation started elsewhere while the test was in flight would run the
// unsaved draft. Here the draft is an argument, `rules`, that travels down one
// call and is dropped when it returns.

/// One editable prompt and the text "Reset to default" restores.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PromptDefault {
    pub kind: settings::EditablePrompt,
    pub default_rules: String,
}

/// The shipped rules for every kind. The UI needs these for the editor's
/// initial value, for Reset, and to grey out Save when the draft matches
/// (the same comparison `PromptOverrides::normalize` makes authoritatively on
/// the way in).
#[tauri::command]
pub fn prompt_defaults() -> Vec<PromptDefault> {
    settings::PromptOverrides::defaults()
        .into_iter()
        .map(|(kind, default_rules)| PromptDefault { kind, default_rules })
        .collect()
}

/// The system turn a kind would actually send, composed by the same builders
/// the dictation path uses.
///
/// `rules` is the unsaved draft (`None` = whatever is saved, else the shipped
/// default). The point is that the Studio shows the *composed* prompt — the
/// user's rules plus everything the app puts around them — rather than the
/// half they typed: the injection stanza, the personal dictionary spliced in
/// among the rules, the transcript-delimiter line, the agent output rules. A
/// user who cannot see those cannot reason about what they changed.
///
/// Returns the **system** turn. For the cleanup kinds the end-marker
/// instruction rides on the user turn instead (`build_polish_messages`), so
/// it does not appear here; for the agent kinds it does, and the marker is
/// minted fresh per request — the one shown is a real example of that line,
/// not the exact bytes a later call will send.
#[tauri::command]
pub fn preview_prompt(
    backend: State<Backend>,
    kind: settings::EditablePrompt,
    rules: Option<String>,
) -> String {
    // A cleared textarea (`Some("")` / whitespace) is not an empty-rules
    // override: `PromptOverrides::normalize` drops such a draft on save, so the
    // preview of it must show what saving does — the saved override or the
    // shipped rules — matching `test_prompt` and the Save button's own copy.
    let rules = rules.filter(|r| !r.trim().is_empty());
    let (dictionary, agent_name, saved) = {
        let s = backend.settings.read().expect("settings lock");
        (
            s.dictionary.clone(),
            s.agent.name.clone(),
            s.prompts.clone(),
        )
    };
    let marker = crate::format::backend::mint_end_marker();
    // A draft beats the saved override beats the shipped default.
    let effective = |stored: Option<&str>| -> Option<String> {
        rules.clone().or_else(|| stored.map(str::to_string))
    };

    match kind {
        settings::EditablePrompt::Light
        | settings::EditablePrompt::Balanced
        | settings::EditablePrompt::High => {
            let level = kind.level().expect("a cleanup kind has a level");
            let base = level.prompt_with_rules(effective(saved.rules_for(level)).as_deref());
            let (system, _user) =
                crate::sarvam::chat::build_polish_messages(&base, &dictionary, "…", &marker, None);
            system
        }
        settings::EditablePrompt::Agent => crate::sarvam::chat::build_agent_system(
            &agent_name,
            &dictionary,
            &marker,
            false,
            crate::sarvam::chat::AgentPrompts {
                brief: effective(saved.agent.as_deref()).as_deref(),
                selection_rules: saved.selection_rules.as_deref(),
            },
        ),
        settings::EditablePrompt::SelectionRules => crate::sarvam::chat::build_agent_system(
            &agent_name,
            &dictionary,
            &marker,
            true,
            crate::sarvam::chat::AgentPrompts {
                brief: saved.agent.as_deref(),
                selection_rules: effective(saved.selection_rules.as_deref()).as_deref(),
            },
        ),
    }
}

/// What a live test run produced, honestly.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PromptTestResult {
    /// The system turn, composed by the same builders the call used.
    ///
    /// Byte-exact for the cleanup kinds (their end marker rides on the
    /// user turn, not this one). For the agent kinds the marker shown is a
    /// freshly minted example rather than the one that went on the wire:
    /// `chat::agent` mints its own inside the call, deliberately, so the
    /// marker it checks the reply against can never drift from the marker it
    /// sent. Everything else is byte-for-byte what was sent.
    pub system_prompt: String,
    /// The model's reply, before the guardrail sees it. Shown because the
    /// guardrail can silently substitute the rule output for it, and a user
    /// tuning a prompt who is shown only the substituted text is debugging
    /// the wrong string.
    pub raw_output: String,
    /// What the app would actually have used.
    pub final_output: String,
    /// The pill copy the dictation would have carried, if any — this is the
    /// guardrail (or a truncation) speaking.
    pub notice: Option<String>,
    /// True when the reply did not end with its marker, or the
    /// API reported `length`. The reply is unusable either way.
    pub truncated: bool,
    /// True when `final_output` is the model's reply rather than a fallback.
    pub used_model_output: bool,
}

/// Run a draft prompt against the live model and report both what came back
/// and what the app would have done with it.
///
/// Nothing is stored: the draft is passed as an argument
/// (`sarvam::chat::AgentPrompts`, `polish`'s `system_prompt_override`) and
/// dropped when this returns. The settings lock is read and released before
/// the network call — never held across an await.
///
/// `selection` is required for the `selectionRules` kind and ignored by every
/// other: those rules only exist when a command is spoken with text selected,
/// and testing them without a selection would render a block the model never
/// receives.
#[tauri::command]
pub async fn test_prompt(
    backend: State<'_, Backend>,
    kind: settings::EditablePrompt,
    rules: Option<String>,
    input: String,
    selection: Option<String>,
) -> Result<PromptTestResult, String> {
    let input = input.trim().to_string();
    if input.is_empty() {
        return Err("Type something for the model to work on first.".into());
    }
    // A cleared textarea (`Some("")` / whitespace) is not an empty-rules
    // override: `PromptOverrides::normalize` drops such a draft on save, so a
    // test of it must show what saving does — the saved override, or the
    // shipped rules — not empty-rules behaviour.
    let rules = rules.filter(|r| !r.trim().is_empty());
    // A blank or absent key is no key — carried as `None` so the resolver can
    // take its `NoSarvamKey` arm by type, and a configured custom endpoint can
    // be tested without a Sarvam key at all (the `chat_credentials` pattern).
    let api_key = backend
        .sarvam_key
        .read()
        .expect("key lock")
        .clone()
        .filter(|k| !k.trim().is_empty());
    let (dictionary, agent_name, saved, polish_model, lane) = {
        let s = backend.settings.read().expect("settings lock");
        (
            s.dictionary.clone(),
            s.agent.name.clone(),
            s.prompts.clone(),
            s.sarvam.polish_model.clone(),
            crate::controller::lane_for(&s),
        )
    };
    // The model this install actually uses — off the resolved backend, which
    // is the custom endpoint's own model when that slot is on. A missing
    // Sarvam key with no custom endpoint is the one thing this cannot do.
    let chat_backend = note_chat_backend(
        &lane,
        api_key.as_deref(),
        &polish_model,
        "Add an API key in Settings → Speech engine first.",
    )
    .await?;
    let http = reqwest::Client::new();
    let effective = |stored: Option<&str>| -> Option<String> {
        rules.clone().or_else(|| stored.map(str::to_string))
    };

    match kind {
        settings::EditablePrompt::Light
        | settings::EditablePrompt::Balanced
        | settings::EditablePrompt::High => {
            let level = kind.level().expect("a cleanup kind has a level");
            let override_rules = effective(saved.rules_for(level));
            let base = level.prompt();
            // The same composition `polish_with_timeout` performs on `base` +
            // `override_rules`, computed here only so the result can report
            // what was sent. `PromptParts::split(base).with_rules(r)` and
            // `level.prompt_with_rules(Some(r))` are the same two halves.
            let system_prompt = crate::sarvam::chat::system_prompt(
                &level.prompt_with_rules(override_rules.as_deref()),
                &dictionary,
                false,
            );
            match crate::sarvam::chat::polish(
                &http,
                &chat_backend,
                &input,
                &dictionary,
                &base,
                override_rules.as_deref(),
            )
            .await
            {
                crate::sarvam::chat::PolishOutcome::Failed(failure) => {
                    // The server's message is returned to the user's own
                    // screen, never logged: a 400 body can echo the request,
                    // and the request carries the text they typed. The log
                    // line stays content-free.
                    tracing::warn!("a prompt test call failed");
                    Err(format!("The model call failed: {}", failure.shown()))
                }
                crate::sarvam::chat::PolishOutcome::Formatted(reply) => {
                    let truncated = reply.was_truncated();
                    let raw_output = reply.text.clone();
                    // The real guardrail, on the real reply. The test would
                    // be a lie without it: `format::guard` is what decides
                    // whether a prompt's output ever reaches a document.
                    let (final_output, notice) = crate::sarvam::ws::resolve_format(
                        &input, &input, Some(&reply), level,
                    );
                    Ok(PromptTestResult {
                        // The notice is the signal, not a string comparison:
                        // `resolve_format` emits one exactly when it fell
                        // back to the rule pipeline, and a reply that happens
                        // to equal its own input must not read as a rejection.
                        used_model_output: notice.is_none(),
                        system_prompt,
                        raw_output,
                        final_output,
                        notice,
                        truncated,
                    })
                }
            }
        }
        settings::EditablePrompt::Agent | settings::EditablePrompt::SelectionRules => {
            let is_selection = kind == settings::EditablePrompt::SelectionRules;
            let selection = selection.filter(|s| !s.trim().is_empty());
            if is_selection && selection.is_none() {
                return Err("Paste some selected text to edit — these rules only \
                            apply when you speak with text selected."
                    .into());
            }
            let brief = if is_selection {
                saved.agent.clone()
            } else {
                effective(saved.agent.as_deref())
            };
            let selection_rules = if is_selection {
                effective(saved.selection_rules.as_deref())
            } else {
                saved.selection_rules.clone()
            };
            let marker = crate::format::backend::mint_end_marker();
            let prompts = crate::sarvam::chat::AgentPrompts {
                brief: brief.as_deref(),
                selection_rules: selection_rules.as_deref(),
            };
            let system_prompt = crate::sarvam::chat::build_agent_system(
                &agent_name,
                &dictionary,
                &marker,
                is_selection,
                prompts,
            );
            let reply = crate::sarvam::chat::agent(
                &http,
                &chat_backend,
                &agent_name,
                &dictionary,
                &input,
                selection.as_deref(),
                prompts,
            )
            .await
            .map_err(|e| {
                // Content-free, for the reason above. The server's own
                // message goes to the screen only (`shown_failure`).
                tracing::warn!("a prompt test call failed");
                let shown = crate::format::backend::shown_failure(&e)
                    .unwrap_or_else(|| format!("{e:#}"));
                format!("The model call failed: {shown}")
            })?;
            let truncated = reply.was_truncated();
            // The agent path has no guardrail — its own rule is that a
            // truncated reply is typed nowhere (`routes::agent`), because
            // half a replacement pasted over a selection deletes the rest of
            // the paragraph. Report that as what the app would have done.
            let final_output = if truncated {
                String::new()
            } else {
                reply.text.trim().to_string()
            };
            Ok(PromptTestResult {
                system_prompt,
                raw_output: reply.text,
                final_output,
                notice: truncated
                    .then(|| "The reply was cut off — nothing would be typed.".to_string()),
                truncated,
                used_model_output: !truncated,
            })
        }
    }
}

/// Validate a key against the live realtime STT endpoint and store it on
/// success. Errors are user-facing copy.
#[tauri::command]
pub async fn validate_sarvam_key(
    backend: State<'_, Backend>,
    key: String,
) -> Result<(), String> {
    let key = key.trim().to_string();
    if key.is_empty() {
        return Err("Enter an API key".into());
    }
    crate::sarvam::key::validate(&key).await?;
    // A credential-store failure (corporate policy, roaming profile) must
    // not brick setup: the validated key still works from the in-memory
    // cache for this run; the user just re-enters it after a restart.
    if let Err(e) = crate::sarvam::key::store(crate::sarvam::key::KeySlot::Sarvam, &key) {
        tracing::warn!("couldn't persist Sarvam key to the credential store: {e:#}");
    }
    *backend.sarvam_key.write().expect("key lock") = Some(key);
    Ok(())
}

/// Download any missing support artifacts (the punctuation model) in the
/// background. Only invoked from Settings → Speech engine alongside an explicit,
/// user-initiated model download — never automatically.
#[tauri::command]
pub fn ensure_support_models(app: AppHandle, backend: State<Backend>) {
    for job in models::missing_support_jobs() {
        if let Some(cancel) = backend.downloads.begin(&job.id) {
            let id = job.id.clone();
            let app2 = app.clone();
            tauri::async_runtime::spawn(async move {
                downloader::run(app2.clone(), job, cancel).await;
                app2.state::<Backend>().downloads.finish(&id);
            });
        }
    }
}

/// Most-recent-first page of filed dictations, failed ones included.
/// `page_size` is clamped server-side (`history::store::list`).
#[tauri::command]
pub fn history_list(backend: State<Backend>, page: u32, page_size: u32) -> Vec<history::Entry> {
    backend.history.list(page, page_size)
}

#[tauri::command]
pub fn history_search(backend: State<Backend>, query: String, limit: u32) -> Vec<history::Entry> {
    backend.history.search(query, limit)
}

/// Deletes one dictation, and tells the controller, which forgets it too if
/// it is the one Paste and Copy last transcript would bring back.
#[tauri::command]
pub fn history_delete(backend: State<Backend>, id: i64) -> bool {
    match backend.history.remove(id) {
        Some((text, raw)) => {
            let _ = backend.control.send(crate::state::ControlMsg::HistoryRemoved(
                crate::state::HistoryRemoval::Row { text, raw },
            ));
            true
        }
        None => false,
    }
}

/// Home's Edit. The corrected text replaces the row's `text`; `false` when
/// the row no longer exists, and an error when the history database is not
/// running (it failed to open), so Home can tell the two apart.
#[tauri::command]
pub fn history_update_text(
    backend: State<Backend>,
    id: i64,
    text: String,
) -> Result<bool, String> {
    backend
        .history
        .update_text(id, text)
        .ok_or_else(|| "the history database is not available".to_string())
}

/// History's "Clear all history". The controller forgets the last dictation
/// too, whatever the count: while history is off it was never filed, but the
/// user has asked for their dictations to go.
#[tauri::command]
pub fn history_clear(backend: State<Backend>) -> u32 {
    let n = backend.history.clear();
    let _ = backend.control.send(crate::state::ControlMsg::HistoryRemoved(
        crate::state::HistoryRemoval::All,
    ));
    n
}

// ---------------------------------------------------------------------------
// Notes and folders.
//
// Every one of these goes through `Recorder::with_connection`, which runs the
// closure on the history DB thread. Two rules hold across the whole block:
//
// - The closure must be `Send + 'static`, so it owns its arguments rather than
//   borrowing the command's — that is why each one takes `String`/`i64` by
//   value and moves them in.
// - LOCK ORDERING: none of these takes the settings lock, and none may start
//   to. `with_connection` blocks until the DB thread answers, and a caller
//   holding a settings guard (read or write) across it would be waiting for a
//   thread that may need the same lock.
//
// `with_connection` answering `None` means the DB thread is gone (the file
// failed to open). Per its contract that is "this feature is off this
// session", never an error to blame the user for — reads degrade to empty,
// writes say so in a sentence.
//
// **Every one is `(async)`**, for the same reason `export_settings` is: without
// it a command dispatches inline on the IPC handler, which *is* the
// main/event-loop thread, so the whole app — overlay included — stalls for the
// length of the round trip. A local WAL round trip is microseconds, but the
// autosave the notes page will run fires one per second per edited note, and
// `delete_folder` cascades over however many notes a folder holds. `(async)`
// routes the (still synchronous) body through Tauri's async runtime pool
// instead. It costs nothing here because every closure already had to be
// `Send + 'static` to cross `with_connection`.
// ---------------------------------------------------------------------------

const NOTES_UNAVAILABLE: &str = "Notes are unavailable this session — the local database didn't open.";

/// Where the markdown mirror writes, or `None` when it must not write.
///
/// Reading it here — and returning an owned `PathBuf` — is what lets every
/// call site below satisfy the lock-ordering rule by construction: the guard
/// is taken and dropped inside this function, so no caller can still be
/// holding it when it blocks on `with_connection`. See
/// `settings::NotesSettings::mirror_root` for why "on" needs a folder too.
fn mirror_root(backend: &Backend) -> Option<std::path::PathBuf> {
    backend
        .settings
        .read()
        .expect("settings lock")
        .notes
        .mirror_root()
        .map(std::path::Path::to_path_buf)
}

#[tauri::command(async)]
pub fn create_note(backend: State<Backend>, note: crate::notes::NewNote) -> Result<i64, String> {
    let root = mirror_root(&backend);
    backend
        .history
        .with_connection(move |conn| {
            crate::notes::mirror::commit_then_mirror(
                conn,
                root.as_deref(),
                |c| crate::notes::create_note(c, &note).map_err(|e| e.to_string()),
                |_, id| crate::notes::mirror::Job::Write(vec![*id]),
            )
        })
        .unwrap_or_else(|| Err(NOTES_UNAVAILABLE.into()))
}

#[tauri::command(async)]
pub fn get_note(backend: State<Backend>, id: i64) -> Option<crate::notes::Note> {
    backend
        .history
        .with_connection(move |conn| {
            crate::notes::get_note(conn, id).unwrap_or_else(|e| {
                // A missing note and a broken query both answer `None` to the
                // caller, which is right — a note the user deleted in another
                // window is not an error to show them. But folding the second
                // into the first silently is how a real fault goes unnoticed,
                // so it gets a line. Id and error only; never note content.
                tracing::warn!("notes get failed for id {id}: {e}");
                None
            })
        })
        .flatten()
}

/// Applies only the allow-listed fields present in `update`; an absent field
/// is left alone and an explicit `null` clears the column. Resolves `false`
/// when the update named no fields or matched no row.
#[tauri::command(async)]
pub fn update_note(
    backend: State<Backend>,
    id: i64,
    update: crate::notes::NoteUpdate,
) -> Result<bool, String> {
    let root = mirror_root(&backend);
    backend
        .history
        .with_connection(move |conn| {
            crate::notes::mirror::commit_then_mirror(
                conn,
                root.as_deref(),
                |c| crate::notes::update_note(c, id, &update).map_err(|e| e.to_string()),
                // Even a save that changed nothing re-writes the file: it is
                // the cheapest reconciliation there is for a mirror that has
                // drifted (an external editor, an unplugged drive), and the
                // committed row is what it writes either way.
                |_, _| crate::notes::mirror::Job::Write(vec![id]),
            )
        })
        .unwrap_or_else(|| Err(NOTES_UNAVAILABLE.into()))
}

/// Hard delete: there is no trash, and the search index loses the row in the
/// same statement (see `notes`' module doc).
#[tauri::command(async)]
pub fn delete_note(backend: State<Backend>, id: i64) -> Result<bool, String> {
    let root = mirror_root(&backend);
    backend
        .history
        .with_connection(move |conn| {
            crate::notes::mirror::commit_then_mirror(
                conn,
                root.as_deref(),
                |c| crate::notes::delete_note(c, id).map_err(|e| e.to_string()),
                |_, _| crate::notes::mirror::Job::Remove { ids: vec![id], dir: None },
            )
        })
        .unwrap_or_else(|| Err(NOTES_UNAVAILABLE.into()))
}

/// Most-recently-edited-first page of `notes::PAGE_SIZE`. `args.folder`
/// absent lists every note, `null` lists the unfiled ones, a number lists one
/// folder.
#[tauri::command(async)]
pub fn list_notes(
    backend: State<Backend>,
    args: crate::notes::ListNotesArgs,
) -> Vec<crate::notes::Note> {
    backend
        .history
        .with_connection(move |conn| {
            crate::notes::list_notes(conn, args.folder, args.page).unwrap_or_else(|e| {
                tracing::warn!("notes list failed: {e}");
                Vec::new()
            })
        })
        .unwrap_or_default()
}

/// `limit` may be omitted, in which case it is `notes::SEARCH_LIMIT` — the
/// server owns the default so a caller that forgets cannot ask for everything.
#[tauri::command(async)]
pub fn search_notes(
    backend: State<Backend>,
    query: String,
    limit: Option<u32>,
) -> Vec<crate::notes::Note> {
    let limit = limit.unwrap_or(crate::notes::SEARCH_LIMIT);
    backend
        .history
        .with_connection(move |conn| {
            crate::notes::search_notes(conn, &query, limit).unwrap_or_else(|e| {
                tracing::warn!("notes search failed: {e}");
                Vec::new()
            })
        })
        .unwrap_or_default()
}

#[tauri::command(async)]
pub fn list_folders(backend: State<Backend>) -> Vec<crate::notes::Folder> {
    backend
        .history
        .with_connection(|conn| {
            crate::notes::list_folders(conn).unwrap_or_else(|e| {
                tracing::warn!("folder list failed: {e}");
                Vec::new()
            })
        })
        .unwrap_or_default()
}

#[tauri::command(async)]
pub fn create_folder(backend: State<Backend>, name: String) -> Result<crate::notes::Folder, String> {
    backend
        .history
        .with_connection(move |conn| crate::notes::create_folder(conn, &name).map_err(|e| e.to_string()))
        .unwrap_or_else(|| Err(NOTES_UNAVAILABLE.into()))
}

/// Renaming a folder renames the directory its notes mirror into, so every
/// note in it is re-written: the mirror write path moves each file and sweeps
/// the copy in the old directory, which is then pruned because it is empty.
#[tauri::command(async)]
pub fn rename_folder(
    backend: State<Backend>,
    id: i64,
    name: String,
) -> Result<crate::notes::Folder, String> {
    let root = mirror_root(&backend);
    backend
        .history
        .with_connection(move |conn| {
            crate::notes::mirror::commit_then_mirror(
                conn,
                root.as_deref(),
                |c| crate::notes::rename_folder(c, id, &name).map_err(|e| e.to_string()),
                |c, _| crate::notes::mirror::Job::Write(notes_in_folder(c, id)),
            )
        })
        .unwrap_or_else(|| Err(NOTES_UNAVAILABLE.into()))
}

/// Deletes the folder and every note in it, both or neither, and resolves the
/// ids of the notes that went with it — which is what the mirror unlinks.
/// Local only: there is no sync to roll back.
#[tauri::command(async)]
pub fn delete_folder(backend: State<Backend>, id: i64) -> Result<Vec<i64>, String> {
    let root = mirror_root(&backend);
    backend
        .history
        .with_connection(move |conn| {
            // The directory name has to be read while the row still exists —
            // the write is what removes it. This is the case
            // `commit_then_mirror` documents as "captured in the closure's
            // environment".
            let dir = crate::notes::mirror::folder_name(conn, id);
            crate::notes::mirror::commit_then_mirror(
                conn,
                root.as_deref(),
                |c| crate::notes::delete_folder(c, id).map_err(|e| e.to_string()),
                move |_, cascaded| crate::notes::mirror::Job::Remove {
                    ids: cascaded.clone(),
                    dir,
                },
            )
        })
        .unwrap_or_else(|| Err(NOTES_UNAVAILABLE.into()))
}

/// Every note id filed under `folder_id`. Read failures answer "no notes"
/// rather than an error: this only ever feeds the mirror, and a folder rename
/// must not fail because the file half could not enumerate.
fn notes_in_folder(conn: &rusqlite::Connection, folder_id: i64) -> Vec<i64> {
    let Ok(mut stmt) = conn.prepare("SELECT id FROM notes WHERE folder_id = ?1") else {
        return Vec::new();
    };
    let Ok(rows) = stmt.query_map([folder_id], |r| r.get::<_, i64>(0)) else {
        return Vec::new();
    };
    rows.flatten().collect()
}

// ---------------------------------------------------------------------------
// Note AI actions and auto-title.
//
// The same two rules as the block above — the closure owns its arguments, and
// nothing here holds the settings lock across `with_connection`. Two more
// apply to the pair that call a model:
//
// - They are real `async fn` commands, awaiting the request rather than
//   `block_on`-ing it the way `transforms::run` does. `transforms::run` is
//   called from a hotkey thread that owns no runtime; a command body runs on
//   the async runtime already, where `async_runtime::block_on` would be
//   starting a runtime inside a runtime.
// - The settings/key snapshot is taken and its guards DROPPED before the
//   first `with_connection`, inside `chat_credentials`. Reading the lock in a
//   phase that also touches the database is the deadlock
//   `Recorder::with_connection`'s doc warns about — and a guard held across
//   an `.await` would not be `Send` either.
//
// Nothing here logs note content, a title, or an action prompt.
// ---------------------------------------------------------------------------

/// Every action the menu should draw, shipped and user-made. Seeds the
/// built-ins on first use — see `notes::actions`' module doc for why seeding
/// can only ever insert.
#[tauri::command(async)]
pub fn list_note_actions(
    backend: State<Backend>,
) -> Result<Vec<crate::notes::actions::NoteAction>, String> {
    backend
        .history
        .with_connection(|conn| {
            crate::notes::actions::list_actions(conn).map_err(|e| {
                // A broken notes DB must read as unavailable, not as an empty
                // menu: flattening it to `Vec::new()` shows "No actions yet" as
                // if healthy while every sibling command surfaces the failure.
                tracing::warn!("note action list failed: {e}");
                e.to_string()
            })
        })
        .unwrap_or_else(|| Err(NOTES_UNAVAILABLE.into()))
}

#[tauri::command(async)]
pub fn create_note_action(
    backend: State<Backend>,
    action: crate::notes::actions::NewAction,
) -> Result<crate::notes::actions::NoteAction, String> {
    backend
        .history
        .with_connection(move |conn| {
            crate::notes::actions::create_action(conn, &action).map_err(|e| e.to_string())
        })
        .unwrap_or_else(|| Err(NOTES_UNAVAILABLE.into()))
}

/// Writes the fields `update` names, out of the five `ActionUpdate` has
/// (`position`, `label`, `summary`, `instruction`, `glyph`). Built-ins are
/// editable through this, deliberately; whether an action is shipped, and
/// its key, cannot be named here at all.
#[tauri::command(async)]
pub fn update_note_action(
    backend: State<Backend>,
    id: i64,
    update: crate::notes::actions::ActionUpdate,
) -> Result<bool, String> {
    backend
        .history
        .with_connection(move |conn| {
            crate::notes::actions::update_action(conn, id, &update).map_err(|e| e.to_string())
        })
        .unwrap_or_else(|| Err(NOTES_UNAVAILABLE.into()))
}

/// Refuses a built-in — the first of the two enforcement points, the other
/// being `ActionManager.svelte` not drawing the button.
#[tauri::command(async)]
pub fn delete_note_action(backend: State<Backend>, id: i64) -> Result<bool, String> {
    backend
        .history
        .with_connection(move |conn| {
            crate::notes::actions::delete_action(conn, id).map_err(|e| e.to_string())
        })
        .unwrap_or_else(|| Err(NOTES_UNAVAILABLE.into()))
}

/// Run one action over a note's body and store the result, resolving the note
/// as it was actually written.
///
/// Four phases, in this order and for these reasons: the settings snapshot is
/// taken and both guards dropped before any `with_connection` (lock
/// ordering); the model call sits *between* two database round trips rather
/// than inside one, so the single DB thread is never held for the length of
/// an HTTP request; and the answer is re-read from the database rather than
/// assembled here, so the webview cannot be shown something that was not
/// stored.
///
/// The chat backend comes from `endpoint::resolve_polish_backend`, so an
/// action runs on the custom OpenAI-compatible endpoint when one is
/// configured for polish, and on Sarvam otherwise — the same one configured
/// model per install that Transforms and the voice agent use.
#[tauri::command]
pub async fn run_note_action(
    backend: State<'_, Backend>,
    note_id: i64,
    action_id: i64,
) -> Result<crate::notes::Note, String> {
    let (api_key, polish_model, lane) = chat_credentials(&backend);
    // Read here for the reason `chat_credentials` is a function: the settings
    // guard dies before the first `with_connection` and before the first
    // `.await`.
    let root = mirror_root(&backend);

    // Phase 1 — the note and the action, read together so a run cannot be
    // built from a note and an action observed a moment apart.
    let found = backend
        .history
        .with_connection(move |conn| {
            let action = crate::notes::actions::get_action(conn, action_id)
                .map_err(|e| e.to_string())?
                .ok_or_else(|| "That action no longer exists.".to_string())?;
            let note = crate::notes::get_note(conn, note_id)
                .map_err(|e| e.to_string())?
                .ok_or_else(|| "That note no longer exists.".to_string())?;
            Ok::<_, String>((note.content, action.instruction))
        })
        .unwrap_or_else(|| Err(NOTES_UNAVAILABLE.into()))?;
    let (content, action_prompt) = found;

    // Phase 2 — the model. Every `Err` out of `enhance` is already a sentence
    // safe to show: the transport's own wording, which renders the endpoint
    // URL, is logged there and replaced.
    let chat = note_chat_backend(
        &lane,
        api_key.as_deref(),
        &polish_model,
        "Note actions need Sarvam AI — add your key in Settings.",
    )
    .await?;
    let enhanced = crate::notes::actions::enhance(
        crate::notes::actions::http(),
        &chat,
        &action_prompt,
        &content,
    )
    .await
    .map_err(|e| e.to_string())?;

    // Phase 3 — store it, mirror it, and answer with what was stored. A note
    // deleted while the model was answering is a sentence, not a panic.
    //
    // `commit_then_mirror` rather than a bare `record_run`: an enhancement
    // changes what `notes::body_source` calls the note's body, so a run that
    // skipped the mirror left the `.md` holding the raw dictation while the
    // app showed the enhanced text. The webview adopts the row this returns
    // without a second `update_note`, so this is the only write that can carry
    // the file — and it does it inside the same closure, after the commit, on
    // the one DB thread, exactly as the autosave path does.
    backend
        .history
        .with_connection(move |conn| {
            crate::notes::mirror::commit_then_mirror(
                conn,
                root.as_deref(),
                |c| {
                    crate::notes::actions::record_run(
                        c,
                        note_id,
                        &action_prompt,
                        &enhanced,
                        &content,
                    )
                    .map_err(|e| e.to_string())?;
                    crate::notes::get_note(c, note_id)
                        .map_err(|e| e.to_string())?
                        .ok_or_else(|| "That note no longer exists.".to_string())
                },
                |_, _| crate::notes::mirror::Job::Write(vec![note_id]),
            )
        })
        .unwrap_or_else(|| Err(NOTES_UNAVAILABLE.into()))
}

/// Generate a title for a note from its enhanced body (or its raw one, if it
/// has not been enhanced), store it, and resolve it.
///
/// A separate command from [`run_note_action`], not a step inside it.
/// Titling that rides along with enhancement and swallows its own errors
/// makes a broken title model show as nothing at all while the enhancement
/// still reports success. Here the person asked for a title and either gets
/// one or gets told why not — and a title that fails cannot take an
/// enhancement down with it, because the enhancement was already committed by
/// its own command.
///
/// Because it is an explicit request, there is no guard against replacing an
/// existing title: such a guard is for automatic titling, and one that
/// second-guessed a button press would just be a button that sometimes does
/// nothing.
#[tauri::command]
pub async fn generate_note_title(
    backend: State<'_, Backend>,
    note_id: i64,
) -> Result<String, String> {
    let (api_key, polish_model, lane) = chat_credentials(&backend);
    let root = mirror_root(&backend);

    let source = backend
        .history
        .with_connection(move |conn| {
            let note = crate::notes::get_note(conn, note_id)
                .map_err(|e| e.to_string())?
                .ok_or_else(|| "That note no longer exists.".to_string())?;
            // The enhanced body when there is one: it is the cleaned-up
            // text, and the better source.
            let body = note
                .polished_body
                .filter(|s| !s.trim().is_empty())
                .unwrap_or(note.content);
            // An imported note opens with `import::incomplete_banner` when its
            // transcript came up short, and the title model reads first lines
            // first — so it would title the note after a warning about the
            // recording. Stripped from the copy the model sees only; the
            // stored note keeps the line, which is the one that tells the user.
            Ok::<_, String>(crate::import::without_incomplete_banner(&body).to_string())
        })
        .unwrap_or_else(|| Err(NOTES_UNAVAILABLE.into()))?;

    let chat = note_chat_backend(
        &lane,
        api_key.as_deref(),
        &polish_model,
        "Auto-title needs Sarvam AI — add your key in Settings.",
    )
    .await?;
    let title = crate::notes::title::generate_title(crate::notes::actions::http(), &chat, &source)
        .await
        .map_err(|e| e.to_string())?;

    let stored = title.clone();
    // `commit_then_mirror`, because a title is half of the mirrored file's
    // identity: the name is `<id> <title>.md` and the frontmatter carries
    // `title:`. A bare `update_note` here left the file under its old title
    // with its old header, and the webview adopts the title this returns
    // without a second `update_note` that would have fixed it.
    backend
        .history
        .with_connection(move |conn| {
            crate::notes::mirror::commit_then_mirror(
                conn,
                root.as_deref(),
                |c| {
                    let wrote = crate::notes::update_note(
                        c,
                        note_id,
                        &crate::notes::NoteUpdate {
                            title: Some(stored),
                            ..Default::default()
                        },
                    )
                    .map_err(|e| e.to_string())?;
                    // The note was deleted while the model answered: nothing
                    // was stored, so reporting Ok would claim a title the note
                    // no longer has. `run_note_action` says the same thing on
                    // the same race — and a failed write mirrors nothing, so
                    // the file keeps matching whatever row is still there.
                    if wrote {
                        Ok(())
                    } else {
                        Err("That note no longer exists.".to_string())
                    }
                },
                |_, _| crate::notes::mirror::Job::Write(vec![note_id]),
            )
        })
        .unwrap_or_else(|| Err(NOTES_UNAVAILABLE.into()))?;
    Ok(title)
}

/// The Sarvam key, the configured chat model and the lane this install is on,
/// as owned values.
///
/// A function rather than three inline reads so the guards provably die here,
/// before the caller's first `with_connection` and before its first `.await`:
/// holding either across `with_connection` is the deadlock
/// `Recorder::with_connection`'s doc names (a read guard is as fatal as a
/// write guard), and holding one across an `.await` would make the command's
/// future `!Send`.
fn chat_credentials(backend: &Backend) -> (Option<String>, String, crate::sarvam::Lane) {
    let api_key = backend
        .sarvam_key
        .read()
        .expect("key lock")
        .clone()
        // A blank or absent key is no key. Carrying it as `None` is what lets
        // the resolver take its `NoSarvamKey` arm by type rather than shipping
        // an empty-string bearer that only 401s once the request is out.
        .filter(|k| !k.trim().is_empty());
    let (polish_model, lane) = {
        let s = backend.settings.read().expect("settings lock");
        (
            s.sarvam.polish_model.clone(),
            crate::controller::lane_for(&s),
        )
    };
    (api_key, polish_model, lane)
}

/// The chat backend for a note command, or the sentence to show when there is
/// none.
///
/// Mirrors how `transforms::run` handles the same resolver (`transforms.rs`):
/// a missing Sarvam key names Settings as the fix — never a generic failure —
/// and a rejected custom endpoint surfaces its own reason rather than silently
/// degrading to a host this note was never bound for. The wording for the
/// no-key case is the caller's, because each feature knows which one the user
/// was reaching for.
///
/// `async` since Cloud mode: on that lane the credential is the user's
/// sign-in, which may have to be refreshed before this note action can be
/// sent. The no-key sentence is never reached there — a Cloud install has no
/// key field to be told about — so the sign-in failure carries its own words.
async fn note_chat_backend(
    lane: &crate::sarvam::Lane,
    api_key: Option<&str>,
    polish_model: &str,
    no_key_message: &str,
) -> Result<crate::format::backend::Backend, String> {
    crate::endpoint::chat_backend_for(lane, api_key, polish_model)
        .await
        .map_err(|e| match e {
            crate::endpoint::ChatUnavailable::Backend(
                crate::endpoint::Unavailable::NoSarvamKey,
            ) => no_key_message.to_string(),
            crate::endpoint::ChatUnavailable::Backend(
                crate::endpoint::Unavailable::CustomEndpoint(why),
            ) => why.message().to_string(),
            crate::endpoint::ChatUnavailable::SignIn(sentence) => sentence,
        })
}

/// Take back a correction the app learned by itself: remove the `auto`
/// replacement **and** zero the evidence behind it, both or neither.
///
/// This is the Undo behind the "Learned: x → y" notification. It is a
/// separate command rather than a plain `set_settings` write from the
/// Dictionary page because the settings file is only half of what a
/// promotion wrote — the other half is a row in `learn_candidates`, and
/// removing only the rule would let the next single observation re-promote
/// what the user just rejected. Removing a *manual* rule needs none of this
/// and stays an ordinary settings write.
///
/// Returns whether it committed. `false` means nothing changed at all.
#[tauri::command]
pub fn undo_learned_correction(backend: State<Backend>, from: String, to: String) -> bool {
    backend.learn.undo(&from, &to)
}

/// Mirror the frontend's *resolved* theme (never `auto`) into the sidecar
/// `theme::paint_before_show` reads at the next launch.
///
/// This is the one piece of the theme that has to cross into Rust, and the
/// reason is timing rather than ownership: the preference stays in
/// `localStorage` — that is what lets `src/app.html` stamp it before first
/// paint — but `setup()` runs before any webview exists, so it cannot ask the
/// window what theme it is about to be. See `theme.rs`'s module doc.
///
/// Called from `theme.svelte.ts` on every resolved-theme change, and only
/// from the `main` window: `capabilities/overlay.json` grants the pill no app
/// command at all, so an overlay call would be an ACL rejection rather than a
/// second writer.
///
/// Not `(async)`: this is a `write` of nine bytes to `%APPDATA%`, not a file
/// dialog, so the file-dialog rule does not apply.
#[tauri::command]
pub fn persist_resolved_theme(resolved: String) -> Result<(), String> {
    crate::theme::store(&resolved).map_err(|e| e.to_string())
}

/// Export the current settings to a user-picked JSON file. The Sarvam API
/// key is never included — `Settings` has no field for it; the key lives
/// only in the OS credential store (`sarvam::key`) and `Backend::sarvam_key`,
/// neither of which this serializes. Returns `false` (not an error) when the
/// user cancels the save dialog.
///
/// `(async)`: `blocking_save_file` below is exactly that — blocking — and the
/// tauri-plugin-dialog docs say it must never run on the main thread. Without
/// `(async)` this command dispatches inline on the IPC handler, which *is*
/// the main/event-loop thread, so the whole app (overlay included) would
/// freeze for as long as the user leaves the save dialog open. `(async)`
/// routes the (still-synchronous) body through Tauri's async runtime thread
/// pool instead, matching every `blocking_*` call site the plugin documents.
#[tauri::command(async)]
pub fn export_settings(app: AppHandle, backend: State<Backend>) -> Result<bool, String> {
    use tauri_plugin_dialog::DialogExt;

    let snapshot = backend.settings.read().expect("settings lock").clone();
    let Some(path) = app
        .dialog()
        .file()
        .add_filter("Settings (JSON)", &["json"])
        .set_file_name("butterfly-speak-settings.json")
        .blocking_save_file()
    else {
        return Ok(false);
    };
    let path = path.into_path().map_err(|e| e.to_string())?;
    let json = serde_json::to_string_pretty(&snapshot).map_err(|e| e.to_string())?;
    std::fs::write(&path, json).map_err(|e| e.to_string())?;
    Ok(true)
}

/// Import settings from a user-picked JSON file, through the exact same
/// `migrate`/`repair` pipeline every normal load runs — an exported file from
/// an older build (a retired chat model, a pre-`level` cleanup block) must
/// come back in exactly as rehabilitated as it would on a fresh app launch,
/// not rejected or loaded half-broken. Returns `false` when the user cancels
/// the open dialog.
///
/// Two parts of the file are not taken: the notes mirror folder and its
/// switch (`keep_this_machines_mirror` — the folder is chosen on this
/// machine), and `cloud`, which `set_settings` carries through from the
/// stored settings for every write.
///
/// `(async)`: same reasoning as `export_settings` — `blocking_pick_file` must
/// not run on the main thread, so this dispatches through the async runtime
/// thread pool instead of inline on the IPC handler.
#[tauri::command(async)]
pub fn import_settings(app: AppHandle, backend: State<Backend>) -> Result<bool, String> {
    use tauri_plugin_dialog::DialogExt;

    let Some(path) = app
        .dialog()
        .file()
        .add_filter("Settings (JSON)", &["json"])
        .blocking_pick_file()
    else {
        return Ok(false);
    };
    let path = path.into_path().map_err(|e| e.to_string())?;
    let raw = std::fs::read_to_string(&path).map_err(|e| e.to_string())?;
    let mut value: serde_json::Value =
        serde_json::from_str(&raw).map_err(|e| format!("not valid JSON: {e}"))?;
    settings::migrate(&mut value);
    settings::repair(&mut value);
    let mut imported: Settings =
        serde_json::from_value(value).map_err(|e| format!("not a Butterfly Speak settings file: {e}"))?;
    {
        let current = backend.settings.read().expect("settings lock");
        settings::keep_this_machines_mirror(&current, &mut imported);
    }
    set_settings(app, backend, imported)?;
    Ok(true)
}

// ---------------------------------------------------------------------------
// Audio/video file import.
//
// What is NOT here: a command that takes a file path. The picker below runs
// the dialog in Rust and enqueues what it returns; drag-drop is handled by
// `lib.rs`'s `WindowEvent::DragDrop` arm and enqueues there. So the webview
// never names a file for the backend to read, and there is no path allowlist
// to keep. Adding a `path: String` parameter to any of these would open that
// hole.
// ---------------------------------------------------------------------------

/// Pick recordings to import. Resolves how many were added to the queue —
/// `0` when the user cancels the dialog, which is not an error.
///
/// Nothing starts transcribing: `import_start` is a separate press. Uploading a
/// recording to a paid API is not something to begin because a dialog closed.
///
/// `(async)`: `blocking_pick_files` must not run on the main thread, for the
/// same reason `export_settings` says.
#[tauri::command(async)]
pub fn import_pick_files(app: AppHandle, backend: State<Backend>) -> Result<usize, String> {
    use tauri_plugin_dialog::DialogExt;

    let Some(files) = app
        .dialog()
        .file()
        // Exactly the list `media::probe` will later agree to read, so the
        // dialog cannot offer a format the probe would refuse.
        .add_filter("Audio & video", crate::media::probe::ACCEPTED_EXTENSIONS)
        .blocking_pick_files()
    else {
        return Ok(0);
    };
    let paths: Vec<std::path::PathBuf> = files
        .into_iter()
        .filter_map(|f| f.into_path().ok())
        .collect();
    Ok(backend.import.enqueue(paths))
}

/// What [`import_start`] says when there is no Sarvam key to transcribe with.
///
/// Importing is Sarvam's batch job API, a REST route the relay does not
/// proxy, so it only ever runs on the user's own key. On Bring your own key
/// and On-device the missing piece really is that key; on Cloud it is not a
/// key the user forgot but a lane that cannot import at all, and telling a
/// Cloud user to add a key they were never asked for is the wrong fix.
fn import_without_a_key(lane: &crate::sarvam::Lane) -> &'static str {
    match lane {
        crate::sarvam::Lane::Byok => {
            "Add your Sarvam API key in Settings before importing a recording — importing \
             transcribes in the cloud."
        }
        crate::sarvam::Lane::Cloud { .. } => {
            "Importing a recording needs Bring your own key — switch to it in \
             Settings → Speech engine."
        }
    }
}

/// Start draining the queue.
///
/// Everything the run needs is snapshotted here, once: the API key, the
/// language, the ceilings. The cost: a settings change mid-run is not seen,
/// so a wrong key cannot be fixed without cancelling first.
///
/// LOCK ORDERING: both guards are dropped before `start`, which spawns a task
/// that reaches the history DB thread. Nothing holds the settings lock across
/// that.
#[tauri::command]
pub fn import_start(backend: State<Backend>) -> Result<(), String> {
    let (language_code, mode, lane) = {
        let s = backend.settings.read().expect("settings lock");
        (
            s.sarvam.language_code.clone(),
            s.sarvam.mode.clone(),
            crate::controller::lane_for(&s),
        )
    };
    let api_key = {
        let key = backend.sarvam_key.read().expect("sarvam key lock");
        key.clone()
    };
    let Some(api_key) = api_key.filter(|k| !k.trim().is_empty()) else {
        return Err(import_without_a_key(&lane).into());
    };

    // The mirror root is snapshotted here with the rest of the run's settings,
    // and for the same reason the others are: `LiveSteps` reads nothing back
    // out of `Backend`. `mirror_root` takes and drops the settings guard
    // inside itself, so nothing holds it across `start`.
    let steps = crate::import::LiveSteps::new(
        api_key,
        &language_code,
        mode,
        crate::media::probe::ImportLimits::default(),
        backend.history.clone(),
        mirror_root(&backend),
    );
    backend.import.start(std::sync::Arc::new(steps))
}

/// Stop the run: the loop, the request on the wire, and the UI, all at once.
/// Resolves how many in-flight operations were aborted — `0` when nothing was
/// running, which is not an error.
#[tauri::command]
pub fn import_cancel(backend: State<Backend>) -> usize {
    backend.import.cancel()
}

/// Empty the queue, stopping whatever run it belonged to.
#[tauri::command]
pub fn import_clear(backend: State<Backend>) {
    backend.import.clear();
}

/// The current queue, in the same shape `import://progress` carries — so a
/// page that mounts halfway through a run renders it immediately instead of
/// staying blank until the next state change.
#[tauri::command]
pub fn import_status(backend: State<Backend>) -> crate::events::ImportProgressPayload {
    backend.import.snapshot()
}

// ---------------------------------------------------------------------------
// Notes on disk: export, and the markdown mirror's two controls.
// ---------------------------------------------------------------------------

/// Write one note to a file the user picks, as Markdown or plain text.
///
/// Both formats export the **same document** — `notes::body_source`, the
/// enhanced body when there is one. Reading the raw `content` for `.txt` would
/// silently export the pre-AI draft; `notes::export`'s
/// `both_formats_export_the_same_document` pins that. Returns `false` (not an
/// error) when the user cancels.
///
/// `(async)`: the file-dialog rule — `blocking_save_file` must not run on the
/// main thread, or the whole app freezes for as long as the dialog is open.
/// Same reasoning as `export_settings`.
#[tauri::command(async)]
pub fn export_note(
    app: AppHandle,
    backend: State<Backend>,
    id: i64,
    format: String,
) -> Result<bool, String> {
    use tauri_plugin_dialog::DialogExt;

    let format = crate::notes::export::Format::parse(&format);
    // Fetched before the dialog opens, so a note that has since been deleted
    // fails before the user has picked a filename rather than after.
    let Some(found) = backend
        .history
        .with_connection(move |conn| crate::notes::get_note(conn, id).ok().flatten())
    else {
        return Err(NOTES_UNAVAILABLE.into());
    };
    let Some(note) = found else {
        return Err("That note no longer exists.".into());
    };

    let Some(path) = app
        .dialog()
        .file()
        .add_filter(format.filter_label(), &[format.ext()])
        .set_file_name(crate::notes::export::default_file_name(&note.title, format))
        .blocking_save_file()
    else {
        return Ok(false);
    };
    let path = path.into_path().map_err(|e| e.to_string())?;
    // `io::Error`'s Display is the OS message alone — no path, no content.
    std::fs::write(&path, crate::notes::export::body(&note, format))
        .map_err(|e| format!("Couldn't write that file: {e}"))?;
    Ok(true)
}

/// Ask for the folder the markdown mirror should write into. `None` when the
/// user cancels — the picker's form of `export_settings`' `Ok(false)`.
///
/// It only *answers*; the caller writes the path into settings through the
/// ordinary save path, so choosing a folder and enabling the mirror are one
/// settings write the user can see and undo.
///
/// `(async)`: `blocking_pick_folder` off the main thread, per the file-dialog
/// rule.
#[tauri::command(async)]
pub fn pick_notes_mirror_dir(app: AppHandle) -> Result<Option<String>, String> {
    use tauri_plugin_dialog::DialogExt;

    let Some(dir) = app.dialog().file().blocking_pick_folder() else {
        return Ok(None);
    };
    let path = dir.into_path().map_err(|e| e.to_string())?;
    Ok(Some(path.to_string_lossy().into_owned()))
}

/// Write every note to the mirror, and answer how many landed.
///
/// This is what makes turning the mirror on mean something: without it the
/// switch only covers notes edited afterwards, and a user who enables it over
/// a year of notes sees an almost-empty folder. It is a button rather than a
/// side effect of the switch, because a full rewrite of every note is a
/// thing that should happen when someone asks for it.
///
/// Path-free by construction: the answer is a count.
///
/// `(async)`: it writes one file per note on the DB thread while the caller
/// waits, which must not be the main thread.
#[tauri::command(async)]
pub fn rebuild_notes_mirror(backend: State<Backend>) -> Result<u32, String> {
    let Some(root) = mirror_root(&backend) else {
        return Err("Turn the notes mirror on and choose a folder first.".into());
    };
    backend
        .history
        .with_connection(move |conn| {
            let mut ids = Vec::new();
            for page in 0.. {
                let batch = crate::notes::list_notes(conn, None, page).map_err(|e| {
                    tracing::warn!("notes mirror rebuild could not list notes: {e}");
                    "Couldn't read the notes to rebuild.".to_string()
                })?;
                let last = (batch.len() as u32) < crate::notes::PAGE_SIZE;
                ids.extend(batch.into_iter().map(|n| n.id));
                if last {
                    break;
                }
            }
            let counts = crate::notes::mirror::sync(
                conn,
                &root,
                crate::notes::mirror::Job::Write(ids),
            );
            tracing::info!(
                written = counts.written,
                failed = counts.failed,
                "notes mirror rebuilt"
            );
            Ok(counts.written)
        })
        .unwrap_or_else(|| Err(NOTES_UNAVAILABLE.into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// This file's command code — everything *above* this test module.
    ///
    /// Searching the whole file would make the tripwire below self-satisfying:
    /// rename a listed command and `find` lands on the signature written in
    /// the test's own list, so the "body" from there to EOF contains whatever
    /// the test is looking for and it passes instead of panicking. Cutting at
    /// the module keeps every match on real code and leaves a rename with
    /// nowhere to hide.
    fn command_source() -> &'static str {
        let source = include_str!("commands.rs");
        let tests = source
            .find("\n#[cfg(test)]")
            .expect("commands.rs ends with a test module");
        &source[..tests]
    }

    /// A copy of `body` with comments cut away, for tripwires that must match
    /// on what the code *does* rather than on what it says about itself.
    fn code_only(body: &str) -> String {
        body.lines()
            .map(|line| line.split("//").next().unwrap_or(""))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// One command's source, from its signature to whatever starts the next
    /// top-level item.
    fn command_body<'a>(source: &'a str, signature: &str) -> &'a str {
        let start = source
            .find(signature)
            .unwrap_or_else(|| panic!("{signature} is not in commands.rs"));
        let rest = &source[start + signature.len()..];
        let end = [
            "\n#[tauri::command",
            "\n/// ",
            "\nfn ",
            "\npub fn ",
            "\npub async fn ",
            "\n// ---",
        ]
        .iter()
        .filter_map(|marker| rest.find(marker))
        .min()
        .unwrap_or(rest.len());
        &rest[..end]
    }

    /// `notes::mirror`'s module doc claims `commit_then_mirror` is the only
    /// write sequence any caller uses, and the mirror's ordering guarantee is
    /// only as true as that claim. Two of these commands made it false: both
    /// write a note's row after a model call, and the webview adopts what they
    /// return without a second save — so a bare `update_note` left the
    /// mirrored file holding the previous body, or sitting under the previous
    /// filename, until the user happened to edit the note again.
    ///
    /// Pinned against the source because a `tauri::command` taking
    /// `State<Backend>` is not reachable from a unit test (see
    /// `the_endpoint_key_status_masks_through_the_one_masker` for the same
    /// limit). The behaviour each sequence produces is tested for real in
    /// `notes::mirror::tests`.
    ///
    /// Matched on the call shape, through `code_only`, over `command_source`:
    /// a bare token over the raw body was satisfiable by the prose that now
    /// explains the rule inside `run_note_action` and `generate_note_title`,
    /// so reverting either to a plain `record_run`/`update_note` while keeping
    /// its comment would have stayed green.
    #[test]
    fn every_note_writing_command_goes_through_commit_then_mirror() {
        let source = command_source();
        for signature in [
            "pub fn create_note(",
            "pub fn update_note(",
            "pub fn delete_note(",
            "pub async fn run_note_action(",
            "pub async fn generate_note_title(",
        ] {
            assert!(
                code_only(command_body(source, signature))
                    .contains("crate::notes::mirror::commit_then_mirror("),
                "{signature} writes a note without the mirror sequence"
            );
        }
    }

    /// Enough to recognise which key it is, never enough to use it — and
    /// nothing at all from a key short enough that four characters would be
    /// half of it.
    #[test]
    fn a_masked_key_shows_its_tail_and_nothing_usable() {
        assert_eq!(mask_key("sk-abcdefghijklmnop"), "••••mnop");
        assert_eq!(mask_key("123456789"), "••••6789");
        for short in ["", "a", "12345678"] {
            assert_eq!(mask_key(short), "••••••••", "key {short:?}");
        }
    }

    /// Both key fields on the Settings screen render through the same rule:
    /// two mask shapes on one screen read as two kinds of secret.
    ///
    /// Goes through the real command. Calling `mask_key` on both sides and
    /// comparing the results would prove only that one function returns one
    /// answer: it would pass just as happily with `custom_endpoint_key_status`
    /// wired to a second masker, which is the exact regression this exists to
    /// catch.
    ///
    /// `sarvam_key_status` needs a `State<Backend>` and so is not reachable
    /// from a unit test; its half is pinned by the literal below being the
    /// documented Sarvam shape, and by `mask_key` having only these two
    /// callers.
    #[test]
    fn the_endpoint_key_status_masks_through_the_one_masker() {
        const KEY: &str = "sk-abcdefghijklmnop";

        // The slot is process-wide, so put back whatever was there. Nothing
        // else in the suite writes it (only `endpoint::init` and this test
        // ever do), and its default is "no key".
        let restore = crate::endpoint::slot().api_key;
        crate::endpoint::set_key(Some(KEY.into()));
        let status = custom_endpoint_key_status();
        crate::endpoint::set_key(restore);

        assert!(status.present);
        assert_eq!(
            status.masked.as_deref(),
            Some("••••mnop"),
            "the endpoint key must render in the Sarvam key's shape, not its own"
        );
        assert_eq!(status.masked, Some(mask_key(KEY)));
    }

    /// The mask counts characters, not bytes: slicing a multibyte key by byte
    /// offsets would panic on a character boundary.
    #[test]
    fn masking_a_multibyte_key_does_not_split_a_character() {
        assert_eq!(mask_key("अआइईउऊऋएऐओऔ"), "••••एऐओऔ");
    }

    /// The Settings panel prints this value verbatim as "Requests go to …",
    /// so it has to be the route rather than a base the webview concatenates
    /// onto. Both halves of that are load-bearing: `/v1` doubles on a base
    /// that already ends in it, and an Azure/gateway paste's query string
    /// ends up in the middle of the path. Neither survives `${base}/v1/…`
    /// in JavaScript, which is what this used to return.
    #[test]
    fn the_endpoint_check_returns_the_route_that_will_be_requested() {
        for input in [
            "http://localhost:11434",
            "http://localhost:11434/",
            "http://localhost:11434/v1",
            "http://localhost:11434/v1/chat/completions",
        ] {
            assert_eq!(
                check_custom_endpoint(input.into()),
                Ok("http://localhost:11434/v1/chat/completions".into()),
                "input {input}"
            );
        }
        assert_eq!(
            check_custom_endpoint("https://h/v1?api-version=2025-01-01-preview".into()),
            Ok("https://h/v1/chat/completions?api-version=2025-01-01-preview".into())
        );
    }

    /// The other half of the same command: the sentence the panel shows
    /// inline, and the one it derives "Falling back to Sarvam: …" from.
    #[test]
    fn the_endpoint_check_reports_an_unusable_url_as_prose() {
        for url in ["", "localhost:11434", "http://api.example.com/v1"] {
            let err = check_custom_endpoint(url.into())
                .expect_err("an unusable URL must not resolve to a route");
            assert!(!err.is_empty(), "url {url:?} produced an empty message");
        }
    }

    /// Import cannot run on Cloud — the relay does not proxy Sarvam's batch
    /// job API — so a Cloud user is told which lane can import and where to
    /// switch, and is never asked for a key or told about Sarvam (the same
    /// rule `a_cloud_dictation_is_never_told_about_sarvam` holds the realtime
    /// path to). The other two engines keep their sentence byte for byte.
    ///
    /// Resolved through `lane_for`, as the command does, so the choice is
    /// pinned per engine the user can pick rather than per lane.
    #[test]
    fn a_cloud_import_is_told_to_switch_lanes_not_to_add_a_key() {
        let on = |provider| {
            import_without_a_key(&crate::controller::lane_for(&settings::Settings {
                provider,
                ..settings::Settings::default()
            }))
        };

        let unchanged = "Add your Sarvam API key in Settings before importing a recording — \
                         importing transcribes in the cloud.";
        assert_eq!(on(settings::Provider::Sarvam), unchanged);
        assert_eq!(on(settings::Provider::Local), unchanged);

        let cloud = on(settings::Provider::Cloud);
        assert_eq!(
            cloud,
            "Importing a recording needs Bring your own key — switch to it in \
             Settings → Speech engine."
        );
        let lower = cloud.to_lowercase();
        assert!(!lower.contains("sarvam"), "a Cloud user was told about Sarvam: {cloud}");
        assert!(!lower.contains("add your"), "a Cloud user was asked for a key: {cloud}");
    }

    /// The status the webview sees carries presence and a mask, never the
    /// key — the whole reason it is not a `Settings` field.
    #[test]
    fn the_key_status_never_carries_the_key() {
        let status = CustomEndpointKeyStatus {
            present: true,
            masked: Some(mask_key("sk-abcdefghijklmnop")),
        };
        let json = serde_json::to_string(&status).expect("status serializes");
        assert!(!json.contains("abcdefghij"), "{json}");
        assert_eq!(json, r#"{"present":true,"masked":"••••mnop"}"#);
    }

    fn on(provider: settings::Provider, model: &str) -> Settings {
        let mut s = Settings {
            provider,
            ..Settings::default()
        };
        s.model.selected_id = model.into();
        s
    }

    /// The on-device model is in memory only while Local is selected: moving
    /// away frees it, and changing the model under another provider loads
    /// nothing.
    #[test]
    fn the_on_device_model_follows_the_provider() {
        use settings::Provider::{Cloud, Local, Sarvam};
        let change = |old, new| model_change(&old, &new, false);
        assert_eq!(change(on(Local, "a"), on(Sarvam, "a")), ModelChange::Unload);
        assert_eq!(change(on(Local, "a"), on(Cloud, "a")), ModelChange::Unload);
        assert_eq!(change(on(Cloud, "a"), on(Local, "a")), ModelChange::Load);
        assert_eq!(change(on(Local, "a"), on(Local, "b")), ModelChange::Load);
        assert_eq!(change(on(Local, "a"), on(Local, "a")), ModelChange::None);
        assert_eq!(change(on(Sarvam, "a"), on(Sarvam, "b")), ModelChange::None);
        assert_eq!(change(on(Sarvam, "a"), on(Cloud, "a")), ModelChange::None);
    }

    /// A switch away from Local during a local dictation keeps the model: the
    /// recording still has to be transcribed. The controller unloads it at
    /// the next Idle instead.
    #[test]
    fn a_switch_during_a_dictation_leaves_the_model_for_it() {
        use settings::Provider::{Cloud, Local};
        assert_eq!(model_change(&on(Local, "a"), &on(Cloud, "a"), true), ModelChange::None);
        assert_eq!(model_change(&on(Cloud, "a"), &on(Local, "a"), true), ModelChange::Load);
        assert!(crate::controller::unloads_at_idle(Cloud));
        assert!(!crate::controller::unloads_at_idle(Local));
    }

    /// The download that finishes a model loads it only on the local
    /// provider, the same rule a settings write follows.
    #[test]
    fn a_finished_download_loads_only_on_the_local_provider() {
        let body = code_only(command_body(command_source(), "pub fn download_model("));
        assert!(body.contains("local && selected == job_id"), "{body}");
    }

    /// The three wirings that keep a page, or a file it imports, from moving
    /// the sign-in token, the audio, the notes or the endpoint key somewhere
    /// the user did not choose. Each helper is tested on its own; these pin
    /// that the command still calls it, and in the right place.
    #[test]
    fn a_page_write_goes_through_carry_stored_fields_before_it_is_saved() {
        let body = code_only(command_body(command_source(), "pub fn set_settings("));
        let carry = body
            .find("settings::carry_stored_fields(&old, &mut settings)")
            .expect("set_settings no longer carries the stored fields");
        let save = body.find("settings::save(&settings)").expect("set_settings saves");
        assert!(carry < save, "carried after the save: {body}");
    }

    #[test]
    fn an_import_keeps_this_machines_mirror_and_goes_through_set_settings() {
        let body = code_only(command_body(command_source(), "pub fn import_settings("));
        let keep = body
            .find("settings::keep_this_machines_mirror(")
            .expect("import_settings takes the file's mirror folder");
        let set = body.find("set_settings(app, backend, imported)").expect("imports through set_settings");
        assert!(keep < set, "{body}");
    }

    #[test]
    fn a_probe_sends_the_key_only_through_key_for_probe() {
        let body = code_only(command_body(command_source(), "async fn run_probe("));
        assert!(body.contains("crate::endpoint::key_for_probe("), "{body}");
        assert!(!body.contains(".api_key"), "the stored key read around the check: {body}");
    }
}
