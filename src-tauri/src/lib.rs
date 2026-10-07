mod asr;
mod audio;
mod auth;
mod autostart;
// Canonical equivalence, shared by the two halves that would otherwise drift:
// `learn` compares words against stored rules, `cleanup` hunts for those rules
// inside a transcript. Both have to agree on when two spellings are the same
// word, so the answer lives in one place.
mod canonical;
mod cleanup;
mod commands;
mod controller;
mod crash_recovery;
// The one custom OpenAI-compatible endpoint slot. Deliberately outside the
// `eval`/`format`/`cleanup`/`sarvam::chat` set that `bin/fmtbench.rs`
// compiles into itself via `#[path]`: nothing in those trees may name it, or
// the benchmark binary stops building (see `endpoint`'s module doc).
mod endpoint;
mod eval;
mod events;
mod format;
mod foreground;
mod history;
mod hotkeys;
// Audio/video file import. The queue lives in the backend rather than in a
// Svelte store so a run survives the user navigating away — and so no command
// on this path ever takes a file path from the webview; see its module doc.
mod import;
mod injection;
// Private: the field monitor calls `diff::corrections_in` and hands the
// result to `candidates::Guard::observe`. Both halves have real callers inside
// this crate and nothing needs to reach them from outside it.
mod learn;
mod logs;
mod media;
mod models;
// Local notes + folders. Its tables live in the history database and it comes
// through `Recorder::with_connection`, the same door `learn::candidates` uses;
// see the module doc for why, and for the two obligations that come with it.
mod notes;
mod overlay;
mod routes;
mod sarvam;
mod settings;
mod speech_gate;
mod state;
mod system_events;
// The window's pre-paint fill colour, and the sidecar the frontend mirrors its
// resolved theme into so `setup()` can pick one. Not part of `settings`: it is
// a launch hint written after the fact, never a setting, and it must not reach
// the exported settings JSON.
mod theme;
mod tones;
mod transforms;
mod tray;
mod uia;
// The self-updater. Rust-only by design: no window holds an `updater:*`
// permission, so the check, the download and the hand-off cannot be started
// from a page — see its module doc for the policy that buys.
mod updater;

use commands::Backend;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, RwLock};
use tauri::Manager;

/// `pub(crate)` so `settings`' uninstall-hook test can hold it to the folder
/// `src-tauri/windows/hooks.nsh` removes.
pub(crate) fn logs_dir() -> PathBuf {
    PathBuf::from(std::env::var("LOCALAPPDATA").expect("LOCALAPPDATA not set"))
        .join("ButterflySpeak")
        .join("logs")
}

/// The log line for a panic: the thread and the source location, never the
/// panic's message, which can quote whatever the panicking code was holding
/// (a transcript, a note, a URL with a credential in it).
fn panic_line(thread: Option<&str>, location: Option<(&str, u32)>) -> String {
    let thread = thread.unwrap_or("an unnamed thread");
    match location {
        Some((file, line)) => format!("panic on {thread} at {file}:{line}"),
        None => format!("panic on {thread}"),
    }
}

/// A release build has no console, so without this a panic on the
/// controller, audio, hook or UI Automation threads would leave no trace in
/// the log at all. The default hook still runs after it, for a debug build's
/// console.
fn log_panics() {
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let current = std::thread::current();
        tracing::error!(
            "{}",
            panic_line(current.name(), info.location().map(|l| (l.file(), l.line())))
        );
        default(info);
    }));
}

/// Returns the log writer's flush guard. `run` holds it until the app exits
/// and drops it there, which writes out the lines still buffered.
fn init_logging() -> tracing_appender::non_blocking::WorkerGuard {
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;

    // `tauri_plugin_updater=off`, and off rather than a level floor because a
    // floor would silence neither half of the problem. Every check failure the
    // plugin logs at `error` is one `updater::check_for_update` already logs itself, with
    // that same error's Display text, and already shows the user as prose —
    // so against an endpoint that is still a placeholder each check writes the
    // same failure to the file twice. Underneath that the plugin logs the
    // whole update manifest at `debug` (its `updater.rs:537`), which this
    // crate's log rule says must never reach the file at all. `RUST_LOG`
    // replaces this string wholesale, so anyone who wants the plugin's own
    // view — the HTTP status behind a non-2xx manifest fetch is the one
    // detail only it prints — asks for it on purpose.
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| "info,butterfly_speak_lib=debug,tauri_plugin_updater=off".into());

    // Logs are kept about a week: files past `logs::MAX_AGE` go now, and the
    // appender holds at most `logs::FILES_KEPT` while the app runs.
    let dir = logs_dir();
    logs::prune_expired(&dir, std::time::SystemTime::now());
    let file_appender = logs::appender(&dir).expect("initializing rolling file appender failed");
    let (file_writer, guard) = tracing_appender::non_blocking(file_appender);

    tracing_subscriber::registry()
        .with(filter)
        .with(tracing_subscriber::fmt::layer())
        .with(
            tracing_subscriber::fmt::layer()
                .with_ansi(false)
                .with_writer(file_writer),
        )
        .init();
    log_panics();
    guard
}

