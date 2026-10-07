//! The selection lane: what the user had selected when they spoke an agent
//! command, and the checked replacement of that text with the agent's edit.
//!
//! A capture starts when the agent chord is released ([`begin`]). UI
//! Automation is asked first; when it has no text, a synthetic Ctrl+C reads the
//! selection through the clipboard and the user's clipboard is put back. Text
//! that passes the acceptance rule and the size cap is filed under a
//! single-use session ([`sessions`]) that records the text, the window and the
//! mechanism that read it. [`plan`] turns the capture into one of the
//! agent route's three outcomes: type at the cursor, edit the selection, or
//! type nothing and say why.
//!
//! [`replace`] redeems the session, brings the window back, reads the
//! selection again through the same mechanism and pastes the agent's reply
//! only when that reading is byte for byte the recorded text. The two
//! mechanisms can spell one selection differently (a provider may report `\r`
//! where the clipboard carries `\r\n`), so a session is never checked through
//! the other one.
//!
//! PRIVACY: selections, readings and replacements are the user's document.
//! Nothing here logs them, emits them or files them to History; log lines
//! carry session ids, counts and notices. [`Selected`] and [`Session`] print a
//! placeholder where the text would be.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use crate::foreground::Target;
use crate::injection::Restore;

/// How long a session stays redeemable after it is minted. A backstop:
/// the controller clears every session each time it returns to Idle
/// ([`SessionStore::clear`]). An edit to the selection runs inside one finalize
/// window, and the longest watchdog it can run under is
/// `controller::CUSTOM_FINALIZE_TIMEOUT` at 106 s (the cloud path's is 82 s).
/// Two minutes covers that with a margin.
pub const SESSION_TTL: Duration = Duration::from_secs(120);

/// The longest selection, in Unicode code points, the agent is asked to edit.
///
/// Counted in code points because that is the unit the reply's cost follows
/// most evenly across scripts: echoing 5,000 code points back unchanged,
/// sarvam-105b spends 199 completion tokens per 1,000 code points of English,
/// 279 of Hindi, 291 of Malayalam and 309 of Telugu. A byte count would give
/// an English selection three times the room of an Indic one, and a count of
/// visible characters would undercount Indic text, where vowel signs and
/// viramas are code points of their own.
///
/// A full-size selection in Telugu, the costliest script measured, came back
/// in about 1,550 tokens and 11 s. The model delivered 116 to 187 tokens a
/// second end to end, so `sarvam::chat::AGENT_TIMEOUT` (20 s) holds about
/// 2,300 tokens even at the slow end: room for an edit that makes the longest
/// selection some 40% longer.
/// `the_selection_cap_fits_the_selection_reply_budget` pins the cap against the
/// reply ceiling.
pub const MAX_SELECTION_CODE_POINTS: usize = 5000;

/// How many times one clipboard read opens the clipboard: the snapshot of the
/// user's text, the clear and the read inside `injection::copy_selection`, the
/// check of what the copy left, and the restore.
const CLIPBOARD_OPENS: u64 = 5;

/// The longest one of those opens can wait: `arboard` retries a clipboard that
/// another process holds open five times, 5 ms apart.
const CLIPBOARD_OPEN_WAIT_MS: u64 = 25;

/// Room in [`CLIPBOARD_RESOLVE_BUDGET`] for what has no wait of its own: the
/// thread start, the foreground checks and the text conversions.
const CLIPBOARD_MARGIN_MS: u64 = 200;

/// The clipboard half of a capture, as the sum of the waits in it: the
/// modifier drain before the synthetic Ctrl+C (`injection::MODIFIER_DRAIN_MS`),
/// the wait for the target to publish (`injection::COPY_BUDGET`, which is
/// checked once per `injection::COPY_POLL_MS` and so can run one poll over),
/// every clipboard open, and the margin. 980 ms.
const CLIPBOARD_RESOLVE_BUDGET: Duration = Duration::from_millis(
    crate::injection::MODIFIER_DRAIN_MS
        + crate::injection::COPY_POLL_MS
        + CLIPBOARD_OPENS * CLIPBOARD_OPEN_WAIT_MS
        + CLIPBOARD_MARGIN_MS,
)
.saturating_add(crate::injection::COPY_BUDGET);

/// The most the UI Automation probe can cost a capture before the clipboard
/// read, which may still have to run: one bind, then one selection read.
///
/// The bind is `uia::BIND_BUDGET` (1.2 s): `uia::element_for_hwnd` asks up to
/// three times, 300 ms apart, because binding depends on focus at the instant
/// it runs, and about one bind in three declines that way on a window that has
/// just been dictated into. The read is one `uia::CALL_TIMEOUT` (200 ms). 1.4 s
/// in all, spent only by a provider that runs every wait out; a working one
/// answers in a millisecond or two.
///
/// Defined from the `uia` constants rather than written as a literal, so it
/// cannot disagree with them. The wall-clock value is pinned by hand in
/// `the_resolve_budget_pays_for_the_uia_probe_on_top_of_the_clipboard_read`,
/// because what has to notice a change is `controller::CLOUD_FINALIZE_TIMEOUT`,
/// whose itemised sum includes it.
const UIA_PROBE_BUDGET: Duration =
    crate::uia::BIND_BUDGET.saturating_add(crate::uia::CALL_TIMEOUT);

/// How long the route seam waits for a capture that has not answered yet: the
/// UIA probe plus the clipboard read behind it, 2.38 s.
///
/// A sum because UIA is asked first. With the clipboard's budget alone, a slow
/// but working clipboard read behind a slow UIA probe would come back
/// [`Why::TimedOut`], which is fatal and types nothing.
///
/// The probe's share is spent only where UIA does not answer. A provider that
/// runs every wait out costs all 1.4 s. A target that refuses the bind outright
/// costs the two 300 ms retry delays: a password box (`uia::worker` refuses to
/// bind one), an elevated window (`E_ACCESSDENIED`), an app stuck at the RPC
/// layer, and a second top-level window of the target's own process (the bind
/// accepts only the target, its child windows and the windows it owns).
/// A window that has lost the foreground never reaches the bind, because
/// `uia_reading` answers `FocusMoved` first, and an element that is not a
/// text field binds and answers in one read.
///
/// Spent inside the finalize window: the controller resolves the capture in
/// its `FinalResult` arm, under the watchdog already running for that
/// dictation. `controller::CLOUD_FINALIZE_TIMEOUT` itemises this by name.
pub(crate) const RESOLVE_BUDGET: Duration =
    CLIPBOARD_RESOLVE_BUDGET.saturating_add(UIA_PROBE_BUDGET);

/// How long [`replace`] can take, start to finish: 2.49 s.
///
/// Spent inside the finalize window. `routes::agent` awaits the replace inside
/// its deferred job, before it answers `RouteDone`, so the controller stays in
/// `Finalizing` under `CLOUD_FINALIZE_TIMEOUT` for the whole sequence. A
/// watchdog that fires mid-replace does not stop it: the session is already
/// redeemed and the paste still lands, next to a timeout notice and a timeout
/// History row. So the watchdogs itemise this sum.
///
/// The terms, each a real bound:
///
/// 1. Bringing the window back: [`crate::foreground::SETTLE_AFTER_SWITCH_MS`].
///    Everything else in `restore_foreground` is a Win32 call with no wait,
///    and the settle is skipped when the window is already in front.
/// 2. The verify read: [`UIA_PROBE_BUDGET`], the slower of the two mechanisms.
///    A clipboard session reads again within [`CLIPBOARD_RESOLVE_BUDGET`]
///    (980 ms); a UIA session binds and reads again, 1.4 s.
/// 3. The paste's settle between writing the clipboard and Ctrl+V:
///    [`crate::injection::SETTLE_MS`].
/// 4. The paste's modifier drain: [`crate::injection::MODIFIER_DRAIN_MS`]. It
///    is spent only while the user still holds part of the chord, which on
///    this path is the usual case, so it is counted in full.
/// 5. The paste's clipboard-restore delay at its largest allowed value,
///    [`crate::settings::RESTORE_DELAY_MAX_MS`]. `settings::repair` clamps the
///    setting to it, so the sum holds for every settings file.
///
/// Nothing enforces this: the replace has no timeout, because cutting it off
/// part way is the same orphaned paste. Its only readers are the controller's
/// pinned sums and this module's pinned wall-clock, which are tests, hence the
/// `allow`. Every term is the real constant, so none of them can move without
/// this moving too.
#[allow(dead_code)]
pub(crate) const REPLACE_BUDGET: Duration = Duration::from_millis(
    crate::foreground::SETTLE_AFTER_SWITCH_MS
        + crate::injection::SETTLE_MS
        + crate::injection::MODIFIER_DRAIN_MS
        + crate::settings::RESTORE_DELAY_MAX_MS,
)
.saturating_add(UIA_PROBE_BUDGET);

/// The selection is over [`MAX_SELECTION_CODE_POINTS`]. Names the limit, so
/// the user knows how much to select.
pub const SELECTION_TOO_LARGE_NOTICE: &str = "Selection over 5,000 characters — nothing typed";

/// The read failed in a way that leaves a selection possible. Deliberately
/// not "no selection found": the app does not know that.
pub const SELECTION_UNREADABLE_NOTICE: &str = "Couldn't read the selection — nothing typed";

/// The window under the copy was not the window the chord was pressed in, so
/// whatever came back describes someone else's document. Also the answer when
/// the replace step cannot bring that window back to the foreground: a paste
/// aimed at a window that will not come forward lands in a stranger's
/// document.
pub const SELECTION_MOVED_NOTICE: &str = "Focus moved mid-command — nothing typed";

/// The verify read succeeded and the document does not say what it said when
/// the command was spoken — the user typed, re-selected, or dropped the
/// selection while the model was thinking. Named separately from
/// [`SELECTION_UNREADABLE_NOTICE`] because the fix is different: select it
/// again and repeat the command.
pub const SELECTION_CHANGED_NOTICE: &str = "Selection changed — nothing replaced";

/// The session was already redeemed, or it aged past [`SESSION_TTL`] before
/// the answer arrived.
pub const SELECTION_EXPIRED_NOTICE: &str = "Selection timed out — nothing typed";

/// The document verified, and the paste itself failed — the clipboard could
/// not be written. The selection is still there and still the user's text.
pub const SELECTION_PASTE_FAILED_NOTICE: &str = "Couldn't replace the selection — nothing typed";

// --- what a capture found ----------------------------------------------------

/// What reading the selection found, as the agent route sees it. `Debug`
/// never prints selected text; [`Selected`] redacts it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Capture {
    /// Nothing is selected, or the window is one whose selection the agent
    /// never edits (a terminal). The answer is typed at the cursor.
    Nothing,
    /// The selection, read whole and filed under a session.
    Text(Selected),
    /// A selection exists and is longer than [`MAX_SELECTION_CODE_POINTS`];
    /// `chars` is its length in code points.
    Oversized { chars: usize },
    /// The chord's window lost the foreground before or during the read, so
    /// what came back belongs to another document.
    FocusMoved,
    /// The read ended without settling whether text is selected.
    Unreadable(Why),
}

/// A selection that was read whole, with the token that will let the
/// verify-then-replace step redeem it.
#[derive(Clone, PartialEq, Eq)]
pub struct Selected {
    /// Opaque, single-use, minted per capture. Not the selection.
    pub session: SessionId,
    /// PRIVACY: the user's document content. Never logged, never stored
    /// outside the session, never filed to History.
    pub text: String,
    /// Code points in `text` — the number the size gate judged.
    pub chars: usize,
}

/// PRIVACY, made structural. `Debug` is derived on every other type here and
/// written by hand on this one, because this is the type that holds the user's
/// document: `Capture` derives `Debug` and delegates into this impl, so a
/// `tracing::debug!(?capture)` — or a `{capture:?}` in an assertion message,
/// which several tests in this file already use — prints a length where the
/// text would be. A field the invariant only *asks* the next author to
/// remember is a field that eventually gets logged.
impl std::fmt::Debug for Selected {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Selected")
            .field("session", &self.session)
            .field("chars", &self.chars)
            .field("text", &Redacted)
            .finish()
    }
}

/// Stands in for a string that must not reach a log. Prints without quotes, so
/// the field reads as a redaction rather than as content that happens to say
/// "redacted".
struct Redacted;

