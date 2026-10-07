//! Recover a window whose underlying WebView2 process dies.
//!
//! Tauri 2 and wry 0.55 offer no "the renderer crashed" event: their
//! `WebviewEvent` carries only drag and drop. WebView2 itself reports process
//! failures through `ICoreWebView2::add_ProcessFailed`, and Tauri's
//! `PlatformWebview::controller()` hands back the typed
//! `ICoreWebView2Controller`, so this module subscribes to that COM event
//! directly, for the main window and for the overlay. There is no polling and
//! no liveness probe.
//!
//! What recovers the page depends on which process died (see [`recovery_for`]):
//! a dead renderer is replaced by reloading the page, but a dead browser
//! process takes every webview of the app with it and leaves them closed for
//! good, so the app relaunches itself.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tauri::{Manager, WebviewWindow};
use webview2_com::Microsoft::Web::WebView2::Win32::{
    COREWEBVIEW2_PROCESS_FAILED_KIND, COREWEBVIEW2_PROCESS_FAILED_KIND_BROWSER_PROCESS_EXITED,
    COREWEBVIEW2_PROCESS_FAILED_KIND_FRAME_RENDER_PROCESS_EXITED,
    COREWEBVIEW2_PROCESS_FAILED_KIND_RENDER_PROCESS_EXITED,
};
use webview2_com::ProcessFailedEventHandler;

/// Pause between the failure event and the reload: room for WebView2 to finish
/// tearing down the dead process, and short enough that the blank window is
/// gone before it reads as a hang.
const RELOAD_PAUSE: Duration = Duration::from_millis(600);

/// How long a relaunch waits for a dictation or a transform in flight to
/// finish. Pasting needs no webview, so work that is under way when the
/// browser process dies is let through first rather than killed.
const RELAUNCH_WAIT: Duration = Duration::from_secs(30);

/// The poll inside that wait. There is no notification for "the controller
/// went Idle".
const RELAUNCH_POLL: Duration = Duration::from_millis(500);

/// Set by the first relaunch. The main window and the overlay share one
/// browser process, so its death reaches both handlers.
static RELAUNCHING: AtomicBool = AtomicBool::new(false);

/// The file beside `settings.json` that holds the unix time of the last
/// relaunch. It outlives the process, which is what stops a WebView2 that
/// dies on every start from restarting the app forever.
const RELAUNCH_MARKER: &str = "last-webview-relaunch";

/// No second relaunch within this long of the last one. A browser process
/// that dies again this soon is not going to be fixed by another start.
const RELAUNCH_GAP: Duration = Duration::from_secs(120);

/// Whether the relaunch should keep waiting, `waited` into the wait. An
/// import is waited out whatever it takes: it has no resume, and it ends on
/// its own. A dictation or a transform gets [`RELAUNCH_WAIT`]. The work it
/// waits for is the work the updater refuses to end (`updater::dictating`
/// and the import queue).
fn keep_waiting(dictating: bool, importing: bool, waited: Duration) -> bool {
    importing || (dictating && waited < RELAUNCH_WAIT)
}

/// Whether a relaunch may happen at `now` (unix seconds), judged by the
/// marker in `dir`; when it may, the marker is written first. A marker that
/// cannot be written allows none: without it, nothing would stop a loop.
fn claim_relaunch(dir: &std::path::Path, now: u64) -> bool {
    let marker = dir.join(RELAUNCH_MARKER);
    let last = std::fs::read_to_string(&marker)
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok());
    if last.is_some_and(|t| now.saturating_sub(t) < RELAUNCH_GAP.as_secs()) {
        return false;
    }
    std::fs::create_dir_all(dir).is_ok() && std::fs::write(&marker, now.to_string()).is_ok()
}