pub fn run() {
    let mut log_guard = Some(init_logging());

    tauri::Builder::default()
        // First, per the plugin's own README — and with its `deep-link`
        // feature on, which is what makes the callback below reachable at all:
        // Windows answers a `butterflylabs://` link by launching a *second*
        // copy of this exe with the URL as its only argument, and this plugin
        // kills that copy. With the feature it hands the dead instance's argv
        // to the deep-link plugin first (`Builder::callback`,
        // single-instance 2.4.2), which emits `deep-link://new-url` here;
        // without it the sign-in would silently never come back.
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            tray::show_main(app);
        }))
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            Some(vec![autostart::TRAY_START_ARG]),
        ))
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_deep_link::init())
        .setup(|app| {
            // First, though not for the reason it looks like: a cold start
            // cannot reach this listener at all. The deep-link plugin parses
            // the launch argv in its *own* `setup`, which Tauri runs before
            // this one, so a URL that started the process has already been
            // emitted by the time this line runs and survives only in
            // `get_current()` — which nothing reads, because the PKCE verifier
            // that code needs died with the process that started the sign-in.
            // The path this does catch is the live one: a second instance's
            // argv, forwarded by single-instance long after setup. It goes
            // first anyway, so nothing slow below can sit in front of it.
            auth::commands::watch_deep_links(app.handle());
            // A dev build was never run through the installer, so nothing has
            // claimed `butterflylabs://` in the registry. `register_all` writes
            // the same keys the NSIS installer does, pointing at *this* exe —
            // which is why it is debug-only: in a release build it would point
            // a machine-wide scheme at whatever copy of the app happened to
            // start last.
            #[cfg(debug_assertions)]
            {
                use tauri_plugin_deep_link::DeepLinkExt;
                app.deep_link().register_all()?;
            }

            let backend = start_backend(app.handle().clone());
            let binding = backend.settings.read().expect("settings lock").hotkey.binding.clone();
            tray::setup(app, backend.paused.clone(), backend.control.clone(), &binding)?;
            overlay::harden(app.handle());
            app.manage(backend);
            // After `manage`, never before: the scheduler reads
            // `Backend.settings` at fire time to decide whether to check.
            updater::start(app.handle());
            // Deliberately NO downloads here: models are fetched only when
            // the user explicitly clicks Download in the app.

            if let Some(overlay) = app.get_webview_window("overlay") {
                crash_recovery::watch(&overlay);
            }
            if let Some(main) = app.get_webview_window("main") {
                crash_recovery::watch(&main);
                // Before `show()`, and only for `main`: this sets the fill the
                // window is painted with in the gap before the webview has a
                // frame, which was WebView2's default white regardless of
                // theme. The overlay is `transparent: true` and never gets a
                // fill — see `theme::paint_before_show`.
                theme::paint_before_show(&main);
                // Autostart passes `--hidden` (see `autostart::TRAY_START_ARG`,
                // wired into the plugin above) — a launch carrying it starts
                // straight to the tray instead of flashing the main window
                // open. The window's own config ships `visible: false` so
                // there is nothing to hide; this is the only place that
                // shows it for an ordinary launch.
                if !std::env::args().any(|a| a == autostart::TRAY_START_ARG) {
                    let _ = main.show();
                }
            }
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::get_settings,
            commands::set_settings,
            commands::list_models,
            commands::polish_status,
            commands::download_model,
            commands::cancel_download,
            commands::delete_model,
            commands::select_model,
            commands::list_mics,
            commands::system_info,
            commands::set_meter,
            commands::hotkey_capture,
            commands::ensure_support_models,
            commands::sarvam_key_status,
            commands::set_sarvam_key,
            commands::validate_sarvam_key,
            commands::custom_endpoint_key_status,
            commands::set_custom_endpoint_key,
            commands::check_custom_endpoint,
            commands::prompt_defaults,
            commands::preview_prompt,
            commands::test_prompt,
            commands::endpoint_test_connection,
            commands::endpoint_list_models,
            commands::history_list,
            commands::history_search,
            commands::history_delete,
            commands::history_update_text,
            commands::history_clear,
            commands::create_note,
            commands::get_note,
            commands::update_note,
            commands::delete_note,
            commands::list_notes,
            commands::search_notes,
            commands::list_folders,
            commands::create_folder,
            commands::rename_folder,
            commands::delete_folder,
            commands::list_note_actions,
            commands::create_note_action,
            commands::update_note_action,
            commands::delete_note_action,
            commands::run_note_action,
            commands::generate_note_title,
            commands::export_settings,
            commands::import_settings,
            commands::undo_learned_correction,
            commands::export_note,
            commands::pick_notes_mirror_dir,
            commands::rebuild_notes_mirror,
            commands::persist_resolved_theme,
            commands::import_pick_files,
            commands::import_start,
            commands::import_cancel,
            commands::import_clear,
            commands::import_status,
            updater::update_status,
            updater::update_check,
            updater::update_install,
            auth::commands::cloud_sign_in,
            auth::commands::cloud_sign_out,
            auth::commands::cloud_status,
            auth::commands::cloud_usage,
            auth::commands::cloud_delete_account,
        ])
        .on_window_event(|window, event| {
            // Closing the main window hides to tray; the app keeps running.
            if window.label() == "main" {
                if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                    api.prevent_close();
                    let _ = window.hide();
                    commands::end_hotkey_capture(window.app_handle());
                    notify_still_running(window.app_handle());
                }
                if let tauri::WindowEvent::DragDrop(drag) = event {
                    on_drag_drop(window.app_handle(), drag);
                }
            }
        })
        // Every load of the main page, the first one included: a page that
        // is loading again (a crash-recovery reload, a manual one) holds no
        // recorder, so none may stay on in the hook.
        .on_page_load(|webview, payload| {
            if webview.label() == "main"
                && matches!(payload.event(), tauri::webview::PageLoadEvent::Started)
            {
                commands::end_hotkey_capture(webview.app_handle());
            }
        })
        .build(tauri::generate_context!())
        .expect("error while building tauri application")
        .run(move |_app, event| {
            if let tauri::RunEvent::Exit = event {
                // Writes out the log lines still buffered.
                drop(log_guard.take());
            }
        });
}

