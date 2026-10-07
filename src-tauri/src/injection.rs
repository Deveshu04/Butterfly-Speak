//! Text injection into the focused application.
//!
//! Single method (the consensus approach across local dictation apps):
//! save the clipboard, write the transcript, synthesize Ctrl+V, then restore
//! the clipboard — but only if nothing else wrote to it in the meantime
//! (checked via GetClipboardSequenceNumber).
//!
//! [`replace_last`] is the one operation that *removes* text rather than
//! adding it, so it does not trust any of that: it selects, reads the
//! selection back through the clipboard, and pastes only if what came back is
//! byte-for-byte what this app injected.

use std::thread;
use std::time::{Duration, Instant};
use windows::Win32::System::DataExchange::GetClipboardSequenceNumber;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, MapVirtualKeyW, SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT,
    KEYBD_EVENT_FLAGS, KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP, MAPVK_VK_TO_VSC, VIRTUAL_KEY,
    VK_APPS, VK_C, VK_CANCEL, VK_CONTROL, VK_DELETE, VK_DIVIDE, VK_DOWN, VK_END, VK_HOME,
    VK_INSERT, VK_LCONTROL, VK_LEFT, VK_LMENU, VK_LSHIFT, VK_LWIN, VK_NEXT, VK_NUMLOCK, VK_PRIOR,
    VK_RCONTROL, VK_RIGHT, VK_RMENU, VK_RSHIFT, VK_RWIN, VK_SHIFT, VK_SNAPSHOT, VK_UP, VK_V,
};

/// How long to let the target app settle after synthetic keys before reading
/// or writing on top of them. Same value `inject_text` has always used
/// between setting the clipboard and sending Ctrl+V.
///
/// `pub(crate)` only so `routes::selection::REPLACE_BUDGET` can add the real
/// constant rather than restate the literal. It is the only *fixed* wait in a
/// paste; the other one is the caller's `restore_delay_ms`, which is a
/// setting rather than a constant.
pub(crate) const SETTLE_MS: u64 = 30;

/// How long [`release_stuck_modifiers`] waits after synthesizing key-ups, so
/// the target sees them before the chord that follows.
///
/// Conditional, unlike [`SETTLE_MS`]: it is spent only when the user is still
/// physically holding part of the chord. On the agent path that is the common
/// case rather than the rare one — the chord is what started the dictation —
/// which is why `routes::selection::REPLACE_BUDGET` budgets it in full rather
/// than calling it noise.
///
/// `pub(crate)` for the same reason `SETTLE_MS` is: so that budget can add the
/// real constant instead of restating the literal.
pub(crate) const MODIFIER_DRAIN_MS: u64 = 25;

/// How often [`copy_selection_within`] asks whether the target app has
/// published its copy yet. `pub(crate)` for the same reason as
/// [`COPY_BUDGET`].
pub(crate) const COPY_POLL_MS: u64 = 30;

/// How long [`copy_selection`] waits for that copy on the transform path.
/// Generous on purpose: the selection is the user's own, can be arbitrarily
/// large, and was made by hand before the shortcut fired — the user is not
/// mid-keystroke while it runs, so the only cost of waiting is waiting.
///
/// `pub(crate)` so `routes::selection::CLIPBOARD_RESOLVE_BUDGET` can add the
/// real constant rather than restate the literal.
pub(crate) const COPY_BUDGET: Duration = Duration::from_millis(600);

/// The same wait on [`replace_last`]'s readback, where the budget is not a
/// cost but an *exposure*: a selection this module made is live in the user's
/// document until the readback answers, and the keyboard hook only observes
/// input (`hotkeys.rs` — `suppress` stops the app reacting to keys, it does
/// not stop the OS delivering them), so a keystroke during that window lands
/// on the selection and replaces it.
///
/// Shorter than [`COPY_BUDGET`] because this selection is nothing like the
/// transform path's: it is a few dozen characters that this app itself pasted
/// into this same app moments ago, so the target has already demonstrated it
/// processes synthetic keys promptly — six polls where the first normally
/// answers. An app slower than this loses nothing but the in-place
/// replace — a timeout reads as "unverified", which collapses the selection
/// and hands the user the transcript on the clipboard.
const UNDO_READBACK_BUDGET: Duration = Duration::from_millis(180);

/// How long a transform's rewrite stays on the clipboard after its Ctrl+V,
/// for the target app to read it, before the user's text goes back.
const TRANSFORM_RESTORE_MS: u64 = 300;

/// Whether `vk`'s hardware scan code is E0-prefixed (an "extended" key).
///
/// `MapVirtualKeyW` cannot express that prefix — it returns the bare byte,
/// so `VK_RIGHT` comes back as `0x4D`, which unprefixed *is* numpad-6, and
/// `VK_LEFT` as `0x4B`, numpad-4. (Even `MAPVK_VK_TO_VSC_EX` returns `0x004D`
/// here; only [`KEYEVENTF_EXTENDEDKEY`] can carry the E0.) Sending a real
/// scan code without the flag is worse than sending none at all: the
/// consumers that read the scan code instead of `wVk` are exactly the ones
/// the scan code was added for — RDP forwarding scan code + E0 to the remote
/// session, VM guests, RawInput games — and with `wScan: 0` they simply
/// dropped the event, where a mislabelled one they act on. Undo's
/// [`collapse_selection`] is the sharp edge: its Right arrow would arrive as
/// numpad-6 and, with NumLock on, type a "6" over the live selection this
/// module just made, on the one path whose contract is "not one character
/// changed".
///
/// The full standard E0 set, not just the keys this module sends today, so a
/// later caller reaching for Home or Delete gets it right for free.
fn is_extended(vk: VIRTUAL_KEY) -> bool {
    matches!(
        vk,
        VK_RCONTROL
            | VK_RMENU
            | VK_LWIN
            | VK_RWIN
            | VK_APPS
            | VK_LEFT
            | VK_RIGHT
            | VK_UP
            | VK_DOWN
            | VK_HOME
            | VK_END
            | VK_PRIOR
            | VK_NEXT
            | VK_INSERT
            | VK_DELETE
            | VK_NUMLOCK
            | VK_DIVIDE
            | VK_SNAPSHOT
            | VK_CANCEL
    )
}