impl std::fmt::Debug for Redacted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("<redacted>")
    }
}

/// Why a capture ended as [`Capture::Unreadable`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Why {
    /// No window was captured at chord-down, so nothing could hold a
    /// selection.
    NoWindow,
    /// The clipboard could not be opened, or what it held after the copy could
    /// not be tied to the copy (see [`accept_copy`]).
    ClipboardFailed,
    /// The capture thread did not answer within [`RESOLVE_BUDGET`].
    TimedOut,
    /// UI Automation could not read the selection. Only a verify read ends
    /// this way: a capture falls through to the clipboard instead.
    UiaFailed,
}

/// The reasons that leave no selection anywhere to protect, where typing at
/// the cursor overwrites nothing, so the answer is typed there. Every other
/// reason types nothing, including any added later, until it is listed here.
const TYPE_AT_CURSOR: &[Why] = &[Why::NoWindow];

/// Which mechanism read a selection.
///
/// Recorded on a [`Session`] because the verify read has to use the same one
/// (see the module doc). Chosen per capture by [`read_any`]: UI Automation
/// when it answers with text, the clipboard otherwise.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// UI Automation's `TextPattern::GetSelection`. The provider hands the
    /// selected text back: no synthetic keystroke and no clipboard.
    Uia,
    /// A synthetic Ctrl+C and the clipboard, judged by [`accept_copy`].
    Clipboard,
}

/// What the agent route does with a capture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Plan<'a> {
    /// Run the command and type the answer at the cursor.
    TypeAtCursor,
    /// Run the command on this selection and replace it with the answer.
    EditSelection(&'a Selected),
    /// Type nothing, ask the model nothing, and show this notice.
    Refuse(&'static str),
}

/// THE OUTCOME RULE. A selection is edited. No capture, nothing selected, and
/// a capture with no window behind it ([`TYPE_AT_CURSOR`]) all type at the
/// cursor, since there is no selection for the answer to overwrite.
/// Everything else leaves open whether a selection is sitting in the
/// document, and an answer typed at the cursor would land on top of it, so it
/// types nothing and says why.
pub fn plan(capture: Option<&Capture>) -> Plan<'_> {
    if let Some(Capture::Text(selected)) = capture {
        return Plan::EditSelection(selected);
    }
    if types_at_the_cursor(capture) {
        return Plan::TypeAtCursor;
    }
    Plan::Refuse(match capture {
        Some(Capture::Oversized { .. }) => SELECTION_TOO_LARGE_NOTICE,
        Some(Capture::FocusMoved) => SELECTION_MOVED_NOTICE,
        _ => SELECTION_UNREADABLE_NOTICE,
    })
}

/// Whether a capture settles that there is no selection to protect.
fn types_at_the_cursor(capture: Option<&Capture>) -> bool {
    match capture {
        None | Some(Capture::Nothing) => true,
        Some(Capture::Unreadable(why)) => TYPE_AT_CURSOR.contains(why),
        Some(_) => false,
    }
}

// --- sessions ---------------------------------------------------------------

/// An opaque, single-use handle on a captured selection. A UUID, so a session
/// cannot be guessed from context, and the *only* thing about a selection
/// that is allowed to be logged.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SessionId(String);

impl std::fmt::Display for SessionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// What a session holds: enough to prove, later, that the document still says
/// what it said when the command was spoken — and enough to go and ask.
///
/// PRIVACY: `text` is the user's document content. See the module doc; the
/// hand-written `Debug` below is the same guard [`Selected`] carries.
pub struct Session {
    pub text: String,
    /// The window the selection was read from. The replace step brings it back
    /// to the foreground and confirms it got there before it pastes.
    pub target: Target,
    /// Which mechanism read [`Session::text`] — and therefore the *only* one
    /// the verify read is allowed to use.
    ///
    /// This is the mixed-source rule made structural. `text` is about to be
    /// byte-compared against a second reading, and two mechanisms reading the
    /// same selection are not guaranteed to spell it the same way (a provider
    /// may report `\r` where the clipboard publishes `\r\n`). Carrying the
    /// source on the session rather than re-deciding it at the verify means
    /// the two halves of the lane physically cannot disagree about what they
    /// are comparing — the same reasoning [`Session::suppress`] is here for.
    pub source: Source,
    /// The keyboard hook's suppression flag, carried from the capture that
    /// minted this session rather than handed in by whoever redeems it.
    ///
    /// The verify read fires the same synthetic Ctrl+C the capture did and has
    /// to hide it from the hook the same way, so the flag is part of "what it
    /// takes to redeem this session" — and taking it from the session means
    /// the two halves of the lane physically cannot suppress different flags.
    /// The alternative was threading an `Arc` through `RouteCtx` and every
    /// route that will never touch it.
    pub suppress: Arc<AtomicBool>,
}

impl std::fmt::Debug for Session {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Session")
            .field("target", &self.target)
            .field("source", &self.source)
            .field("text", &Redacted)
            .finish_non_exhaustive()
    }
}

/// Every live session, by token. The app uses one per process ([`sessions`]);
/// tests make their own.
///
/// One lock around the whole map, because every operation is a few map
/// operations and the store is used from the capture thread, the replace
/// thread and the controller. Expired sessions are dropped by the next
/// [`mint`](Self::mint) or [`take`](Self::take), so none outlives its TTL by
/// more than one store operation while holding document text.
#[derive(Default)]
pub struct SessionStore {
    live: Mutex<HashMap<SessionId, Entry>>,
}

/// A session and the instant it stops being redeemable.
struct Entry {
    session: Session,
    expires_at: Instant,
}

impl Entry {
    /// Expired from `expires_at` on, that instant included.
    fn live_at(&self, now: Instant) -> bool {
        now < self.expires_at
    }
}

type Live = HashMap<SessionId, Entry>;

/// Drop every entry that has expired by `now`, so no document text is held
/// past its session's life for longer than one store operation.
fn sweep(live: &mut Live, now: Instant) {
    live.retain(|_, entry| entry.live_at(now));
}

impl SessionId {
    /// A token nobody can guess: a random (version 4) UUID.
    fn fresh() -> Self {
        Self(uuid::Uuid::new_v4().hyphenated().to_string())
    }
}

impl SessionStore {
    /// The map, whether or not a panic elsewhere poisoned the lock: every
    /// operation leaves the map whole, so the data is still sound.
    fn entries(&self) -> std::sync::MutexGuard<'_, Live> {
        self.live.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Revoke every session.
    pub fn clear(&self) {
        self.entries().clear();
    }

    /// File a selection under a new random token and return the token.
    pub fn mint(
        &self,
        text: String,
        target: Target,
        suppress: Arc<AtomicBool>,
        source: Source,
        now: Instant,
    ) -> SessionId {
        let session = Session {
            text,
            target,
            source,
            suppress,
        };
        let entry = Entry {
            session,
            expires_at: now + SESSION_TTL,
        };
        let id = SessionId::fresh();
        let mut live = self.entries();
        sweep(&mut live, now);
        live.insert(id.clone(), entry);
        id
    }

    /// Redeem `id`. The session is removed before anything else is looked
    /// at, so a token works once whatever the caller does next; an expired one
    /// is removed and gives nothing.
    pub fn take(&self, id: &SessionId, now: Instant) -> Option<Session> {
        let mut live = self.entries();
        let redeemed = live.remove(id);
        sweep(&mut live, now);
        drop(live);
        match redeemed {
            Some(entry) if entry.live_at(now) => Some(entry.session),
            _ => None,
        }
    }

    /// How many sessions the store holds, expired ones included.
    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.entries().len()
    }
}

/// The process-wide session store. One store, because a session is minted on
/// the capture thread and redeemed from whatever thread the replace runs on,
/// and the token is the only thing that travels between them.
pub fn sessions() -> &'static SessionStore {
    static SESSIONS: OnceLock<SessionStore> = OnceLock::new();
    SESSIONS.get_or_init(SessionStore::default)
}

// --- capturing --------------------------------------------------------------

/// A capture that has been started. Held by the controller from the moment
/// the chord is released until the route seam asks what it found.
pub enum PendingCapture {
    /// Decided without touching the document — no window to read, or a
    /// terminal, settled before any synthetic key is sent.
    Ready(Capture),
    /// A synthetic copy is in flight on its own thread.
    InFlight(crossbeam_channel::Receiver<Capture>),
}

impl PendingCapture {
    /// What the capture found, waiting up to [`RESOLVE_BUDGET`] if it is
    /// still running.
    ///
    /// Waiting matters: fast cloud transcription can finish before the read,
    /// and treating an unfinished read as "no selection" would type the answer
    /// over the selection.
    pub fn resolve(self) -> Capture {
        match self {
            PendingCapture::Ready(c) => c,
            PendingCapture::InFlight(rx) => rx
                .recv_timeout(RESOLVE_BUDGET)
                .unwrap_or(Capture::Unreadable(Why::TimedOut)),
        }
    }
}

/// Start reading the selection in `target`.
///
/// Called when the agent chord is released, not when it is pressed, and the
/// difference is not a preference. `injection::copy_selection` opens with
/// `release_stuck_modifiers`, which synthesizes key-**up** events for every
/// modifier the user is physically holding. The keyboard hook cannot tell a
/// synthetic release from a real one (`hotkeys.rs` — it observes the OS's
/// event stream), so at chord-down, with push-to-talk still held, those
/// releases read as the user letting go: `chord_active` clears and `ChordUp`
/// fires, ending the dictation microseconds after it started. By the time
/// this runs the chord is already over and `chord_active` is already `None`,
/// so the same events land harmlessly. The cost is that the read overlaps
/// transcription rather than the user's speech — the same overlap, one step
/// later.
///
/// `target` is the window the controller captured at chord-down, passed in
/// rather than re-captured: the whole point of that capture is that it
/// records where the user was looking when they pressed the key.
///
/// **[`Capture::FocusMoved`] is therefore judged against the chord-down
/// window, not one taken at the release.** A user who moves focus while
/// speaking has a selection somewhere other than where the command was aimed,
/// and that reads as `FocusMoved` and types nothing rather than reading
/// another document.
pub fn begin(target: Option<Target>, suppress: Arc<AtomicBool>) -> PendingCapture {
    let Some(target) = target else {
        return PendingCapture::Ready(Capture::Unreadable(Why::NoWindow));
    };
    // Terminals: a shell would run a pasted multi-line reply line by line, so
    // a terminal is treated as having nothing selected and the agent's answer
    // is typed at the cursor. Deciding that here, before any thread starts,
    // also keeps a synthetic Ctrl+C away from a console, which takes it as
    // "stop the running program".
    if crate::foreground::is_terminal(&target) {
        return PendingCapture::Ready(Capture::Nothing);
    }
    let (tx, rx) = crossbeam_channel::bounded(1);
    std::thread::spawn(move || {
        let _ = tx.send(read(&target, &suppress));
    });
    PendingCapture::InFlight(rx)
}

/// Holds the keyboard hook shut for as long as it is alive.
///
/// Every path in this app that synthesizes input suppresses the hook while it
/// does — `controller::start_injection`, `controller::start_replace`,
/// `transforms::spawn` — because `SendInput` events come back through the same
/// keyboard hook the user's chords do. Without this the hook pushes our own
/// Ctrl, C and V into its `down` set and evaluates the user's bindings against
/// it, so a binding that overlaps Ctrl+V fires a phantom `ChordDown` off a
/// keystroke the user never pressed; `injection::release_stuck_modifiers`'
/// synthetic key-*ups* ride the same gap and read as the user letting go.
///
/// A guard rather than a `store(true)` / `store(false)` pair: these run on
/// spawned threads, so a panic between the two stores would unwind the thread
/// and leave the flag `true` for the life of the process, every hotkey in the
/// app dead, with nothing left running to notice.
/// `transforms::Flags` is the same shape for the same reason.
///
/// Not re-entrant, and does not need to be: no window here nests inside
/// another. `Relaxed` because the flag is a hint read by one other thread and
/// orders nothing.
struct Suppressed<'a>(&'a AtomicBool);

impl<'a> Suppressed<'a> {
    fn new(flag: &'a AtomicBool) -> Self {
        flag.store(true, Ordering::Relaxed);
        Self(flag)
    }
}

impl Drop for Suppressed<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Relaxed);
    }
}