/// Files dragged onto the main window go straight into the import queue.
///
/// A page that receives a dropped file's path, and hands it back to the
/// backend to read, needs an allowlist on every path-consuming handler so a
/// forged path cannot be read. Here the OS hands the paths to Rust and
/// nothing forwards them: the webview learns only that a drag is happening
/// (`IMPORT_DROP_HOVER`) and, afterwards, the file *names* already in the
/// queue. No `#[tauri::command]` on this path accepts a path at all, so there
/// is no allowlist to keep and nothing to forge.
///
/// This requires `dragDropEnabled` on the main window, which is Tauri's
/// default and is stated explicitly in `tauri.conf.json` so it cannot be
/// turned off by accident — with it off, the OS event never arrives and the
/// only report would be "drag and drop silently does nothing".
fn on_drag_drop(app: &tauri::AppHandle, event: &tauri::DragDropEvent) {
    use tauri::Emitter;

    let hover = |over: bool| {
        let _ = app.emit(
            events::IMPORT_DROP_HOVER,
            events::ImportDropHoverPayload { over },
        );
    };
    match event {
        tauri::DragDropEvent::Enter { .. } | tauri::DragDropEvent::Over { .. } => hover(true),
        tauri::DragDropEvent::Leave => hover(false),
        tauri::DragDropEvent::Drop { paths, .. } => {
            hover(false);
            // Counts only — a dropped path is never logged.
            tracing::info!(dropped = paths.len(), "files dropped on the main window");
            app.state::<Backend>().import.enqueue(paths.clone());
            // A drop is accepted wherever the user is standing, and every
            // dropped file becomes a row on the Import page (a refused one as
            // a failed row that says why), so the window goes there. "import"
            // has to match its entry in `NAV_MAIN` (`+page.svelte`): the
            // renderer ignores a page it cannot find.
            if !paths.is_empty() {
                let _ = app.emit(events::NAVIGATE, events::NavigatePayload { page: "import" });
            }
        }
        // `DragDropEvent` is `#[non_exhaustive]`; a variant added later is not
        // something to guess at.
        _ => {}
    }
}

/// Toast on the first window close: the app keeps running in the tray and
/// dictation stays available. Once is enough — a marker file (rather than a
/// settings field) keeps this out of the settings the UI round-trips.
fn notify_still_running(app: &tauri::AppHandle) {
    use tauri_plugin_notification::NotificationExt;

    let marker = settings::config_dir().join("background-notice-shown");
    if marker.exists() {
        return;
    }
    if let Err(e) = std::fs::create_dir_all(settings::config_dir())
        .and_then(|_| std::fs::write(&marker, b""))
    {
        tracing::warn!("couldn't persist background-notice marker: {e}");
    }

    let binding = app
        .state::<Backend>()
        .settings
        .read()
        .expect("settings lock")
        .hotkey
        .binding
        .clone();
    if let Err(e) = app
        .notification()
        .builder()
        .title("Butterfly Speak is still running")
        .body(format!(
            "Dictation stays active in the background — hold {binding} to dictate in any app. Quit from the tray icon."
        ))
        .show()
    {
        tracing::warn!("background notice failed: {e}");
    }
}