/// One synthetic keyboard event for `vk`, pressed or released.
///
/// Carries the scan code the active layout maps `vk` to, because some
/// programs read the scan code and ignore the virtual key (games, remote
/// desktop clients, Java UIs). E0-prefixed keys also get
/// `KEYEVENTF_EXTENDEDKEY` ([`is_extended`]). Time and extra info stay zero,
/// so Windows stamps the event itself.
fn key_input(vk: VIRTUAL_KEY, up: bool) -> INPUT {
    let scan = unsafe { MapVirtualKeyW(u32::from(vk.0), MAPVK_VK_TO_VSC) } as u16;
    let mut flags = KEYBD_EVENT_FLAGS::default();
    if up {
        flags |= KEYEVENTF_KEYUP;
    }
    if is_extended(vk) {
        flags |= KEYEVENTF_EXTENDEDKEY;
    }
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: vk,
                wScan: scan,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

fn send(inputs: &[INPUT]) {
    unsafe {
        SendInput(inputs, std::mem::size_of::<INPUT>() as i32);
    }
}

/// Release any physically-held modifiers (the user may still be lifting off
/// the chord) so the synthetic input is not corrupted into e.g. Win+V.
///
/// Side-specific VKs, not the generic `VK_CONTROL`/`VK_SHIFT`/`VK_MENU`:
/// Windows tracks the two halves separately and derives the generic state
/// from them, so a generic keyup carries the *left* key's scan code and
/// leaves a physically-held right-hand modifier down — the exact state this
/// is here to clear. Checking each half also lets [`key_input`] label
/// right Ctrl/Alt as extended, which the generic VK cannot express.
fn release_stuck_modifiers() {
    let mods = [
        VK_LCONTROL,
        VK_RCONTROL,
        VK_LSHIFT,
        VK_RSHIFT,
        VK_LMENU,
        VK_RMENU,
        VK_LWIN,
        VK_RWIN,
    ];
    let ups: Vec<INPUT> = mods
        .into_iter()
        .filter(|vk| unsafe { (GetAsyncKeyState(vk.0 as i32) as u16 & 0x8000) != 0 })
        .map(|vk| key_input(vk, true))
        .collect();
    if !ups.is_empty() {
        send(&ups);
        thread::sleep(Duration::from_millis(MODIFIER_DRAIN_MS));
    }
}

/// Whether a synthetic Ctrl+C may be sent at whatever holds the foreground
/// **right now**.
///
/// THE TOCTOU THIS CLOSES. Every caller decides the copy is safe by looking at
/// a window some milliseconds earlier — `routes::selection::read` checks
/// `foreground::is_foreground` on the window captured at chord-down,
/// `controller::undo_decision` checks `foreground::is_terminal` on a fresh
/// capture — and then this function's own [`release_stuck_modifiers`] spends
/// up to 25 ms draining the user's held modifiers before a single key is sent.
/// A user finishing an Alt+Tab inside that ~25-45 ms window puts a console in
/// front of the keystroke, and Ctrl+C into a console is not "copy", it is
/// SIGINT: their build dies, and no readback can un-send it.
///
/// So the question is asked again here, as late as it can be asked, against
/// the window that will actually receive the keys — the same "derive the chord
/// from the window that receives it, not the one you aimed at" rule
/// `controller::start_injection` already applies to pastes. Declining costs
/// nothing a caller wanted: a terminal is `Capture::Nothing` by construction on
/// the selection lane (`routes::selection::begin`), Undo refuses to probe one
/// (`controller::undo_decision`), and a Transform the user can simply re-run.
///
/// `None` — no foreground window at all — proceeds. There is no console to
/// interrupt and the keystroke lands nowhere; treating "nobody is focused" as
/// a hazard would decline every copy on a machine between windows.
fn safe_to_copy(foreground: Option<&crate::foreground::Target>) -> bool {
    !foreground.is_some_and(crate::foreground::is_terminal)
}

/// What a [`copy_selection`] got.
#[derive(Debug, PartialEq, Eq)]
pub enum CopyOutcome {
    /// Refused before the clipboard was touched: a console held the
    /// foreground, so no Ctrl+C was sent.
    Declined,
    /// The clipboard was emptied and the Ctrl+C sent, but no text came back.
    Nothing,
    /// The selected text, which is now on the clipboard.
    Selection(String),
}

impl CopyOutcome {
    /// The selected text, if the copy got any.
    pub fn text(&self) -> Option<&str> {
        match self {
            CopyOutcome::Selection(text) => Some(text),
            CopyOutcome::Declined | CopyOutcome::Nothing => None,
        }
    }
}

/// Grab the focused app's current selection via a synthetic Ctrl+C. Does NOT
/// restore the clipboard — the caller owns save/restore around the whole
/// operation, with [`after_copy`].
pub fn copy_selection(clipboard: &mut arboard::Clipboard) -> CopyOutcome {
    copy_selection_within(clipboard, COPY_BUDGET)
}

/// [`copy_selection`] with an explicit patience budget, so the readback in
/// [`replace_last`] can be less patient than the transform path without
/// changing the transform path.
fn copy_selection_within(clipboard: &mut arboard::Clipboard, budget: Duration) -> CopyOutcome {
    // Modifiers first, then the safety question, then the clipboard: the
    // 25 ms drain is the longest thing between a caller's decision and the
    // keystroke, so asking after it is what makes the answer current, and
    // asking before the `clear` means a declined copy leaves the user's
    // clipboard exactly as it found it.
    release_stuck_modifiers();
    if !safe_to_copy(crate::foreground::capture().as_ref()) {
        tracing::warn!("a console holds the foreground; sending no copy chord");
        return CopyOutcome::Declined;
    }

    let _ = clipboard.clear();
    let seq_before = unsafe { GetClipboardSequenceNumber() };
    send(&[
        key_input(VK_CONTROL, false),
        key_input(VK_C, false),
        key_input(VK_C, true),
        key_input(VK_CONTROL, true),
    ]);

    // Wait for the target app to publish the copy (slow apps need a moment).
    let deadline = Instant::now() + budget;
    loop {
        thread::sleep(Duration::from_millis(COPY_POLL_MS));
        if unsafe { GetClipboardSequenceNumber() } != seq_before {
            return match clipboard.get_text() {
                Ok(text) if !text.trim().is_empty() => CopyOutcome::Selection(text),
                _ => CopyOutcome::Nothing,
            };
        }
        if Instant::now() >= deadline {
            return CopyOutcome::Nothing;
        }
    }
}

/// Presses `modifiers` in order, taps `key`, then releases the modifiers in
/// reverse, as one batch for [`send`].
fn chord(modifiers: &[VIRTUAL_KEY], key: VIRTUAL_KEY) -> Vec<INPUT> {
    let mut events = Vec::with_capacity(modifiers.len() * 2 + 2);
    events.extend(modifiers.iter().map(|&m| key_input(m, false)));
    events.push(key_input(key, false));
    events.push(key_input(key, true));
    events.extend(modifiers.iter().rev().map(|&m| key_input(m, true)));
    events
}

/// The paste keystroke for the target window: Ctrl+V for ordinary apps, and
/// Ctrl+Shift+V for terminals, most of which pass Ctrl+V to the program as a
/// control character instead of pasting. Ctrl+Shift+V is the paste binding
/// Windows Terminal ships and the one most other terminal emulators share.
fn paste_chord(terminal: bool) -> Vec<INPUT> {
    if terminal {
        // Shift first and Ctrl second; the order is arbitrary, since both are
        // down before V either way.
        chord(&[VK_SHIFT, VK_CONTROL], VK_V)
    } else {
        chord(&[VK_CONTROL], VK_V)
    }
}

/// Put `text` on the clipboard, marked with the three formats Windows
/// documents for keeping it out of clipboard history, the cloud clipboard and
/// clipboard monitors. The Rust side's paste writes and restores go through
/// here: a dictation is on the clipboard only for as long as the paste takes.
/// The restore is marked on purpose: put back unmarked, something copied from
/// a password manager, which marked it, would land in history and sync.
/// Copies the user asks for use [`set_plain_text`]; the webview's Copy
/// buttons are ordinary copies too.
///
/// Fails exactly where `set_text` fails, when the text cannot be placed, plus
/// in one case arboard never expects: the marks cannot be added once it is.
pub(crate) fn set_private_text(
    clipboard: &mut arboard::Clipboard,
    text: &str,
) -> Result<(), arboard::Error> {
    use arboard::SetExtWindows;
    clipboard
        .set()
        .exclude_from_history()
        .exclude_from_cloud()
        .exclude_from_monitoring()
        .text(text)
}

/// Put `text` on the clipboard as an ordinary copy, with no marks: for copies
/// the user asked for (Copy last transcript, and Undo's "copied instead"),
/// which belong in clipboard history and sync like any other copy.
pub(crate) fn set_plain_text(
    clipboard: &mut arboard::Clipboard,
    text: &str,
) -> Result<(), arboard::Error> {
    clipboard.set_text(text)
}

/// Paste a transform's rewrite into the focused app, then put `saved` back
/// as [`paste_and_restore`] does for a dictation: only if the user asked for
/// that and nothing else has written to the clipboard since the paste.
///
/// Always the plain Ctrl+V chord: this backs the transform path, which
/// refuses a terminal before it copies anything (`transforms::refusal`) and
/// pastes only after bringing the window it copied from back to the front.
pub fn paste_text(
    clipboard: &mut arboard::Clipboard,
    text: &str,
    saved: Option<&str>,
    restore_clipboard: bool,
) -> anyhow::Result<()> {
    paste_and_restore(
        clipboard,
        text,
        saved,
        restore_clipboard,
        TRANSFORM_RESTORE_MS,
        false,
    )
}

/// Extend the selection `char_count` characters to the left of the caret:
/// hold Shift, tap Left `char_count` times, release Shift — one SendInput
/// batch, the same `key_input`/`send` idiom as `release_stuck_modifiers` and
/// `copy_selection` above.
fn select_left(char_count: usize) {
    let mut batch: Vec<INPUT> = Vec::with_capacity(char_count * 2 + 2);
    batch.push(key_input(VK_SHIFT, false));
    for _ in 0..char_count {
        batch.push(key_input(VK_LEFT, false));
        batch.push(key_input(VK_LEFT, true));
    }
    batch.push(key_input(VK_SHIFT, true));
    send(&batch);
    // Let the target app finish updating its selection before anything reads
    // it or pastes over it.
    thread::sleep(Duration::from_millis(SETTLE_MS));
}

/// Drop a selection this module created, putting the caret back where it was
/// before [`select_left`] ran and changing not one character. Right-arrow
/// with a live selection collapses to the selection's *end* in standard
/// Windows edit controls — and the end of a leftward Shift+Left selection is
/// precisely the original caret position.
///
/// The [`release_stuck_modifiers`] call is load-bearing, not copied
/// boilerplate: **a held Shift turns this Right into Shift+Right, which
/// extends the selection instead of collapsing it.** Undo AI Edit ships
/// unbound so the chord is the user's own, but the two shortcuts that do ship
/// bound are `Alt+Shift+Z` and `Alt+Shift+X` (`settings.rs`) — a Shift-bearing
/// chord is the house pattern, and this runs milliseconds after the tap while
/// the user is still lifting off it. Losing the release here would leave a
/// live selection sitting in the document for the user's next keystroke to
/// overwrite, which is the exact data loss this function exists to prevent,
/// on the path taken when the readback has already said "touch nothing".
fn collapse_selection() {
    release_stuck_modifiers();
    send(&[key_input(VK_RIGHT, false), key_input(VK_RIGHT, true)]);
    thread::sleep(Duration::from_millis(SETTLE_MS));
}

/// What to do with the user's clipboard once a copy of their selection is over.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Restore {
    /// Leave it as it is.
    Leave,
    /// Write the user's text back, marked.
    Put(String),
    /// Empty it.
    Clear,
}

