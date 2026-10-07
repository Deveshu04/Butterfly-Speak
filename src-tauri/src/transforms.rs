//! Transforms: select text in any app, press a shortcut, get it rewritten in
//! place by Sarvam. The whole flow runs on a one-shot thread — clipboard
//! round-trip, network call, paste — with the hotkey hook suppressed so our
//! synthetic Ctrl+C/V can't retrigger anything.

use crate::state::ControlMsg;
use crate::{events, injection, overlay};
use crossbeam_channel::Sender;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tauri::Emitter;

const MAX_WORDS: usize = 1000;
const FLASH_MS: u64 = 1800;

/// Shown instead of starting a transform in a terminal: there Ctrl+C is an
/// interrupt, not a copy, and a pasted multi-line rewrite runs line by line.
const TERMINAL_NOTICE: &str = "Transforms don't work in a terminal";

/// Shown when the rewrite came back but the window it was copied from could
/// not be brought back to the front: it is left on the clipboard instead of
/// being pasted into whatever has focus now.
const WINDOW_GONE_NOTICE: &str = "Couldn't return to that window — the rewrite is on the clipboard";

/// Shown when the window the shortcut was pressed in is not in front, and
/// cannot be brought back, before anything is copied. Nothing was changed.
const WINDOW_LEFT_NOTICE: &str = "Couldn't return to that window — nothing was changed";

pub struct TransformJob {
    pub name: String,
    pub prompt: String,
    pub model: String,
    pub api_key: Option<String>,
    /// Which host this transform's chat call goes to — the user's own Sarvam
    /// key, or Butterfly Labs' relay. Snapshotted with the rest of the job at
    /// the chord, like every other field here.
    pub lane: crate::sarvam::Lane,
    pub restore_clipboard: bool,
    /// The window the shortcut was pressed in (`foreground::capture` at the
    /// chord). The model call can take many seconds, and the rewrite is
    /// pasted back only into this window.
    pub target: Option<crate::foreground::Target>,
}

/// Why a transform will not start in `target`, or `None` when it may.
fn refusal(target: Option<&crate::foreground::Target>) -> Option<&'static str> {
    target
        .filter(|t| crate::foreground::is_terminal(t))
        .map(|_| TERMINAL_NOTICE)
}

/// Whether the transform's own window is in front, bringing it back if it
/// can. Asked twice: before the copy, which goes to whatever holds focus,
/// and before the paste, after a model call long enough for focus to move.
/// Either one in another app would read or rewrite that app's text.
fn back_in_target(target: Option<&crate::foreground::Target>) -> bool {
    target.is_some_and(crate::foreground::restore_foreground)
}

/// Guard that always clears the busy + suppress flags, whatever path exits.
struct Flags {
    suppress: Arc<AtomicBool>,
    busy: Arc<AtomicBool>,
}

impl Drop for Flags {
    fn drop(&mut self) {
        self.suppress.store(false, Ordering::Relaxed);
        self.busy.store(false, Ordering::Relaxed);
    }
}

#[allow(clippy::too_many_arguments)]
pub fn spawn(
    app: tauri::AppHandle,
    ctl_tx: Sender<ControlMsg>,
    suppress: Arc<AtomicBool>,
    busy: Arc<AtomicBool>,
    http: reqwest::Client,
    overlay_offset: f64,
    job: TransformJob,
) {
    std::thread::spawn(move || {
        let running = busy.clone();
        guarded_then_notice(
            suppress,
            busy,
            || run(&app, &http, overlay_offset, job),
            |notice| flash(&app, &ctl_tx, &running, overlay_offset, &notice),
        );
    });
}

/// Runs `work` with the keyboard hook suppressed and the transform marked
/// busy, then shows the notice it returned, if any, once both flags are
/// clear again. A notice holds its thread for `FLASH_MS`: shown while the
/// hook is suppressed, it would leave the dictation chord, Escape and every
/// shortcut dead for that long.
fn guarded_then_notice(
    suppress: Arc<AtomicBool>,
    busy: Arc<AtomicBool>,
    work: impl FnOnce() -> Option<String>,
    show: impl FnOnce(String),
) {
    let notice = {
        let _flags = Flags {
            suppress: suppress.clone(),
            busy,
        };
        suppress.store(true, Ordering::Relaxed);
        work()
    };
    if let Some(notice) = notice {
        show(notice);
    }
}

