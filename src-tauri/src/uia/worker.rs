//! The one thread in Butterfly Speak that touches a UI Automation pointer.
//!
//! Everything here runs on the STA thread [`super::worker`] spawns. Nothing
//! here is `pub` beyond [`run`], nothing here returns a COM object to a
//! caller, and no `IUIAutomation*` value is ever moved off this thread — the
//! elements live in a map keyed by the id that [`super::UiaHandle`] carries.
//!
//! PRIVACY: the `BSTR`s read below are the user's field content. They are
//! turned into `String`s and sent straight back to the requester; they are
//! never logged, never held past the reply, and never inspected for anything
//! but length. The only thing this file logs is an `HRESULT`.

use super::{still_wanted, Req, UiaError};
use crossbeam_channel::Receiver;
use std::collections::HashMap;
use windows::core::Interface;
use windows::Win32::Foundation::HWND;
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_INPROC_SERVER,
    COINIT_APARTMENTTHREADED,
};
use windows::Win32::UI::Accessibility::{
    CUIAutomation, IUIAutomation, IUIAutomationElement, IUIAutomationTextPattern,
    IUIAutomationValuePattern, UIA_PATTERN_ID, UIA_TextPatternId, UIA_ValuePatternId,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetAncestor, GetWindowThreadProcessId, IsChild, IsWindow, GA_ROOTOWNER,
};

/// Enter the apartment, create the single `IUIAutomation`, then serve
/// requests until every sender is gone.
///
/// Any failure on the way in is terminal *for the session*: the thread
/// returns, its receiver drops, and every subsequent
/// `Sender::send` in the parent module fails — which the client side already
/// turns into [`UiaError::Unavailable`]. That is the right outcome. A machine
/// where COM or the UIA class object will not come up is a machine where
/// there is nothing to retry into, and callers all have a fallback.
pub(super) fn run(rx: Receiver<Req>) {
    unsafe {
        let hr = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        if hr.is_err() {
            // Includes RPC_E_CHANGED_MODE (some other library got here first
            // and made this an MTA). Returning without CoUninitialize is
            // required on that path — the apartment is not ours to leave.
            tracing::warn!("UI Automation off this session: CoInitializeEx {hr:?}");
            return;
        }

        let created: windows::core::Result<IUIAutomation> =
            CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER);
        match created {
            Ok(automation) => serve(&automation, rx),
            Err(e) => tracing::warn!(
                "UI Automation off this session: CUIAutomation {:?}",
                e.code()
            ),
        }

        CoUninitialize();
    }
}

/// The request loop. Single-threaded by construction, so the element map
/// needs no lock and ids are handed out from a plain counter.
unsafe fn serve(automation: &IUIAutomation, rx: Receiver<Req>) {
    let mut elements: HashMap<u64, IUIAutomationElement> = HashMap::new();
    let mut next_id: u64 = 1;

    while let Ok(req) = rx.recv() {
        match req {
            // No deadline check: a release must always be honoured, or the
            // element outlives its handle.
            Req::Release { id } => {
                elements.remove(&id);
            }
            Req::Element {
                hwnd,
                deadline,
                reply,
            } => {
                if !still_wanted(deadline) {
                    continue;
                }
                match resolve(automation, hwnd) {
                    Ok(element) => {
                        // Re-check the deadline: `resolve` is the slow part
                        // of this arm (up to several cross-process calls),
                        // so the caller can easily have given up while it
                        // ran. Dropping `element` here is free — no handle
                        // was ever minted for it.
                        if !still_wanted(deadline) {
                            continue;
                        }
                        let id = next_id;
                        next_id += 1;
                        // Publish the element only once the caller has taken
                        // its id: a caller that never receives one will never
                        // drop a handle, and an element inserted first would
                        // live as long as the process.
                        //
                        // This narrows the race to the gap between the check
                        // above and this send, but it does NOT close it. The
                        // caller's `recv_deadline` can expire an instant
                        // before `reply_rx` is actually dropped, and a send
                        // landing in that gap succeeds into the `bounded(1)`
                        // buffer that nobody will ever read — one element
                        // leaked for the life of the process. Closing it
                        // properly needs an ack from the caller, which would
                        // add a round trip to every bind to reclaim one
                        // element in a race that is nanoseconds wide and can
                        // only happen on a bind that timed out anyway.
                        if reply.send(Ok(id)).is_ok() {
                            elements.insert(id, element);
                        }
                    }
                    Err(e) => {
                        let _ = reply.send(Err(e));
                    }
                }
            }
            Req::Value {
                id,
                deadline,
                reply,
            } => {
                if !still_wanted(deadline) {
                    continue;
                }
                let answer = match elements.get(&id) {
                    Some(element) => value_of(element),
                    // The handle outlived its element only if something
                    // released it early; treat an unknown id the same as a
                    // dead element rather than as a defect.
                    None => Err(UiaError::Unavailable),
                };
                let _ = reply.send(answer);
            }
            Req::Selection {
                id,
                deadline,
                reply,
            } => {
                if !still_wanted(deadline) {
                    continue;
                }
                let answer = match elements.get(&id) {
                    Some(element) => selection_of(element),
                    None => Err(UiaError::Unavailable),
                };
                let _ = reply.send(answer);
            }
        }
    }
}