impl Restore {
    /// Carries the plan out. A text that cannot be written back leaves the
    /// copied selection in place, unmarked, so the clipboard is emptied
    /// instead; the write's error is still returned.
    pub(crate) fn apply(self, clipboard: &mut arboard::Clipboard) -> Result<(), arboard::Error> {
        match self {
            Restore::Leave => Ok(()),
            Restore::Put(text) => set_private_text(clipboard, &text).inspect_err(|_| {
                let _ = clipboard.clear();
            }),
            Restore::Clear => clipboard.clear(),
        }
    }
}

/// What goes back after [`copy_selection`]: nothing changes after a refused
/// copy, which never touched the clipboard. Otherwise the user's earlier text
/// (`saved`) goes back, marked, or, when they had no text there, the
/// clipboard is emptied.
///
/// A copy is written by the target app, unmarked, so it must not stay. Any
/// content from before that is not text cannot come back: the copy emptied
/// the clipboard before its Ctrl+C, and only text is saved, so emptying is
/// the closest to "as it was" that Windows allows. Neither step reaches what
/// the target's write already set off: clipboard history and cloud sync are
/// told of a write as it is made, so an entry they took from it stays.
pub(crate) fn after_copy(copy: &CopyOutcome, saved: Option<&str>) -> Restore {
    match (copy, saved) {
        (CopyOutcome::Declined, _) => Restore::Leave,
        (_, Some(text)) => Restore::Put(text.to_string()),
        (_, None) => Restore::Clear,
    }
}

