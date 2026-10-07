//! UI Automation reads: what is in the field the user is typing into, and
//! what they have selected in it.
//!
//! # PRIVACY: everything this module returns is field content
//! [`focused_value`] and [`selection_text`] hand back whatever the user's
//! document currently holds — a password box that reports a value, a private
//! message, a colleague's address. **Nothing in this module or any caller may
//! log it, persist it, put it in an event payload, or send it anywhere.** A
//! field monitor that prints what it reads puts the user's documents in a log
//! file, which is why the rule is written on each returning function as well
//! as here. The only values this module ever logs are `HRESULT`s.
//!
//! One field is refused outright rather than merely handled carefully: a
//! password box. [`element_for_hwnd`] reads `IsPassword` once and will not
//! hand back a handle for one, so no caller can read a password even by
//! mistake — see [`is_readable_field`].
//!
//! # One thread owns UIA, forever
//! UI Automation is COM. A COM interface pointer belongs to the apartment it
//! was created in, and Butterfly Speak has no COM apartment of its own —
//! `media.rs`'s GSMTC use goes through WinRT, and nothing else in the app has
//! ever called `CoInitializeEx`. So this module creates one: a dedicated STA
//! thread (`COINIT_APARTMENTTHREADED`) that creates the single
//! `IUIAutomation` instance and is the only thread that ever touches a UIA
//! pointer. Callers get [`UiaHandle`], an opaque id that is `Send` because it
//! is just a `u64` and a channel sender; the element it names never leaves
//! the thread, and dropping the handle sends the release back to that thread
//! rather than releasing a COM pointer from wherever the drop happened.
//!
//! The shape (one thread owns a `!Send` resource, everyone else talks to it
//! over a channel, replies come back on a per-request oneshot) is the same
//! one `history::spawn` uses for `rusqlite::Connection`; see that module for
//! the precedent.
//!
//! No message pump runs on that thread, which is deliberate and has one
//! consequence worth knowing: an STA with no pump cannot receive COM
//! callbacks, so this module can never register a UIA event handler. Reads
//! only — which is all the polling monitor that consumes it needs, and it
//! keeps the module to its other rule: never turn the target's accessibility
//! tree on as a side effect of watching it. Read-only, or skip.
//!
//! # Every call is on a leash
//! A UIA read is a cross-process call into an app the user chose, which may
//! be busy, hung, or hostile. [`CALL_TIMEOUT`] bounds the *caller*: after
//! 200 ms it stops waiting and gets [`UiaError::Unavailable`], and falls back
//! to whatever it did before UIA existed. It does not bound the worker — a
//! wedged provider still has that thread — so each request also carries the
//! caller's deadline, and the worker drops requests whose caller has already
//! given up ([`still_wanted`]) instead of working through a backlog of reads
//! nobody is listening for.
//!
//! # The bind is the one call that is retried
//! Binding is focus-dependent in a way the reads through a handle are not.
//! [`element_for_hwnd`] resolves the *focused* element and then requires it to
//! belong to the window it was asked about, so a target that is still settling
//! answers [`UiaError::Unavailable`] to a question it would have answered a
//! moment later — Windows 11's Notepad creates and activates its window over a
//! second after the spawn returns, and roughly one bind in three declines
//! transiently on Notepad *and* VS Code. One ask turns "not yet" into "never".
//!
//! So the bind, and only the bind, is retried: [`BIND_ATTEMPTS`] asks
//! [`BIND_RETRY_DELAY`] apart. Two things are deliberately outside that.
//! [`UiaError::Com`] is not retried — it is an `HRESULT` nobody has
//! classified, which is a defect to look at rather than a race to wait out,
//! and asking again would only log it three times. And [`focused_value`] /
//! [`selection_text`] are not retried — an `Unavailable` through an
//! already-bound handle means the element has gone, which is a caller's cue to
//! stop rather than to wait.
//!
//! The ladder lives here rather than in a caller so there is one of it: a
//! caller that retried on top of it would compound to nine attempts.
//! [`BIND_BUDGET`] is a constant for the same reason: a caller that has to
//! price this ([`routes::selection`]'s probe budget, which is itself a term in
//! `controller::CLOUD_FINALIZE_TIMEOUT`) must not re-derive the arithmetic
//! and drift.
//!
//! [`routes::selection`]: crate::routes::selection

mod worker;

use crossbeam_channel::Sender;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

/// How long any single UIA round trip may take before the caller gives up and
/// treats the window as unavailable.
///
/// This is a *caller* budget, not a provider budget: there is no way to
/// cancel a COM call in flight. 200 ms is chosen against what the calls cost
/// when they work (a cross-process property read is sub-millisecond) rather
/// than against how slow a bad provider can be — anything past that is a
/// provider that is not going to answer usefully, and both consumers have a
/// silent fallback ready.
pub const CALL_TIMEOUT: Duration = Duration::from_millis(200);

/// How many times [`element_for_hwnd`] asks for the element before giving up,
/// and how long it waits between asks.
///
/// `learn::monitor` calls this once rather than retrying on top of it, so the
/// total stays three attempts rather than compounding to nine. 300 ms is the
/// same order as the settle a paste is given before the monitor starts, and
/// three is what a window that is mid-activation needs: Notepad has been seen
/// taking over a second to bind after activation, and about one bind in
/// three fails that way.
///
/// Neither is tuned finer than that, and neither should be: the honest bound
/// is [`BIND_BUDGET`], and every caller either has a silent fallback or has
/// priced it.
pub const BIND_ATTEMPTS: u32 = 3;

/// See [`BIND_ATTEMPTS`].
pub const BIND_RETRY_DELAY: Duration = Duration::from_millis(300);