/// The transform itself. Returns the notice the pill should show, or `None`
/// when there is nothing to say (the rewrite was pasted, or the clipboard
/// could not be opened at all).
fn run(
    app: &tauri::AppHandle,
    http: &reqwest::Client,
    overlay_offset: f64,
    job: TransformJob,
) -> Option<String> {
    if let Some(sentence) = refusal(job.target.as_ref()) {
        return Some(sentence.into());
    }
    // Resolved before anything is copied or pasted, and resolved as "is there
    // a backend" rather than "is there a Sarvam key". An install that runs
    // entirely on its own endpoint has no Sarvam account to add a key to, and
    // telling that user to go get one on every transform chord would be
    // advice they cannot take about a feature that works.
    //
    // `block_on` because this thread's whole job is the round trip below,
    // which already blocks on the model call; on the Cloud lane the bearer
    // has to be awaited the same way.
    let backend = match tauri::async_runtime::block_on(crate::endpoint::chat_backend_for(
        &job.lane,
        job.api_key.as_deref(),
        &job.model,
    )) {
        Ok(backend) => backend,
        // Sarvam-specific wording only where Sarvam is the missing piece.
        Err(crate::endpoint::ChatUnavailable::Backend(
            crate::endpoint::Unavailable::NoSarvamKey,
        )) => {
            return Some("Transforms need Sarvam AI — add your key in Settings".into());
        }
        Err(crate::endpoint::ChatUnavailable::Backend(
            crate::endpoint::Unavailable::CustomEndpoint(why),
        )) => {
            return Some(why.message().into());
        }
        // The Cloud lane's own shape, and the reason it is not folded into
        // the sentence above: "add your key in Settings" is advice a Cloud
        // user cannot take, and `auth::session`'s sentence already says the
        // right thing — sign in again, or you are offline.
        Err(crate::endpoint::ChatUnavailable::SignIn(sentence)) => {
            return Some(sentence);
        }
    };

    // The copy goes to whatever holds focus, so the window the shortcut was
    // pressed in has to be in front first: the backend lookup above can take
    // long enough on the Cloud lane for focus to move, and a copy there
    // would rewrite another app's selection.
    if !back_in_target(job.target.as_ref()) {
        return Some(WINDOW_LEFT_NOTICE.into());
    }
    let mut clipboard = match arboard::Clipboard::new() {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!("transform: clipboard unavailable: {e}");
            return None;
        }
    };
    let saved = clipboard.get_text().ok();

    let copy = injection::copy_selection(&mut clipboard);
    // The copy is the user's document, written unmarked by the target app.
    // It comes off the clipboard now, before the model call, so no way out
    // of the transform leaves it there.
    if injection::after_copy(&copy, saved.as_deref()).apply(&mut clipboard).is_err() {
        tracing::warn!("transform: could not put the clipboard back after the copy");
    }
    let Some(selection) = copy.text() else {
        return Some("Select some text first, then press the shortcut".into());
    };
    if selection.split_whitespace().count() > MAX_WORDS {
        return Some("Select up to 1,000 words".into());
    }

    // Working pill while the model runs.
    overlay::show(app, overlay_offset);
    emit_status(app, &format!("{}…", job.name));

    let result = tauri::async_runtime::block_on(crate::sarvam::chat::transform(
        http,
        &backend,
        &job.prompt,
        selection,
        // No kind on the Prompts page maps to a Transform: `job.prompt` already IS
        // the user's own text (`Settings::transforms`), so there is no
        // shipped scaffold here for them to rewrite.
        None,
    ));

    emit_status(app, "");
    match result {
        Ok(out) if !out.trim().is_empty() && !back_in_target(job.target.as_ref()) => {
            // The user's own clipboard is not put back: the rewrite is there
            // instead, for them to paste where they meant it to go.
            let _ = injection::set_private_text(&mut clipboard, out.trim());
            Some(WINDOW_GONE_NOTICE.into())
        }
        Ok(out) if !out.trim().is_empty() => {
            // Read again rather than reusing `saved`: the user may have copied
            // something during the model call, and that is what goes back. A
            // clipboard busy for the read still gets the earlier text back.
            let current = match clipboard.get_text() {
                Ok(text) => Some(text),
                Err(arboard::Error::ClipboardOccupied) => saved,
                Err(_) => None,
            };
            if let Err(e) = injection::paste_text(
                &mut clipboard,
                out.trim(),
                current.as_deref(),
                job.restore_clipboard,
            ) {
                tracing::warn!("transform paste failed: {e:#}");
            }
            overlay::hide(app);
            None
        }
        Ok(_) | Err(_) => {
            if let Err(e) = &result {
                // Through `redact_urls`: reqwest's Display embeds the request
                // URL, which for the custom slot is a pasted string that can
                // carry a credential (`https://user:pw@host`, `?api_key=`).
                let reason = crate::format::backend::redact_urls(&format!("{e:#}"));
                tracing::warn!("transform failed: {reason}");
            }
            let sentence = result
                .as_ref()
                .err()
                .and_then(crate::sarvam::chat::failure_sentence)
                .unwrap_or("Couldn't transform — try again");
            Some(sentence.into())
        }
    }
}

