//! Launch-at-login truth.
//!
//! `tauri_plugin_autostart` is backed by the `auto-launch` crate (pinned at
//! 0.5.0 in `Cargo.lock`). Its `is_enabled()` reads Explorer's
//! `StartupApproved\Run` key (`task_manager_enabled` in
//! `auto-launch-0.5.0/src/windows.rs`), but infers enabled or disabled from
//! whether the value's trailing 8 bytes are all zero. That heuristic is
//! wrong for a documented Windows encoding (see [`blob_marks_disabled`]): an
//! enabled entry can carry a non-zero trailing timestamp, which
//! `auto-launch` misreads as disabled, so the settings toggle would show off
//! while Windows still launches the app. [`true_state`] reads both registry
//! keys directly and decodes `StartupApproved\Run` against the documented
//! format.
//!
//! `auto-launch`'s `enable()` writes the `Run` key and, whenever the
//! `StartupApproved\Run` subkey already exists, that key too (always to the
//! enabled encoding). So turning this app's toggle back on usually also
//! clears an earlier disable from Task Manager; [`true_state`] still reads
//! the key because a user can disable from Task Manager again at any time,
//! with no call into this app at all.
//!
//! The app relaunched at login carries [`TRAY_START_ARG`], read back
//! from `std::env::args()` (Windows passes no login marker of its own), so
//! `lib.rs` can start straight to the tray instead of flashing the main
//! window open.

use windows::Win32::Foundation::ERROR_SUCCESS;
use windows::Win32::System::Registry::{
    RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_BINARY, RRF_RT_REG_SZ,
};
use windows::core::PCWSTR;

/// Passed to `tauri_plugin_autostart::init` as the login-item arg, and
/// checked against `std::env::args()` at startup. Must be the one and only
/// arg the plugin is configured with — `true_state` doesn't need to know it
/// (registry presence alone is the signal), but `lib.rs`'s `--hidden` check
/// and the plugin's own write have to agree on the literal string. The
/// string itself must not change either: a `Run` entry already on a user's
/// machine carries it.
pub const TRAY_START_ARG: &str = "--hidden";

const RUN_KEY: &str = r"SOFTWARE\Microsoft\Windows\CurrentVersion\Run";
const STARTUP_APPROVED_KEY: &str =
    r"SOFTWARE\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved\Run";

/// UTF-16, NUL-terminated: the shape every wide-string Win32 call here needs.
/// Same helper `overlay.rs` defines for its own registry read — kept
/// separate rather than shared, since sharing a two-line private helper
/// across modules isn't worth the indirection.
fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Whether the `Run` key carries an entry named `app_name` at all. The
/// plugin deletes the value on `disable()` and writes it (unconditionally,
/// no existence check) on `enable()`, so presence alone is the write side of
/// the truth — this app never needs to compare the value's *contents* (the
/// exact command line and its args) because both the write (the plugin,
/// configured once in `lib.rs`) and this read agree on `app_name` being the
/// only thing that varies.
fn run_key_has_entry(app_name: &str) -> bool {
    let subkey = wide(RUN_KEY);
    let value = wide(app_name);
    let status = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            PCWSTR(subkey.as_ptr()),
            PCWSTR(value.as_ptr()),
            RRF_RT_REG_SZ,
            None,
            None,
            None,
        )
    };
    status == ERROR_SUCCESS
}

/// Whether Explorer's `StartupApproved\Run` override blocks the entry. No
/// recorded override at all (the common case for an entry nobody has ever
/// touched from Task Manager / Settings > Startup Apps) reads as *not*
/// blocked. The byte decoding is [`blob_marks_disabled`] — split out so it's
/// pinned by tests against a real disabled blob, not just the read plumbing.
fn startup_approved_blocks(app_name: &str) -> bool {
    let subkey = wide(STARTUP_APPROVED_KEY);
    let value = wide(app_name);
    let mut buf = [0u8; 32];
    let mut size = buf.len() as u32;
    let status = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            PCWSTR(subkey.as_ptr()),
            PCWSTR(value.as_ptr()),
            RRF_RT_REG_BINARY,
            None,
            Some(buf.as_mut_ptr().cast()),
            Some(&mut size),
        )
    };
    status == ERROR_SUCCESS && blob_marks_disabled(&buf[..size as usize])
}