/// The worst a single [`element_for_hwnd`] can cost the thread that calls it:
/// every attempt runs its [`CALL_TIMEOUT`] out in full, with a
/// [`BIND_RETRY_DELAY`] between each pair of attempts and **none after the
/// last** — a wait after the final failure would buy nothing.
///
/// 3 × 200 ms + 2 × 300 ms = 1.2 s. Public because the selection lane spends
/// it inside a budget the finalize watchdog itemizes, and re-deriving this
/// there is exactly how the two would drift apart.
pub const BIND_BUDGET: Duration = CALL_TIMEOUT
    .saturating_mul(BIND_ATTEMPTS)
    .saturating_add(BIND_RETRY_DELAY.saturating_mul(BIND_ATTEMPTS - 1));

/// Why a UIA read did not produce text.
///
/// The split is about what the caller should *do*, not about severity:
///
/// - [`UiaError::Unavailable`] — this window will not answer. An elevated
///   target refusing a non-elevated client, a provider that never loaded, an
///   element that has gone away, a call that ran past [`CALL_TIMEOUT`]. All
///   of these are ordinary on a real desktop, all of them are the user's
///   choice of app rather than a defect, and every caller has a silent
///   fallback. Never surfaced to the user, never worth a log line.
/// - [`UiaError::Com`] — an `HRESULT` this module does not recognise as a
///   refusal. Worth a log line so it can be looked at, and the `HRESULT` is
///   the *only* thing that may go into it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UiaError {
    Unavailable,
    Com(i32),
}

impl std::fmt::Display for UiaError {
    /// Carries an `HRESULT` and nothing else — see the module's privacy note.
    /// This type never holds field content, so there is nothing here to leak,
    /// and that must stay true if a variant is ever added.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            UiaError::Unavailable => write!(f, "UI Automation unavailable for this window"),
            UiaError::Com(hr) => write!(f, "UI Automation call failed (HRESULT 0x{hr:08X})"),
        }
    }
}

impl std::error::Error for UiaError {}

// HRESULTs this module classifies. `windows` exposes the UIA-specific ones as
// `u32`; the COM/RPC ones it does not expose at all under the features this
// crate enables, so they are written out here with their canonical names.
const S_OK: i32 = 0;
const E_NOTIMPL: i32 = 0x8000_4001u32 as i32;
const E_NOINTERFACE: i32 = 0x8000_4002u32 as i32;
const E_POINTER: i32 = 0x8000_4003u32 as i32;
const E_ACCESSDENIED: i32 = 0x8007_0005u32 as i32;
const E_HANDLE: i32 = 0x8007_0006u32 as i32;
const CO_E_OBJNOTCONNECTED: i32 = 0x8004_01FDu32 as i32;
const RPC_E_CALL_REJECTED: i32 = 0x8001_0001u32 as i32;
const RPC_E_CALL_CANCELED: i32 = 0x8001_0002u32 as i32;
const RPC_E_SERVERFAULT: i32 = 0x8001_0105u32 as i32;
const RPC_E_DISCONNECTED: i32 = 0x8001_0108u32 as i32;
const RPC_E_SERVERCALL_RETRYLATER: i32 = 0x8001_010Au32 as i32;
const RPC_E_TIMEOUT: i32 = 0x8001_011Fu32 as i32;
const RPC_S_SERVER_UNAVAILABLE: i32 = 0x8007_06BAu32 as i32;
const RPC_S_CALL_FAILED: i32 = 0x8007_06BEu32 as i32;

/// The `HRESULT`s that mean "this element does not implement that pattern".
///
/// `S_OK` is in the list and is not a typo. `IUIAutomationElement::
/// GetCurrentPatternAs` answers a missing pattern by succeeding with a null
/// out-pointer, and the `windows` binding turns a null out-pointer into
/// `Err(Error::empty())`, whose `code()` is `S_OK` — so "no ValuePattern
/// here" arrives as an `Err` carrying success. Classifying it anywhere else
/// would turn the single most common outcome on the planet (a window that
/// simply isn't a text field) into a logged COM error.
///
/// `UIA_E_INVALIDOPERATION` is deliberately NOT here. It means "not right
/// now", not "not ever", and the two answers differ for a caller: `Ok(None)`
/// says this is not a text field and there is nothing to come back for, while
/// [`UiaError::Unavailable`] says stop asking this element.
fn is_pattern_missing(hr: i32) -> bool {
    matches!(
        hr,
        S_OK | E_NOTIMPL | E_NOINTERFACE | E_POINTER | UIA_E_NOTSUPPORTED
    )
}

// The UIA-specific codes, re-declared as `i32` so the tables above and below
// read as one list. Values come from `windows::Win32::UI::Accessibility`,
// which declares them as `u32`.
const UIA_E_ELEMENTNOTAVAILABLE: i32 =
    windows::Win32::UI::Accessibility::UIA_E_ELEMENTNOTAVAILABLE as i32;
const UIA_E_ELEMENTNOTENABLED: i32 =
    windows::Win32::UI::Accessibility::UIA_E_ELEMENTNOTENABLED as i32;
const UIA_E_INVALIDOPERATION: i32 =
    windows::Win32::UI::Accessibility::UIA_E_INVALIDOPERATION as i32;
const UIA_E_NOCLICKABLEPOINT: i32 =
    windows::Win32::UI::Accessibility::UIA_E_NOCLICKABLEPOINT as i32;
const UIA_E_NOTSUPPORTED: i32 = windows::Win32::UI::Accessibility::UIA_E_NOTSUPPORTED as i32;
const UIA_E_PROXYASSEMBLYNOTLOADED: i32 =
    windows::Win32::UI::Accessibility::UIA_E_PROXYASSEMBLYNOTLOADED as i32;