/// Turn an `HRESULT`-bearing COM error into this module's two-way split.
fn map(e: windows::core::Error) -> UiaError {
    UiaError::from_hresult(e.code().0)
}

/// The focused element inside `window`, or [`UiaError::Unavailable`] if focus
/// is somewhere else or the field is a password box.
///
/// `GetFocusedElement` rather than `ElementFromHandle(window)` because a top
/// level window's own element implements no text pattern — the field is a
/// descendant, and in a windowless UI (WinUI, Chromium) that descendant has
/// no handle of its own to look up. The containment check is what ties the
/// focused element to the window the caller named.
///
/// The password check is here, at the bind, rather than on each read: the
/// element a handle names is fixed the moment it is resolved and `IsPassword`
/// does not change under it, so one property read per bind covers every later
/// [`value_of`] and [`selection_of`] through that handle — and a password box
/// never gets a handle at all, which is a stronger guarantee than remembering
/// to check at two read sites. [`UiaError::Unavailable`] rather than
/// `Ok(None)` is chosen deliberately: `Ok(None)` means "not a text field,
/// nothing to watch", which leaves the field monitor happily polling a
/// password box for its whole 30 s window; `Unavailable` is the answer every
/// caller already treats as "stop, and say nothing".
unsafe fn resolve(
    automation: &IUIAutomation,
    hwnd: isize,
) -> Result<IUIAutomationElement, UiaError> {
    let window = HWND(hwnd as *mut core::ffi::c_void);
    if !IsWindow(Some(window)).as_bool() {
        // The window the caller captured has closed. Not an error worth an
        // HRESULT: the answer is just "no".
        return Err(UiaError::Unavailable);
    }
    let focused = automation.GetFocusedElement().map_err(map)?;
    if !belongs_to(&focused, window) {
        return Err(UiaError::Unavailable);
    }
    let is_password = focused.CurrentIsPassword().ok().map(|b| b.as_bool());
    if !super::is_readable_field(is_password) {
        return Err(UiaError::Unavailable);
    }
    Ok(focused)
}

/// Whether `element` is inside `window`.
///
/// Two tiers, because providers differ in what they will admit to:
///
/// 1. If the element reports a native window handle, that handle must be
///    `window` itself, one of its child windows, or a window it owns (a combo
///    dropdown or an autocomplete list is part of the window that put it
///    there, and `IsChild` says no for an owned window — hence the
///    `GA_ROOTOWNER` walk alongside it). This tier is exact and it is a real
///    filter, not a formality: Windows 11's Notepad keeps several top-level
///    windows in one process, and a handle captured while the foreground was
///    still moving names one of them while focus is in another. Refusing that
///    is the whole point.
/// 2. If the element reports no handle — normal for the leaves of a
///    windowless UI, where a whole XAML or Chromium tree hangs off one HWND —
///    fall back to process identity. Weaker: it cannot tell two windows of
///    the same app apart. It is still the difference between "some field in
///    the app I pasted into" and "whatever had focus when I got round to
///    asking".
unsafe fn belongs_to(element: &IUIAutomationElement, window: HWND) -> bool {
    match element.CurrentNativeWindowHandle() {
        Ok(native) if !native.is_invalid() => {
            native.0 == window.0
                || IsChild(window, native).as_bool()
                || GetAncestor(native, GA_ROOTOWNER).0 == window.0
        }
        _ => match (element.CurrentProcessId(), window_pid(window)) {
            (Ok(element_pid), Some(window_pid)) => element_pid as u32 == window_pid,
            _ => false,
        },
    }
}