fn start_backend(app: tauri::AppHandle) -> Backend {
    let loaded = settings::load();

    let (ctl_tx, ctl_rx) = crossbeam_channel::unbounded::<state::ControlMsg>();
    let (asr_tx, asr_rx) = crossbeam_channel::unbounded::<asr::offline::AsrMsg>();
    let (audio_cmd_tx, audio_cmd_rx) = crossbeam_channel::unbounded::<audio::AudioCmd>();

    let suppress = Arc::new(AtomicBool::new(false));
    let paused = Arc::new(AtomicBool::new(false));
    let capture = Arc::new(AtomicBool::new(false));
    // The controller writes it on every state transition; the updater reads it
    // before it hands the installer the bytes (`updater::install_blocker`).
    let dictation_busy = Arc::new(AtomicBool::new(false));
    // Held by the transform thread for its whole run; read by the updater for
    // the same reason as `dictation_busy`.
    let transform_busy = Arc::new(AtomicBool::new(false));
    let session_lock_reset = Arc::new(AtomicBool::new(false));
    let binding = Arc::new(RwLock::new(hotkeys::parse_binding(&loaded.hotkey.binding)));
    let transform_bindings = Arc::new(RwLock::new(hotkeys::transform_bindings(&loaded)));
    let app_shortcut_bindings = Arc::new(RwLock::new(hotkeys::app_shortcut_bindings(&loaded)));
    let route_chord_bindings = Arc::new(RwLock::new(hotkeys::route_chord_bindings(&loaded)));
    let cleanup_shared = Arc::new(RwLock::new(cleanup::CleanupSettings::from(&loaded)));
    let settings_shared = Arc::new(RwLock::new(loaded.clone()));

    let cat = models::catalog::catalog();
    let punct_dir = models::downloader::model_dir(&cat.punctuation.dir_name)
        .to_string_lossy()
        .into_owned();

    // Only load a local model into RAM when the local provider is active;
    // switching to it later triggers a load via set_settings.
    let initial = if loaded.provider == settings::Provider::Local {
        let spec = models::catalog::entry(&loaded.model.selected_id)
            .filter(|e| models::downloader::dir_installed(&e.dir_name))
            .map(commands::spec_for);
        if spec.is_none() {
            tracing::warn!(
                "selected model {} not installed; dictation disabled until a model is downloaded",
                loaded.model.selected_id
            );
        }
        spec
    } else {
        None
    };

    asr::offline::spawn(
        app.clone(),
        initial,
        punct_dir,
        cleanup_shared.clone(),
        asr_rx,
        ctl_tx.clone(),
    );
    let gate = audio::spawn(ctl_tx.clone(), audio_cmd_rx);
    hotkeys::spawn(
        app.clone(),
        ctl_tx.clone(),
        hotkeys::HookShared {
            binding: binding.clone(),
            transforms: transform_bindings.clone(),
            app_shortcuts: app_shortcut_bindings.clone(),
            route_chords: route_chord_bindings.clone(),
            suppress: suppress.clone(),
            paused: paused.clone(),
            capture: capture.clone(),
            session_lock_reset: session_lock_reset.clone(),
        },
    );
    system_events::spawn(session_lock_reset, ctl_tx.clone());

    // Restore audio device preference.
    if loaded.audio.device_name.is_some() {
        let _ = audio_cmd_tx.send(audio::AudioCmd::SetDevice(loaded.audio.device_name.clone()));
    }

    // The custom endpoint's settings half comes from settings.json, its key
    // half from the credential store — same split as the Sarvam key below,
    // and the reason neither one is a `Settings` field.
    endpoint::init(&loaded);

    let sarvam_key: sarvam::SharedKey =
        Arc::new(RwLock::new(sarvam::key::load(sarvam::key::KeySlot::Sarvam)));
    let cloud_tx = sarvam::spawn(ctl_tx.clone(), sarvam_key.clone(), cleanup_shared.clone());
    // The third transcriber. Spawned unconditionally like the other two: it
    // costs one idle task, and the alternative is deciding at startup a thing
    // the user can change from the Settings screen a second later.
    let custom_stt_tx = asr::custom::spawn(
        ctl_tx.clone(),
        // For the polish half only — the transcription request itself
        // authenticates with the endpoint's own key, out of the slot.
        sarvam_key.clone(),
        cleanup_shared.clone(),
    );

    let history_recorder = history::spawn(
        settings::config_dir().join("history.db"),
        history::RetentionCfg {
            enabled: loaded.history.enabled,
            keep_days: loaded.history.keep_days,
        },
    );

    // The frequency guard behind auto-learn. Built here rather than lazily so
    // there is exactly one of it: the candidate counts and the settings write
    // both have to go through the same handle or two of them could promote
    // the same pair twice.
    let learn_guard = learn::candidates::Guard::new(
        history_recorder.clone(),
        settings_shared.clone(),
        cleanup_shared.clone(),
        app.clone(),
    );

    // Taken before `controller::spawn` consumes `app`.
    let app_for_import = app.clone();
    // Converted audio from an import the app was killed in. Off this thread:
    // %TEMP% can hold many thousands of files.
    std::thread::spawn(|| {
        let n = import::sweep_stale_scratch(&std::env::temp_dir(), std::time::SystemTime::now());
        if n > 0 {
            tracing::info!("removed {n} converted import(s) left behind by an earlier run");
        }
    });
    // Before `Backend` exists, so no download can be running yet.
    models::downloader::sweep_leftover_unpacks();
    // History's delete and clear tell the controller (see
    // `ControlMsg::HistoryRemoved`); `controller::spawn` takes the original.
    let control_for_commands = ctl_tx.clone();

    controller::spawn(
        app,
        ctl_rx,
        ctl_tx,
        asr_tx.clone(),
        cloud_tx,
        custom_stt_tx,
        gate,
        suppress.clone(),
        settings_shared.clone(),
        sarvam_key.clone(),
        // The controller writes; the Tauri commands read. Both hold their own
        // clone of the same channel handle — see `history::Recorder`.
        history_recorder.clone(),
        // The same guard the commands hold, not a second one: the controller
        // is where observations are produced (the field monitor) and
        // `Backend` is where Undo consumes them, and both have to count
        // against the same candidate rows.
        learn_guard.clone(),
        // The controller is the only writer; `Backend` hands the updater the
        // read end of the same flag.
        dictation_busy.clone(),
        transform_busy.clone(),
    );

    Backend {
        settings: settings_shared,
        cleanup: cleanup_shared,
        asr_tx,
        audio_cmd: audio_cmd_tx,
        binding,
        transform_bindings,
        app_shortcut_bindings,
        route_chord_bindings,
        capture,
        paused,
        dictation_busy,
        transform_busy,
        downloads: models::DownloadRegistry::default(),
        sarvam_key,
        history: history_recorder,
        control: control_for_commands,
        learn: learn_guard,
        // Built here rather than lazily so there is exactly one queue: the
        // picker command, the drag-drop handler and the Cancel button all have
        // to reach the same run id, or a cancel would miss the drain it meant.
        import: import::ImportQueue::new(Box::new(import::AppSink(app_for_import))),
    }
}