/// What a process failure calls for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Recovery {
    /// A peripheral process (GPU, utility) that WebView2 restarts on its own,
    /// or `RENDER_PROCESS_UNRESPONSIVE`, where the page still exists and is
    /// only slow: reloading it would throw away whatever it was doing.
    None,
    /// The page's renderer is gone. The webview itself survives and a reload
    /// gives it a new renderer.
    Reload,
    /// The browser process is gone. WebView2 closes every webview it hosted,
    /// and a closed webview cannot reload; only a new one can show a page.
    Relaunch,
}

fn recovery_for(kind: COREWEBVIEW2_PROCESS_FAILED_KIND) -> Recovery {
    match kind {
        COREWEBVIEW2_PROCESS_FAILED_KIND_BROWSER_PROCESS_EXITED => Recovery::Relaunch,
        COREWEBVIEW2_PROCESS_FAILED_KIND_RENDER_PROCESS_EXITED
        | COREWEBVIEW2_PROCESS_FAILED_KIND_FRAME_RENDER_PROCESS_EXITED => Recovery::Reload,
        _ => Recovery::None,
    }
}

/// Start this app again: the browser process that hosted its webviews is
/// gone. Once only, whichever window noticed first, after the work in flight
/// has finished (see [`keep_waiting`]), and never twice within
/// [`RELAUNCH_GAP`].
fn relaunch(app: tauri::AppHandle) {
    if RELAUNCHING.swap(true, Ordering::SeqCst) {
        return;
    }
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(RELOAD_PAUSE).await;
        let in_flight = || {
            app.try_state::<crate::commands::Backend>().map_or((false, false), |b| {
                (
                    crate::updater::dictating(&b.dictation_busy, &b.transform_busy),
                    b.import.snapshot().running,
                )
            })
        };
        let mut waited = Duration::ZERO;
        loop {
            let (dictating, importing) = in_flight();
            if !keep_waiting(dictating, importing, waited) {
                break;
            }
            tokio::time::sleep(RELAUNCH_POLL).await;
            waited += RELAUNCH_POLL;
        }
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        if claim_relaunch(&crate::settings::config_dir(), now) {
            tracing::warn!("relaunching: the WebView2 browser process is gone");
            app.request_restart();
        } else {
            tracing::error!(
                "the WebView2 browser process is gone again within {RELAUNCH_GAP:?} of the last \
                 relaunch; not relaunching. Dictation still works; quit from the tray and start \
                 the app again to get the windows back"
            );
        }
    });
}