const UIA_E_TIMEOUT: i32 = windows::Win32::UI::Accessibility::UIA_E_TIMEOUT as i32;

impl UiaError {
    /// Classify an `HRESULT` from a UIA call.
    ///
    /// The refusal list is everything that means "this window, right now,
    /// will not answer": the target refused us (`E_ACCESSDENIED` — the
    /// elevated case), the element or its window went away
    /// (`UIA_E_ELEMENTNOTAVAILABLE`, `E_HANDLE`, `CO_E_OBJNOTCONNECTED`), the
    /// provider never loaded (`UIA_E_PROXYASSEMBLYNOTLOADED`), the target is
    /// busy or gone at the RPC layer, or UIA gave up on its own
    /// (`UIA_E_TIMEOUT`). Everything a missing pattern can arrive as
    /// ([`is_pattern_missing`]) is a refusal too, so that a caller which
    /// reaches this classifier by a different route than the pattern lookup
    /// still falls back silently rather than logging.
    ///
    /// Anything else is [`UiaError::Com`], on purpose: an unrecognised
    /// `HRESULT` should show up in a log once so it can be added here
    /// deliberately, not be swallowed by a catch-all.
    fn from_hresult(hr: i32) -> UiaError {
        let refusal = is_pattern_missing(hr)
            || matches!(
                hr,
                E_ACCESSDENIED
                    | E_HANDLE
                    | CO_E_OBJNOTCONNECTED
                    | RPC_E_CALL_REJECTED
                    | RPC_E_CALL_CANCELED
                    | RPC_E_SERVERFAULT
                    | RPC_E_DISCONNECTED
                    | RPC_E_SERVERCALL_RETRYLATER
                    | RPC_E_TIMEOUT
                    | RPC_S_SERVER_UNAVAILABLE
                    | RPC_S_CALL_FAILED
                    | UIA_E_ELEMENTNOTAVAILABLE
                    | UIA_E_ELEMENTNOTENABLED
                    | UIA_E_INVALIDOPERATION
                    | UIA_E_NOCLICKABLEPOINT
                    | UIA_E_PROXYASSEMBLYNOTLOADED
                    | UIA_E_TIMEOUT
            );
        if refusal {
            UiaError::Unavailable
        } else {
            UiaError::Com(hr)
        }
    }
}

/// A bound element, named by an id the worker thread understands.
///
/// Deliberately opaque and deliberately not `Clone`: the `IUIAutomationElement`
/// it stands for lives in the worker's map and is released when this drops.
/// Two handles for one id would mean two releases.
pub struct UiaHandle {
    id: u64,
    /// The channel this handle was minted on, so [`Drop`] and every read can
    /// reach the worker without a global lookup — which also means the whole
    /// client side is exercisable against a stub worker in tests.
    tx: Sender<Req>,
}

impl Drop for UiaHandle {
    fn drop(&mut self) {
        // Fire-and-forget: if the worker is gone the element went with it.
        let _ = self.tx.send(Req::Release { id: self.id });
    }
}

/// Whether an element whose `IsPassword` property read as `is_password` may
/// be bound at all. `None` means the property could not be read.
///
/// A password box is the one field this module must never look inside, and
/// UI Automation will happily hand over the plaintext of one whose provider
/// implements `ValuePattern`. Nothing downstream can undo that: the field
/// monitor diffs whatever it is given and the auto-learn store persists what
/// the diff produces, so a password read here becomes a password on disk.
/// The guard therefore lives at this module's own door rather than in any
/// caller — it is the only place that is guaranteed to be on the path.
///
/// `None` is treated exactly like `Some(true)`. A privacy guard that fails
/// open is not a guard, and the cost of being wrong is a silent fallback to
/// the behaviour this app had before UIA existed. (In practice a `None` here
/// means the element is broken or gone, which is `Unavailable` anyway.)
fn is_readable_field(is_password: Option<bool>) -> bool {
    matches!(is_password, Some(false))
}

/// Requests to the worker. Each carries the caller's deadline so the worker
/// can skip work nobody is waiting for any more; `Release` does not, because
/// it has no reply and must always be honoured.
///
/// TRIPWIRE: no `windows::` type may ever become a field of this enum. It is
/// the one value that crosses off the UIA thread, and the compiler will not
/// stop you: windows-rs marks COM interfaces `Send + Sync` unconditionally,
/// so an `IUIAutomationElement` smuggled in here would compile and would
/// silently put a pointer from one apartment into everybody else's hands.
/// Ids and plain data only.
enum Req {
    Element {
        hwnd: isize,
        deadline: Instant,
        reply: Sender<Result<u64, UiaError>>,
    },
    Value {
        id: u64,
        deadline: Instant,
        reply: Sender<Result<Option<String>, UiaError>>,
    },
    Selection {
        id: u64,
        deadline: Instant,
        reply: Sender<Result<Option<String>, UiaError>>,
    },
    Release {
        id: u64,
    },
}

/// Whether a request whose caller set `deadline` is still worth serving.
///
/// The worker calls this before every read. A provider that hangs for five
/// seconds while a 500 ms poll keeps queueing reads behind it would otherwise
/// leave the worker grinding through a backlog of answers no one will ever
/// receive, each of which can hang again.
fn still_wanted(deadline: Instant) -> bool {
    Instant::now() < deadline
}

/// The process-wide worker. Started on first use; if the thread cannot be
/// spawned the receiver is dropped with the closure, every `send` below
/// fails, and every call reads as [`UiaError::Unavailable`] — which is
/// exactly the "UIA is not available here" behaviour callers already handle.
fn worker() -> &'static Sender<Req> {
    static WORKER: OnceLock<Sender<Req>> = OnceLock::new();
    WORKER.get_or_init(|| {
        let (tx, rx) = crossbeam_channel::unbounded();
        if let Err(e) = std::thread::Builder::new()
            .name("uia".into())
            .spawn(move || worker::run(rx))
        {
            tracing::warn!("UI Automation thread could not start ({e}); UIA reads are off");
        }
        tx
    })
}

