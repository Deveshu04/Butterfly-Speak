//! OS session events: session lock, seen through
//! `WTSRegisterSessionNotification` on a message-only window, and system
//! resume, seen through `PowerRegisterSuspendResumeNotification`. One thread
//! owns both.
//!
//! **Session lock.** The low-level keyboard hook's held-key set
//! (`hotkeys::spawn`'s `down`) goes stale when the secure desktop takes over
//! (Win+L, a UAC prompt): key-ups never reach a low-level hook running on
//! the interactive desktop, so a modifier can stay "held" until the app
//! restarts. `hotkeys::prune_stale_modifiers` repairs that key by key, on
//! the next real press. This module gives the hook an immediate signal
//! instead: `WM_WTSSESSION_CHANGE` / `WTS_SESSION_LOCK` arrives the moment
//! Windows locks the session, and the flag it sets is drained on the hook's
//! next event rather than waiting for `GetAsyncKeyState` to notice one key
//! at a time.
//!
//! The same moment also goes straight to the controller as
//! `ControlMsg::SessionLocked`, which cancels a live recording. The hook
//! cannot do that part: it hears nothing until the session is unlocked, and
//! a recording left running would record the room through the lock.
//!
//! **System resume.** `PBT_APMRESUMEAUTOMATIC` fires when the machine wakes
//! from suspend. A cloud dictation's WebSocket does not survive S3 sleep,
//! but nothing tells the dispatcher task until its own timeouts elapse. This
//! sends `ControlMsg::SystemResumed` straight to the controller, so an
//! in-flight cloud session is invalidated as soon as the OS says the machine
//! is back. Like `SessionLocked`, and unlike the hook's session-lock flag,
//! this is pushed rather than polled.
//!
//! Resume is not read as `WM_POWERBROADCAST` from this module's message
//! loop. Message-only windows (`HWND_MESSAGE`) never receive broadcast
//! messages, and `WM_POWERBROADCAST` is a *sent* message: `GetMessageW`
//! dispatches pending sent messages to the window procedure before it
//! returns the next posted one, so the loop would never see it even on a
//! top-level window. `PowerRegisterSuspendResumeNotification` with
//! `DEVICE_NOTIFY_CALLBACK` avoids both: the OS calls `power_callback`
//! directly, with no window or message queue involved.

use crossbeam_channel::Sender;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use windows::Win32::Foundation::{ERROR_SUCCESS, HANDLE, HWND};
use windows::Win32::System::Power::{
    DEVICE_NOTIFY_SUBSCRIBE_PARAMETERS, PowerRegisterSuspendResumeNotification,
};
use windows::Win32::System::RemoteDesktop::{NOTIFY_FOR_THIS_SESSION, WTSRegisterSessionNotification};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DEVICE_NOTIFY_CALLBACK, DispatchMessageW, GetMessageW, HWND_MESSAGE, MSG,
    PBT_APMRESUMEAUTOMATIC, TranslateMessage, WINDOW_EX_STYLE, WINDOW_STYLE, WM_WTSSESSION_CHANGE,
    WTS_SESSION_LOCK,
};
use windows::core::PCWSTR;

use crate::state::ControlMsg;

/// Invoked directly by the OS on every suspend/resume transition, once
/// `register_resume_notification` below has registered it — no window, no
/// message queue, none of the delivery pitfalls this module's own doc
/// comment describes for `WM_POWERBROADCAST`. `context` is the raw pointer
/// to the leaked `Sender<ControlMsg>` `register_resume_notification` passed
/// as `Context` at registration time: reconstructed as a borrow, never taken
/// by value, so the sender is never dropped out from under a registration
/// this module never unregisters (see `spawn`'s doc comment on
/// process-lifetime cleanup).
unsafe extern "system" fn power_callback(
    context: *const core::ffi::c_void,
    event_type: u32,
    _setting: *const core::ffi::c_void,
) -> u32 {
    if event_type == PBT_APMRESUMEAUTOMATIC {
        let ctl_tx = unsafe { &*context.cast::<Sender<ControlMsg>>() };
        let _ = ctl_tx.send(ControlMsg::SystemResumed);
    }
    0 // NO_ERROR — this return value is only inspected for power-*setting*
      // callbacks (to veto/defer a change), never for APM suspend/resume
      // events like this one.
}