/// Put the user's clipboard back after a probe that pasted nothing.
///
/// Unconditional, deliberately: the `restore_clipboard` setting governs
/// whether the text we *pasted* is left on the clipboard afterwards, and on
/// this path we pasted nothing. What is sitting there is the readback probe's
/// collateral — a copy of the user's own document that they never asked for —
/// so there is no reading of the setting under which keeping it is wanted.
fn restore_probe_clipboard(
    clipboard: &mut arboard::Clipboard,
    copy: &CopyOutcome,
    saved: Option<&str>,
) {
    let _ = after_copy(copy, saved).apply(clipboard);
}

/// Put `text` on the clipboard, Ctrl+V it into the focused app, then put
/// `saved` back if the user asked for that and nothing else claimed the
/// clipboard in between.
///
/// The clipboard handle and `saved` come from the caller so a multi-step
/// operation can save the user's *real* clipboard once, up front, instead of
/// re-saving whatever scratch value an earlier step happened to leave there.
///
/// `saved` is text only, so text is all that comes back: an image or a file
/// list the user had copied is replaced by the paste, and formatted text
/// returns as plain text. `settings::InjectionSettings::restore_clipboard`
/// and the README say the same.
/// The only fallible step is [`set_private_text`], and it fails before
/// anything is pasted — callers that have already disturbed the document can
/// rely on `Err` meaning "nothing was pasted".
///
/// `terminal` selects the paste chord (see [`paste_chord`]); it says nothing
/// about *whether* to paste, only *how*.
fn paste_and_restore(
    clipboard: &mut arboard::Clipboard,
    text: &str,
    saved: Option<&str>,
    restore_clipboard: bool,
    restore_delay_ms: u64,
    terminal: bool,
) -> anyhow::Result<()> {
    set_private_text(clipboard, text)?;
    let seq_ours = unsafe { GetClipboardSequenceNumber() };
    thread::sleep(Duration::from_millis(SETTLE_MS));

    release_stuck_modifiers();
    send(&paste_chord(terminal));

    thread::sleep(Duration::from_millis(restore_delay_ms));
    if restore_clipboard {
        if let Some(prev) = saved {
            let seq_now = unsafe { GetClipboardSequenceNumber() };
            if seq_now == seq_ours {
                let _ = set_private_text(clipboard, prev);
            }
        }
    }
    Ok(())
}

/// Whether a clipboard readback confirms the selection is exactly the text
/// this app injected. `None` is "nothing came back at all" — an empty or
/// unchanged clipboard from [`copy_selection`], which happens when nothing
/// got selected or the app never published a copy.
///
/// Byte-for-byte and deliberately unforgiving: the answer decides whether
/// characters get deleted, so "close enough" is not a category that exists
/// here. Anything that normalizes on the way through the clipboard — an app
/// turning `\n` into `\r\n`, a rich-text control dropping a trailing space —
/// reads as a mismatch, and a mismatch costs the user a fallback notice
/// rather than a paragraph.
fn readback_matches(seen: Option<&str>, expected: &str) -> bool {
    seen == Some(expected)
}