unsafe fn window_pid(window: HWND) -> Option<u32> {
    let mut pid = 0u32;
    GetWindowThreadProcessId(window, Some(&mut pid));
    (pid != 0).then_some(pid)
}

/// `Ok(Some(_))` is the pattern's own object, so a missing pattern is
/// `Ok(None)` rather than an error — see `super::is_pattern_missing` for why
/// a missing pattern can arrive as an `Err` carrying `S_OK`.
unsafe fn pattern<T: Interface>(
    element: &IUIAutomationElement,
    id: UIA_PATTERN_ID,
) -> Result<Option<T>, UiaError> {
    match element.GetCurrentPatternAs::<T>(id) {
        Ok(p) => Ok(Some(p)),
        Err(e) if super::is_pattern_missing(e.code().0) => Ok(None),
        Err(e) => Err(map(e)),
    }
}

/// The bound field's whole current text, or `Ok(None)` when the element has
/// neither `ValuePattern` nor `TextPattern`.
///
/// `ValuePattern` is asked first. On Notepad's editor, Notepad's find box (a
/// WinUI text box), a Chromium `<textarea>` and VS Code's editor, the two
/// patterns return the same text, line breaks included, and the value
/// answers faster: a median of 1.4 to 2.0 ms against 2.1 to 8.5 ms for the
/// whole document range, over 25 reads of a 1,500-character field.
/// `TextPattern` answers when `ValuePattern` is missing, fails, or reports an
/// empty value, since a control can expose an empty value and keep its text
/// behind `TextPattern`.
///
/// A failure is not "not a text field". When one pattern fails and the other
/// is missing or empty, the failure comes back rather than `Ok(None)`:
/// `learn::monitor` stops watching an element that answers `Ok(None)`, and a
/// single refused read must not end the watch on a field it can read.
///
/// PRIVACY: the `String` returned here is field content. It goes straight
/// back down the reply channel and is never logged or retained.
unsafe fn value_of(element: &IUIAutomationElement) -> Result<Option<String>, UiaError> {
    let by_value = value_pattern_text(element);
    if matches!(&by_value, Ok(Some(text)) if !text.is_empty()) {
        return by_value;
    }
    match (by_value, text_pattern_text(element)) {
        (_, Ok(Some(text))) if !text.is_empty() => Ok(Some(text)),
        (Err(e), _) | (_, Err(e)) => Err(e),
        (Ok(Some(empty)), _) | (_, Ok(Some(empty))) => Ok(Some(empty)),
        (Ok(None), Ok(None)) => Ok(None),
    }
}

/// `ValuePattern`'s current value, `Ok(None)` without the pattern.
unsafe fn value_pattern_text(element: &IUIAutomationElement) -> Result<Option<String>, UiaError> {
    let Some(value) = pattern::<IUIAutomationValuePattern>(element, UIA_ValuePatternId)? else {
        return Ok(None);
    };
    Ok(Some(value.CurrentValue().map_err(map)?.to_string()))
}

/// The text of `TextPattern`'s whole document range, `Ok(None)` without the
/// pattern. `GetText(-1)` means no length limit.
unsafe fn text_pattern_text(element: &IUIAutomationElement) -> Result<Option<String>, UiaError> {
    let Some(text) = pattern::<IUIAutomationTextPattern>(element, UIA_TextPatternId)? else {
        return Ok(None);
    };
    let document = text.DocumentRange().map_err(map)?;
    Ok(Some(document.GetText(-1).map_err(map)?.to_string()))
}

/// PRIVACY: the `String` returned here is field content. It goes straight
/// back down the reply channel and is never logged or retained.
unsafe fn selection_of(element: &IUIAutomationElement) -> Result<Option<String>, UiaError> {
    let Some(text) = pattern::<IUIAutomationTextPattern>(element, UIA_TextPatternId)? else {
        return Ok(None);
    };

    let ranges = text.GetSelection().map_err(map)?;
    let count = ranges.Length().map_err(map)?;
    let mut selected = String::new();
    for i in 0..count {
        let range = ranges.GetElement(i).map_err(map)?;
        selected.push_str(&range.GetText(-1).map_err(map)?.to_string());
    }

    // A caret with no selection reports one range of zero length; that and
    // "no ranges at all" are the same answer to a caller.
    if selected.is_empty() {
        Ok(None)
    } else {
        Ok(Some(selected))
    }
}