/// The per-window capability split, asserted rather than trusted.
///
/// Three files have to agree for the overlay to stay unable to reach a command:
/// the macro above (what is registered), `build.rs`'s `COMMANDS` (what gets an
/// `allow-<command>` permission generated, and therefore whether the ACL runs on
/// app commands at all), and `capabilities/*.json` (who may use them). None of
/// those is checked against the others by the compiler — a command added to the
/// macro but not to `build.rs` is simply un-grantable, and one added to
/// `build.rs` but not to `capabilities/main.json` fails at runtime with
/// "not allowed by ACL" the first time the settings page calls it. So the
/// agreement is read back out of the files here.
///
/// The scraping is deliberate. `generate_handler!` takes bare paths, so the list
/// cannot be lifted into a `const` that both this crate and `build.rs` share;
/// reading the source text back is the only way to compare what is *registered*
/// against what is *permitted*.
#[cfg(test)]
mod capability_split {
    // What `capabilities/main.json` grants besides the app commands, and why:
    // event listen/unlisten/emit (every page listens, the shortcuts dialog
    // emits); minimise, toggle-maximize and close for the title bar's
    // buttons, plus start-dragging and internal-toggle-maximize for its drag
    // region; internal-toggle-devtools for the Ctrl+Shift+I script Tauri adds
    // to debug builds; and open-url scoped to the Sarvam dashboard.
    //
    // No window holds a dialog, autostart, updater or deep-link permission:
    // those plugins are driven from Rust, which the ACL does not gate. That
    // does not hide the sign-in callback from a page, though. The deep-link
    // plugin broadcasts it as an event, and `allow-listen` has no event
    // scope, so a page could read the code. PKCE is what protects it: the
    // verifier is minted in Rust, never leaves the process, and the Rust
    // listener spends the code as soon as it arrives.