// --- reading -----------------------------------------------------------------

/// One reading of the selection, before the acceptance rule and the size cap
/// judge it.
///
/// PRIVACY: the text in `Uia` and in `Clipboard` is the user's document. The
/// type has no `Debug`, so it cannot end up in a log line by accident.
enum Reading {
    /// UI Automation's answer: the selected text, never empty.
    Uia(String),
    /// What the clipboard showed around a synthetic Ctrl+C.
    Clipboard(CopyReading),
    /// The target was not in the foreground before or after the read.
    FocusMoved,
    /// The read could not be made.
    Unreadable(Why),
}

/// The evidence one clipboard read collects, for [`accept_copy`] to judge.
struct CopyReading {
    /// What `injection::copy_selection` returned: the first non-blank text on
    /// the clipboard after the keystroke, or `None` when none appeared within
    /// its budget.
    copied: Option<String>,
    /// What the clipboard held once the copy was over.
    now: Held,
    /// The process that owns the clipboard's content, when it has an owner.
    owner: Option<u32>,
    /// Whether the clipboard's sequence number stayed the same while `now` and
    /// `owner` were read, so that both describe the same content.
    steady: bool,
    /// The processes behind the target window: its own, and those of the
    /// windows inside it (a hosted web view, or the app a frame window hosts).
    target_pids: Vec<u32>,
    /// The user's text from before the copy, `None` when there was none.
    before: Option<String>,
}

/// Who wrote what the clipboard holds, judged by its owner window's process.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Writer {
    /// A process behind the target window.
    Target,
    /// Nobody: the writer opened the clipboard without a window.
    Unknown,
    /// A process that has no window in the target.
    Elsewhere,
}

impl CopyReading {
    fn writer(&self) -> Writer {
        match self.owner {
            None => Writer::Unknown,
            Some(pid) if self.target_pids.contains(&pid) => Writer::Target,
            Some(_) => Writer::Elsewhere,
        }
    }
}

/// What the clipboard held at one moment.
#[derive(Clone, PartialEq, Eq)]
enum Held {
    Empty,
    Text(String),
    /// Content with no text form (an image, a file list), or a clipboard that
    /// stayed busy.
    Other,
}

/// What a clipboard reading proves about the selection.
enum Copied {
    /// The target published this text in answer to the copy.
    Selection(String),
    /// Nothing came from the target: nothing is selected.
    Nothing,
    /// Something is on the clipboard that the rule cannot tie to the copy, or
    /// the target published something that is not text. Whether text is
    /// selected is not settled.
    Unproven,
}

/// THE ACCEPTANCE RULE for a clipboard reading.
///
/// `injection::copy_selection` empties the clipboard before it sends Ctrl+C,
/// so whatever is on it afterwards was written after the keystroke. The rule
/// then asks two things of the text the copy returned: that the clipboard
/// still holds exactly that text, with a sequence number that did not move
/// while it was read, and that no evidence points at another writer.
///
/// The evidence is the clipboard's owner. Writing to the clipboard empties it
/// first, and emptying it makes the writer's window the owner. An owner window
/// in one of the target's processes (the target window's own, or one that has
/// a window inside it, as a hosted web view or a frame-hosted app does) is the
/// target answering the copy, and is accepted. An owner in any other process
/// is someone else writing during the read (a clipboard manager, a password
/// manager, the user copying elsewhere), and is refused. No owner at all means
/// the writer opened the clipboard without a window, which apps do; that is
/// accepted unless the text is exactly what the user had before the copy,
/// which a program putting the old value back would also produce.
///
/// Nothing from the copy means nothing is selected, which is what most apps
/// answer to Ctrl+C with no selection, unless the target's own process is seen
/// to have written something the copy did not return: text that came after
/// the copy stopped waiting, or content that is not text. Then a selection
/// exists that this read did not get, and the reading is unproven.
///
/// What the rule cannot tell apart, pinned by tests:
/// - A copy that returns the same text the clipboard held before it. When the
///   target owns what it writes, the text is published again by the target
///   and accepted; with no owner, the same case is refused as unproven.
/// - A write during the read by another window of one of the target's
///   processes, or by a program with no clipboard window, of text other than
///   the user's old value. Both are accepted. The verify before a paste reads
///   again with a second Ctrl+C, so a stray write would have to repeat with
///   the same text for a wrong paste to go through.
/// - A target whose copy is written by a process with no window in it. That
///   is refused, so its selections read as unreadable through the clipboard.
fn accept_copy(read: &CopyReading) -> Copied {
    let writer = read.writer();
    let Some(text) = read.copied.as_deref().filter(|text| !text.is_empty()) else {
        let target_wrote_something = writer == Writer::Target
            && match &read.now {
                Held::Text(now) => !now.trim().is_empty(),
                Held::Other => true,
                Held::Empty => false,
            };
        return if target_wrote_something {
            Copied::Unproven
        } else {
            Copied::Nothing
        };
    };
    let intact = read.steady && matches!(&read.now, Held::Text(now) if now == text);
    let trusted = match writer {
        Writer::Target => true,
        Writer::Unknown => read.before.as_deref() != Some(text),
        Writer::Elsewhere => false,
    };
    if intact && trusted {
        Copied::Selection(text.to_string())
    } else {
        Copied::Unproven
    }
}

/// THE RESTORE RULE, decided by what the clipboard holds, not by who wrote it.
/// `snapshot` is the user's text from before the copy (`None` when the
/// clipboard was empty or held something that is not text), `now` is what it
/// holds afterwards, and `copied` is the text the copy returned.
///
/// - Already back where it was, empty before and after included: leave it.
/// - Something other than the copy's text and other than blank: the user or
///   another program put it there during the read, so leave it. That holds
///   for a write from the target's own process too, since the user may have
///   pressed Ctrl+C in it.
/// - Otherwise the clipboard holds the copied document text, or nothing: put
///   the user's text back, or, with no text to put back, empty it, so the
///   document text never stays on it.
///
/// Text the target publishes after the copy stopped waiting is not the copy's
/// text by this rule and is left, like anything else written during the read.
/// Anything other than text on the clipboard before the copy is not put back,
/// the same limit `injection::inject_text` has.
fn restore_plan(snapshot: Option<&str>, now: &Held, copied: Option<&str>) -> Restore {
    let back_where_it_was = match now {
        Held::Text(now) => snapshot == Some(now.as_str()),
        Held::Empty => snapshot.is_none(),
        Held::Other => false,
    };
    let written_by_someone_else = match now {
        Held::Text(now) => !now.trim().is_empty() && copied != Some(now.as_str()),
        Held::Other => true,
        Held::Empty => false,
    };
    if back_where_it_was || written_by_someone_else {
        return Restore::Leave;
    }
    match snapshot {
        Some(text) => Restore::Put(text.to_string()),
        None => Restore::Clear,
    }
}

/// One clipboard read of `target`'s selection, with the user's clipboard put
/// back afterwards.
///
/// The target must hold the foreground before the keystroke, since Ctrl+C goes
/// wherever focus is, and after it, so the answer is known to come from the
/// same window. The copy runs with the keyboard hook suppressed, so the
/// synthetic keys are not read as the user's chords.
fn clipboard_reading(target: &Target, suppress: &AtomicBool) -> Reading {
    if !crate::foreground::is_foreground(target) {
        return Reading::FocusMoved;
    }
    let Ok(mut clipboard) = arboard::Clipboard::new() else {
        return Reading::Unreadable(Why::ClipboardFailed);
    };
    let before = match clipboard.get_text() {
        Ok(text) => Some(text),
        // Busy: the user's clipboard cannot be saved, so it must not be
        // emptied by a copy either.
        Err(arboard::Error::ClipboardOccupied) => {
            return Reading::Unreadable(Why::ClipboardFailed)
        }
        Err(_) => None,
    };
    // A refused copy touched nothing, so the restore rule finds the clipboard
    // back where it was and leaves it.
    let copied = {
        let _hook = Suppressed::new(suppress);
        crate::injection::copy_selection(&mut clipboard)
            .text()
            .map(str::to_owned)
    };
    let (now, owner, steady) = clipboard_state(&mut clipboard);
    let restored = restore_plan(before.as_deref(), &now, copied.as_deref()).apply(&mut clipboard);
    if restored.is_err() {
        tracing::warn!("could not put the user's clipboard back after a selection read");
    }
    if !crate::foreground::is_foreground(target) {
        return Reading::FocusMoved;
    }
    Reading::Clipboard(CopyReading {
        copied,
        now,
        owner,
        steady,
        target_pids: processes_behind(target),
        before,
    })
}

/// The target window's process, then the process of every window inside it
/// that belongs to a different one. A web view hosted in an app's window runs
/// in its own process and writes the clipboard from there; a frame-hosted app
/// draws inside a frame window owned by a separate host process.
fn processes_behind(target: &Target) -> Vec<u32> {
    use windows::core::BOOL;
    use windows::Win32::Foundation::{HWND, LPARAM};
    use windows::Win32::UI::WindowsAndMessaging::{EnumChildWindows, GetWindowThreadProcessId};

    unsafe extern "system" fn note_process(window: HWND, list: LPARAM) -> BOOL {
        // SAFETY: `list` is the `&mut Vec<u32>` passed below, alive for the
        // whole synchronous enumeration.
        let pids = unsafe { &mut *(list.0 as *mut Vec<u32>) };
        let mut pid = 0u32;
        unsafe { GetWindowThreadProcessId(window, Some(&mut pid)) };
        if pid != 0 && !pids.contains(&pid) {
            pids.push(pid);
        }
        BOOL(1)
    }

    let mut pids = vec![target.pid];
    let window = HWND(target.hwnd() as *mut core::ffi::c_void);
    // SAFETY: the callback only touches `pids`, which outlives the call.
    unsafe {
        let _ = EnumChildWindows(
            Some(window),
            Some(note_process),
            LPARAM(&mut pids as *mut Vec<u32> as isize),
        );
    }
    pids
}

/// The clipboard's content, the process that owns it, and whether the content
/// stayed the same while both were read.
fn clipboard_state(clipboard: &mut arboard::Clipboard) -> (Held, Option<u32>, bool) {
    use windows::Win32::System::DataExchange::{
        CountClipboardFormats, GetClipboardOwner, GetClipboardSequenceNumber,
    };
    use windows::Win32::UI::WindowsAndMessaging::GetWindowThreadProcessId;

    let before = unsafe { GetClipboardSequenceNumber() };
    let owner = unsafe { GetClipboardOwner() }.ok().and_then(|window| {
        let mut pid = 0u32;
        unsafe { GetWindowThreadProcessId(window, Some(&mut pid)) };
        (pid != 0).then_some(pid)
    });
    let (now, readable) = match clipboard.get_text() {
        Ok(text) => (Held::Text(text), true),
        Err(arboard::Error::ClipboardOccupied) => (Held::Other, false),
        Err(_) if unsafe { CountClipboardFormats() } == 0 => (Held::Empty, true),
        Err(_) => (Held::Other, true),
    };
    let after = unsafe { GetClipboardSequenceNumber() };
    (now, owner, readable && before == after)
}

/// One UI Automation read of `target`'s selection.
///
/// Costs no keystroke and no clipboard: a few milliseconds when the target
/// answers, at worst [`UIA_PROBE_BUDGET`]. `element_for_hwnd` retries a
/// refusal, so a target that will never bind waits out the retries before
/// this falls through to the clipboard; [`UIA_PROBE_BUDGET`] prices that.
///
/// The handle is bound and dropped inside this call, so the element is
/// released on the UIA thread the moment the read is over.
///
/// PRIVACY: the string this returns is the user's document, exactly like the
/// clipboard path's, and `uia`'s own contract says the same about it. Nothing
/// here logs it — including on the failure paths, where the honest thing to
/// log is a code, and [`UiaError`](crate::uia::UiaError) is a type that can
/// only ever render an `HRESULT`.
fn uia_reading(target: &Target) -> Reading {
    // Asked for the same reason the clipboard path asks it, even though
    // nothing here synthesizes a keystroke: `uia::element_for_hwnd` binds the
    // *focused* element and requires it to belong to `hwnd`, but its
    // windowless-provider tier settles for a matching process id, and Windows
    // 11's own Notepad keeps several top-level windows in one process.
    // `is_foreground` is the strict question about *this* window, the
    // clipboard path asks it too, and the UIA path must not be the looser of
    // the two: a capture read out of a sibling window is a capture of the
    // wrong document.
    if !crate::foreground::is_foreground(target) {
        return Reading::FocusMoved;
    }
    // There is deliberately no second check after the read, where the
    // clipboard path has one. That one exists because a *keystroke* goes
    // wherever focus is at the moment `SendInput` fires; this read goes to an
    // element that was already required to belong to `target` when it was
    // bound, and it keeps going there whatever the foreground does next.
    let text = crate::uia::element_for_hwnd(target.hwnd())
        .ok()
        .and_then(|element| settled(crate::uia::selection_text(&element)));
    match text {
        Some(text) => Reading::Uia(text),
        None => Reading::Unreadable(Why::UiaFailed),
    }
}