/// Registers `power_callback` for suspend/resume notifications. Independent
/// of the message-only window `run` creates below for `WTSRegisterSessionNotification`
/// — `DEVICE_NOTIFY_CALLBACK` needs no window at all — so this runs first and
/// resume detection still works even if window creation fails afterwards.
///
/// Both the leaked `Sender` clone and the `DEVICE_NOTIFY_SUBSCRIBE_PARAMETERS`
/// the OS reads at registration time need `'static` storage a stack frame
/// can't provide, and — like the window and the WTS registration below —
/// there is no shutdown hook to free them from, so they are deliberately
/// leaked for the OS to reclaim when the process exits.
fn register_resume_notification(ctl_tx: &Sender<ControlMsg>) {
    let tx: &'static Sender<ControlMsg> = Box::leak(Box::new(ctl_tx.clone()));
    let params: &'static DEVICE_NOTIFY_SUBSCRIBE_PARAMETERS =
        Box::leak(Box::new(DEVICE_NOTIFY_SUBSCRIBE_PARAMETERS {
            Callback: Some(power_callback),
            Context: (tx as *const Sender<ControlMsg>).cast_mut().cast(),
        }));
    let mut registration_handle: *mut core::ffi::c_void = std::ptr::null_mut();
    // Not a real handle: when `Flags` is `DEVICE_NOTIFY_CALLBACK`,
    // `Recipient` is documented to be a pointer to the
    // `DEVICE_NOTIFY_SUBSCRIBE_PARAMETERS` above, reinterpreted as `HANDLE`
    // only because that's this function's one raw parameter type for both
    // registration modes.
    let recipient = HANDLE(std::ptr::from_ref(params) as *mut core::ffi::c_void);
    let err = unsafe {
        PowerRegisterSuspendResumeNotification(DEVICE_NOTIFY_CALLBACK, recipient, &mut registration_handle)
    };
    if err != ERROR_SUCCESS {
        tracing::warn!(
            "PowerRegisterSuspendResumeNotification failed ({err:?}); \
             a stuck cloud session after sleep will only recover on its own timeout"
        );
    }
}

/// UTF-16, NUL-terminated: the shape every wide-string Win32 call here needs.
fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Spawn the message-only window + notification thread. `reset` is set to
/// `true` on `WTS_SESSION_LOCK` and drained by the hotkey hook on its next
/// event (`hotkeys::reset_after_session_lock`), and `ctl_tx` gets a
/// `ControlMsg::SessionLocked` at the same moment. `ctl_tx` also gets a
/// `ControlMsg::SystemResumed` pushed on `PBT_APMRESUMEAUTOMATIC`, via
/// `power_callback` rather than this window's own message loop — see this
/// module's doc comment for why. A send error (the controller thread is
/// gone) is silently ignored, the same as every other best-effort
/// `ctl_tx.send` in this app during shutdown.
///
/// Process-lifetime: there is no app-shutdown hook to unregister either
/// notification or destroy the window from, so all of it is left for the OS
/// to reclaim when the process exits rather than chasing a clean teardown
/// this app never needs.
pub fn spawn(reset: Arc<AtomicBool>, ctl_tx: Sender<ControlMsg>) {
    std::thread::Builder::new()
        .name("system-events".into())
        .spawn(move || run(&reset, &ctl_tx))
        .expect("spawn system-events thread");
}

fn run(reset: &AtomicBool, ctl_tx: &Sender<ControlMsg>) {
    // Independent of the window below — see `register_resume_notification`'s
    // doc comment — so it runs first and keeps working even if window
    // creation fails.
    register_resume_notification(ctl_tx);

    // A message-only window needs *a* window class to be created with, but
    // not a custom one: "STATIC" is always pre-registered by user32, and its
    // default window procedure is exactly what a window that only exists to
    // receive `WTSRegisterSessionNotification`'s posts needs — nothing is
    // ever drawn or dispatched to the user. `WM_WTSSESSION_CHANGE` needs no
    // custom WNDPROC either: the message loop below inspects each `MSG`
    // directly, before dispatching it to that default procedure.
    let class_name = wide("STATIC");
    let window_name = wide("");
    let hwnd = unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            PCWSTR(class_name.as_ptr()),
            PCWSTR(window_name.as_ptr()),
            WINDOW_STYLE(0),
            0,
            0,
            0,
            0,
            Some(HWND_MESSAGE),
            None,
            None,
            None,
        )
    };
    let hwnd: HWND = match hwnd {
        Ok(h) => h,
        Err(e) => {
            tracing::warn!(
                "system-events notification window couldn't be created ({e}); \
                 a lock will not cancel a recording, and the hook's held-key state \
                 will only recover key-by-key"
            );
            return;
        }
    };
    if let Err(e) = unsafe { WTSRegisterSessionNotification(hwnd, NOTIFY_FOR_THIS_SESSION) } {
        tracing::warn!(
            "WTSRegisterSessionNotification failed ({e}); \
             a lock will not cancel a recording, and the hook's held-key state \
             will only recover key-by-key"
        );
        // Session-lock detection is unavailable, but resume detection was
        // already registered above, independent of this window entirely —
        // keep pumping messages rather than returning.
    }

    let mut msg = MSG::default();
    loop {
        // GetMessageW's return is tri-state: >0 a real message, 0 is
        // WM_QUIT, <0 is an error. This window never receives WM_QUIT (no
        // WM_CLOSE handler posts it), so either non-positive case means the
        // thread's message queue is gone and there's nothing left to pump.
        let ok = unsafe { GetMessageW(&mut msg, None, 0, 0) };
        if ok.0 <= 0 {
            break;
        }
        if msg.message == WM_WTSSESSION_CHANGE && msg.wParam.0 as u32 == WTS_SESSION_LOCK {
            reset.store(true, Ordering::Relaxed);
            let _ = ctl_tx.send(ControlMsg::SessionLocked);
        }
        unsafe {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}