    use std::collections::BTreeSet;
    use std::path::PathBuf;

    /// One capability file, reduced to what decides which window a grant reaches.
    struct Capability {
        file: String,
        windows: Vec<String>,
        webviews: Vec<String>,
        permissions: Vec<String>,
    }

    /// Permissions with no `plugin:` prefix are the app's own — an unprefixed
    /// identifier resolves against `APP_ACL_KEY`
    /// (`tauri-build-2.6.3/src/acl.rs:356`). That is the thing the overlay must
    /// never name.
    fn is_app_permission(identifier: &str) -> bool {
        !identifier.contains(':')
    }

    /// The metacharacters `glob::Pattern` treats as a pattern rather than text.
    fn is_glob(pattern: &str) -> bool {
        pattern.contains(['*', '?', '['])
    }

    /// Whether `cap` can reach the window labelled `label`.
    ///
    /// Deliberately conservative, because the obvious `== label` is wrong. Tauri
    /// documents both targeting keys as globs — "List of windows that are
    /// affected by this capability. Can be a glob pattern"
    /// (`tauri-utils-2.9.3/src/acl/capability.rs:150`), and the same for
    /// `webviews` (`:166`) — and compiles them with `glob::Pattern::new`
    /// (`resolved.rs:199-204`, used at `:241`). A capability declaring
    /// `"windows": ["*"]` therefore reaches the overlay while never naming it,
    /// and a `webviews` entry grants "regardless of whether the webview's window
    /// label matches a pattern in windows". So anything this cannot prove to be
    /// a literal mismatch counts as a hit.
    fn covers(cap: &Capability, label: &str) -> bool {
        !cap.webviews.is_empty() || cap.windows.iter().any(|w| w == label || is_glob(w))
    }

    /// Why `windows`/`webviews` do not target exactly one literally-named
    /// window, or `None` when they do.
    ///
    /// Split out of the file walk so a synthetic capability can be pushed
    /// through the identical check — see `glob_and_webview_targeting_are_rejected`.
    fn targeting_fault(windows: &[String], webviews: &[String]) -> Option<String> {
        if !webviews.is_empty() {
            return Some(format!(
                "targets webviews {webviews:?}; that key grants regardless of the \
                 windows list, so the split would have to be read in two places"
            ));
        }
        match windows {
            [one] if is_glob(one) => Some(format!(
                "targets the glob {one:?}; Tauri matches window labels as glob \
                 patterns, so this covers windows it never names"
            )),
            [one] if !matches!(one.as_str(), "main" | "overlay") => {
                Some(format!("targets the unknown window {one:?}"))
            }
            [_] => None,
            _ => Some(format!(
                "targets {} windows {windows:?}; one file per window is what keeps \
                 a grant from leaking sideways",
                windows.len()
            )),
        }
    }

    fn read(relative: &str) -> String {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(relative);
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
    }