fn emit_status(app: &tauri::AppHandle, text: &str) {
    let _ = app.emit(
        events::OVERLAY_STATUS,
        events::StatusPayload { text: text.into() },
    );
}

/// Counts the notices shown, so a notice's timer can tell whether a later
/// one has taken the pill over.
static FLASH_GENERATION: AtomicU64 = AtomicU64::new(0);

/// Shows `message` on the pill for `FLASH_MS`, then hides the pill unless
/// someone else has it by then (see [`hide_after_flash`]).
fn flash(
    app: &tauri::AppHandle,
    ctl_tx: &Sender<ControlMsg>,
    busy: &AtomicBool,
    overlay_offset: f64,
    message: &str,
) {
    let generation = FLASH_GENERATION.fetch_add(1, Ordering::SeqCst) + 1;
    overlay::show(app, overlay_offset);
    let _ = app.emit(
        events::NOTICE_ERROR,
        events::NoticePayload {
            message: message.into(),
        },
    );
    std::thread::sleep(Duration::from_millis(FLASH_MS));
    if hide_after_flash(
        generation,
        FLASH_GENERATION.load(Ordering::SeqCst),
        busy.load(Ordering::Relaxed),
    ) {
        let _ = ctl_tx.send(ControlMsg::HideOverlay);
    }
}