/// Whether a UI Automation selection read settles the question this lane is
/// asking. Pure, so the rule is a unit test rather than something only a live
/// desktop can reach.
///
/// **Only `Ok(Some(non-empty))` counts**, and each of the other answers is
/// refused for its own reason:
///
/// - `Ok(None)` is `uia`'s single answer to two different questions — "this
///   element has no `TextPattern`" and "it has one and nothing is selected".
///   The first is not evidence that the user has nothing selected, and taking
///   it as such would let the agent type at the cursor over a live selection
///   in any control UIA cannot see into. That is the exact outcome this module
///   exists to prevent, so it falls back to the clipboard instead.
/// - `Err(Unavailable)` is ordinary and expected: an elevated target, a
///   password box (`uia::element_for_hwnd` refuses to hand out a handle for
///   one), a Chromium tab whose accessibility tree nothing has built yet, a
///   call past the 200 ms leash. Falls back, silently, by design.
/// - `Err(Com(_))` is an `HRESULT` `uia` has not classified. It has already
///   been logged there; here it is one more reason to use the clipboard.
/// - `Ok(Some(""))` cannot happen today — `uia::worker::selection_of` maps an
///   empty concatenation to `Ok(None)` because a caret reports one zero-length
///   range — and is refused here anyway rather than resting on that staying
///   true.
fn settled(read: Result<Option<String>, crate::uia::UiaError>) -> Option<String> {
    match read {
        Ok(Some(text)) if !text.is_empty() => Some(text),
        _ => None,
    }
}

/// THE PREFERENCE RULE: UI Automation if it answers with text, the clipboard
/// otherwise.
///
/// Written as a fall-*through* rather than a choice, and takes both readers as
/// thunks so that is provable: `clipboard` runs on every path `uia` does not
/// answer, unchanged and unaware it was second. There is no outcome in which
/// consulting UIA changes what the clipboard path would have returned, which
/// is the whole argument that preferring UIA can only add captures.
fn prefer_uia(uia: impl FnOnce() -> Reading, clipboard: impl FnOnce() -> Reading) -> Reading {
    match uia() {
        Reading::Uia(text) => Reading::Uia(text),
        _ => clipboard(),
    }
}

/// THE SAME-SOURCE RULE: read through `source` and through nothing else.
///
/// The verify's counterpart to [`prefer_uia`], and deliberately *not* a
/// preference — there is no fall-through here. A session captured through UIA
/// gets a UIA re-read or no re-read at all, because its recorded text is about
/// to be byte-compared and the two mechanisms are not guaranteed to spell the
/// same selection the same way. See the module doc.
fn from_source(
    source: Source,
    uia: impl FnOnce() -> Reading,
    clipboard: impl FnOnce() -> Reading,
) -> Reading {
    match source {
        Source::Uia => uia(),
        Source::Clipboard => clipboard(),
    }
}

/// The capture's read: [`prefer_uia`], wired to the real mechanisms.
fn read_any(target: &Target, suppress: &AtomicBool) -> Reading {
    prefer_uia(
        || uia_reading(target),
        || clipboard_reading(target, suppress),
    )
}

/// The verify's read: [`from_source`], wired to the real mechanisms.
fn read_from(source: Source, target: &Target, suppress: &AtomicBool) -> Reading {
    from_source(
        source,
        || uia_reading(target),
        || clipboard_reading(target, suppress),
    )
}

/// A reading reduced to what it says about the selection, with the mechanism
/// that read any text. The capture and the verify both judge readings through
/// this, so the acceptance rule is the same on both sides.
enum Found {
    Text(String, Source),
    Nothing,
    Moved,
    Unreadable(Why),
    /// A clipboard reading the acceptance rule could not tie to the copy.
    Unproven,
}

fn found(reading: Reading) -> Found {
    match reading {
        Reading::Uia(text) if !text.is_empty() => Found::Text(text, Source::Uia),
        Reading::Uia(_) => Found::Nothing,
        Reading::Clipboard(read) => match accept_copy(&read) {
            Copied::Selection(text) => Found::Text(text, Source::Clipboard),
            Copied::Nothing => Found::Nothing,
            Copied::Unproven => Found::Unproven,
        },
        Reading::FocusMoved => Found::Moved,
        Reading::Unreadable(why) => Found::Unreadable(why),
    }
}

/// THE SIZE CAP, the same whichever mechanism read the text: its length in
/// code points when that is at most [`MAX_SELECTION_CODE_POINTS`], else the
/// length it has.
fn within_cap(text: &str) -> Result<usize, usize> {
    let chars = text.chars().count();
    if chars <= MAX_SELECTION_CODE_POINTS {
        Ok(chars)
    } else {
        Err(chars)
    }
}

/// The capture thread's work: read `target`'s selection and judge it.
fn read(target: &Target, suppress: &Arc<AtomicBool>) -> Capture {
    let capture = file(read_any(target, suppress), target, suppress, sessions(), Instant::now());
    // `Capture`'s Debug shows a session id and a length, never the text.
    tracing::debug!(capture = ?capture, "selection capture finished");
    capture
}

/// Turn a capture reading into a [`Capture`], filing selected text that fits
/// the cap in `store` under a new session that records the mechanism.
fn file(
    reading: Reading,
    target: &Target,
    suppress: &Arc<AtomicBool>,
    store: &SessionStore,
    now: Instant,
) -> Capture {
    let (text, source) = match found(reading) {
        Found::Text(text, source) => (text, source),
        Found::Nothing => return Capture::Nothing,
        Found::Moved => return Capture::FocusMoved,
        Found::Unreadable(why) => return Capture::Unreadable(why),
        // A selection may be there, unread: fatal, never "nothing selected".
        Found::Unproven => return Capture::Unreadable(Why::ClipboardFailed),
    };
    match within_cap(&text) {
        Err(chars) => Capture::Oversized { chars },
        Ok(chars) => {
            let session = store.mint(text.clone(), target.clone(), suppress.clone(), source, now);
            Capture::Text(Selected {
                session,
                text,
                chars,
            })
        }
    }
}

// --- verify, then replace ---------------------------------------------------

/// What [`replace`] did to the document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Replacement {
    /// The document read back byte-for-byte as the session recorded it, and
    /// the replacement was pasted over the live selection.
    Replaced,
    /// Nothing was pasted, and this is the sentence that says why.
    Declined(&'static str),
}

/// Redeem `session` and, only if the document still holds exactly the text
/// read when the command was spoken, paste `replacement` over it.
///
/// In order: redeem the session (an unknown or expired token touches no
/// window), bring its window back, read the selection again through the
/// mechanism that captured it, compare, and paste with
/// `injection::inject_text`. The paste replaces the live selection, so there is
/// no separate delete. There is no timeout and nothing after the redeem can
/// be cancelled; [`REPLACE_BUDGET`] is how long it can take.
///
/// Blocking: the caller runs it on its own thread.
pub fn replace(
    session: &SessionId,
    replacement: &str,
    restore_clipboard: bool,
    restore_delay_ms: u64,
) -> Replacement {
    let outcome = redeem_and_paste(session, replacement, restore_clipboard, restore_delay_ms);
    match outcome {
        Replacement::Replaced => {
            tracing::debug!(session = %session, "selection replaced");
        }
        Replacement::Declined(notice) => {
            tracing::debug!(session = %session, notice, "selection replace declined");
        }
    }
    outcome
}

fn redeem_and_paste(
    session: &SessionId,
    replacement: &str,
    restore_clipboard: bool,
    restore_delay_ms: u64,
) -> Replacement {
    let Some(held) = sessions().take(session, Instant::now()) else {
        return Replacement::Declined(SELECTION_EXPIRED_NOTICE);
    };
    if !crate::foreground::restore_foreground(&held.target) {
        return Replacement::Declined(SELECTION_MOVED_NOTICE);
    }
    let reading = read_from(held.source, &held.target, &held.suppress);
    if let Some(notice) = verdict_of(reading, &held.text) {
        return Replacement::Declined(notice);
    }
    let _hook = Suppressed::new(&held.suppress);
    match crate::injection::inject_text(replacement, restore_clipboard, restore_delay_ms, false) {
        Ok(()) => Replacement::Replaced,
        Err(_) => Replacement::Declined(SELECTION_PASTE_FAILED_NOTICE),
    }
}

/// THE VERIFY RULE: whether a second reading authorises the paste. `None`
/// pastes; `Some` is the notice to decline with.
///
/// The reading goes through the same acceptance rule and size cap as the
/// capture, and then has to equal `expected` byte for byte: no trimming, no
/// line-ending or Unicode normalisation, no case folding. Anything else reads
/// as changed: nothing selected, a selection over the cap, and a clipboard
/// reading the acceptance rule could not tie to the copy. A window that lost
/// the foreground reads as moved, and a reading that could not be made at all
/// as unreadable.
fn verdict_of(reading: Reading, expected: &str) -> Option<&'static str> {
    match found(reading) {
        Found::Moved => Some(SELECTION_MOVED_NOTICE),
        Found::Unreadable(_) => Some(SELECTION_UNREADABLE_NOTICE),
        Found::Nothing | Found::Unproven => Some(SELECTION_CHANGED_NOTICE),
        Found::Text(text, _) => match within_cap(&text) {
            Ok(_) if text == expected => None,
            _ => Some(SELECTION_CHANGED_NOTICE),
        },
    }
}

/// A `Selected` capture for tests in other modules — `routes::agent` consults
/// this type but cannot mint a [`SessionId`], whose inner string is private so
/// that a token can only ever come from [`SessionStore::mint`].
#[cfg(test)]
pub fn test_selection(text: &str) -> Capture {
    Capture::Text(Selected {
        session: test_session("test-session"),
        text: text.to_string(),
        chars: text.chars().count(),
    })
}

