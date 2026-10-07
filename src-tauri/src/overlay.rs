//! Overlay pill window management: positioning on the foreground window's
//! monitor (falling back to the cursor's), focus hardening (the overlay must
//! NEVER activate, or the paste target would change), hiding it from screen
//! capture so no share or recording shows it, and keeping it on top of
//! whatever else is fighting for the z-order.

use tauri::{AppHandle, Manager, PhysicalPosition, WebviewWindow};
use windows::Win32::Foundation::{HWND, POINT, RECT};
use windows::Win32::Graphics::Gdi::{
    GetMonitorInfoW, MonitorFromPoint, MonitorFromWindow, HMONITOR, MONITORINFO,
    MONITOR_DEFAULTTONEAREST,
};
use windows::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
use windows::Win32::UI::WindowsAndMessaging::{GetCursorPos, GetForegroundWindow};

const PILL_W: f64 = 420.0;
const PILL_H: f64 = 72.0;

/// Windows' baseline DPI; a monitor's scale factor is `dpi / BASELINE_DPI`.
const BASELINE_DPI: f64 = 96.0;

/// UTF-16, NUL-terminated: the shape every wide-string Win32 call here needs.
fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Windows build 19041 (2004) is the first to support
/// `WDA_EXCLUDEFROMCAPTURE`. Below that, only `WDA_MONITOR` exists, and it
/// *blanks* the window in a screen share instead of hiding it — worse than no
/// protection at all — so older builds skip the call entirely rather than
/// risk it.
const MIN_BUILD_FOR_EXCLUDE_FROM_CAPTURE: u32 = 19041;

/// Whether this Windows build supports `WDA_EXCLUDEFROMCAPTURE`. `None`
/// (the registry read failed) is treated the same as "too old" — the safe
/// default when the build number can't be determined.
fn supports_exclude_from_capture(build: Option<u32>) -> bool {
    build.is_some_and(|b| b >= MIN_BUILD_FOR_EXCLUDE_FROM_CAPTURE)
}

/// Real Windows build number, read from the registry rather than
/// `GetVersionEx` — that API is manifest-gated and reports Windows 8 (build
/// 9200) for any process without an OS-compatibility manifest declaring
/// Windows 10 support, which this app doesn't ship.
fn windows_build() -> Option<u32> {
    use windows::Win32::Foundation::ERROR_SUCCESS;
    use windows::Win32::System::Registry::{HKEY_LOCAL_MACHINE, RRF_RT_REG_SZ, RegGetValueW};
    use windows::core::PCWSTR;

    let subkey = wide(r"SOFTWARE\Microsoft\Windows NT\CurrentVersion");
    let value = wide("CurrentBuildNumber");
    let mut buf = [0u16; 32];
    let mut size = std::mem::size_of_val(&buf) as u32;
    let status = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            PCWSTR(subkey.as_ptr()),
            PCWSTR(value.as_ptr()),
            RRF_RT_REG_SZ,
            None,
            Some(buf.as_mut_ptr().cast()),
            Some(&mut size),
        )
    };
    if status != ERROR_SUCCESS {
        return None;
    }
    // `size` comes back as the byte count, including the terminating NUL.
    let chars = (size as usize / 2).saturating_sub(1);
    String::from_utf16_lossy(&buf[..chars]).parse().ok()
}

/// Apply WS_EX_NOACTIVATE + WS_EX_TOOLWINDOW, make the pill click-through,
/// and (build permitting) exclude it from screen captures. Call once at
/// startup.
pub fn harden(app: &AppHandle) {
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::WindowsAndMessaging::{
        GWL_EXSTYLE, GetWindowLongPtrW, SetWindowDisplayAffinity, SetWindowLongPtrW,
        WDA_EXCLUDEFROMCAPTURE, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW,
    };

    let Some(win) = app.get_webview_window("overlay") else {
        return;
    };
    let _ = win.set_ignore_cursor_events(true);
    if let Ok(hwnd) = win.hwnd() {
        let hwnd = HWND(hwnd.0 as *mut core::ffi::c_void);
        unsafe {
            let ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
            SetWindowLongPtrW(
                hwnd,
                GWL_EXSTYLE,
                ex | WS_EX_NOACTIVATE.0 as isize | WS_EX_TOOLWINDOW.0 as isize,
            );
        }
        // Hide the pill from screen capture (`WDA_EXCLUDEFROMCAPTURE`) on the
        // builds that have it; see `supports_exclude_from_capture`.
        if supports_exclude_from_capture(windows_build()) {
            unsafe {
                if let Err(e) = SetWindowDisplayAffinity(hwnd, WDA_EXCLUDEFROMCAPTURE) {
                    tracing::warn!("setting the pill's display affinity failed: {e}");
                }
            }
        }
    }
}

