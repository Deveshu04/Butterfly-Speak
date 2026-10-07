//! System tray: the app lives here. Closing the main window hides it; the
//! tray menu is the way to reopen, view history, pause, or quit.

use crate::state::ControlMsg;
use crossbeam_channel::Sender;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tauri::menu::{CheckMenuItem, Menu, MenuItem};
use tauri::tray::{TrayIconBuilder, TrayIconEvent};
use tauri::{Emitter, Manager};

const TRAY_ID: &str = "main-tray";

fn pause_label(paused: bool) -> &'static str {
    if paused {
        "Resume dictation"
    } else {
        "Pause dictation"
    }
}

/// The tray tooltip, naming the dictation chord the user actually has.
fn tooltip_for(paused: bool, binding: &str) -> String {
    if paused {
        "Butterfly Speak — dictation paused".into()
    } else {
        format!("Butterfly Speak — hold {binding} to dictate")
    }
}

/// The dictation chord as settings hold it right now.
fn current_binding(app: &tauri::AppHandle) -> String {
    app.state::<crate::commands::Backend>()
        .settings
        .read()
        .expect("settings lock")
        .hotkey
        .binding
        .clone()
}

/// Redraw the tooltip after the pause state or the dictation chord changed.
pub fn refresh_tooltip(app: &tauri::AppHandle, paused: bool, binding: &str) {
    if let Some(tray) = app.tray_by_id(TRAY_ID) {
        let _ = tray.set_tooltip(Some(tooltip_for(paused, binding)));
    }
}

/// `control` is the controller's channel: pausing cancels a live recording
/// (`ControlMsg::Paused`), because with the hook paused no key could end it.
pub fn setup(
    app: &tauri::App,
    paused: Arc<AtomicBool>,
    control: Sender<ControlMsg>,
    binding: &str,
) -> tauri::Result<()> {
    let open = MenuItem::with_id(app, "open", "Open Butterfly Speak", true, None::<&str>)?;
    let history = MenuItem::with_id(app, "history", "Dictation history", true, None::<&str>)?;
    let pause = CheckMenuItem::with_id(
        app,
        "pause",
        pause_label(paused.load(Ordering::Relaxed)),
        true,
        false,
        None::<&str>,
    )?;
    let quit = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&open, &history, &pause, &quit])?;

    let pause_item = pause.clone();
    // The tray is drawn at 16-24 px, where the full mark smears into a blob:
    // this is the small-size variant, rendered from icon-small.svg. Named
    // explicitly rather than taken from `default_window_icon()`, which is
    // whatever layer sits first in icon.ico and silently changes when the
    // icons are regenerated.
    TrayIconBuilder::with_id(TRAY_ID)
        .icon(tauri::include_image!("icons/32x32.png"))
        .tooltip(tooltip_for(paused.load(Ordering::Relaxed), binding))
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(move |app, event| match event.id.as_ref() {
            "open" => show_main(app),
            "history" => show_history(app),
            "pause" => {
                let now = !paused.load(Ordering::Relaxed);
                paused.store(now, Ordering::Relaxed);
                if now {
                    let _ = control.send(ControlMsg::Paused);
                }
                let _ = pause_item.set_checked(now);
                let _ = pause_item.set_text(pause_label(now));
                refresh_tooltip(app, now, &current_binding(app));
                tracing::info!("dictation {}", if now { "paused" } else { "resumed" });
            }
            "quit" => app.exit(0),
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::DoubleClick { .. } = event {
                show_main(tray.app_handle());
            }
        })
        .build(app)?;
    Ok(())
}

pub fn show_main(app: &tauri::AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.show();
        let _ = w.unminimize();
        let _ = w.set_focus();
    }
}

/// Opens the main window on the History page (`NAV_MAIN`'s "history" entry
/// in `+page.svelte`). The id has to match that entry exactly: the renderer's
/// `NAVIGATE` handler looks the payload up in its nav list and silently
/// ignores anything it can't find, so a typo here is a dead menu item, not
/// an error.
fn show_history(app: &tauri::AppHandle) {
    show_main(app);
    let _ = app.emit(
        crate::events::NAVIGATE,
        crate::events::NavigatePayload { page: "history" },
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The tooltip names the chord the user set, not the default one.
    #[test]
    fn the_tooltip_names_the_users_own_chord() {
        assert_eq!(
            tooltip_for(false, "Alt+Shift+D"),
            "Butterfly Speak — hold Alt+Shift+D to dictate"
        );
        assert_eq!(tooltip_for(true, "Alt+Shift+D"), "Butterfly Speak — dictation paused");
    }
}