/// A token with a known spelling, for tests in other modules that need to name
/// one. Never mints a store entry, so redeeming it is always the "expired"
/// path unless the test also called [`SessionStore::mint`].
#[cfg(test)]
pub fn test_session(id: &str) -> SessionId {
    SessionId(id.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn a_target() -> Target {
        crate::foreground::test_target("notepad", "Notepad")
    }

    /// The hook-suppression flag a session carries. Nothing in a unit test
    /// sends a synthetic key, so nothing ever reads it.
    fn no_suppress() -> Arc<AtomicBool> {
        Arc::new(AtomicBool::new(false))
    }

    fn selected(text: &str) -> Selected {
        Selected {
            session: SessionId("s".into()),
            text: text.into(),
            chars: text.chars().count(),
        }
    }

    // --- the plan for each capture -------------------------------------------

    /// No capture at all: the chord was not the agent chord, so nothing was
    /// read.
    #[test]
    fn no_capture_types_at_the_cursor() {
        assert_eq!(plan(None), Plan::TypeAtCursor);
    }

    /// Nothing selected, and a terminal, which is never read, both type the
    /// answer at the cursor.
    #[test]
    fn a_capture_of_nothing_types_at_the_cursor() {
        assert_eq!(plan(Some(&Capture::Nothing)), Plan::TypeAtCursor);
    }

    #[test]
    fn a_capture_of_text_is_edited_in_place() {
        let s = selected("Dispatch the Kochi samples on Thursday");
        assert_eq!(
            plan(Some(&Capture::Text(s.clone()))),
            Plan::EditSelection(&s)
        );
    }

    /// No window was captured at chord-down, so there is no selection the
    /// answer could overwrite.
    #[test]
    fn a_capture_with_no_window_types_at_the_cursor() {
        assert_eq!(
            plan(Some(&Capture::Unreadable(Why::NoWindow))),
            Plan::TypeAtCursor
        );
    }

    /// Every read that failed with a selection still possible refuses to
    /// type. This is the case that keeps an answer from landing on top of the
    /// user's live selection.
    #[test]
    fn a_failed_read_types_nothing_and_says_why() {
        for why in [Why::ClipboardFailed, Why::TimedOut, Why::UiaFailed] {
            assert_eq!(
                plan(Some(&Capture::Unreadable(why))),
                Plan::Refuse(SELECTION_UNREADABLE_NOTICE),
                "{why:?}"
            );
        }
    }

    /// Only the reasons in [`TYPE_AT_CURSOR`] type at the cursor. The `match`
    /// below has no wildcard arm, so a new `Why` does not compile until it is
    /// added to `ALL` and so classified by this test.
    #[test]
    fn every_unreadable_reason_but_no_window_types_nothing() {
        const ALL: [Why; 4] = [
            Why::NoWindow,
            Why::ClipboardFailed,
            Why::TimedOut,
            Why::UiaFailed,
        ];
        for why in ALL {
            let at_cursor = match why {
                Why::NoWindow => true,
                Why::ClipboardFailed | Why::TimedOut | Why::UiaFailed => false,
            };
            let want = if at_cursor {
                Plan::TypeAtCursor
            } else {
                Plan::Refuse(SELECTION_UNREADABLE_NOTICE)
            };
            assert_eq!(plan(Some(&Capture::Unreadable(why))), want, "{why:?}");
        }
    }

    /// An oversized selection is known to exist, so typing at the cursor
    /// would overwrite it; nothing is typed.
    #[test]
    fn an_oversized_selection_types_nothing() {
        assert_eq!(
            plan(Some(&Capture::Oversized {
                chars: MAX_SELECTION_CODE_POINTS + 1
            })),
            Plan::Refuse(SELECTION_TOO_LARGE_NOTICE)
        );
    }

    #[test]
    fn a_moved_target_types_nothing() {
        assert_eq!(
            plan(Some(&Capture::FocusMoved)),
            Plan::Refuse(SELECTION_MOVED_NOTICE)
        );
    }

    // --- the acceptance rule ---------------------------------------------------

    /// The target window's process in the clipboard readings below.
    const TARGET_PID: u32 = 4242;
    /// A process with a window inside the target's: a hosted web view, or the
    /// app a frame window hosts.
    const HOSTED_PID: u32 = 5151;
    /// Any other process: a clipboard manager, a password manager, the user
    /// copying somewhere else.
    const OTHER_PID: u32 = 777;

    /// The clean case: the target published `text` in answer to the copy and
    /// nothing else touched the clipboard, which held other text before.
    fn published_by_target(text: &str) -> CopyReading {
        CopyReading {
            copied: Some(text.into()),
            now: Held::Text(text.into()),
            owner: Some(TARGET_PID),
            steady: true,
            target_pids: vec![TARGET_PID, HOSTED_PID],
            before: Some("a phone number the user copied earlier".into()),
        }
    }

    /// The copy got no answer: the clipboard is as `copy_selection` left it,
    /// empty and owned by nobody.
    fn nothing_published() -> CopyReading {
        CopyReading {
            copied: None,
            now: Held::Empty,
            owner: None,
            ..published_by_target("")
        }
    }

    fn selection_of(read: &CopyReading) -> Option<String> {
        match accept_copy(read) {
            Copied::Selection(text) => Some(text),
            Copied::Nothing | Copied::Unproven => None,
        }
    }

    fn is_nothing(read: &CopyReading) -> bool {
        matches!(accept_copy(read), Copied::Nothing)
    }

    fn is_unproven(read: &CopyReading) -> bool {
        matches!(accept_copy(read), Copied::Unproven)
    }

    #[test]
    fn text_the_target_published_for_the_copy_is_the_selection() {
        assert_eq!(
            selection_of(&published_by_target("Invoice 42 is overdue")).as_deref(),
            Some("Invoice 42 is overdue")
        );
    }

    /// A copy written from a process with a window inside the target's (a
    /// hosted web view, a frame-hosted app) is the target's answer too.
    #[test]
    fn a_copy_written_from_inside_the_target_window_is_the_selection() {
        let hosted = CopyReading {
            owner: Some(HOSTED_PID),
            ..published_by_target("Reply by Friday with the signed form")
        };
        assert_eq!(
            selection_of(&hosted).as_deref(),
            Some("Reply by Friday with the signed form")
        );
    }

    /// Apps that open the clipboard without a window leave no owner. Their
    /// copy is accepted, since the clipboard was emptied before the keystroke.
    #[test]
    fn a_copy_with_no_owner_window_is_the_selection() {
        let ownerless = CopyReading {
            owner: None,
            ..published_by_target("Ship the Pune order on Monday")
        };
        assert_eq!(
            selection_of(&ownerless).as_deref(),
            Some("Ship the Pune order on Monday")
        );
    }

    /// Most apps publish nothing on Ctrl+C with nothing selected, so an empty
    /// answer is the ordinary "no selection", not a failure. Blank text from
    /// the target counts the same: there is nothing to edit.
    #[test]
    fn nothing_from_the_target_means_nothing_is_selected() {
        assert!(is_nothing(&nothing_published()));
        assert!(is_nothing(&CopyReading {
            now: Held::Text("  \n".into()),
            owner: Some(TARGET_PID),
            ..nothing_published()
        }));
    }

    /// The value the user had before the copy, put back during the read by
    /// something else, is refused: by a clipboard manager that owns what it
    /// writes, and by a writer with no window, which cannot be told from the
    /// target only in this case.
    #[test]
    fn a_value_from_before_the_copy_is_refused() {
        let before = "a phone number the user copied earlier";
        let put_back_by_a_manager = CopyReading {
            owner: Some(OTHER_PID),
            ..published_by_target(before)
        };
        assert!(is_unproven(&put_back_by_a_manager));
        let put_back_without_an_owner = CopyReading {
            owner: None,
            ..published_by_target(before)
        };
        assert!(is_unproven(&put_back_without_an_owner));
    }

    /// A write by another process during the read is refused, and so is a
    /// reading whose content changed under it.
    #[test]
    fn a_value_another_process_wrote_during_the_read_is_refused() {
        let someone_else = CopyReading {
            owner: Some(OTHER_PID),
            ..published_by_target("a password from the password manager")
        };
        assert!(is_unproven(&someone_else));

        let replaced_after_the_copy = CopyReading {
            now: Held::Text("something written later".into()),
            ..published_by_target("the selected sentence")
        };
        assert!(is_unproven(&replaced_after_the_copy));

        let moving = CopyReading {
            steady: false,
            ..published_by_target("the selected sentence")
        };
        assert!(is_unproven(&moving));
    }

    /// The target answered, but not with anything `copy_selection` returned:
    /// its text came after the copy stopped waiting, or it published an image.
    /// A selection exists, so this is not "nothing selected".
    #[test]
    fn a_target_answer_the_copy_did_not_return_is_unproven() {
        assert!(is_unproven(&CopyReading {
            now: Held::Text("a slow app's answer".into()),
            owner: Some(TARGET_PID),
            ..nothing_published()
        }));
        assert!(is_unproven(&CopyReading {
            now: Held::Other,
            owner: Some(HOSTED_PID),
            ..nothing_published()
        }));
    }

    /// The cases the acceptance rule cannot tell apart, pinned so that a
    /// change to any of them is deliberate.
    #[test]
    fn what_the_acceptance_rule_cannot_tell_apart() {
        // The copy returns the same text the clipboard held before it. When
        // the target owns the copy that is accepted, and the restore then
        // finds the user's text already in place. With no owner it is refused.
        let already_there = "Invoice 42 is overdue";
        let recopied = CopyReading {
            before: Some(already_there.into()),
            ..published_by_target(already_there)
        };
        assert_eq!(selection_of(&recopied).as_deref(), Some(already_there));
        assert_eq!(
            restore_plan(
                Some(already_there),
                &Held::Text(already_there.into()),
                Some(already_there)
            ),
            Restore::Leave
        );
        assert!(is_unproven(&CopyReading {
            owner: None,
            ..recopied
        }));

        // A background write from one of the target's own processes, or from
        // a writer with no window, looks like the copy's answer.
        assert_eq!(
            selection_of(&published_by_target("written by another window of the same app"))
                .as_deref(),
            Some("written by another window of the same app")
        );
        assert_eq!(
            selection_of(&CopyReading {
                owner: None,
                ..published_by_target("written by a program with no clipboard window")
            })
            .as_deref(),
            Some("written by a program with no clipboard window")
        );

        // A copy written by a process with no window in the target is
        // refused, even when it is the target's answer.
        assert!(is_unproven(&CopyReading {
            owner: Some(OTHER_PID),
            ..published_by_target("copied through a helper process")
        }));
    }

    // --- putting the clipboard back -------------------------------------------

    /// (a) The clipboard already holds what the user had, empty before and
    /// after included: nothing is written.
    #[test]
    fn a_clipboard_already_back_where_it_was_is_left_alone() {
        assert_eq!(
            restore_plan(Some("user text"), &Held::Text("user text".into()), Some("the selection")),
            Restore::Leave
        );
        assert_eq!(restore_plan(None, &Held::Empty, None), Restore::Leave);
    }

    /// (b) Something that is neither blank nor the copy's text was put there
    /// during the read and stays, whether or not the clipboard held text
    /// before, and whoever wrote it: the user may have pressed Ctrl+C in the
    /// target itself.
    #[test]
    fn something_copied_during_the_read_is_left_alone() {
        for copied in [Some("the selection"), None] {
            assert_eq!(
                restore_plan(Some("user text"), &Held::Text("copied mid-read".into()), copied),
                Restore::Leave
            );
            assert_eq!(
                restore_plan(None, &Held::Text("copied mid-read".into()), copied),
                Restore::Leave
            );
            assert_eq!(restore_plan(Some("user text"), &Held::Other, copied), Restore::Leave);
        }
    }

    /// (c) The copy's text, or the empty clipboard a copy that published
    /// nothing leaves behind, is replaced by the user's text. Who owns the
    /// clipboard is not an input: a copy written without an owner window, or
    /// from a helper process, is removed the same way.
    #[test]
    fn the_copy_is_replaced_by_the_users_text() {
        assert_eq!(
            restore_plan(
                Some("user text"),
                &Held::Text("the selection".into()),
                Some("the selection")
            ),
            Restore::Put("user text".into())
        );
        assert_eq!(
            restore_plan(Some("user text"), &Held::Empty, None),
            Restore::Put("user text".into())
        );
        assert_eq!(
            restore_plan(Some("user text"), &Held::Text(" \r\n".into()), None),
            Restore::Put("user text".into())
        );
    }

    /// (d) With no text to put back, the clipboard is emptied rather than left
    /// holding the document.
    #[test]
    fn with_nothing_to_put_back_the_clipboard_is_emptied() {
        assert_eq!(
            restore_plan(None, &Held::Text("the selection".into()), Some("the selection")),
            Restore::Clear
        );
    }

    // --- the size cap ------------------------------------------------------------

    fn filed(reading: Reading, store: &SessionStore) -> Capture {
        file(reading, &a_target(), &no_suppress(), store, Instant::now())
    }

    #[test]
    fn a_selection_at_the_cap_passes() {
        let store = SessionStore::default();
        let text = "a".repeat(MAX_SELECTION_CODE_POINTS);
        let Capture::Text(s) = filed(Reading::Uia(text), &store) else {
            panic!("a selection at the cap is a selection");
        };
        assert_eq!(s.chars, MAX_SELECTION_CODE_POINTS);
    }

    #[test]
    fn one_over_the_cap_is_too_large_and_counted() {
        let store = SessionStore::default();
        let text = "a".repeat(MAX_SELECTION_CODE_POINTS + 1);
        assert_eq!(
            filed(Reading::Uia(text), &store),
            Capture::Oversized {
                chars: MAX_SELECTION_CODE_POINTS + 1
            }
        );
        assert_eq!(store.len(), 0, "an oversized selection gets no session");
    }

    /// The unit is code points. Devanagari and emoji take three and four bytes
    /// each, so a byte count would refuse the first of these and a count of
    /// visible characters would accept the second.
    #[test]
    fn the_cap_counts_code_points() {
        let store = SessionStore::default();
        let at_cap = "क".repeat(MAX_SELECTION_CODE_POINTS);
        assert!(matches!(
            filed(Reading::Uia(at_cap), &store),
            Capture::Text(_)
        ));
        let over: String = "कि".repeat(MAX_SELECTION_CODE_POINTS / 2) + "😀";
        assert_eq!(
            filed(Reading::Uia(over), &store),
            Capture::Oversized {
                chars: MAX_SELECTION_CODE_POINTS + 1
            }
        );
    }

    /// Both mechanisms pass through the same cap.
    #[test]
    fn the_cap_is_the_same_for_both_mechanisms() {
        let store = SessionStore::default();
        let at_cap = "b".repeat(MAX_SELECTION_CODE_POINTS);
        let over = "b".repeat(MAX_SELECTION_CODE_POINTS + 1);
        for reading in [Reading::Uia(at_cap.clone()), a_clipboard_reading(&at_cap)] {
            assert!(matches!(filed(reading, &store), Capture::Text(_)));
        }
        for reading in [Reading::Uia(over.clone()), a_clipboard_reading(&over)] {
            assert_eq!(
                filed(reading, &store),
                Capture::Oversized {
                    chars: MAX_SELECTION_CODE_POINTS + 1
                }
            );
        }
        assert_eq!(
            plan(Some(&Capture::Oversized {
                chars: MAX_SELECTION_CODE_POINTS + 1
            })),
            Plan::Refuse(SELECTION_TOO_LARGE_NOTICE)
        );
    }

    /// The notice states the cap the way a user reads a number.
    #[test]
    fn the_too_large_notice_names_the_cap() {
        let digits = MAX_SELECTION_CODE_POINTS.to_string();
        let mut grouped = String::new();
        for (i, digit) in digits.chars().enumerate() {
            if i > 0 && (digits.len() - i) % 3 == 0 {
                grouped.push(',');
            }
            grouped.push(digit);
        }
        assert!(
            SELECTION_TOO_LARGE_NOTICE.contains(&format!(" {grouped} ")),
            "{SELECTION_TOO_LARGE_NOTICE:?} should name {grouped}"
        );
    }

    /// A full-size selection has to fit back through the selection reply
    /// ceiling with room to double. 309 tokens per 1,000 code points is what
    /// sarvam-105b spent echoing Telugu, the costliest script measured
    /// (Malayalam 291, Hindi 279, English 199).
    #[test]
    fn the_selection_cap_fits_the_selection_reply_budget() {
        const TOKENS_PER_THOUSAND_CODE_POINTS: usize = 309;
        let full_selection = MAX_SELECTION_CODE_POINTS * TOKENS_PER_THOUSAND_CODE_POINTS / 1000;
        let ceiling = crate::sarvam::chat::SELECTION_MAX_OUTPUT_TOKENS as usize;
        assert!(
            ceiling >= 2 * full_selection,
            "{ceiling} tokens cannot hold a reply twice a {full_selection}-token selection"
        );
        assert!(ceiling <= 4096, "over the output limit Sarvam documents for its entry plan");
    }

    /// Three failures, three different sentences. A user told "couldn't read
    /// the selection" when the real answer is "it is too big" has been
    /// pointed at the wrong fix.
    #[test]
    fn every_selection_decline_says_something_different() {
        let notices = [
            SELECTION_TOO_LARGE_NOTICE,
            SELECTION_UNREADABLE_NOTICE,
            SELECTION_MOVED_NOTICE,
        ];
        for (i, a) in notices.iter().enumerate() {
            assert!(!a.is_empty());
            for b in &notices[i + 1..] {
                assert_ne!(a, b);
            }
        }
    }

    // --- which mechanism read it --------------------------------------------

    /// The readings a stubbed mechanism can hand back, and a flag saying
    /// whether it was consulted at all. The whole point of the preference rule
    /// is *which of the two runs*, so "did this one run" has to be observable.
    fn recording(answer: Reading, ran: &std::cell::Cell<bool>) -> impl FnOnce() -> Reading + '_ {
        move || {
            ran.set(true);
            answer
        }
    }

    fn a_uia_reading(text: &str) -> Reading {
        Reading::Uia(text.to_string())
    }

    /// A clipboard reading the acceptance rule takes as `text`.
    fn a_clipboard_reading(text: &str) -> Reading {
        Reading::Clipboard(published_by_target(text))
    }

    /// UIA WINS WHEN IT ANSWERS, and the clipboard is not even asked — which
    /// is the part worth pinning. Not asking means no synthetic Ctrl+C, no
    /// clipboard save and restore, and no `COPY_BUDGET` spent.
    #[test]
    fn a_uia_answer_is_the_capture_and_the_clipboard_is_never_touched() {
        let clipboard_ran = std::cell::Cell::new(false);
        let got = prefer_uia(
            || a_uia_reading("the quick brown fox"),
            recording(a_clipboard_reading("something else"), &clipboard_ran),
        );
        assert!(
            matches!(&got, Reading::Uia(t) if t == "the quick brown fox"),
            "a UIA answer must be the capture"
        );
        assert!(
            !clipboard_ran.get(),
            "the clipboard read must not run when UIA already answered"
        );
    }

    /// Anything but text from UIA falls through to the clipboard, and the
    /// clipboard's reading comes back as if UIA had never been asked.
    #[test]
    fn every_uia_non_answer_falls_through_to_the_clipboard_unchanged() {
        let non_answers: [fn() -> Reading; 3] = [
            || Reading::Unreadable(Why::UiaFailed),
            || Reading::FocusMoved,
            || Reading::Unreadable(Why::NoWindow),
        ];
        for uia in non_answers {
            let got = prefer_uia(uia, || a_clipboard_reading("the quick brown fox"));
            let Reading::Clipboard(read) = got else {
                panic!("a UIA non-answer must fall through to the clipboard");
            };
            assert_eq!(read.copied.as_deref(), Some("the quick brown fox"));
            assert!(read.now == Held::Text("the quick brown fox".into()));
            assert_eq!(read.owner, Some(TARGET_PID));
            assert!(read.steady);
            assert!(matches!(
                found(Reading::Clipboard(read)),
                Found::Text(text, Source::Clipboard) if text == "the quick brown fox"
            ));
        }
    }

    /// THE MIXED-SOURCE RULE, from the UIA side: a session captured through
    /// UIA is re-read through UIA and *never* falls back to the clipboard. The
    /// two mechanisms do not have to spell the same selection the same way — a
    /// provider reporting `\r` where the clipboard publishes `\r\n` is not a
    /// user edit — and a byte-compare across them would report every multi-line
    /// selection in such an app as changed.
    #[test]
    fn a_uia_session_never_verifies_against_a_clipboard_read() {
        let clipboard_ran = std::cell::Cell::new(false);
        let got = from_source(
            Source::Uia,
            || Reading::Unreadable(Why::UiaFailed),
            recording(a_clipboard_reading("the same selection, spelled differently"), &clipboard_ran),
        );
        assert!(
            !clipboard_ran.get(),
            "a UIA session must not be verified against a clipboard read"
        );
        assert!(matches!(got, Reading::Unreadable(Why::UiaFailed)));
        // ...and having nowhere to fall through to is a decline, not a paste.
        assert_eq!(
            verdict_of(
                Reading::Unreadable(Why::UiaFailed),
                "the quick brown fox"
            ),
            Some(SELECTION_UNREADABLE_NOTICE)
        );
    }

    /// A UIA verify read is byte-compared exactly like a clipboard one — the
    /// mechanism is cheaper, the standard is not lower.
    #[test]
    fn a_uia_verify_read_is_byte_compared_like_any_other() {
        let original = "he dont like it";
        assert_eq!(verdict_of(Reading::Uia(original.into()), original), None);
        for changed in [
            "he don't like it",
            "he dont like it ",
            "he dont like it\r\n",
        ] {
            assert_eq!(
                verdict_of(Reading::Uia(changed.into()), original),
                Some(SELECTION_CHANGED_NOTICE),
                "{changed:?} is not what was captured"
            );
        }
        assert_eq!(
            verdict_of(Reading::FocusMoved, original),
            Some(SELECTION_MOVED_NOTICE)
        );
    }

    /// ...and from the clipboard side: a session captured through the
    /// clipboard is re-read through the clipboard, and UIA is not consulted at
    /// the verify even if it would answer. Same reason, opposite direction.
    #[test]
    fn a_clipboard_session_never_verifies_against_a_uia_read() {
        let uia_ran = std::cell::Cell::new(false);
        let got = from_source(
            Source::Clipboard,
            recording(a_uia_reading("the quick brown fox"), &uia_ran),
            || a_clipboard_reading("the quick brown fox"),
        );
        assert!(
            !uia_ran.get(),
            "a clipboard session must not be verified against a UIA read"
        );
        assert!(matches!(got, Reading::Clipboard { .. }));
    }

    /// A UIA read only settles the question when it comes back with text.
    /// `Ok(None)` is the one that matters: it means "no TextPattern" *or*
    /// "nothing selected", and the first is not evidence of the second — so it
    /// falls back rather than telling the agent to type at the cursor over a
    /// selection UIA simply cannot see.
    #[test]
    fn only_text_from_uia_settles_the_question() {
        assert_eq!(
            settled(Ok(Some("selected words".into()))),
            Some("selected words".to_string())
        );
        for refused in [
            Ok(None),
            Ok(Some(String::new())),
            Err(crate::uia::UiaError::Unavailable),
            Err(crate::uia::UiaError::Com(0x8000_4005u32 as i32)),
        ] {
            assert_eq!(
                settled(refused.clone()),
                None,
                "{refused:?} must fall back to the clipboard"
            );
        }
    }

    /// The route seam's budget has to cover the UIA probe *as well as* the
    /// clipboard read it may still have to do, or preferring UIA would turn a
    /// slow but working capture into `Why::TimedOut`, which types nothing.
    ///
    /// The numbers are wall-clock literals on purpose. An equation between
    /// constants that move together keeps passing when they both move; a
    /// literal fails with the new number, which is the prompt to re-add
    /// `controller::CLOUD_FINALIZE_TIMEOUT`'s itemised sum.
    #[test]
    fn the_resolve_budget_pays_for_the_uia_probe_on_top_of_the_clipboard_read() {
        assert_eq!(
            UIA_PROBE_BUDGET,
            Duration::from_millis(1400),
            "3 bind attempts at 200 ms, 2 waits of 300 ms, then one 200 ms read"
        );
        assert_eq!(
            RESOLVE_BUDGET,
            CLIPBOARD_RESOLVE_BUDGET + UIA_PROBE_BUDGET,
            "the clipboard read must keep exactly the budget it had"
        );
        assert_eq!(
            RESOLVE_BUDGET,
            Duration::from_millis(2380),
            "controller::CLOUD_FINALIZE_TIMEOUT itemizes this by name — move both"
        );
    }

    /// The replace step runs inside the finalize window — `routes::agent`
    /// awaits it before answering `RouteDone` — so it is a term in
    /// `controller::CLOUD_FINALIZE_TIMEOUT` and pinned the same way: derived
    /// from the real sub-budgets, with the wall-clock asserted by hand so
    /// moving any of them fails here and forces the ceiling to be re-derived.
    #[test]
    fn the_replace_budget_covers_every_wait_between_burning_the_session_and_the_paste() {
        assert_eq!(
            REPLACE_BUDGET,
            Duration::from_millis(2490),
            "35 ms foreground settle + 1.4 s verify re-read + 30 ms paste settle \
             + 25 ms modifier drain + 1 s clamped clipboard-restore delay"
        );
        // The verify re-read is budgeted at the WORSE of the two sources. If
        // the clipboard path ever overtakes UIA, this term is understated.
        let clipboard_verify = CLIPBOARD_RESOLVE_BUDGET;
        assert!(
            UIA_PROBE_BUDGET > clipboard_verify,
            "the UIA re-read stopped being the worse half: {UIA_PROBE_BUDGET:?} \
             vs {clipboard_verify:?} — re-derive REPLACE_BUDGET"
        );
    }

    /// Term 5 of [`REPLACE_BUDGET`] is the *clamped maximum* of a setting,
    /// not its default, and that is what makes the sum hold for every settings
    /// file, including one edited by hand.
    ///
    /// Two things are pinned here. That the ceiling is still 1 s — moving it
    /// moves `controller::CLOUD_FINALIZE_TIMEOUT` — and that the default the
    /// app ships is inside it, since a default outside its own clamp would be
    /// rewritten on the first load.
    #[test]
    fn the_replace_budget_prices_the_clamped_maximum_restore_delay() {
        assert_eq!(
            1000,
            crate::settings::RESTORE_DELAY_MAX_MS,
            "REPLACE_BUDGET is derived from this; controller::CLOUD_FINALIZE_TIMEOUT from that"
        );
        assert!(
            crate::settings::InjectionSettings::default().restore_delay_ms
                <= crate::settings::RESTORE_DELAY_MAX_MS,
            "the shipped default is outside its own clamp — repair() would rewrite every file"
        );
    }

    // --- sessions -----------------------------------------------------------

    #[test]
    fn a_minted_session_hands_back_the_exact_selection() {
        let store = SessionStore::default();
        let now = Instant::now();
        let id = store.mint("the quick brown fox".into(), a_target(), no_suppress(), Source::Clipboard, now);
        let got = store.take(&id, now).expect("a fresh session redeems");
        assert_eq!(got.text, "the quick brown fox");
        assert_eq!(got.target.app.as_deref(), Some("notepad"));
    }

    /// SINGLE USE. The token is burned by the *first* redemption, whatever
    /// the caller then decides about it, so a verification that fails cannot
    /// leave a token behind for a second attempt against a document that has
    /// moved on.
    #[test]
    fn a_session_redeems_exactly_once() {
        let store = SessionStore::default();
        let now = Instant::now();
        let id = store.mint("selected".into(), a_target(), no_suppress(), Source::Clipboard, now);
        assert!(store.take(&id, now).is_some());
        assert!(
            store.take(&id, now).is_none(),
            "the second redemption must find nothing"
        );
        assert_eq!(store.len(), 0);
    }

    #[test]
    fn a_session_expires_after_its_ttl() {
        let store = SessionStore::default();
        let start = Instant::now();
        let early = store.mint("one".into(), a_target(), no_suppress(), Source::Clipboard, start);
        let late = store.mint("two".into(), a_target(), no_suppress(), Source::Clipboard, start);
        assert!(
            store
                .take(&early, start + SESSION_TTL - Duration::from_millis(1))
                .is_some(),
            "still live a moment before the TTL"
        );
        assert!(
            store
                .take(&late, start + SESSION_TTL + Duration::from_secs(1))
                .is_none(),
            "gone after it"
        );
    }

    /// Expired sessions hold document text, so the next store operation of
    /// either kind drops them, whether or not anyone asks for them.
    #[test]
    fn expired_sessions_do_not_outlive_the_next_store_operation() {
        let store = SessionStore::default();
        let start = Instant::now();
        store.mint("one".into(), a_target(), no_suppress(), Source::Clipboard, start);
        store.mint("two".into(), a_target(), no_suppress(), Source::Clipboard, start);
        store.mint("three".into(), a_target(), no_suppress(), Source::Uia, start + SESSION_TTL);
        assert_eq!(store.len(), 1, "a mint drops what has expired");

        let store = SessionStore::default();
        store.mint("one".into(), a_target(), no_suppress(), Source::Clipboard, start);
        assert!(store
            .take(&SessionId("another".into()), start + SESSION_TTL)
            .is_none());
        assert_eq!(store.len(), 0, "a redemption drops what has expired");
    }

    /// `clear` revokes everything, which is what makes a late replace after
    /// an Escape or a timeout find nothing.
    #[test]
    fn clearing_the_store_revokes_every_session() {
        let store = SessionStore::default();
        let now = Instant::now();
        let id = store.mint("one".into(), a_target(), no_suppress(), Source::Uia, now);
        store.clear();
        assert!(store.take(&id, now).is_none());
    }

    /// A capture's session records the mechanism that read it, since that is
    /// the one the verify read must use. Readings without text file nothing.
    #[test]
    fn a_capture_files_the_source_that_read_it() {
        let store = SessionStore::default();
        for (reading, source) in [
            (a_uia_reading("read by UI Automation"), Source::Uia),
            (a_clipboard_reading("read through the clipboard"), Source::Clipboard),
        ] {
            let Capture::Text(s) = filed(reading, &store) else {
                panic!("text within the cap is a selection");
            };
            let held = store.take(&s.session, Instant::now()).expect("filed");
            assert_eq!(held.source, source);
            assert!(held.text == s.text, "the session holds the captured text");
        }
        for reading in [
            Reading::Clipboard(nothing_published()),
            Reading::FocusMoved,
            Reading::Unreadable(Why::ClipboardFailed),
        ] {
            assert!(!matches!(filed(reading, &store), Capture::Text(_)));
        }
        assert_eq!(store.len(), 0);
    }

    /// The TTL is measured from the mint, not from the last touch.
    #[test]
    fn the_ttl_starts_at_the_mint() {
        let store = SessionStore::default();
        let start = Instant::now();
        let id = store.mint("selected".into(), a_target(), no_suppress(), Source::Clipboard, start);
        assert!(store.take(&id, start + SESSION_TTL).is_none(), "exactly at expiry");
    }

    #[test]
    fn an_unknown_session_redeems_nothing() {
        let store = SessionStore::default();
        assert!(store
            .take(&SessionId("never-minted".into()), Instant::now())
            .is_none());
    }

    #[test]
    fn every_session_gets_its_own_token() {
        let store = SessionStore::default();
        let now = Instant::now();
        let a = store.mint("one".into(), a_target(), no_suppress(), Source::Clipboard, now);
        let b = store.mint("two".into(), a_target(), no_suppress(), Source::Clipboard, now);
        assert_ne!(a, b);
        assert_eq!(store.len(), 2);
    }

    // --- privacy -------------------------------------------------------------

    /// The invariant the module doc promises, made structural: a `Debug` of a
    /// capture — the natural thing to reach for in a `tracing::debug!` while
    /// chasing a bug — must not carry the user's document with it.
    #[test]
    fn debugging_a_capture_never_prints_the_selection() {
        let secret = "the merger closes on the fourteenth";
        let capture = Capture::Text(selected(secret));
        let rendered = format!("{capture:?}");
        assert!(
            !rendered.contains(secret),
            "a capture's Debug leaked the document: {rendered}"
        );
        assert!(rendered.contains("chars"), "the length is still useful");
        assert!(rendered.contains("session"), "so is the opaque session id");

        let held = Session {
            text: secret.into(),
            target: a_target(),
            source: Source::Clipboard,
            suppress: no_suppress(),
        };
        let rendered = format!("{held:?}");
        assert!(
            !rendered.contains(secret),
            "a session's Debug leaked the document: {rendered}"
        );
    }

    // --- verify, then replace ------------------------------------------------

    /// A token that was never minted — or one whose session has already been
    /// redeemed — replaces nothing, and gets there *without* touching the
    /// document: the early return happens before any window is activated and
    /// before any synthetic key is sent. (`a_target()` has no real window
    /// behind it, so anything past this point would fail anyway; the point of
    /// the assertion is that it does not have to.)
    #[test]
    fn an_unredeemable_session_replaces_nothing() {
        assert_eq!(
            replace(&test_session("never-minted"), "new text", true, 0),
            Replacement::Declined(SELECTION_EXPIRED_NOTICE)
        );
    }

    /// SINGLE USE ON THE REPLACE PATH. The token is burned by the first
    /// attempt whatever that attempt then decides, so a failed verification
    /// cannot be retried against a document that has since moved on. Here the
    /// first call fails at the foreground restore (a `test_target` has no
    /// window), and the second must still find nothing to redeem.
    #[test]
    fn a_replace_burns_the_session_even_when_it_declines() {
        let id = sessions().mint(
            "the quick brown fox".into(),
            a_target(),
            no_suppress(),
            Source::Clipboard,
            Instant::now(),
        );
        let first = replace(&id, "new text", true, 0);
        assert_eq!(
            first,
            Replacement::Declined(SELECTION_MOVED_NOTICE),
            "a window that cannot be brought forward must fail closed"
        );
        assert_eq!(
            replace(&id, "new text", true, 0),
            Replacement::Declined(SELECTION_EXPIRED_NOTICE),
            "the token must not survive its first use"
        );
    }

    // --- the keyboard hook --------------------------------------------------

    /// The guard's whole contract: shut while it lives, open the moment it
    /// dies, and dying by unwind counts. With a hand-written pair of stores, a
    /// panic between them on one of this module's spawned threads would leave
    /// the flag `true` for the life of the process — every hotkey in the app
    /// dead, on a thread with nothing left running to notice.
    #[test]
    fn the_hook_guard_opens_again_on_every_exit_including_a_panic() {
        let flag = AtomicBool::new(false);
        {
            let _hook = Suppressed::new(&flag);
            assert!(flag.load(Ordering::Relaxed), "shut while the guard lives");
        }
        assert!(!flag.load(Ordering::Relaxed), "open again when it drops");

        let flag = AtomicBool::new(false);
        let unwound = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _hook = Suppressed::new(&flag);
            assert!(flag.load(Ordering::Relaxed));
            panic!("deliberate: the guard must survive this");
        }));
        assert!(unwound.is_err(), "the fixture must actually panic");
        assert!(
            !flag.load(Ordering::Relaxed),
            "an unwind must not leave the hook deaf for the life of the process"
        );
    }

    /// Whatever `replace` does, it hands the hook back. A paste made with the
    /// hook *listening* puts its own Ctrl+V and `release_stuck_modifiers`'
    /// key-ups into the hook's `down` set, where they are evaluated against
    /// the user's bindings — an overlapping binding fires a phantom chord off
    /// a keystroke nobody pressed.
    ///
    /// This run declines at the foreground restore (a `test_target` has no
    /// window behind it), which is exactly the kind of early exit a
    /// hand-written `store(false)` is most likely to miss.
    #[test]
    fn a_replace_always_hands_the_keyboard_hook_back() {
        let suppress = no_suppress();
        let id = sessions().mint(
            "the quick brown fox".into(),
            a_target(),
            suppress.clone(),
            Source::Clipboard,
            Instant::now(),
        );
        assert_eq!(
            replace(&id, "new text", true, 0),
            Replacement::Declined(SELECTION_MOVED_NOTICE)
        );
        assert!(
            !suppress.load(Ordering::Relaxed),
            "the hook must be listening again however the replace ended"
        );

        // ...and on the path that never touches a window at all.
        let suppress = no_suppress();
        assert_eq!(
            replace(&test_session("never-minted"), "new text", true, 0),
            Replacement::Declined(SELECTION_EXPIRED_NOTICE)
        );
        assert!(!suppress.load(Ordering::Relaxed));
    }

    /// The flag the paste suppresses is the session's own — the same one the
    /// capture and the verify read use — so the two halves of the lane cannot
    /// end up muting different hooks. Asserted through the store, since that is
    /// the only thing that carries it from one half to the other.
    #[test]
    fn the_paste_suppresses_the_flag_the_capture_used() {
        let suppress = no_suppress();
        let id = sessions().mint("x".into(), a_target(), suppress.clone(), Source::Clipboard, Instant::now());
        let held = sessions()
            .take(&id, Instant::now())
            .expect("a fresh session redeems");
        assert!(
            Arc::ptr_eq(&held.suppress, &suppress),
            "the session must hand back the very flag it was minted with"
        );
    }

    /// Every way the replace can decline says something different, so the pill
    /// points the user at the right fix — "select it again" is not the same
    /// advice as "check what has focus".
    #[test]
    fn every_replace_decline_says_something_different() {
        let notices = [
            SELECTION_CHANGED_NOTICE,
            SELECTION_EXPIRED_NOTICE,
            SELECTION_PASTE_FAILED_NOTICE,
            SELECTION_MOVED_NOTICE,
            SELECTION_UNREADABLE_NOTICE,
        ];
        for (i, a) in notices.iter().enumerate() {
            assert!(!a.is_empty());
            for b in &notices[i + 1..] {
                assert_ne!(a, b);
            }
        }
    }

    // --- the byte compare ----------------------------------------------------

    const RECORDED: &str = "Dispatch the samples to Pune\nby Thursday.";

    /// Only a second reading that is byte for byte the recorded text lets the
    /// paste through.
    #[test]
    fn an_exact_second_reading_authorises_the_paste() {
        assert_eq!(verdict_of(a_clipboard_reading(RECORDED), RECORDED), None);
    }

    /// No trimming, no line-ending or case normalisation: any difference
    /// declines as changed.
    #[test]
    fn any_difference_at_all_declines_the_paste() {
        for changed in [
            " Dispatch the samples to Pune\nby Thursday.",
            "Dispatch the samples to Pune\nby Thursday. ",
            "Dispatch the samples to Puna\nby Thursday.",
            "Dispatch the samples to Pune",
            "Dispatch the samples to Pune\r\nby Thursday.",
            "dispatch the samples to Pune\nby Thursday.",
        ] {
            assert_eq!(
                verdict_of(a_clipboard_reading(changed), RECORDED),
                Some(SELECTION_CHANGED_NOTICE),
                "{changed:?} is not what was captured"
            );
        }
    }

    /// The selection is gone by the time of the second read: nothing to
    /// replace.
    #[test]
    fn a_second_reading_with_no_selection_replaces_nothing() {
        assert_eq!(
            verdict_of(Reading::Clipboard(nothing_published()), RECORDED),
            Some(SELECTION_CHANGED_NOTICE)
        );
    }

    /// A second reading over the cap declines as changed even when it matches
    /// the recorded text, since no capture over the cap is ever recorded.
    #[test]
    fn an_oversized_second_reading_declines_as_changed() {
        let long = "c".repeat(MAX_SELECTION_CODE_POINTS + 1);
        assert_eq!(
            verdict_of(a_clipboard_reading(&long), &long),
            Some(SELECTION_CHANGED_NOTICE)
        );
    }

    /// A second clipboard reading the acceptance rule refuses is not an exact
    /// match, so it declines as changed, like any other non-match.
    #[test]
    fn an_unproven_second_reading_declines_as_changed() {
        let foreign = CopyReading {
            owner: Some(OTHER_PID),
            ..published_by_target(RECORDED)
        };
        assert_eq!(
            verdict_of(Reading::Clipboard(foreign), RECORDED),
            Some(SELECTION_CHANGED_NOTICE)
        );
    }

    /// At capture time the same refusal is fatal as unreadable: a selection
    /// may be there, unread, so the answer is not typed at the cursor.
    #[test]
    fn an_unproven_capture_types_nothing() {
        let store = SessionStore::default();
        let foreign = CopyReading {
            owner: Some(OTHER_PID),
            ..published_by_target("the selected sentence")
        };
        let capture = filed(Reading::Clipboard(foreign), &store);
        assert_eq!(capture, Capture::Unreadable(Why::ClipboardFailed));
        assert_eq!(
            plan(Some(&capture)),
            Plan::Refuse(SELECTION_UNREADABLE_NOTICE)
        );
        assert_eq!(store.len(), 0);
    }

    // --- terminals -------------------------------------------------------------

    /// A shell would run a pasted multi-line reply line by line. A terminal
    /// captures as `Nothing` before any synthetic key is sent, so the answer
    /// is typed at the cursor.
    #[test]
    fn a_terminal_captures_nothing_without_touching_the_keyboard() {
        let never_suppressed = Arc::new(AtomicBool::new(false));
        for (app, class) in [
            ("windowsterminal", "CASCADIA_HOSTING_WINDOW_CLASS"),
            ("python", "ConsoleWindowClass"),
            ("powershell", "SomeOrdinaryClass"),
            ("electerm", "Chrome_WidgetWin_1"),
        ] {
            let pending = begin(
                Some(crate::foreground::test_target(app, class)),
                never_suppressed.clone(),
            );
            assert!(
                matches!(pending, PendingCapture::Ready(Capture::Nothing)),
                "{app}/{class} must capture nothing without spawning a copy"
            );
            assert!(
                !never_suppressed.load(Ordering::Relaxed),
                "a terminal is sent no synthetic input, so nothing is suppressed"
            );
            assert_eq!(plan(Some(&Capture::Nothing)), Plan::TypeAtCursor);
        }
    }

    /// No window captured at chord-down: typed at the cursor, and again with
    /// no synthetic input.
    #[test]
    fn no_window_types_at_the_cursor_without_a_copy() {
        let suppress = Arc::new(AtomicBool::new(false));
        let PendingCapture::Ready(capture) = begin(None, suppress.clone()) else {
            panic!("with no window there is nothing to read");
        };
        assert_eq!(capture, Capture::Unreadable(Why::NoWindow));
        assert_eq!(plan(Some(&capture)), Plan::TypeAtCursor);
        assert!(!suppress.load(Ordering::Relaxed));
    }

    // --- resolving ----------------------------------------------------------

    #[test]
    fn a_ready_capture_resolves_to_itself() {
        assert_eq!(
            PendingCapture::Ready(Capture::FocusMoved).resolve(),
            Capture::FocusMoved
        );
    }

    #[test]
    fn an_in_flight_capture_resolves_to_what_the_thread_sends() {
        let (tx, rx) = crossbeam_channel::bounded(1);
        tx.send(Capture::Nothing).expect("send");
        assert_eq!(PendingCapture::InFlight(rx).resolve(), Capture::Nothing);
    }

    /// A capture thread that died without answering leaves the question open,
    /// and an open question is fatal — never a quiet "no selection".
    #[test]
    fn a_capture_that_never_answers_is_fatal() {
        let (tx, rx) = crossbeam_channel::bounded::<Capture>(1);
        drop(tx);
        let capture = PendingCapture::InFlight(rx).resolve();
        assert_eq!(capture, Capture::Unreadable(Why::TimedOut));
        assert_eq!(
            plan(Some(&capture)),
            Plan::Refuse(SELECTION_UNREADABLE_NOTICE)
        );
    }

    // --- live -------------------------------------------------------------

    /// The whole path against a real window, which nothing above can cover:
    /// `SendInput` needs a focused caret and the clipboard needs a live
    /// window station, so this cannot run on CI or alongside other tests.
    ///
    /// To run it: open Notepad, type `the quick brown fox`, select it all
    /// (Ctrl+A), leave Notepad focused, then from another machine-local shell
    ///
    /// ```text
    /// cargo test --lib selection::tests::live_capture -- --ignored --nocapture
    /// ```
    ///
    /// The five-second head start is there so the terminal you launched it
    /// from can be clicked away from; the capture reads whatever is
    /// foreground when it fires.
    #[test]
    #[ignore = "needs a focused window with a live selection"]
    fn live_capture_reads_the_selection_and_files_it_under_a_session() {
        std::thread::sleep(Duration::from_secs(5));
        let target = crate::foreground::capture().expect("a foreground window");
        println!("target: app={:?}", target.app);
        let capture = begin(Some(target), Arc::new(AtomicBool::new(false))).resolve();
        let Capture::Text(s) = capture else {
            panic!("expected a selection, got {capture:?}");
        };
        println!("captured {} code points, session {}", s.chars, s.session);
        let held = sessions()
            .take(&s.session, Instant::now())
            .expect("the session holds the selection");
        println!("read through: {:?}", held.source);
        // `assert!`, not `assert_eq!`: a failure message would print the
        // user's document content, and this file does not do that.
        assert!(
            held.text == s.text,
            "the session and the route must see one string"
        );
        assert!(sessions().take(&s.session, Instant::now()).is_none());
    }

    /// THE SURVEY: what each mechanism makes of the selection in the window
    /// that is in front, for comparing apps. Prints counts and verdicts, never
    /// the text.
    ///
    /// To run it: select some text in the app to survey, then from another
    /// shell run
    ///
    /// ```text
    /// cargo test --lib selection::tests::live_uia_selection_survey -- --ignored --nocapture
    /// ```
    ///
    /// and click back into the app within five seconds. The clipboard half
    /// sends a real Ctrl+C and puts the clipboard back afterwards. Its line
    /// names the verdict, whether the copy returned text, what the clipboard
    /// held, who wrote it (the `Writer` the acceptance rule judged) and
    /// whether the sequence number held still.
    #[test]
    #[ignore = "needs a focused window with a live selection"]
    fn live_uia_selection_survey() {
        std::thread::sleep(Duration::from_secs(5));
        let target = crate::foreground::capture().expect("a foreground window");
        println!("target: app={:?}", target.app);

        let started = Instant::now();
        let uia = uia_reading(&target);
        let uia_ms = started.elapsed().as_millis();
        match &uia {
            Reading::Uia(text) => println!("uia: text, {} code points, {uia_ms} ms", text.chars().count()),
            Reading::FocusMoved => println!("uia: target changed, {uia_ms} ms"),
            Reading::Unreadable(why) => println!("uia: unreadable ({why:?}), {uia_ms} ms"),
            Reading::Clipboard(_) => unreachable!("the UIA reader never reads the clipboard"),
        }

        let suppress = AtomicBool::new(false);
        let started = Instant::now();
        let clipboard = clipboard_reading(&target, &suppress);
        let clipboard_ms = started.elapsed().as_millis();
        match &clipboard {
            Reading::Clipboard(read) => {
                let verdict = match accept_copy(read) {
                    Copied::Selection(text) => format!("selection, {} code points", text.chars().count()),
                    Copied::Nothing => "nothing selected".to_string(),
                    Copied::Unproven => "unproven".to_string(),
                };
                let held = match &read.now {
                    Held::Empty => "empty".to_string(),
                    Held::Text(text) => format!("text, {} code points", text.chars().count()),
                    Held::Other => "other".to_string(),
                };
                let writer = match read.writer() {
                    Writer::Target if read.owner == Some(target.pid) => "target window's process",
                    Writer::Target => "a process with a window inside the target",
                    Writer::Unknown => "no owner window",
                    Writer::Elsewhere => "another process",
                };
                println!(
                    "clipboard: {verdict}; copied={} held={held} writer={writer} \
                     processes_behind_target={} steady={}, {clipboard_ms} ms",
                    read.copied.is_some(),
                    read.target_pids.len(),
                    read.steady,
                );
            }
            Reading::FocusMoved => println!("clipboard: target changed, {clipboard_ms} ms"),
            Reading::Unreadable(why) => println!("clipboard: unreadable ({why:?}), {clipboard_ms} ms"),
            Reading::Uia(_) => unreachable!("the clipboard reader never asks UIA"),
        }
        if let (Reading::Uia(a), Reading::Clipboard(read)) = (&uia, &clipboard) {
            if let Copied::Selection(b) = accept_copy(read) {
                println!("both mechanisms read the same bytes: {}", *a == b);
            }
        }
    }

    /// THE WHOLE LANE against a real window: capture, mint, re-read,
    /// byte-compare, replace. Nothing above can cover it — the byte-compare
    /// only means anything when a real app has published a real selection
    /// through a real clipboard, and that is exactly where the round trip goes
    /// wrong (an app that normalizes line endings, a control that trims).
    ///
    /// To run it: open Notepad, type `the quick brown fox`, select it all
    /// (Ctrl+A), leave Notepad focused, then from another machine-local shell
    ///
    /// ```text
    /// cargo test --lib selection::tests::live_replace -- --ignored --nocapture
    /// ```
    ///
    /// PASS: after five seconds Notepad reads `THE LAZY DOG` and nothing else,
    /// and the clipboard holds whatever it held before. A decline printed
    /// instead means the verify refused — which is the safe direction, but
    /// note *which* notice, because "changed" against an untouched Notepad
    /// would mean the round trip is not byte-transparent on this machine.
    #[test]
    #[ignore = "needs a focused window with a live selection"]
    fn live_replace_verifies_the_selection_before_overwriting_it() {
        std::thread::sleep(Duration::from_secs(5));
        let target = crate::foreground::capture().expect("a foreground window");
        println!("target: app={:?}", target.app);
        let capture = begin(Some(target), Arc::new(AtomicBool::new(false))).resolve();
        let Capture::Text(s) = capture else {
            panic!("expected a selection, got {capture:?}");
        };
        println!("captured {} code points, session {}", s.chars, s.session);
        let outcome = replace(&s.session, "THE LAZY DOG", true, 300);
        println!("replace: {outcome:?}");
        assert_eq!(outcome, Replacement::Replaced);
        assert!(
            sessions().take(&s.session, Instant::now()).is_none(),
            "the session must be burned by the replace"
        );
    }
}