    /// The text between `open` and the next `close`, exclusive.
    fn between<'a>(haystack: &'a str, open: &str, close: &str) -> &'a str {
        let (_, rest) = haystack
            .split_once(open)
            .unwrap_or_else(|| panic!("expected {open:?} in the source"));
        let (body, _) = rest
            .split_once(close)
            .unwrap_or_else(|| panic!("expected {close:?} to close {open:?}"));
        body
    }

    /// Every command `generate_handler!` registers, scraped from this file.
    fn registered() -> BTreeSet<String> {
        between(&read("src/lib.rs"), "generate_handler![", "]")
            .split(',')
            .map(str::trim)
            .filter(|entry| !entry.is_empty())
            // The last path segment, not a fixed `commands::` prefix: the ACL
            // names a command by its *function* name whatever module it lives
            // in (`generate_handler!` takes bare paths and keys the wrapper on
            // the final ident), and `updater::` is the second module to
            // register one. Stripping only one known prefix silently produced
            // `allow-updater::update-check` here and nowhere else.
            .map(|entry| entry.rsplit("::").next().unwrap_or(entry).to_string())
            .collect()
    }

    /// Every command `build.rs` hands to `AppManifest::commands`.
    fn manifested() -> BTreeSet<String> {
        between(&read("build.rs"), "const COMMANDS: &[&str] = &[", "];")
            .split('"')
            .skip(1)
            .step_by(2)
            .map(str::to_string)
            .collect()
    }

    /// Every capability file in `capabilities/`, whatever it is called.
    fn capabilities() -> Vec<Capability> {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("capabilities");
        let mut found = Vec::new();
        for entry in std::fs::read_dir(&dir).expect("capabilities dir") {
            let path = entry.expect("capability dir entry").path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let file = path
                .file_name()
                .and_then(|n| n.to_str())
                .expect("capability file name")
                .to_string();
            let json: serde_json::Value =
                serde_json::from_str(&std::fs::read_to_string(&path).expect("read capability"))
                    .unwrap_or_else(|e| panic!("{file} is not valid JSON: {e}"));

            let labels = |key: &str, required: bool| -> Vec<String> {
                match &json[key] {
                    serde_json::Value::Null if !required => Vec::new(),
                    value => value
                        .as_array()
                        .unwrap_or_else(|| panic!("{file} has no {key} array"))
                        .iter()
                        .map(|w| w.as_str().expect("window label").to_string())
                        .collect(),
                }
            };
            let windows = labels("windows", true);
            // Absent from every file today, and the tests below keep it that
            // way; read it anyway so its arrival is a failure, not a blind spot.
            let webviews = labels("webviews", false);

            // A permission entry is either a bare identifier or an object
            // carrying a scope (`opener:allow-open-url` is the latter).
            let permissions = json["permissions"]
                .as_array()
                .unwrap_or_else(|| panic!("{file} has no permissions array"))
                .iter()
                .map(|p| match p {
                    serde_json::Value::String(s) => s.clone(),
                    scoped => scoped["identifier"]
                        .as_str()
                        .unwrap_or_else(|| panic!("{file}: scoped permission without identifier"))
                        .to_string(),
                })
                .collect();

            found.push(Capability {
                file,
                windows,
                webviews,
                permissions,
            });
        }
        assert!(!found.is_empty(), "no capability files found in {dir:?}");
        found
    }

    /// The pill is not merely uninterested in commands, it is unable to name
    /// one.
    #[test]
    fn overlay_names_no_app_command() {
        for cap in capabilities() {
            if !covers(&cap, "overlay") {
                continue;
            }
            for identifier in &cap.permissions {
                assert!(
                    !is_app_permission(identifier),
                    "{} grants the overlay the app permission {identifier:?}; \
                     the pill only listens — see capabilities/overlay.json",
                    cap.file
                );
            }
        }
    }

    /// Events in, nothing out. Widening this is a decision, not a detail: the
    /// pill never invokes, never emits, and never touches its own window.
    #[test]
    fn overlay_gets_events_only() {
        const ALLOWED: &[&str] = &["core:event:allow-listen", "core:event:allow-unlisten"];
        for cap in capabilities() {
            if !covers(&cap, "overlay") {
                continue;
            }
            for identifier in &cap.permissions {
                assert!(
                    ALLOWED.contains(&identifier.as_str()),
                    "{} grants the overlay {identifier:?}, which is outside \
                     the listen-only set {ALLOWED:?}",
                    cap.file
                );
            }
        }
    }

    /// One capability reaching both windows is how the split silently reverts —
    /// listing both windows does it, and a glob would do it without listing
    /// anything.
    #[test]
    fn every_capability_targets_one_literal_window() {
        for cap in capabilities() {
            if let Some(fault) = targeting_fault(&cap.windows, &cap.webviews) {
                panic!("{} {fault}", cap.file);
            }
        }
    }

    /// The guard above is only worth having if it rejects the shapes that would
    /// hand a command back to the pill, so push those through it directly.
    #[test]
    fn glob_and_webview_targeting_are_rejected() {
        let none: Vec<String> = Vec::new();
        let one = |s: &str| vec![s.to_string()];

        assert!(targeting_fault(&one("main"), &none).is_none());
        assert!(targeting_fault(&one("overlay"), &none).is_none());

        // The scenario this exists for: a new `capabilities/notes.json` granting
        // an app command to `"*"` passes tauri-build's validation untouched, and
        // a reader comparing labels with `==` walks straight past it.
        for pattern in ["*", "overl?y", "[mo]ain", "main*", "?"] {
            assert!(
                targeting_fault(&one(pattern), &none).is_some(),
                "the glob {pattern:?} was accepted as a literal window label"
            );
        }
        assert!(targeting_fault(&one("notes"), &none).is_some());
        assert!(targeting_fault(&["main".into(), "overlay".into()], &none).is_some());
        assert!(
            targeting_fault(&one("main"), &one("overlay")).is_some(),
            "a webviews key was ignored; it grants regardless of the windows list"
        );

        // And the reader has to agree with the guard: a glob capability counts
        // as covering the overlay even though it never says "overlay".
        let sneaky = Capability {
            file: "notes.json".into(),
            windows: one("*"),
            webviews: none.clone(),
            permissions: vec!["allow-get-settings".into()],
        };
        assert!(covers(&sneaky, "overlay"));
        assert!(covers(&sneaky, "main"));

        let by_webview = Capability {
            file: "notes.json".into(),
            windows: one("main"),
            webviews: one("overlay"),
            permissions: vec!["allow-get-settings".into()],
        };
        assert!(covers(&by_webview, "overlay"));
    }

    /// Without this, adding a command to the macro and forgetting `build.rs`
    /// leaves it permanently un-grantable, with no error anywhere.
    #[test]
    fn build_manifest_lists_every_registered_command() {
        assert_eq!(
            manifested(),
            registered(),
            "build.rs's COMMANDS and generate_handler! disagree"
        );
    }

    /// And without this, adding a command to both and forgetting the capability
    /// leaves the settings page calling a command the ACL rejects at runtime.
    #[test]
    fn main_grants_exactly_the_registered_commands() {
        let expected: BTreeSet<String> = registered()
            .iter()
            .map(|command| format!("allow-{}", command.replace('_', "-")))
            .collect();

        let granted: BTreeSet<String> = capabilities()
            .into_iter()
            .filter(|cap| covers(cap, "main"))
            .flat_map(|cap| cap.permissions)
            .filter(|identifier| is_app_permission(identifier))
            .collect();

        assert_eq!(
            granted, expected,
            "capabilities/main.json and generate_handler! disagree"
        );
    }
}