/// What [`replace_last`] did, so the caller can fall through to a
/// non-destructive path instead of assuming the document changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplaceOutcome {
    /// The selection read back byte-for-byte as the caller recorded it, so
    /// the replacement was pasted over it. The document changed.
    Replaced,
    /// The selection did not read back as expected, or nothing came back at
    /// all. The selection was collapsed and the document was left exactly as
    /// it was — not one character changed.
    Unverified,
}

/// Select the `char_count` characters immediately left of the caret, read
/// them back, and paste `replacement` over them **only** if what came back is
/// byte-for-byte `expected`.
///
/// This is Undo AI Edit's destructive path, and the readback is what makes it
/// safe to call at all. The previous design inferred that the selection had
/// to be the app's own injected text from four heuristics — a record exists,
/// under 30 s elapsed, same foreground app, ASCII-only — and then deleted
/// `char_count` characters on the strength of that inference. Every one of
/// those heuristics can hold while the caret sits somewhere else entirely:
/// the user typed after the paste, clicked into another paragraph, switched
/// to a second window of the *same process*, or the machine slept through the
/// 30 s window. Inference cannot be made airtight here; verification can.
/// Synthesizing Ctrl+C and comparing costs one clipboard round-trip (at most
/// [`UNDO_READBACK_BUDGET`]) on a path the user explicitly asked for, and
/// converts every one of those failure modes into "nothing changed".
///
/// Two costs the verification does carry, neither of them removed by the
/// caller's pre-filter:
///
/// * **The probe holds a live selection.** From [`select_left`] until the
///   readback answers, `char_count` characters are selected in the user's
///   document, and the keyboard hook observes input rather than swallowing
///   it. A keystroke inside that window replaces the selection. Typically the
///   window is ~60 ms; what bounds it is the settle plus at most
///   [`UNDO_READBACK_BUDGET`] of *waiting* — plus however long the target
///   takes to chew through `char_count` queued synthetic arrow presses, which
///   for a long injection into a slow editor is the larger term. Nothing in
///   here can prevent a keystroke landing on it, so the window is kept short
///   instead (see [`UNDO_READBACK_BUDGET`]).
/// * **Ctrl+C is not universally "copy".** A console delivers it to the
///   foreground program as an interrupt, which can kill a running command or
///   make the shell abandon the input line the paste is sitting on — and no
///   readback can undo an interrupt already sent. `controller::undo_decision`
///   therefore refuses to probe at all when its `is_terminal` argument is
///   true (fed from `foreground::is_terminal`'s `CONSOLE_CLASSES`/
///   `TERMINAL_PROCESSES` lists there; best-effort by construction, see its
///   comment), because the *same-app* condition next to it is no mitigation —
///   a terminal the text was pasted into passes it trivially. [`safe_to_copy`]
///   asks the same question again at send time, so a console that took the
///   foreground *after* that gate is also refused; Undo into a console the
///   list itself misses may still send an interrupt. What even that
///   cannot do is paste:
///   a console that read Ctrl+C as an interrupt published nothing to the
///   clipboard, so the readback finds no match and this returns
///   [`ReplaceOutcome::Unverified`] having pasted nothing.
///
/// `Err` means the clipboard itself failed and nothing was pasted; like
/// [`ReplaceOutcome::Unverified`] the document is unchanged.
pub fn replace_last(
    char_count: usize,
    expected: &str,
    replacement: &str,
    restore_clipboard: bool,
    restore_delay_ms: u64,
) -> anyhow::Result<ReplaceOutcome> {
    if char_count == 0 {
        // No selection to make, so nothing can be verified; a Ctrl+V here
        // would insert rather than replace, which is the original bug.
        return Ok(ReplaceOutcome::Unverified);
    }

    // Every fallible clipboard step that CAN happen before the document is
    // touched does. Creating the handle first is what keeps a dead clipboard
    // from leaving the user staring at N selected characters with no error
    // shown, one keystroke away from losing them.
    let mut clipboard = arboard::Clipboard::new()?;
    // Saved ONCE, up front. `copy_selection` clears the clipboard and the
    // target app then writes the selection into it, and the paste overwrites
    // it again — anything that re-saved partway through would "restore" the
    // user's clipboard to a fragment of their own document.
    let saved = clipboard.get_text().ok();

    release_stuck_modifiers();
    select_left(char_count);

    // What is ACTUALLY selected, not what we hope is there. On the shorter
    // budget: every millisecond spent here is a millisecond the selection is
    // live in front of the user's keyboard.
    let copy = copy_selection_within(&mut clipboard, UNDO_READBACK_BUDGET);
    if !readback_matches(copy.text(), expected) {
        tracing::warn!(
            "undo readback mismatch (wanted {} chars, got {:?}); leaving the document alone",
            char_count,
            copy.text().map(|s| s.chars().count())
        );
        collapse_selection();
        restore_probe_clipboard(&mut clipboard, &copy, saved.as_deref());
        return Ok(ReplaceOutcome::Unverified);
    }

    // Verified: those characters are exactly the ones this app injected.
    //
    // `terminal: false` unconditionally — never a live question here.
    // `controller::undo_decision` refuses to reach `replace_last` at all for
    // a target `foreground::is_terminal` says is a console (see its doc),
    // so the plain-app chord is the only one this call can ever need.
    if let Err(e) = paste_and_restore(
        &mut clipboard,
        replacement,
        saved.as_deref(),
        restore_clipboard,
        restore_delay_ms,
        false,
    ) {
        // Setting the clipboard is the only fallible step and it fails before
        // the Ctrl+V, so the selection is still live and still the user's text.
        collapse_selection();
        restore_probe_clipboard(&mut clipboard, &copy, saved.as_deref());
        return Err(e);
    }
    Ok(ReplaceOutcome::Replaced)
}