/// The monitor to place the pill on: the FOREGROUND window's monitor when
/// there is one, else the monitor under the cursor: `GetForegroundWindow`,
/// then `MonitorFromWindow` or `MonitorFromPoint`, no process spawn.
fn target_monitor() -> Option<HMONITOR> {
    let fg = unsafe { GetForegroundWindow() };
    if !fg.is_invalid() {
        let hmon = unsafe { MonitorFromWindow(fg, MONITOR_DEFAULTTONEAREST) };
        if !hmon.is_invalid() {
            return Some(hmon);
        }
    }
    let mut pt = POINT::default();
    if unsafe { GetCursorPos(&mut pt) }.is_ok() {
        let hmon = unsafe { MonitorFromPoint(pt, MONITOR_DEFAULTTONEAREST) };
        if !hmon.is_invalid() {
            return Some(hmon);
        }
    }
    None
}

/// `hmonitor`'s work area (physical px, taskbar excluded) and DPI scale
/// factor, or `None` if either Win32 call fails.
fn monitor_metrics(hmonitor: HMONITOR) -> Option<(RECT, f64)> {
    let mut info = MONITORINFO {
        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    if !unsafe { GetMonitorInfoW(hmonitor, &mut info) }.as_bool() {
        return None;
    }
    let mut dpi_x = 0u32;
    let mut dpi_y = 0u32;
    let scale = if unsafe { GetDpiForMonitor(hmonitor, MDT_EFFECTIVE_DPI, &mut dpi_x, &mut dpi_y) }
        .is_ok()
    {
        dpi_x as f64 / BASELINE_DPI
    } else {
        1.0
    };
    Some((info.rcWork, scale))
}

/// Bottom-center pill position (physical px) within `work_area`, scaled to
/// `scale_factor`. The result may be negative and is returned as computed:
/// Windows measures every screen from the main screen's top-left corner, so
/// a screen set to the left of it has negative `x` values and one set above
/// it has negative `y` values.
///
/// `bottom_margin` is held to the Settings range
/// (`settings::OVERLAY_OFFSET_MAX`): it comes from the settings file, which
/// an import or a hand edit can fill with a value that puts the pill
/// off-screen.
fn pill_position(work_area: RECT, scale_factor: f64, bottom_margin: f64) -> (i32, i32) {
    let bottom_margin = bottom_margin.clamp(0.0, crate::settings::OVERLAY_OFFSET_MAX);
    let w = (PILL_W * scale_factor) as i32;
    let h = (PILL_H * scale_factor) as i32;
    let width = work_area.right - work_area.left;
    let height = work_area.bottom - work_area.top;
    let x = work_area.left + (width - w) / 2;
    let y = work_area.top + height - h - (bottom_margin * scale_factor) as i32;
    (x, y)
}

/// Re-assert `HWND_TOPMOST` on `win` without moving, resizing, or activating
/// it — raw `SetWindowPos`, the cheap defense against another topmost window
/// (a screen-share picker, a fullscreen game's exclusive surface) stealing
/// the z-order out from under the pill after it was first shown.
fn set_topmost(win: &WebviewWindow) {
    use windows::Win32::UI::WindowsAndMessaging::{
        SetWindowPos, HWND_TOPMOST, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE,
    };

    let Ok(hwnd) = win.hwnd() else { return };
    let hwnd = HWND(hwnd.0 as *mut core::ffi::c_void);
    unsafe {
        let _ = SetWindowPos(
            hwnd,
            Some(HWND_TOPMOST),
            0,
            0,
            0,
            0,
            SWP_NOACTIVATE | SWP_NOMOVE | SWP_NOSIZE,
        );
    }
}

/// Re-assert TOPMOST on the overlay if it exists. Called from `show()` and
/// again on every dictation state transition (`controller::set_state`) while
/// the overlay is up. This stands in for a foreground-window-change hook: a
/// state transition is a cheap moment to undo anything that has taken the
/// top of the z-order since the pill was shown.
pub fn reassert_topmost(app: &AppHandle) {
    if let Some(win) = app.get_webview_window("overlay") {
        set_topmost(&win);
    }
}

/// Position the pill bottom-center on the target monitor (see
/// `target_monitor`), respecting its work area, re-assert TOPMOST, then show
/// it.
pub fn show(app: &AppHandle, bottom_margin: f64) {
    let Some(win) = app.get_webview_window("overlay") else {
        return;
    };

    if let Some((work_area, scale_factor)) = target_monitor().and_then(monitor_metrics) {
        let (x, y) = pill_position(work_area, scale_factor, bottom_margin);
        let _ = win.set_position(PhysicalPosition::new(x, y));
    }
    set_topmost(&win);
    let _ = win.show();
}

pub fn hide(app: &AppHandle) {
    if let Some(win) = app.get_webview_window("overlay") {
        let _ = win.hide();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_10_2004_and_later_supports_exclude_from_capture() {
        assert!(supports_exclude_from_capture(Some(19041)));
        assert!(supports_exclude_from_capture(Some(26200))); // Windows 11 25H2
    }

    #[test]
    fn older_builds_do_not_support_exclude_from_capture() {
        assert!(!supports_exclude_from_capture(Some(19040)));
        assert!(!supports_exclude_from_capture(Some(18363))); // Windows 10 1909
    }

    /// A registry read that fails must fall back to the safe (skip the call)
    /// side, not the capable one.
    #[test]
    fn an_undetectable_build_is_treated_as_unsupported() {
        assert!(!supports_exclude_from_capture(None));
    }

    fn work_area(left: i32, top: i32, right: i32, bottom: i32) -> RECT {
        RECT { left, top, right, bottom }
    }

    #[test]
    fn pill_centers_horizontally_and_sits_bottom_margin_above_the_work_area_floor() {
        let wa = work_area(0, 0, 1920, 1040); // 1080p minus a 40px taskbar
        let (x, y) = pill_position(wa, 1.0, 24.0);
        assert_eq!(x, (1920 - PILL_W as i32) / 2);
        assert_eq!(y, 1040 - PILL_H as i32 - 24);
    }

    /// A laptop screen set to the left of the main one, and a second screen
    /// set above it: each gets the pill at its own bottom centre, which means
    /// a negative `x` for the first and a negative `y` for the second.
    #[test]
    fn a_screen_left_of_or_above_the_main_one_keeps_its_negative_coordinates() {
        let left = work_area(-1366, 0, 0, 728); // 1366x768 with a 40px taskbar
        let (x, y) = pill_position(left, 1.0, 16.0);
        assert!(x < 0, "the pill belongs on the left-hand laptop screen, at x {x}");
        assert_eq!(x, -1366 + (1366 - PILL_W as i32) / 2);
        assert_eq!(y, 728 - PILL_H as i32 - 16);

        let above = work_area(0, -1200, 1920, 0); // 1920x1200, no taskbar
        let (x, y) = pill_position(above, 1.0, 16.0);
        assert!(y < 0, "the pill belongs on the upper screen, at y {y}");
        assert_eq!(x, (1920 - PILL_W as i32) / 2);
        assert_eq!(y, -PILL_H as i32 - 16);
    }

    /// Both the pill's own size and the bottom margin scale with the target
    /// monitor's DPI, not the primary's.
    #[test]
    fn position_scales_with_the_monitors_scale_factor() {
        let wa = work_area(0, 0, 3840, 2160);
        let (x, y) = pill_position(wa, 2.0, 24.0);
        let w = (PILL_W * 2.0) as i32;
        let h = (PILL_H * 2.0) as i32;
        assert_eq!(x, (3840 - w) / 2);
        assert_eq!(y, 2160 - h - (24.0 * 2.0) as i32);
    }

    /// The margin comes from the settings file, which an import or a hand
    /// edit can fill with anything; outside the Settings range of 0-400 it
    /// is held to that range, so the pill never lands off-screen.
    #[test]
    fn a_bottom_margin_outside_the_settings_range_is_held_to_it() {
        let wa = work_area(0, 0, 1920, 1040);
        assert_eq!(pill_position(wa, 1.0, 2000.0), pill_position(wa, 1.0, 400.0));
        assert_eq!(pill_position(wa, 1.0, -50.0), pill_position(wa, 1.0, 0.0));
        assert_eq!(pill_position(wa, 1.0, 0.0).1, 1040 - PILL_H as i32);
    }
}