/// Attach the crash handler to `window`'s WebView2 process. Best-effort: a
/// failure here (an unexpectedly old WebView2 runtime, a COM error) leaves
/// the window without auto-recovery rather than blocking startup — the same
/// posture every other best-effort Win32 call in this crate takes.
pub fn watch(window: &WebviewWindow) {
    let target = window.clone();
    let label = window.label().to_string();
    let result = window.with_webview(move |platform| {
        let controller = platform.controller();
        let core = match unsafe { controller.CoreWebView2() } {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!("crash recovery ({label}): no ICoreWebView2 handle: {e}");
                return;
            }
        };

        let handler = ProcessFailedEventHandler::create(Box::new(move |_sender, args| {
            let recovery = args
                .map(|a| {
                    let mut kind = COREWEBVIEW2_PROCESS_FAILED_KIND::default();
                    match unsafe { a.ProcessFailedKind(&mut kind) } {
                        Ok(()) => recovery_for(kind),
                        Err(_) => Recovery::None,
                    }
                })
                .unwrap_or(Recovery::None);
            match recovery {
                Recovery::None => {}
                Recovery::Reload => {
                    tracing::warn!(
                        "{}'s WebView2 renderer died; reloading in {RELOAD_PAUSE:?}",
                        target.label()
                    );
                    let win = target.clone();
                    tauri::async_runtime::spawn(async move {
                        tokio::time::sleep(RELOAD_PAUSE).await;
                        if let Err(e) = win.reload() {
                            tracing::warn!("crash recovery: reload failed: {e}");
                        }
                    });
                }
                Recovery::Relaunch => relaunch(target.app_handle().clone()),
            }
            Ok(())
        }));

        let mut token = 0i64;
        if let Err(e) = unsafe { core.add_ProcessFailed(&handler, &mut token) } {
            tracing::warn!("crash recovery ({label}): add_ProcessFailed failed: {e}");
        }
    });
    if let Err(e) = result {
        tracing::warn!("crash recovery: couldn't reach the platform webview: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A dead renderer is replaced by a reload; a dead browser process closes
    /// the webview for good, and reloading a closed webview does nothing.
    #[test]
    fn a_dead_browser_process_relaunches_and_a_dead_renderer_reloads() {
        assert_eq!(
            recovery_for(COREWEBVIEW2_PROCESS_FAILED_KIND_BROWSER_PROCESS_EXITED),
            Recovery::Relaunch
        );
        assert_eq!(
            recovery_for(COREWEBVIEW2_PROCESS_FAILED_KIND_RENDER_PROCESS_EXITED),
            Recovery::Reload
        );
        assert_eq!(
            recovery_for(COREWEBVIEW2_PROCESS_FAILED_KIND_FRAME_RENDER_PROCESS_EXITED),
            Recovery::Reload
        );
    }

    /// A hang (the page is alive, just unresponsive) and peripheral-process
    /// exits must not trigger a reload: the hung page may still recover with
    /// its state intact, and a GPU or utility process restarting on its own
    /// is normal WebView2 behaviour, not a dead page.
    #[test]
    fn transient_and_peripheral_process_kinds_are_not_fatal() {
        use webview2_com::Microsoft::Web::WebView2::Win32::{
            COREWEBVIEW2_PROCESS_FAILED_KIND_GPU_PROCESS_EXITED,
            COREWEBVIEW2_PROCESS_FAILED_KIND_RENDER_PROCESS_UNRESPONSIVE,
            COREWEBVIEW2_PROCESS_FAILED_KIND_UTILITY_PROCESS_EXITED,
        };
        for kind in [
            COREWEBVIEW2_PROCESS_FAILED_KIND_RENDER_PROCESS_UNRESPONSIVE,
            COREWEBVIEW2_PROCESS_FAILED_KIND_GPU_PROCESS_EXITED,
            COREWEBVIEW2_PROCESS_FAILED_KIND_UTILITY_PROCESS_EXITED,
        ] {
            assert_eq!(recovery_for(kind), Recovery::None);
        }
    }

    /// A folder of its own under the temp directory, removed on drop.
    struct TempDir(std::path::PathBuf);

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// A WebView2 that dies on every start must not restart the app forever:
    /// the marker outlives the process, and a second relaunch inside the gap
    /// is refused.
    #[test]
    fn no_second_relaunch_inside_the_gap() {
        let dir = TempDir(std::env::temp_dir().join(format!("bs-relaunch-{}", uuid::Uuid::new_v4())));
        let t = 1_800_000_000;
        assert!(claim_relaunch(&dir.0, t), "the first relaunch goes ahead");
        assert!(!claim_relaunch(&dir.0, t + 5), "a relaunched process that dies again stays down");
        assert!(!claim_relaunch(&dir.0, t + RELAUNCH_GAP.as_secs() - 1));
        assert!(claim_relaunch(&dir.0, t + RELAUNCH_GAP.as_secs()), "a later failure is a new one");
        assert!(!claim_relaunch(&dir.0, t + 1), "a clock that went back is inside the gap");
    }

    /// An import has no resume, so the relaunch waits it out however long it
    /// runs; a dictation or a transform gets a bounded wait.
    #[test]
    fn a_running_import_blocks_the_relaunch() {
        let long = Duration::from_secs(20 * 60);
        assert!(keep_waiting(false, true, long));
        assert!(keep_waiting(true, true, long));
        assert!(keep_waiting(true, false, Duration::ZERO));
        assert!(!keep_waiting(true, false, RELAUNCH_WAIT));
        assert!(!keep_waiting(false, false, Duration::ZERO));
    }
}