#[cfg(test)]
mod panic_log {
    /// The line names where the panic happened and nothing it said.
    #[test]
    fn a_panic_is_logged_by_thread_and_location_only() {
        assert_eq!(
            super::panic_line(Some("controller"), Some(("src\\controller.rs", 42))),
            "panic on controller at src\\controller.rs:42"
        );
        assert_eq!(super::panic_line(None, None), "panic on an unnamed thread");
    }
}

#[cfg(test)]
mod bundle_config {
    /// WebView2's general autofill ("Suggestions") can keep what is typed
    /// into a form field, a History search for a dictated phrase included,
    /// in the webview profile's own store, where no history setting reaches.
    /// Tauri turns it on unless a window says otherwise.
    #[test]
    fn every_window_turns_general_autofill_off() {
        let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tauri.conf.json");
        let conf: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        let windows = conf["app"]["windows"].as_array().expect("app.windows");
        assert!(!windows.is_empty());
        for w in windows {
            assert_eq!(
                w["generalAutofillEnabled"],
                serde_json::Value::Bool(false),
                "window {} leaves general autofill on",
                w["label"]
            );
        }
    }

    /// The uninstall hook guards Tauri's own removal of the bundle folders by
    /// name. The installer takes the name from `BUNDLEID`; the hook's own
    /// value, used by its test harness, must be the same string.
    #[test]
    fn the_uninstall_hook_names_the_bundle_folder() {
        let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tauri.conf.json");
        let conf: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        let id = conf["identifier"].as_str().expect("identifier");
        const HOOK: &str = include_str!("../windows/hooks.nsh");
        assert!(HOOK.contains(&format!("!define BS_BUNDLE_FOLDER \"{id}\"")));
        assert!(HOOK.contains("!define /redef BS_BUNDLE_FOLDER \"${BUNDLEID}\""));
    }

    /// The uninstall hook stops Tauri's own "Delete app data" block from
    /// running and does its work itself, registry lines included, copied
    /// from the Tauri CLI's installer template. A CLI upgrade can change that
    /// block, so this fails until someone re-reads the new template's
    /// Section Uninstall and updates the hook (and this version).
    #[test]
    fn the_uninstall_hook_copies_tauris_block_from_the_cli_in_use() {
        const CLI: &str = "2.11.4";
        let lock = std::fs::read_to_string(
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../pnpm-lock.yaml"),
        )
        .unwrap();
        assert!(
            lock.contains(&format!("'@tauri-apps/cli@{CLI}':")),
            "the Tauri CLI is no longer {CLI}: re-check its installer template's \
             \"Delete app data\" block against src-tauri/windows/hooks.nsh"
        );
        const HOOK: &str = include_str!("../windows/hooks.nsh");
        assert!(HOOK.contains(&format!("copied from its {CLI} template")));
        for line in [
            "DeleteRegKey SHCTX \"${MANUPRODUCTKEY}\"",
            "DeleteRegKey /ifempty SHCTX \"${MANUKEY}\"",
            "DeleteRegValue HKCU \"${MANUPRODUCTKEY}\" \"Installer Language\"",
            "DeleteRegKey /ifempty HKCU \"${MANUPRODUCTKEY}\"",
            "DeleteRegKey /ifempty HKCU \"${MANUKEY}\"",
            "StrCpy $DeleteAppDataCheckboxState 0",
        ] {
            assert!(HOOK.contains(line), "hooks.nsh lacks: {line}");
        }
    }
}