/// Whether a notice's timer may hide the pill. Not while a transform is
/// running (`busy`): the pill shows that transform's progress. And not once
/// a later notice has been shown (`latest` is past this notice's own
/// `generation`): that notice's own timer hides it when its time is up.
fn hide_after_flash(generation: u64, latest: u64, busy: bool) -> bool {
    !busy && latest == generation
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::foreground::test_target;

    /// A terminal is refused before anything is copied: Ctrl+C there is an
    /// interrupt. Any other window, or none captured, goes ahead.
    #[test]
    fn a_transform_never_starts_in_a_terminal() {
        let terminal = test_target("windowsterminal", "CASCADIA_HOSTING_WINDOW_CLASS");
        assert_eq!(refusal(Some(&terminal)), Some(TERMINAL_NOTICE));
        let word = test_target("winword", "OpusApp");
        assert_eq!(refusal(Some(&word)), None);
        assert_eq!(refusal(None), None);
    }

    /// A window that cannot be brought back (closed, or never captured) gets
    /// no paste; the rewrite stays on the clipboard instead.
    #[test]
    fn a_rewrite_is_pasted_only_back_into_its_own_window() {
        assert!(!back_in_target(None));
        let gone = test_target("winword", "OpusApp");
        assert!(!back_in_target(Some(&gone)));
    }

    /// The copy and the paste both go to whatever holds focus, so each waits
    /// on the transform's own window being back in front.
    #[test]
    fn the_copy_and_the_paste_both_wait_for_the_transforms_own_window() {
        let source = include_str!("transforms.rs").replace("\r\n", "\n");
        let run = &source[source.find("\nfn run(").unwrap()..source.find("\nfn emit_status(").unwrap()];
        let copy = run.find("injection::copy_selection(").unwrap();
        let paste = run.find("injection::paste_text(").unwrap();
        let checks: Vec<usize> = run
            .match_indices("back_in_target(job.target.as_ref())")
            .map(|(i, _)| i)
            .collect();
        assert!(checks.iter().any(|&i| i < copy), "the copy goes out unchecked");
        assert!(checks.iter().any(|&i| copy < i && i < paste), "the paste goes out unchecked");
    }

    /// `flash` holds its thread for `FLASH_MS`, and `spawn` keeps the hook
    /// suppressed until `run` returns, so a notice flashed from inside `run`
    /// would leave the dictation chord, Escape and every shortcut dead for
    /// that long. `run` hands its notice back instead.
    #[test]
    fn run_shows_no_notice_while_the_hook_is_suppressed() {
        let source = include_str!("transforms.rs").replace("\r\n", "\n");
        let run = &source[source.find("\nfn run(").unwrap()..source.find("\nfn emit_status(").unwrap()];
        assert!(!run.contains("flash("), "run() flashes a notice while the hook is suppressed");
    }

    /// The notice `run` hands back is shown only once the hook listens again
    /// and the next transform may start; while the work runs, both flags
    /// are set.
    #[test]
    fn a_notice_is_shown_after_the_hook_is_released() {
        let suppress = Arc::new(AtomicBool::new(false));
        let busy = Arc::new(AtomicBool::new(true));
        let mut during = None;
        let mut shown = None;
        guarded_then_notice(
            suppress.clone(),
            busy.clone(),
            || {
                during = Some(suppress.load(Ordering::Relaxed));
                Some("Select some text first".to_string())
            },
            |notice| {
                shown = Some((notice, suppress.load(Ordering::Relaxed), busy.load(Ordering::Relaxed)));
            },
        );
        assert_eq!(during, Some(true), "the work ran with the hook listening");
        assert_eq!(shown, Some(("Select some text first".to_string(), false, false)));

        // No notice, nothing shown, and the flags are still cleared.
        busy.store(true, Ordering::Relaxed);
        guarded_then_notice(suppress.clone(), busy.clone(), || None, |_| panic!("nothing to show"));
        assert!(!suppress.load(Ordering::Relaxed) && !busy.load(Ordering::Relaxed));
    }

    /// A notice's timer hides the pill only when the pill is still its own:
    /// no transform running, and no later notice shown in the meantime.
    #[test]
    fn a_notice_timer_leaves_a_later_notice_or_a_running_transform_alone() {
        assert!(hide_after_flash(3, 3, false));
        assert!(!hide_after_flash(3, 3, true), "a running transform owns the pill");
        assert!(!hide_after_flash(3, 4, false), "a later notice owns the pill");
        assert!(!hide_after_flash(3, 4, true));
    }

    /// The copied selection is cleaned off the clipboard straight after the
    /// copy, before any way out of the transform, and the paste is what puts
    /// the user's clipboard back afterwards, so no failure path restores
    /// over something written during the model call.
    #[test]
    fn the_copied_selection_is_cleaned_up_before_any_exit() {
        let source = include_str!("transforms.rs").replace("\r\n", "\n");
        let run = &source[source.find("\nfn run(").unwrap()..source.find("\nfn emit_status(").unwrap()];
        let copy = run.find("injection::copy_selection(").unwrap();
        let cleanup = run.find("injection::after_copy(").expect("no cleanup after the copy");
        let first_exit = copy + run[copy..].find("return").unwrap();
        assert!(copy < cleanup && cleanup < first_exit, "the copy can exit uncleaned");
        assert!(!run.contains("restore(&mut clipboard"), "a restore that checks nothing remains");
    }
}