/// `terminal` should come from `foreground::is_terminal` on the captured
/// dictation `Target`, so the chord matches the window the paste actually
/// lands on rather than whatever happens to be foreground right now.
pub fn inject_text(
    text: &str,
    restore_clipboard: bool,
    restore_delay_ms: u64,
    terminal: bool,
) -> anyhow::Result<()> {
    let mut clipboard = arboard::Clipboard::new()?;
    let saved = clipboard.get_text().ok();
    paste_and_restore(
        &mut clipboard,
        text,
        saved.as_deref(),
        restore_clipboard,
        restore_delay_ms,
        terminal,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    // Most of this module needs SendInput and a live system clipboard, which
    // needs a focused window with a caret in it and cannot run headless on
    // CI — `readback_matches` covers the one question there that decides
    // whether characters get deleted. `paste_chord` is pure data assembly
    // (no window, no focus, `MapVirtualKeyW` is a keyboard-layout table
    // lookup) and is covered directly below.

    const INJECTED: &str = "The meeting is at 3:30 PM.";

    /// Every Rust source file under `src/`, with its path relative to it.
    fn sources() -> Vec<(String, String)> {
        fn walk(dir: &std::path::Path, root: &std::path::Path, out: &mut Vec<(String, String)>) {
            for entry in std::fs::read_dir(dir).expect("read src dir") {
                let path = entry.expect("dir entry").path();
                if path.is_dir() {
                    walk(&path, root, out);
                } else if path.extension().and_then(|e| e.to_str()) == Some("rs") {
                    let name = path
                        .strip_prefix(root)
                        .expect("under src")
                        .display()
                        .to_string();
                    let text = std::fs::read_to_string(&path).expect("read source");
                    out.push((name, text.replace("\r\n", "\n")));
                }
            }
        }
        let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut out = Vec::new();
        walk(&root, &root, &mut out);
        out
    }

    /// The source of `file` under `src/`.
    fn source(file: &str) -> String {
        sources()
            .into_iter()
            .find(|(name, _)| name == file)
            .map(|(_, text)| text)
            .unwrap_or_else(|| panic!("{file} not found under src/"))
    }

    /// The definition of the function `head` starts, from that line to the
    /// closing brace at the start of a line. Anchored on a real line break,
    /// so a test's own string does not count as the definition.
    fn definition<'a>(text: &'a str, head: &str) -> Option<&'a str> {
        let start = text.find(&format!("\n{head}("))?;
        let rest = &text[start..];
        let end = rest.find("\n}\n")? + 3;
        Some(&rest[..end])
    }

    /// The function definitions allowed to write to the clipboard their own
    /// way: the two helpers, and the one caller of the plain one.
    const ALLOWED_WRITERS: &[(&str, &str)] = &[
        ("injection.rs", "pub(crate) fn set_private_text"),
        ("injection.rs", "pub(crate) fn set_plain_text"),
        ("controller.rs", "fn copy_to_clipboard"),
    ];

    /// `text` with the allowed definitions in `file` blanked line for line,
    /// so the line numbers of everything else stay true.
    fn without_allowed_writers(file: &str, text: String) -> String {
        let mut text = text;
        for (owner, head) in ALLOWED_WRITERS {
            if *owner != file {
                continue;
            }
            let def = definition(&text, head)
                .unwrap_or_else(|| panic!("{head} is defined in {file}"))
                .to_string();
            text = text.replacen(&def, &"\n".repeat(def.matches('\n').count()), 1);
        }
        text
    }

    /// `text` without this file's own test module, blanked line for line: its
    /// string literals name every pattern the scan looks for.
    fn without_this_test_module(file: &str, text: String) -> String {
        if file != "injection.rs" {
            return text;
        }
        let start = text
            .find("\n#[cfg(test)]\nmod tests {")
            .expect("injection.rs has its test module");
        let blank = "\n".repeat(text[start..].matches('\n').count());
        format!("{}{blank}", &text[..start])
    }

    /// Every clipboard write the Rust side makes goes through
    /// [`set_private_text`], except the copies the user asked for, which go
    /// through [`set_plain_text`] from `copy_to_clipboard` alone. The real
    /// clipboard cannot be touched from a test, so this reads the source: a
    /// new write that skips the marks fails here, whether it is a
    /// `set_text`, any use of `set_plain_text` elsewhere, an arboard `.set()`
    /// builder in any shape (chained, bound to a variable, or followed by a
    /// comment), or a raw `set_html`, `set_image` or `SetClipboardData`. (The
    /// tray's menu items have a `set_text` of their own, which is not the
    /// clipboard.)
    #[test]
    fn no_clipboard_write_skips_the_privacy_marks() {
        let mut unmarked = Vec::new();
        for (file, text) in sources() {
            let text = without_this_test_module(&file, without_allowed_writers(&file, text));
            for (n, line) in text.lines().enumerate() {
                let code = line.trim_start();
                if code.starts_with("//") || code.contains("pause_item.set_text(") {
                    continue;
                }
                let write = code.contains(".set_text(")
                    || code.contains("set_plain_text")
                    || code.contains(".set()")
                    || code.contains("set_html(")
                    || code.contains("set_image(")
                    || code.contains("SetClipboardData");
                if write {
                    unmarked.push(format!("{file}:{}: {code}", n + 1));
                }
            }
        }
        assert!(
            unmarked.is_empty(),
            "clipboard writes that skip set_private_text:\n{}",
            unmarked.join("\n")
        );
    }

    /// Copy last transcript, and Undo's "copied instead", are copies the
    /// user asked for, and stay ordinary ones: in clipboard history and sync,
    /// like the webview's Copy buttons. `copy_to_clipboard` is their one
    /// writer, and it makes exactly one plain write.
    #[test]
    fn copies_the_user_asked_for_stay_ordinary() {
        let text = source("controller.rs");
        let body = definition(&text, "fn copy_to_clipboard")
            .expect("copy_to_clipboard is defined in controller.rs");
        assert_eq!(body.matches("set_plain_text(").count(), 1, "{body}");
        assert!(!body.contains("set_private_text("), "{body}");
    }

    /// The helper sets all three formats Windows documents for keeping
    /// clipboard content out of history, cloud sync and clipboard monitors.
    #[test]
    fn the_private_write_sets_all_three_marks() {
        let text = source("injection.rs");
        let body = definition(&text, "pub(crate) fn set_private_text")
            .expect("set_private_text is defined in injection.rs");
        for mark in [
            ".exclude_from_history()",
            ".exclude_from_cloud()",
            ".exclude_from_monitoring()",
        ] {
            assert!(body.contains(mark), "set_private_text lacks {mark}");
        }
    }

    /// After a copy of the user's selection, their earlier text goes back
    /// marked, and with no text to put back the clipboard is emptied, so the
    /// copied document text never stays on it unmarked.
    #[test]
    fn after_a_copy_the_selection_never_stays_on_the_clipboard() {
        for copy in [CopyOutcome::Nothing, CopyOutcome::Selection("the selection".into())] {
            assert_eq!(after_copy(&copy, Some("user text")), Restore::Put("user text".into()));
            assert_eq!(after_copy(&copy, None), Restore::Clear);
        }
    }

    /// A copy refused before its Ctrl+C touched nothing, so whatever the user
    /// had on the clipboard, an image included, stays exactly as it is.
    #[test]
    fn a_refused_copy_leaves_the_clipboard_alone() {
        assert_eq!(after_copy(&CopyOutcome::Declined, Some("user text")), Restore::Leave);
        assert_eq!(after_copy(&CopyOutcome::Declined, None), Restore::Leave);
    }

    /// Undo's probe cleans up after its copy by the same rule.
    #[test]
    fn the_undo_probe_cleans_up_after_its_copy() {
        let text = source("injection.rs");
        let body = definition(&text, "fn restore_probe_clipboard")
            .expect("restore_probe_clipboard is defined in injection.rs");
        assert!(body.contains("after_copy("), "{body}");
    }

    /// A transform's paste puts the clipboard back the way a dictation's
    /// does: only if nothing else has written to it since the paste.
    #[test]
    fn a_transform_paste_restores_only_an_unchanged_clipboard() {
        let text = source("injection.rs");
        let body = definition(&text, "pub fn paste_text")
            .expect("paste_text is defined in injection.rs");
        assert!(body.contains("paste_and_restore("), "{body}");
    }

    /// (vk, is_keyup) pairs, in send order — the shape that actually decides
    /// what the target app receives.
    fn chord_shape(inputs: &[INPUT]) -> Vec<(u16, bool)> {
        inputs
            .iter()
            .map(|i| {
                let ki = unsafe { i.Anonymous.ki };
                (ki.wVk.0, ki.dwFlags.contains(KEYEVENTF_KEYUP))
            })
            .collect()
    }

    fn flags_of(vk: VIRTUAL_KEY, up: bool) -> KEYBD_EVENT_FLAGS {
        unsafe { key_input(vk, up).Anonymous.ki.dwFlags }
    }

    #[test]
    fn ordinary_apps_get_plain_ctrl_v() {
        assert_eq!(
            chord_shape(&paste_chord(false)),
            vec![
                (VK_CONTROL.0, false),
                (VK_V.0, false),
                (VK_V.0, true),
                (VK_CONTROL.0, true),
            ]
        );
    }

    /// Most terminals read plain Ctrl+V as a control character, so they get
    /// Ctrl+Shift+V. Pinned as a shape rather than one exact sequence: both
    /// modifiers held before V, V released before either, and the modifiers
    /// released in the reverse of their press order.
    #[test]
    fn terminals_get_ctrl_shift_v_with_the_modifiers_wrapped_around_v() {
        let shape = chord_shape(&paste_chord(true));
        assert_eq!(shape.len(), 6, "{shape:?}");

        let at = |vk: VIRTUAL_KEY, up: bool| {
            let hits: Vec<usize> = shape
                .iter()
                .enumerate()
                .filter(|(_, e)| **e == (vk.0, up))
                .map(|(i, _)| i)
                .collect();
            assert_eq!(hits.len(), 1, "vk {vk:?} up={up} should appear once: {shape:?}");
            hits[0]
        };
        let (ctrl_down, ctrl_up) = (at(VK_CONTROL, false), at(VK_CONTROL, true));
        let (shift_down, shift_up) = (at(VK_SHIFT, false), at(VK_SHIFT, true));
        let (v_down, v_up) = (at(VK_V, false), at(VK_V, true));

        assert!(ctrl_down < v_down && shift_down < v_down, "{shape:?}");
        assert!(v_down < v_up, "{shape:?}");
        assert!(v_up < ctrl_up && v_up < shift_up, "{shape:?}");
        assert_eq!(
            ctrl_down < shift_down,
            shift_up < ctrl_up,
            "the modifiers must come up in reverse order: {shape:?}"
        );
    }

    /// Every synthetic key event carries a real scan code, not the `0` this
    /// module used to send — some apps (games, RDP, Java UIs) match on scan
    /// code and ignore vk-only input.
    #[test]
    fn every_chord_key_carries_a_nonzero_scan_code() {
        for inputs in [paste_chord(false), paste_chord(true)] {
            for input in &inputs {
                let ki = unsafe { input.Anonymous.ki };
                assert_ne!(ki.wScan, 0, "vk {:?} has no scan code", ki.wVk);
            }
        }
    }

    /// THE REGRESSION: the arrow keys `select_left`/`collapse_selection`
    /// send are E0-prefixed in hardware, and `MapVirtualKeyW` returns the
    /// bare byte — `VK_LEFT` → `0x4B` and `VK_RIGHT` → `0x4D`, which
    /// unprefixed are numpad-4 and numpad-6. A scan code without
    /// `KEYEVENTF_EXTENDEDKEY` therefore labels them as numpad keys for
    /// every consumer that reads the scan code rather than `wVk` (RDP, VM
    /// guests, RawInput games), and with NumLock on Undo's collapse would
    /// type a "6" over its own live selection.
    #[test]
    fn extended_keys_carry_the_extended_flag_with_their_scan_code() {
        for vk in [VK_LEFT, VK_RIGHT, VK_LWIN, VK_RWIN, VK_RCONTROL, VK_RMENU] {
            for up in [false, true] {
                assert!(
                    flags_of(vk, up).contains(KEYEVENTF_EXTENDEDKEY),
                    "vk {vk:?} (up={up}) is extended but was not flagged"
                );
            }
        }
    }

    /// The other half of the same rule: flagging a non-extended key would
    /// mislabel it just as badly. `VK_RSHIFT` is the trap — its left/right
    /// twin `VK_RCONTROL` *is* extended, but right Shift's scan code (0x36)
    /// carries no E0.
    #[test]
    fn ordinary_keys_are_not_flagged_extended() {
        for vk in [VK_V, VK_C, VK_CONTROL, VK_SHIFT, VK_LCONTROL, VK_LSHIFT, VK_RSHIFT] {
            assert!(
                !flags_of(vk, false).contains(KEYEVENTF_EXTENDEDKEY),
                "vk {vk:?} is not an extended key but was flagged"
            );
        }
    }

    /// Key-up must stay distinguishable from key-down now that a second flag
    /// can share the field — a `dwFlags == KEYEVENTF_KEYUP` equality check
    /// would read every extended key-up as a key-down.
    #[test]
    fn the_keyup_flag_survives_alongside_the_extended_flag() {
        assert!(flags_of(VK_RIGHT, true).contains(KEYEVENTF_KEYUP));
        assert!(!flags_of(VK_RIGHT, false).contains(KEYEVENTF_KEYUP));
    }

    // --- the send-time console re-check ------------------------------------

    /// THE RACE THIS CLOSES. Callers gate the copy on a window they looked at
    /// up to ~45 ms earlier (`release_stuck_modifiers` alone spends 25 ms of
    /// that), so an Alt+Tab finishing inside the gap can put a console in
    /// front of a chord that was cleared against a text editor. Ctrl+C there
    /// is SIGINT, not copy.
    #[test]
    fn a_console_that_took_the_foreground_gets_no_copy_chord() {
        for (app, class) in [
            ("windowsterminal", "CASCADIA_HOSTING_WINDOW_CLASS"),
            ("python", "ConsoleWindowClass"),
            ("powershell", "SomeOrdinaryClass"),
            ("electerm", "Chrome_WidgetWin_1"),
        ] {
            let now = crate::foreground::test_target(app, class);
            assert!(
                !safe_to_copy(Some(&now)),
                "{app}/{class} must not receive a bare Ctrl+C"
            );
        }
    }

    #[test]
    fn an_ordinary_window_still_gets_the_copy_chord() {
        let now = crate::foreground::test_target("notepad", "Notepad");
        assert!(safe_to_copy(Some(&now)));
    }

    /// No foreground window is not a hazard — there is no console to
    /// interrupt and the keystroke lands nowhere. Failing closed here would
    /// decline every copy on a machine that is momentarily between windows.
    #[test]
    fn no_foreground_window_is_not_a_console() {
        assert!(safe_to_copy(None));
    }

    #[test]
    fn an_exact_readback_is_the_only_thing_that_authorizes_a_replace() {
        assert!(readback_matches(Some(INJECTED), INJECTED));
    }

    /// The readback returns `None` when the target app never published a
    /// copy: nothing was selected, the app ignored Ctrl+C (or read it as an
    /// interrupt), or it took longer than `UNDO_READBACK_BUDGET`. All three
    /// mean "we have no idea what is in front of the caret", which is the
    /// opposite of authorization.
    #[test]
    fn nothing_copied_is_not_a_match() {
        assert!(!readback_matches(None, INJECTED));
    }

    /// The failure the readback exists for: the user typed after the paste,
    /// so Shift+Left selected their characters instead of ours. The old
    /// heuristics (recent, same app, ASCII) all still hold here — only
    /// looking at the document catches it.
    #[test]
    fn a_selection_shifted_by_later_typing_is_not_a_match() {
        assert!(!readback_matches(Some("eting is at 3:30 PM. and"), INJECTED));
    }

    /// Sub- and super-strings both fail. A miscounted selection is exactly
    /// as destructive as a misplaced one.
    #[test]
    fn a_partial_or_overlong_selection_is_not_a_match() {
        assert!(!readback_matches(Some("meeting is at 3:30 PM."), INJECTED));
        assert!(!readback_matches(
            Some("Note: The meeting is at 3:30 PM."),
            INJECTED
        ));
    }

    /// Round-tripping through the clipboard can normalize text — a control
    /// that stores `\r\n`, or one that trims. That is indistinguishable from
    /// a genuinely wrong selection, so it fails closed: the user gets the
    /// transcript on their clipboard instead of a silent guess.
    #[test]
    fn normalized_whitespace_fails_closed() {
        assert!(!readback_matches(Some("Line one.\r\nLine two."), "Line one.\nLine two."));
        assert!(!readback_matches(Some("Trailing space."), "Trailing space. "));
    }
}