/// Decodes a raw `StartupApproved\Run` value. 12 bytes: the first DWORD is
/// the enabled/disabled marker, the trailing QWORD (only meaningful when
/// disabled) is the `FILETIME` of the disable.
///
/// Checked against two independent sources: (1) the pinned `auto-launch`
/// 0.5.0 dependency this app also uses for `enable()`/`disable()`
/// (`auto-launch-0.5.0/src/windows.rs`), whose
/// `enable()` writes exactly `[0x02, 0x00 x11]`; and (2) the publicly
/// documented format (Harlan Carvey, "StartupApproved\Run, pt II",
/// windowsir.blogspot.com, 2022-07): first DWORD `0x02` *or* `0x06` means
/// enabled, `0x03` means disabled. Deliberately checking for `0x03` rather
/// than "not `0x02`": the `0x06` encoding is a real enabled state (Explorer
/// re-enabling a previously-disabled entry can leave a non-zero trailing
/// timestamp on an otherwise-enabled value), and `auto-launch`'s own
/// `is_enabled()` gets exactly this case wrong — it infers the state from
/// whether the trailing 8 bytes are all zero
/// (`last_eight_bytes_all_zeros`, windows.rs) rather than reading the first
/// byte, so a `0x06` entry with a real timestamp reads as disabled under its
/// rule. Checking the documented marker byte directly doesn't share that
/// failure mode.
fn blob_marks_disabled(bytes: &[u8]) -> bool {
    bytes.first() == Some(&0x03)
}

/// The TRUE launch-at-login state for `app_name` (Tauri's
/// `package_info().name`, the same value the plugin registers the entry
/// under): the `Run` key carries this app's entry AND Explorer hasn't
/// disabled it out from under the toggle. This reflects a disable from Task
/// Manager, unlike a raw `Run`-key-only read (or the persisted
/// `Settings::app.launch_at_login`, which only records this app's own last
/// write and has no way to see a later disable from outside it).
pub fn true_state(app_name: &str) -> bool {
    run_key_has_entry(app_name) && !startup_approved_blocks(app_name)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Not a live-registry test (that needs an actual login-item entry to
    /// exist, which unit tests must not depend on) — just pins that an
    /// unregistered, made-up app name reads as disabled rather than
    /// panicking or false-reporting enabled. The registry reads themselves
    /// run for real on every launch via `commands::get_settings`'s call
    /// into `true_state`.
    #[test]
    fn an_app_name_with_no_registry_entry_reads_as_disabled() {
        assert!(!true_state("ButterflySpeak — nonexistent test entry — 3f9a2c"));
    }

    #[test]
    fn the_tray_start_arg_looks_like_an_option() {
        assert!(TRAY_START_ARG.starts_with("--"));
    }

    /// The exact value `auto-launch` 0.5.0's `enable()` writes
    /// (`TASK_MANAGER_OVERRIDE_ENABLED_VALUE` in its `windows.rs`): a real
    /// enabled blob, not a hypothetical one.
    #[test]
    fn the_auto_launch_enabled_blob_is_not_blocked() {
        assert!(!blob_marks_disabled(&[
            0x02, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00
        ]));
    }

    /// A real disabled blob — first DWORD `0x03`, trailing QWORD a genuine
    /// (non-zero) `FILETIME`. This is also the case `auto-launch`'s own
    /// `last_eight_bytes_all_zeros` heuristic happens to get right (a
    /// non-zero tail reads as disabled there too); the case it gets *wrong*
    /// is the next test.
    #[test]
    fn a_real_disabled_blob_with_a_nonzero_timestamp_is_blocked() {
        assert!(blob_marks_disabled(&[
            0x03, 0x00, 0x00, 0x00, 0x11, 0x89, 0x89, 0x59, 0x5C, 0xE7, 0xDB, 0x01
        ]));
    }

    /// `0x06` is a documented *enabled* encoding (Explorer re-enabling a
    /// previously-disabled entry) that can still carry a non-zero trailing
    /// timestamp. `auto-launch`'s own heuristic
    /// (`last_eight_bytes_all_zeros`) would misread this as disabled;
    /// reading the marker byte directly must not.
    #[test]
    fn a_reenabled_blob_with_a_leftover_timestamp_is_not_blocked() {
        assert!(!blob_marks_disabled(&[
            0x06, 0x00, 0x00, 0x00, 0x11, 0x89, 0x89, 0x59, 0x5C, 0xE7, 0xDB, 0x01
        ]));
    }

    #[test]
    fn a_missing_value_reads_as_not_blocked() {
        assert!(!blob_marks_disabled(&[]));
    }
}