/// Send one request and wait no longer than [`CALL_TIMEOUT`] for its answer.
///
/// Every way of not getting an answer — worker gone, worker dropped the reply
/// without sending, deadline passed — collapses to
/// [`UiaError::Unavailable`], because they are the same thing to a caller:
/// no text, fall back, say nothing.
fn dispatch<T: Send>(
    tx: &Sender<Req>,
    make: impl FnOnce(Instant, Sender<Result<T, UiaError>>) -> Req,
) -> Result<T, UiaError> {
    let (reply_tx, reply_rx) = crossbeam_channel::bounded(1);
    let deadline = Instant::now() + CALL_TIMEOUT;
    if tx.send(make(deadline, reply_tx)).is_err() {
        return Err(UiaError::Unavailable);
    }
    reply_rx
        .recv_deadline(deadline)
        .unwrap_or(Err(UiaError::Unavailable))
}

/// Bind the element the user is typing into inside `hwnd`.
///
/// `hwnd` is the window the caller means — the dictation `Target` captured at
/// chord-down, not "whatever has focus now". That distinction is the whole
/// point of taking a window handle here: binding whatever answers
/// `GetFocusedElement` half a second after a paste, with nothing tying it to
/// the window the paste went into, can watch a field that never received the
/// dictation. This resolves the focused element too — it is the
/// only way to reach the text control inside a modern windowless UI — but
/// then requires it to belong to `hwnd`, and returns
/// [`UiaError::Unavailable`] when it does not.
///
/// A **password box is refused here** — [`UiaError::Unavailable`], no handle,
/// nothing to read later. See [`is_readable_field`].
///
/// The binding is a snapshot: the element is resolved once, here. When it
/// later goes away, reads through this handle answer
/// [`UiaError::Unavailable`], which is a caller's cue to stop.
///
/// **This blocks for up to [`BIND_BUDGET`].** A transient refusal is retried
/// — see the module doc's "The bind is the one call that is retried" — so a
/// caller on a latency-sensitive path has to have priced that, and
/// `routes::selection` does. A [`UiaError::Com`] comes straight back on the
/// first attempt.
pub fn element_for_hwnd(hwnd: isize) -> Result<UiaHandle, UiaError> {
    bind(worker(), hwnd)
}

fn bind(tx: &Sender<Req>, hwnd: isize) -> Result<UiaHandle, UiaError> {
    bind_waiting(tx, hwnd, std::thread::sleep)
}

/// [`bind`] with the wait injected.
///
/// Only so the ladder is testable: a test that drove the real one would spend
/// [`BIND_RETRY_DELAY`] per retry sleeping, and the point of the stubbed-worker
/// tests in this module is that the whole client side runs without a desktop
/// *and* without waiting for one.
fn bind_waiting(
    tx: &Sender<Req>,
    hwnd: isize,
    wait: impl FnMut(Duration),
) -> Result<UiaHandle, UiaError> {
    let id = retry_transient(
        || {
            dispatch(tx, |deadline, reply| Req::Element {
                hwnd,
                deadline,
                reply,
            })
        },
        wait,
    )?;
    Ok(UiaHandle { id, tx: tx.clone() })
}

/// Call `attempt` until it answers something other than
/// [`UiaError::Unavailable`], at most [`BIND_ATTEMPTS`] times, with a
/// `wait(BIND_RETRY_DELAY)` between each pair of attempts and none after the
/// last.
///
/// The whole ladder is here, in one function with no COM in it and no clock of
/// its own, because it is a policy rather than a mechanism: which error is
/// worth a second ask, how many, how far apart. [`UiaError::Com`] returns on
/// the spot — an unclassified `HRESULT` is a defect to be looked at once, not
/// a race to be waited out, and retrying would put it in the log three times.
///
/// `BIND_ATTEMPTS` is never zero; the trailing answer is what a zero would
/// mean anyway, which keeps this total rather than reachable-only-by-luck.
fn retry_transient<T>(
    mut attempt: impl FnMut() -> Result<T, UiaError>,
    mut wait: impl FnMut(Duration),
) -> Result<T, UiaError> {
    for remaining in (0..BIND_ATTEMPTS).rev() {
        match attempt() {
            Err(UiaError::Unavailable) if remaining > 0 => wait(BIND_RETRY_DELAY),
            settled => return settled,
        }
    }
    Err(UiaError::Unavailable)
}

/// Read the whole current value of the bound field.
///
/// PRIVACY: the returned `String` is the user's field content. Never log it,
/// never store it, never put it in an event or a history row. See the module
/// doc comment.
///
/// `ValuePattern` is asked first: on the editors and text boxes measured
/// (Notepad, a WinUI text box, Chromium, VS Code) it returns the same text as
/// `TextPattern` in less time (see `worker::value_of`).
/// `TextPattern`'s whole document range answers when `ValuePattern` is
/// missing, fails or reports an empty value. `Ok(None)` means the element
/// implements neither, i.e. it is not a text field at all, and the caller
/// should treat it as "nothing to watch here". When one pattern fails and the
/// other has nothing, the failure comes back as an error rather than
/// `Ok(None)`, so one refused read does not end a watch on a real field.
pub fn focused_value(h: &UiaHandle) -> Result<Option<String>, UiaError> {
    dispatch(&h.tx, |deadline, reply| Req::Value {
        id: h.id,
        deadline,
        reply,
    })
}

/// Read the text the user currently has selected in the bound field.
///
/// PRIVACY: the returned `String` is the user's field content. Never log it,
/// never store it, never put it in an event or a history row. See the module
/// doc comment.
///
/// `TextPattern::GetSelection`. `Ok(None)` covers both "this element has no
/// `TextPattern`" and "there is a `TextPattern` but nothing is selected" —
/// the caller's response to either is the same, so they are not worth
/// distinguishing. A provider that reports several disjoint ranges (a column
/// selection in a table, say) gets them concatenated in the order it returned
/// them, with no separator inserted — so a caller that could be handed a
/// disjoint selection (a column in a table) gets the pieces run together.
/// `routes::selection` is not one: what it does with the answer is
/// byte-compare it against a second read of the same selection and then paste
/// over it, and both readings concatenate the same way.
pub fn selection_text(h: &UiaHandle) -> Result<Option<String>, UiaError> {
    dispatch(&h.tx, |deadline, reply| Req::Selection {
        id: h.id,
        deadline,
        reply,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossbeam_channel::{unbounded, Receiver};
    use std::thread;

    /// A worker that reads one request, answers it with `answer`, and stops.
    fn stub_answering(
        rx: Receiver<Req>,
        answer: Result<Option<String>, UiaError>,
    ) -> thread::JoinHandle<()> {
        thread::spawn(move || {
            if let Ok(Req::Value { reply, .. }) = rx.recv() {
                let _ = reply.send(answer);
            }
        })
    }

    /// A handle that talks to `tx` without going through the real worker.
    fn handle_on(tx: &Sender<Req>) -> UiaHandle {
        UiaHandle {
            id: 1,
            tx: tx.clone(),
        }
    }

    #[test]
    fn a_worker_that_never_answers_is_given_up_on_at_the_timeout() {
        let (tx, rx) = unbounded::<Req>();
        // Holds the request — and with it the reply sender — so the channel
        // is NOT disconnected: this must exercise the deadline, not the
        // "sender dropped" path below.
        let held = thread::spawn(move || {
            let _req = rx.recv().expect("stub worker gets the request");
            thread::sleep(CALL_TIMEOUT * 4);
        });

        let h = handle_on(&tx);
        let started = Instant::now();
        let got = focused_value(&h);

        assert_eq!(got, Err(UiaError::Unavailable));
        assert!(
            started.elapsed() < CALL_TIMEOUT * 2,
            "waited {:?}, which is past the caller's leash",
            started.elapsed()
        );
        drop(h);
        held.join().expect("stub worker");
    }

    #[test]
    fn a_worker_that_never_started_reads_as_unavailable_not_as_a_com_error() {
        let (tx, rx) = unbounded::<Req>();
        drop(rx); // the thread failed to spawn, or has exited

        let h = handle_on(&tx);
        assert_eq!(focused_value(&h), Err(UiaError::Unavailable));
        assert_eq!(selection_text(&h), Err(UiaError::Unavailable));
        // Through the no-sleep seam: the bind ladder is real and this case
        // walks all of it, so the live `bind` would spend two retry delays
        // proving something about a channel that is already closed.
        assert!(matches!(
            bind_waiting(&tx, 42, |_| {}),
            Err(UiaError::Unavailable)
        ));
    }

    #[test]
    fn a_worker_that_drops_the_reply_without_answering_reads_as_unavailable() {
        let (tx, rx) = unbounded::<Req>();
        let stub = thread::spawn(move || {
            // Exactly what the worker does with a request whose deadline has
            // already passed: drop it on the floor.
            drop(rx.recv().expect("stub worker gets the request"));
        });

        let h = handle_on(&tx);
        let started = Instant::now();
        assert_eq!(focused_value(&h), Err(UiaError::Unavailable));
        assert!(
            started.elapsed() < CALL_TIMEOUT,
            "a dropped reply should answer immediately, not wait out the leash"
        );
        stub.join().expect("stub worker");
    }

    #[test]
    fn an_answer_comes_back_verbatim() {
        let (tx, rx) = unbounded::<Req>();
        let stub = stub_answering(rx, Ok(Some("value from the stub".into())));

        let h = handle_on(&tx);
        assert_eq!(focused_value(&h), Ok(Some("value from the stub".into())));
        stub.join().expect("stub worker");
    }

    #[test]
    fn a_com_error_from_the_worker_reaches_the_caller_as_com() {
        let (tx, rx) = unbounded::<Req>();
        let stub = stub_answering(rx, Err(UiaError::Com(E_NOTIMPL)));

        let h = handle_on(&tx);
        assert_eq!(focused_value(&h), Err(UiaError::Com(E_NOTIMPL)));
        stub.join().expect("stub worker");
    }

    #[test]
    fn binding_asks_the_worker_for_the_window_the_caller_named() {
        let (tx, rx) = unbounded::<Req>();
        let stub = thread::spawn(move || match rx.recv() {
            Ok(Req::Element { hwnd, reply, .. }) => {
                assert_eq!(hwnd, 0x1234, "the hwnd must reach the worker unchanged");
                let _ = reply.send(Ok(7));
                // Keep the receiver alive so the handle's Release lands.
                rx.recv().ok()
            }
            other => panic!("expected an Element request, got something else: {}", other.is_ok()),
        });

        let h = bind(&tx, 0x1234).expect("stub worker binds");
        assert_eq!(h.id, 7);
        drop(h);
        stub.join().expect("stub worker");
    }

    // --- the bind's retry ladder ------------------------------------------
    //
    // Split in two on purpose. `retry_transient` is the policy — which error
    // is worth another ask, how many, how far apart — and is tested as pure
    // arithmetic over a scripted sequence. The wiring test below then proves
    // that `bind` is actually the thing wearing that policy, which the policy
    // tests on their own could never show.

    /// A script of answers, and a record of every wait the ladder asked for.
    fn ladder(
        answers: Vec<Result<u64, UiaError>>,
    ) -> (Result<u64, UiaError>, usize, Vec<Duration>) {
        let mut remaining = answers.into_iter();
        let mut asked = 0usize;
        let mut waits = Vec::new();
        let got = retry_transient(
            || {
                asked += 1;
                remaining.next().unwrap_or(Err(UiaError::Unavailable))
            },
            |d| waits.push(d),
        );
        (got, asked, waits)
    }

    /// THE POINT OF THE WHOLE CHANGE. The bind resolves the *focused* element
    /// and requires it to belong to the window it was asked about, so a target
    /// that is still settling — a freshly activated Win11 Notepad window, a
    /// VS Code that has just been pasted into — refuses a question it would
    /// answer a moment later. One ask turned "not yet" into "never".
    #[test]
    fn a_transient_unavailable_followed_by_success_binds() {
        let (got, asked, waits) = ladder(vec![Err(UiaError::Unavailable), Ok(7)]);

        assert_eq!(got, Ok(7));
        assert_eq!(asked, 2, "it stops asking the moment it has an answer");
        assert_eq!(waits, vec![BIND_RETRY_DELAY], "one wait, before the retry");
    }

    /// The ladder is bounded, and the bound is the one every caller's budget
    /// is written against: three asks, two waits, none after the last.
    #[test]
    fn a_target_that_never_answers_is_asked_exactly_three_times() {
        let (got, asked, waits) = ladder(vec![]);

        assert_eq!(got, Err(UiaError::Unavailable));
        assert_eq!(asked, BIND_ATTEMPTS as usize);
        assert_eq!(
            waits,
            vec![BIND_RETRY_DELAY; BIND_ATTEMPTS as usize - 1],
            "a wait between each pair of attempts, and none after the last"
        );

        // BIND_BUDGET is pinned HERE rather than in a test of its own, and
        // that placement is the whole point. The constant is *defined* as
        // `CALL_TIMEOUT × ATTEMPTS + RETRY_DELAY × (ATTEMPTS - 1)`, so
        // asserting that equation on its own proves nothing about the ladder
        // — a mutant that waits after the final failed attempt too leaves
        // both the equation and the number 1200 untouched while making the
        // real worst case 1.5 s. The three assertions above are what rule
        // that out, and only after them does this literal mean what it says.
        assert_eq!(
            BIND_BUDGET,
            Duration::from_millis(1200),
            "{asked} attempts on a {CALL_TIMEOUT:?} leash plus {} waits of {BIND_RETRY_DELAY:?}",
            waits.len()
        );
    }

    /// An unclassified `HRESULT` is a defect to look at once, not a focus race
    /// to wait out. Retrying it would cost a second and a bit and put the same
    /// line in the log three times.
    #[test]
    fn a_com_error_is_not_retried() {
        let (got, asked, waits) = ladder(vec![Err(UiaError::Com(E_NOTIMPL)), Ok(7)]);

        assert_eq!(got, Err(UiaError::Com(E_NOTIMPL)));
        assert_eq!(asked, 1, "the second answer must never have been reached");
        assert!(waits.is_empty());
    }

    /// The ladder is wired into the bind itself, not just available beside it
    /// — and the handle it finally mints carries the id the *successful*
    /// attempt returned, not the failed one's.
    #[test]
    fn the_bind_retries_a_refusing_worker_and_hands_back_the_second_id() {
        let (tx, rx) = unbounded::<Req>();
        let stub = thread::spawn(move || {
            match rx.recv() {
                Ok(Req::Element { reply, .. }) => {
                    let _ = reply.send(Err(UiaError::Unavailable));
                }
                _ => panic!("expected an Element request"),
            }
            match rx.recv() {
                Ok(Req::Element { reply, .. }) => {
                    let _ = reply.send(Ok(9));
                }
                _ => panic!("expected a second Element request"),
            }
            // Keep the receiver alive so the handle's Release lands.
            rx.recv().ok()
        });

        let mut waits = Vec::new();
        let h = bind_waiting(&tx, 0x1234, |d| waits.push(d)).expect("the retry binds");

        assert_eq!(h.id, 9);
        assert_eq!(waits, vec![BIND_RETRY_DELAY]);
        drop(h);
        stub.join().expect("stub worker");
    }

    /// A dropped handle must release its element ON the worker thread — the
    /// whole reason the id is opaque. If this stops happening, every bound
    /// element leaks for the life of the process.
    #[test]
    fn dropping_a_handle_releases_its_element_on_the_worker_thread() {
        let (tx, rx) = unbounded::<Req>();

        let h = handle_on(&tx);
        let id = h.id;
        drop(h);

        match rx.try_recv() {
            Ok(Req::Release { id: released }) => assert_eq!(released, id),
            _ => panic!("dropping a handle must send exactly one Release for its id"),
        }
        assert!(rx.try_recv().is_err(), "and nothing else");
    }

    #[test]
    fn a_request_whose_caller_has_already_given_up_is_not_worth_serving() {
        let past = Instant::now() - Duration::from_millis(1);
        assert!(!still_wanted(past));
        assert!(still_wanted(Instant::now() + CALL_TIMEOUT));
    }

    #[test]
    fn refusals_map_to_unavailable_so_callers_fall_back_silently() {
        for hr in [
            E_ACCESSDENIED,
            E_HANDLE,
            CO_E_OBJNOTCONNECTED,
            RPC_E_CALL_REJECTED,
            RPC_E_CALL_CANCELED,
            RPC_E_SERVERFAULT,
            RPC_E_DISCONNECTED,
            RPC_E_SERVERCALL_RETRYLATER,
            RPC_E_TIMEOUT,
            RPC_S_SERVER_UNAVAILABLE,
            RPC_S_CALL_FAILED,
            UIA_E_ELEMENTNOTAVAILABLE,
            UIA_E_ELEMENTNOTENABLED,
            UIA_E_INVALIDOPERATION,
            UIA_E_NOCLICKABLEPOINT,
            UIA_E_PROXYASSEMBLYNOTLOADED,
            UIA_E_TIMEOUT,
        ] {
            assert_eq!(
                UiaError::from_hresult(hr),
                UiaError::Unavailable,
                "0x{hr:08X} should not be logged as a COM defect"
            );
        }
    }

    /// A missing pattern is the ordinary case (most windows are not text
    /// fields), and `GetCurrentPatternAs` signals it with a null out-pointer
    /// that the `windows` binding reports as `Err` carrying `S_OK`.
    #[test]
    fn a_missing_pattern_is_recognised_including_the_s_ok_null_pointer_case() {
        for hr in [
            S_OK,
            E_NOTIMPL,
            E_NOINTERFACE,
            E_POINTER,
            UIA_E_NOTSUPPORTED,
        ] {
            assert!(is_pattern_missing(hr), "0x{hr:08X} means 'no such pattern'");
        }
        assert!(!is_pattern_missing(E_ACCESSDENIED));
        // "not right now" is not "not ever" — see is_pattern_missing's doc.
        assert!(!is_pattern_missing(UIA_E_INVALIDOPERATION));
    }

    /// Whatever [`is_pattern_missing`] accepts must also classify as a
    /// refusal: the two lists are consulted on different code paths and a
    /// pattern-missing HRESULT reaching the classifier by the other route
    /// must still be silent rather than logged.
    #[test]
    fn every_pattern_missing_code_is_also_a_refusal() {
        for hr in [
            S_OK,
            E_NOTIMPL,
            E_NOINTERFACE,
            E_POINTER,
            UIA_E_NOTSUPPORTED,
        ] {
            assert_eq!(UiaError::from_hresult(hr), UiaError::Unavailable);
        }
    }

    #[test]
    fn an_unrecognised_hresult_stays_a_com_error_so_it_gets_looked_at() {
        // E_FAIL: nothing in the refusal table, and genuinely "something
        // broke" rather than "this window won't answer".
        let e_fail = 0x8000_4005u32 as i32;
        assert_eq!(UiaError::from_hresult(e_fail), UiaError::Com(e_fail));
        let e_invalidarg = 0x8007_0057u32 as i32;
        assert_eq!(
            UiaError::from_hresult(e_invalidarg),
            UiaError::Com(e_invalidarg)
        );
    }

    /// A password box is the one field this module must never look inside,
    /// and UI Automation will hand over the plaintext of one whose provider
    /// implements `ValuePattern`. Nothing downstream can undo that — the
    /// field monitor diffs what it is given and auto-learn persists what the
    /// diff produces — so the refusal has to happen here.
    #[test]
    fn a_password_field_is_never_readable() {
        assert!(!is_readable_field(Some(true)));
    }

    /// A guard that fails open is not a guard: an element whose `IsPassword`
    /// property cannot be read at all has not told us it is safe, so it is
    /// treated exactly like one that said yes. The cost of being wrong in
    /// this direction is a silent fallback; the cost in the other direction
    /// is a password on disk.
    #[test]
    fn an_unreadable_password_property_fails_closed() {
        assert!(!is_readable_field(None));
    }

    #[test]
    fn an_ordinary_field_is_readable() {
        assert!(is_readable_field(Some(false)));
    }

    /// The tripwire from the module doc: this error type is the only thing
    /// about a UIA read that is allowed to be logged, so it must be incapable
    /// of carrying anything but an HRESULT.
    #[test]
    fn the_error_type_can_only_ever_render_an_hresult() {
        assert_eq!(
            UiaError::Unavailable.to_string(),
            "UI Automation unavailable for this window"
        );
        assert_eq!(
            UiaError::Com(E_ACCESSDENIED).to_string(),
            "UI Automation call failed (HRESULT 0x80070005)"
        );
    }

    /// Live smoke test — needs a real interactive desktop (a window manager,
    /// a focused window, working `SendInput`), so it is `#[ignore]`d and
    /// never runs in `cargo test --lib`.
    ///
    /// Run it by hand with:
    ///
    /// ```text
    /// cargo test --lib uia_reads_back_text_typed_into_notepad -- --ignored --nocapture
    /// ```
    ///
    /// Do not touch the keyboard while it runs: it launches Notepad, pastes
    /// through the real clipboard, and reads the field back through UIA.
    /// It prints only lengths and comparison verdicts, never the text —
    /// this is a test of a privacy-sensitive path and its own output must
    /// follow the same rule the module does.
    ///
    /// Notepad on Windows 11 is single-instance and tabbed, so `notepad.exe`
    /// may open a tab in a window that already existed, asynchronously, and
    /// `Child::kill` then kills a launcher stub rather than that window —
    /// the tab is left open for you to close. It also means the foreground
    /// window keeps moving for a moment after the spawn, which is why this
    /// waits for it to settle before capturing the handle: a handle grabbed
    /// mid-switch names a window that is about to lose focus, and
    /// [`element_for_hwnd`] correctly refuses to bind an element in a
    /// different window than the one it was asked about.
    #[test]
    #[ignore = "needs a real interactive desktop; see the doc comment for the invocation"]
    fn uia_reads_back_text_typed_into_notepad() {
        use std::process::Command;

        const TYPED: &str = "butterfly speak uia smoke test";

        let mut notepad = Command::new("notepad.exe")
            .spawn()
            .expect("launch notepad.exe");

        let (pid, hwnd) = settled_notepad_window().expect("notepad settled in the foreground");
        println!("notepad pid={pid} hwnd={hwnd:#x}");

        crate::injection::inject_text(TYPED, true, 100, false).expect("paste into notepad");
        thread::sleep(Duration::from_millis(500));

        // If the foreground moved during the paste, the rest of this test is
        // measuring the wrong window and `Unavailable` below would be
        // correct behaviour reported as a failure. Say which it is.
        let now_foreground = foreground_hwnd();
        assert_eq!(
            now_foreground, hwnd,
            "the foreground moved from {hwnd:#x} to {now_foreground:#x} during the paste; \
             re-run with nothing else competing for focus"
        );

        let handle = element_for_hwnd(hwnd);
        let bound = handle.is_ok();
        println!("element_for_hwnd -> {}", if bound { "Ok" } else { "Err" });
        let handle = handle.expect("notepad's focused element binds");

        let value = focused_value(&handle);
        match &value {
            Ok(Some(v)) => println!(
                "focused_value -> Ok(Some(<{} chars>)); contains the typed text: {}",
                v.chars().count(),
                v.contains(TYPED)
            ),
            Ok(None) => println!("focused_value -> Ok(None) (no ValuePattern and no TextPattern)"),
            Err(e) => println!("focused_value -> Err({e})"),
        }

        let empty_selection = selection_text(&handle);
        match &empty_selection {
            Ok(Some(s)) => println!(
                "selection_text (nothing selected) -> Ok(Some(<{} chars>))",
                s.chars().count()
            ),
            Ok(None) => println!("selection_text (nothing selected) -> Ok(None)"),
            Err(e) => println!("selection_text (nothing selected) -> Err({e})"),
        }

        select_all();
        thread::sleep(Duration::from_millis(200));
        let selected = selection_text(&handle);
        match &selected {
            Ok(Some(s)) => println!(
                "selection_text (after Ctrl+A) -> Ok(Some(<{} chars>)); contains the typed text: {}",
                s.chars().count(),
                s.contains(TYPED)
            ),
            Ok(None) => println!("selection_text (after Ctrl+A) -> Ok(None)"),
            Err(e) => println!("selection_text (after Ctrl+A) -> Err({e})"),
        }

        drop(handle);
        let _ = notepad.kill(); // TerminateProcess: no "save changes?" prompt
        let _ = notepad.wait();

        match value {
            Ok(Some(v)) => assert!(
                v.contains(TYPED),
                "the field's value should contain what was just pasted into it"
            ),
            other => panic!(
                "expected a value from notepad, got {}",
                match other {
                    Ok(None) => "Ok(None)".to_string(),
                    Err(e) => e.to_string(),
                    Ok(Some(_)) => unreachable!(),
                }
            ),
        }
    }

    fn foreground_hwnd() -> isize {
        unsafe { windows::Win32::UI::WindowsAndMessaging::GetForegroundWindow().0 as isize }
    }

    /// Poll until Notepad has been the foreground window under the *same*
    /// handle for [`STABLE_SAMPLES`] consecutive reads, and return that
    /// window's pid and handle.
    ///
    /// Two agreeing reads is not enough. Notepad's window can take a second
    /// or more to appear after the spawn, so a short streak simply catches
    /// whatever was in front before it — and then the real window steals
    /// focus during the paste and the bind is asked about a window that no
    /// longer has any.
    fn settled_notepad_window() -> Option<(u32, isize)> {
        const STABLE_SAMPLES: u32 = 10; // 1.5s of not moving
        const SAMPLE_MS: u64 = 150;

        let mut previous: Option<isize> = None;
        let mut streak = 0u32;
        for _ in 0..100 {
            thread::sleep(Duration::from_millis(SAMPLE_MS));
            let Some(target) = crate::foreground::capture() else {
                previous = None;
                streak = 0;
                continue;
            };
            if target.app.as_deref() != Some("notepad") {
                previous = None;
                streak = 0;
                continue;
            }
            let hwnd = foreground_hwnd();
            if previous == Some(hwnd) {
                streak += 1;
                if streak >= STABLE_SAMPLES {
                    return Some((target.pid, hwnd));
                }
            } else {
                streak = 0;
            }
            previous = Some(hwnd);
        }
        None
    }

    /// Ctrl+A, hand-built because `injection`'s key helpers are private to
    /// that module. Safe to build by hand here only because neither `Ctrl`
    /// nor `A` is an E0-prefixed key — see `injection::is_extended` for what
    /// goes wrong when that is not true.
    fn select_all() {
        use windows::Win32::UI::Input::KeyboardAndMouse::{
            SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_KEYUP, VIRTUAL_KEY,
            VK_CONTROL,
        };
        const VK_A: VIRTUAL_KEY = VIRTUAL_KEY(0x41);
        fn key(vk: VIRTUAL_KEY, up: bool) -> INPUT {
            INPUT {
                r#type: INPUT_KEYBOARD,
                Anonymous: INPUT_0 {
                    ki: KEYBDINPUT {
                        wVk: vk,
                        wScan: 0,
                        dwFlags: if up {
                            KEYEVENTF_KEYUP
                        } else {
                            Default::default()
                        },
                        time: 0,
                        dwExtraInfo: 0,
                    },
                },
            }
        }
        let inputs = [
            key(VK_CONTROL, false),
            key(VK_A, false),
            key(VK_A, true),
            key(VK_CONTROL, true),
        ];
        let _ = unsafe { SendInput(&inputs, std::mem::size_of::<INPUT>() as i32) };
    }
}
