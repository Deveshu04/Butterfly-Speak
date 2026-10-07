//! Global hotkey detection via a low-level keyboard hook (rdev).
//!
//! The default chord Ctrl+Win is modifier-only, which RegisterHotKey-style
//! APIs cannot express — hence the hook. The callback does nothing but
//! bookkeeping and channel sends: slow low-level hooks get silently removed
//! by Windows.
//!
//! Also implements capture mode for the settings UI: while active, chord
//! detection is suspended and every change of the held-key set is emitted to
//! the frontend so the user can record a new binding.

use crate::events;
use crate::routes::ChordKind;
use crate::state::ControlMsg;
use crossbeam_channel::Sender;
use rdev::{listen, Event, EventType, Key};
use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use tauri::Emitter;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BindKey {
    Ctrl,
    Win,
    Alt,
    Shift,
    Key(Key),
}

pub type Binding = Vec<BindKey>;

pub fn parse_binding(s: &str) -> Binding {
    let parsed: Vec<BindKey> = s.split('+').filter_map(|p| parse_key(p.trim())).collect();
    if parsed.is_empty() {
        vec![BindKey::Ctrl, BindKey::Win]
    } else {
        parsed
    }
}

fn parse_key(name: &str) -> Option<BindKey> {
    let lower = name.to_lowercase();
    Some(match lower.as_str() {
        "ctrl" | "control" => BindKey::Ctrl,
        "win" | "meta" | "super" => BindKey::Win,
        "alt" => BindKey::Alt,
        "shift" => BindKey::Shift,
        _ => BindKey::Key(named_key(&lower)?),
    })
}

fn named_key(lower: &str) -> Option<Key> {
    use Key::*;
    Some(match lower {
        "space" => Space,
        "tab" => Tab,
        "capslock" => CapsLock,
        "home" => Home,
        "end" => End,
        "insert" => Insert,
        "delete" => Delete,
        "pageup" => PageUp,
        "pagedown" => PageDown,
        "backspace" => Backspace,
        "enter" | "return" => Return,
        "up" => UpArrow,
        "down" => DownArrow,
        "left" => LeftArrow,
        "right" => RightArrow,
        "printscreen" => PrintScreen,
        "scrolllock" => ScrollLock,
        "pause" => Pause,
        "numlock" => NumLock,
        "`" => BackQuote,
        "-" => Minus,
        "=" => Equal,
        "[" => LeftBracket,
        "]" => RightBracket,
        ";" => SemiColon,
        "'" => Quote,
        "\\" => BackSlash,
        "," => Comma,
        "." => Dot,
        "/" => Slash,
        // Numpad. Named "Num*" so nothing collides with the top-row digits
        // above, and so no display name ever contains the '+' that bindings
        // are split on.
        "num0" => Kp0,
        "num1" => Kp1,
        "num2" => Kp2,
        "num3" => Kp3,
        "num4" => Kp4,
        "num5" => Kp5,
        "num6" => Kp6,
        "num7" => Kp7,
        "num8" => Kp8,
        "num9" => Kp9,
        "numenter" => KpReturn,
        "numminus" => KpMinus,
        "numplus" => KpPlus,
        "nummultiply" => KpMultiply,
        "numdivide" => KpDivide,
        "numdelete" => KpDelete,
        "f1" => F1,
        "f2" => F2,
        "f3" => F3,
        "f4" => F4,
        "f5" => F5,
        "f6" => F6,
        "f7" => F7,
        "f8" => F8,
        "f9" => F9,
        "f10" => F10,
        "f11" => F11,
        "f12" => F12,
        "a" => KeyA,
        "b" => KeyB,
        "c" => KeyC,
        "d" => KeyD,
        "e" => KeyE,
        "f" => KeyF,
        "g" => KeyG,
        "h" => KeyH,
        "i" => KeyI,
        "j" => KeyJ,
        "k" => KeyK,
        "l" => KeyL,
        "m" => KeyM,
        "n" => KeyN,
        "o" => KeyO,
        "p" => KeyP,
        "q" => KeyQ,
        "r" => KeyR,
        "s" => KeyS,
        "t" => KeyT,
        "u" => KeyU,
        "v" => KeyV,
        "w" => KeyW,
        "x" => KeyX,
        "y" => KeyY,
        "z" => KeyZ,
        "0" => Num0,
        "1" => Num1,
        "2" => Num2,
        "3" => Num3,
        "4" => Num4,
        "5" => Num5,
        "6" => Num6,
        "7" => Num7,
        "8" => Num8,
        "9" => Num9,
        _ => return None,
    })
}

fn display_key(k: Key) -> Option<&'static str> {
    use Key::*;
    Some(match k {
        Space => "Space",
        Tab => "Tab",
        CapsLock => "CapsLock",
        Home => "Home",
        End => "End",
        Insert => "Insert",
        Delete => "Delete",
        PageUp => "PageUp",
        PageDown => "PageDown",
        Backspace => "Backspace",
        Return => "Enter",
        UpArrow => "Up",
        DownArrow => "Down",
        LeftArrow => "Left",
        RightArrow => "Right",
        PrintScreen => "PrintScreen",
        ScrollLock => "ScrollLock",
        Pause => "Pause",
        NumLock => "NumLock",
        BackQuote => "`",
        Minus => "-",
        Equal => "=",
        LeftBracket => "[",
        RightBracket => "]",
        SemiColon => ";",
        Quote => "'",
        BackSlash => "\\",
        Comma => ",",
        Dot => ".",
        Slash => "/",
        Kp0 => "Num0",
        Kp1 => "Num1",
        Kp2 => "Num2",
        Kp3 => "Num3",
        Kp4 => "Num4",
        Kp5 => "Num5",
        Kp6 => "Num6",
        Kp7 => "Num7",
        Kp8 => "Num8",
        Kp9 => "Num9",
        KpReturn => "NumEnter",
        KpMinus => "NumMinus",
        KpPlus => "NumPlus",
        KpMultiply => "NumMultiply",
        KpDivide => "NumDivide",
        KpDelete => "NumDelete",
        F1 => "F1",
        F2 => "F2",
        F3 => "F3",
        F4 => "F4",
        F5 => "F5",
        F6 => "F6",
        F7 => "F7",
        F8 => "F8",
        F9 => "F9",
        F10 => "F10",
        F11 => "F11",
        F12 => "F12",
        KeyA => "A",
        KeyB => "B",
        KeyC => "C",
        KeyD => "D",
        KeyE => "E",
        KeyF => "F",
        KeyG => "G",
        KeyH => "H",
        KeyI => "I",
        KeyJ => "J",
        KeyK => "K",
        KeyL => "L",
        KeyM => "M",
        KeyN => "N",
        KeyO => "O",
        KeyP => "P",
        KeyQ => "Q",
        KeyR => "R",
        KeyS => "S",
        KeyT => "T",
        KeyU => "U",
        KeyV => "V",
        KeyW => "W",
        KeyX => "X",
        KeyY => "Y",
        KeyZ => "Z",
        Num0 => "0",
        Num1 => "1",
        Num2 => "2",
        Num3 => "3",
        Num4 => "4",
        Num5 => "5",
        Num6 => "6",
        Num7 => "7",
        Num8 => "8",
        Num9 => "9",
        _ => return None,
    })
}

fn is_ctrl(k: Key) -> bool {
    matches!(k, Key::ControlLeft | Key::ControlRight)
}
fn is_win(k: Key) -> bool {
    matches!(k, Key::MetaLeft | Key::MetaRight)
}
fn is_alt(k: Key) -> bool {
    matches!(k, Key::Alt | Key::AltGr)
}
fn is_shift(k: Key) -> bool {
    matches!(k, Key::ShiftLeft | Key::ShiftRight)
}

/// Whether a key can take part in a binding. Escape is deliberately excluded:
/// it is the capture UI's cancel gesture, not a bindable key.
fn is_bindable(k: Key) -> bool {
    is_ctrl(k) || is_win(k) || is_alt(k) || is_shift(k) || display_key(k).is_some()
}

/// How the opt-in hook trace (`BS_DEBUG_HOTKEYS`) names a key press. The hook
/// sees every key pressed in every app, passwords included, and the trace
/// goes to the log file, so only a modifier is named. Any other key is only
/// "a key", or "a key rdev cannot name" (how the right Windows key once
/// showed up); which binding a press completed is traced where it matches,
/// by its kind or index, never by its keys.
fn traced_press(k: Key) -> String {
    if is_ctrl(k) || is_win(k) || is_alt(k) || is_shift(k) {
        format!("{k:?}")
    } else if matches!(k, Key::Unknown(_)) {
        "a key rdev cannot name".to_string()
    } else {
        "a key".to_string()
    }
}

/// rdev 0.5.3's Windows keycode table (`windows/keycodes.rs`) maps VK 91 to
/// `MetaLeft` but has no entry at all for VK_RWIN (92) — `MetaRight` is never
/// produced on Windows. Without this the right Windows key is `Unknown(92)`,
/// which `is_win` never matches and `is_bindable` rejects, so it can neither
/// trigger the default Ctrl+Win chord nor be recorded as part of a new one.
fn normalize(k: Key) -> Key {
    match k {
        Key::Unknown(92) => Key::MetaRight,
        other => other,
    }
}

/// A binding has to be specific enough that it can't fire during ordinary
/// typing: either two modifiers (Ctrl+Win) or a modifier plus a key
/// (Alt+Shift+Z). Without this a quick tap of a lone modifier committed
/// itself as the binding — a real session bound push-to-talk to a bare
/// "Ctrl", which then fired on every copy/paste.
fn is_usable_chord(keys: &HashSet<Key>) -> bool {
    let checks: [fn(Key) -> bool; 4] = [is_ctrl, is_win, is_alt, is_shift];
    let modifiers = checks
        .iter()
        .filter(|test| keys.iter().any(|&k| test(k)))
        .count();
    let plain = keys
        .iter()
        .filter(|&&k| !is_ctrl(k) && !is_win(k) && !is_alt(k) && !is_shift(k))
        .count();
    modifiers >= 2 || (modifiers >= 1 && plain >= 1)
}

/// What the recorder wants the UI to do after one key event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CaptureStep {
    /// Nothing to report.
    Idle,
    /// Chord still being held; show these keys live.
    Live(String),
    /// Everything released. An empty string means "cancelled".
    Done(String),
    /// Press ignored; tell the user why and keep recording.
    Hint(&'static str),
}

pub const HINT_UNKNOWN_KEY: &str = "That key can't be used in a shortcut";
pub const HINT_NEEDS_MODIFIER: &str =
    "Add a modifier — hold Ctrl, Alt, Shift or Win with another key";

/// Shortcut recorder.
///
/// It deliberately keeps its **own** set of held keys. The hook's global
/// `down` set is process-lifetime and provably goes stale: a low-level hook
/// loses KeyRelease whenever the secure desktop takes over (Win+L, a UAC
/// prompt) or an elevated window has focus, and nothing ever removes the
/// entry. Recording used to sit behind that set's key-repeat guard, so a
/// stale Ctrl or Win — exactly the two keys push-to-talk holds — silently
/// dropped the press before the recorder ever saw it. When every key in the
/// chord was stale the recorder emitted nothing at all and hung forever on
/// "Press your combination…".
#[derive(Default)]
pub struct Capture {
    active: bool,
    down: HashSet<Key>,
    /// The chord at its widest, so releasing in any order still records it.
    max: HashSet<Key>,
}

impl Capture {
    /// Sync with the shared flag. Every transition starts from a clean slate.
    pub fn set_active(&mut self, on: bool) {
        if on != self.active {
            self.down.clear();
            self.max.clear();
            self.active = on;
        }
    }

    pub fn press(&mut self, k: Key) -> CaptureStep {
        if !self.active {
            return CaptureStep::Idle;
        }
        // Escape cancels: an empty chord tells the UI to leave capture.
        if k == Key::Escape {
            self.down.clear();
            self.max.clear();
            return CaptureStep::Done(String::new());
        }
        if !is_bindable(k) {
            return CaptureStep::Hint(HINT_UNKNOWN_KEY);
        }
        if !self.down.insert(k) {
            return CaptureStep::Idle; // auto-repeat of a key we already hold
        }
        self.max = self.down.clone();
        CaptureStep::Live(describe(&self.down))
    }

    pub fn release(&mut self, k: Key) -> CaptureStep {
        if !self.active {
            return CaptureStep::Idle;
        }
        self.down.remove(&k);
        if !self.down.is_empty() || self.max.is_empty() {
            return CaptureStep::Idle;
        }
        let chord = std::mem::take(&mut self.max);
        if !is_usable_chord(&chord) {
            return CaptureStep::Hint(HINT_NEEDS_MODIFIER);
        }
        CaptureStep::Done(describe(&chord))
    }
}

/// The first thing the hook does with a key press while the recorder is on:
/// the press goes to the recorder, and only then into the hook's own `down`
/// set. `None` when the recorder is off and the press is the hook's.
///
/// The order is the point. `down` goes stale (a key-up lost to Win+L, a UAC
/// prompt or an elevated window), and behind its key-repeat guard a phantom
/// Ctrl or Win, the two keys push-to-talk holds, would swallow the press
/// before the recorder saw it, leaving the dialog on "Press your
/// combination…" with no way out.
fn record_press(capture: &mut Capture, down: &mut HashSet<Key>, k: Key) -> Option<CaptureStep> {
    if !capture.active {
        return None;
    }
    let step = capture.press(k);
    down.insert(k);
    Some(step)
}

/// Drop keys Windows no longer reports as physically held.
///
/// This is the other half of the stale-key problem: a phantom Ctrl+Win left in
/// `down` also pins `chord_active`, so push-to-talk stops responding until the
/// app restarts. Eight `GetAsyncKeyState` calls are far too cheap to endanger
/// the low-level hook's timeout budget.
fn prune_stale_modifiers(down: &mut HashSet<Key>) {
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        GetAsyncKeyState, VIRTUAL_KEY, VK_LCONTROL, VK_LMENU, VK_LSHIFT, VK_LWIN, VK_RCONTROL,
        VK_RMENU, VK_RSHIFT, VK_RWIN,
    };
    fn held(vk: VIRTUAL_KEY) -> bool {
        unsafe { (GetAsyncKeyState(vk.0 as i32) as u16 & 0x8000) != 0 }
    }
    down.retain(|&k| match k {
        Key::ControlLeft => held(VK_LCONTROL),
        Key::ControlRight => held(VK_RCONTROL),
        Key::Alt => held(VK_LMENU),
        Key::AltGr => held(VK_RMENU),
        Key::ShiftLeft => held(VK_LSHIFT),
        Key::ShiftRight => held(VK_RSHIFT),
        Key::MetaLeft => held(VK_LWIN),
        Key::MetaRight => held(VK_RWIN),
        _ => true,
    });
}

/// What a session-lock notification (`system_events`) does to the hook's
/// chord bookkeeping: drop every held key and every held chord, rather than
/// waiting for `prune_stale_modifiers` to notice one key at a time on the
/// next real press.
///
/// It tells the controller nothing. The recording a held chord started was
/// already cancelled at the lock itself (`ControlMsg::SessionLocked`), and
/// this runs on the first key after the unlock, where a `ChordUp` would be
/// read as a release: it would finish the recording and paste it.
///
/// A free function, like `prune_stale_modifiers`, so it is tested without a
/// live low-level hook.
fn reset_after_session_lock(
    down: &mut HashSet<Key>,
    chord_active: &mut Option<ChordKind>,
    transforms_active: &mut HashSet<usize>,
    app_shortcuts_active: &mut HashSet<usize>,
) {
    down.clear();
    transforms_active.clear();
    app_shortcuts_active.clear();
    *chord_active = None;
}

fn chord_satisfied(binding: &Binding, down: &HashSet<Key>) -> bool {
    !binding.is_empty()
        && binding.iter().all(|b| match b {
            BindKey::Ctrl => down.iter().any(|&k| is_ctrl(k)),
            BindKey::Win => down.iter().any(|&k| is_win(k)),
            BindKey::Alt => down.iter().any(|&k| is_alt(k)),
            BindKey::Shift => down.iter().any(|&k| is_shift(k)),
            BindKey::Key(want) => down.contains(want),
        })
}

/// Canonical display string for the currently held keys (capture mode).
fn describe(down: &HashSet<Key>) -> String {
    let mut parts: Vec<&str> = Vec::new();
    if down.iter().any(|&k| is_ctrl(k)) {
        parts.push("Ctrl");
    }
    if down.iter().any(|&k| is_win(k)) {
        parts.push("Win");
    }
    if down.iter().any(|&k| is_alt(k)) {
        parts.push("Alt");
    }
    if down.iter().any(|&k| is_shift(k)) {
        parts.push("Shift");
    }
    for &k in down.iter() {
        if !is_ctrl(k) && !is_win(k) && !is_alt(k) && !is_shift(k) {
            if let Some(name) = display_key(k) {
                parts.push(name);
            }
        }
    }
    parts.join("+")
}

pub struct HookShared {
    pub binding: Arc<RwLock<Binding>>,
    /// Transform shortcuts, index-aligned with settings.transforms. Empty
    /// entries (unparseable/blank shortcuts) never match.
    pub transforms: Arc<RwLock<Vec<Binding>>>,
    /// App shortcuts (paste last / copy last / open notes / undo AI
    /// edit), index-aligned with `app_shortcut_bindings`. Empty entries
    /// never match.
    pub app_shortcuts: Arc<RwLock<Vec<Binding>>>,
    /// The other dictation chords (translate / voice agent), index-aligned
    /// with `route_chord_bindings`. Unlike `app_shortcuts` these are *not*
    /// one-shot actions: they press and release exactly like `binding`, and
    /// share its single-active-chord slot. Empty entries never match, which
    /// is how an unbound chord stays unregistered.
    pub route_chords: Arc<RwLock<Vec<Binding>>>,
    /// Set while injecting our own Ctrl+V.
    pub suppress: Arc<AtomicBool>,
    /// Set from the tray "Pause" item.
    pub paused: Arc<AtomicBool>,
    /// Set while the settings UI is recording a new binding.
    pub capture: Arc<AtomicBool>,
    /// Set by `system_events::spawn` on `WTS_SESSION_LOCK`, drained by the
    /// hook on its next event (`reset_after_session_lock`). See
    /// `system_events`'s module doc for why the hook needs this in addition
    /// to `prune_stale_modifiers`.
    pub session_lock_reset: Arc<AtomicBool>,
}

/// Indices into `app_shortcut_bindings` / `ControlMsg::AppShortcut`.
pub const SHORTCUT_PASTE_LAST: usize = 0;
pub const SHORTCUT_COPY_LAST: usize = 1;
pub const SHORTCUT_SCRATCHPAD: usize = 2;
pub const SHORTCUT_UNDO_AI: usize = 3;

fn parse_optional(s: &str) -> Binding {
    if s.trim().is_empty() {
        Vec::new()
    } else {
        s.split('+').filter_map(|p| parse_key(p.trim())).collect()
    }
}

/// App-level shortcut bindings, index-aligned with the SHORTCUT_* constants.
pub fn app_shortcut_bindings(settings: &crate::settings::Settings) -> Vec<Binding> {
    vec![
        parse_optional(&settings.shortcuts.paste_last),
        parse_optional(&settings.shortcuts.copy_last),
        parse_optional(&settings.shortcuts.scratchpad),
        parse_optional(&settings.shortcuts.undo_ai_edit),
    ]
}

/// Indices into `route_chord_bindings`, paired with the `ChordKind` each one
/// stamps by `route_chord_kind`. Same positional coupling as the `SHORTCUT_*`
/// constants above, pinned by the same style of test.
pub const ROUTE_CHORD_TRANSLATE: usize = 0;
pub const ROUTE_CHORD_AGENT: usize = 1;

/// Does `a` shadow `b`? True when every key `a` needs is also needed by `b`,
/// so `b` can never be satisfied without `a` being satisfied too.
///
/// `chord_satisfied` ignores keys held beyond the ones a binding names, which
/// is what makes push-to-talk on Ctrl+Win work while the user is also holding
/// something else. The cost is that a binding is satisfied by every superset
/// of itself, so Ctrl+Win shadows Ctrl+Win+T. An empty binding shadows
/// nothing and is shadowed by nothing — "unbound" stays a no-op.
fn shadows(a: &Binding, b: &Binding) -> bool {
    !a.is_empty() && !b.is_empty() && a.iter().all(|k| b.contains(k))
}

/// The dictation-grade chords that are not the main push-to-talk binding,
/// index-aligned with the `ROUTE_CHORD_*` constants.
///
/// Blank strings become empty bindings, which `chord_satisfied` rejects — an
/// unbound chord is simply never registered, the same way a blank app
/// shortcut is.
///
/// **A chord that shadows, or is shadowed by, the main dictation binding (or
/// an earlier route chord) is dropped here rather than registered.** The hook
/// checks the main binding first and one chord at a time, so with push-to-talk
/// on Ctrl+Win a translate chord bound Ctrl+Win+T never fires: the main chord
/// matches the moment Ctrl and Win are down and the T is never looked at. For
/// the agent chord that is not merely useless, it is the silent-wrong-route
/// failure this whole module exists to prevent — a spoken command pasted as
/// polished prose — reached through configuration rather than a missing key.
///
/// Rejecting it here, in the hook's own data path, rather than only in the
/// settings dialog: `import_settings` and a hand-edited `settings.json` both
/// reach the hook without passing through any UI, and this is the last place
/// that can guarantee the property. Longest-match-wins in the press check
/// would not fix it — pressing Ctrl, Win, then T fires the main chord at the
/// Win, before the T that would have made the longer chord match exists. The
/// dialog rejects the binding too, so the user is told at the moment they
/// record it instead of being left with a row that shows a chord that does
/// nothing.
pub fn route_chord_bindings(settings: &crate::settings::Settings) -> Vec<Binding> {
    let main = parse_binding(&settings.hotkey.binding);
    let mut out: Vec<Binding> = Vec::with_capacity(2);
    for (name, raw) in [
        ("translate dictation", &settings.shortcuts.translate_dictation),
        ("voice agent", &settings.shortcuts.voice_agent),
    ] {
        let candidate = parse_optional(raw);
        let clashes = |other: &Binding| shadows(other, &candidate) || shadows(&candidate, other);
        if clashes(&main) || out.iter().any(clashes) {
            tracing::warn!(
                "the {name} chord ({raw}) overlaps another dictation chord and cannot fire; \
                 leaving it unbound"
            );
            out.push(Vec::new());
        } else {
            out.push(candidate);
        }
    }
    out
}

/// What a route-chord slot stamps on the dictation it starts.
///
/// The one place the index → intent mapping lives, so the hook never has to
/// know what slot 1 means and a new slot cannot be added without answering
/// the question.
fn route_chord_kind(i: usize) -> Option<ChordKind> {
    match i {
        ROUTE_CHORD_TRANSLATE => Some(ChordKind::Translate),
        ROUTE_CHORD_AGENT => Some(ChordKind::Agent),
        _ => None,
    }
}

/// The inverse of `route_chord_kind`, for finding the held chord's binding
/// again on release. `Dictation` has no slot — it is `HookShared::binding`.
fn route_chord_index(kind: ChordKind) -> Option<usize> {
    match kind {
        ChordKind::Dictation => None,
        ChordKind::Translate => Some(ROUTE_CHORD_TRANSLATE),
        ChordKind::Agent => Some(ROUTE_CHORD_AGENT),
    }
}

/// Parse the enabled transform shortcuts into hook bindings. Disabled or
/// blank shortcuts become empty bindings, which `chord_satisfied` rejects.
pub fn transform_bindings(settings: &crate::settings::Settings) -> Vec<Binding> {
    if !settings.transforms_enabled {
        return Vec::new();
    }
    settings
        .transforms
        .iter()
        .map(|t| {
            if t.shortcut.trim().is_empty() {
                Vec::new()
            } else {
                let parsed: Binding = t
                    .shortcut
                    .split('+')
                    .filter_map(|p| parse_key(p.trim()))
                    .collect();
                parsed
            }
        })
        .collect()
}

pub fn spawn(app: tauri::AppHandle, tx: Sender<ControlMsg>, shared: HookShared) {
    // A low-level keyboard-hook callback has a strict, system-enforced time
    // budget. Sending a Tauri event from inside it can need the webview/event
    // loop and, when that stalls, Windows silently removes the hook. Keep the
    // callback to state changes + channel sends; this worker owns delivery to
    // the settings UI instead.
    let (capture_events_tx, capture_events_rx) = crossbeam_channel::unbounded::<CaptureStep>();
    let capture_event_app = app.clone();
    std::thread::Builder::new()
        .name("hotkey-capture-events".into())
        .spawn(move || {
            let trace = std::env::var_os("BS_DEBUG_HOTKEYS").is_some();
            for step in capture_events_rx {
                let payload = match step {
                    CaptureStep::Idle => continue,
                    CaptureStep::Live(keys) => events::HotkeyCapturePayload::live(keys),
                    CaptureStep::Done(keys) => events::HotkeyCapturePayload::done(keys),
                    CaptureStep::Hint(message) => events::HotkeyCapturePayload::hint(message),
                };
                let sent = capture_event_app.emit(events::HOTKEY_CAPTURE, payload.clone());
                if trace {
                    tracing::info!(
                        "hotkey capture -> keys={:?} done={} hint={:?} emit={:?}",
                        payload.keys,
                        payload.done,
                        payload.hint,
                        sent.as_ref().err().map(|e| e.to_string())
                    );
                } else if let Err(e) = sent {
                    tracing::warn!("couldn't deliver hotkey capture event: {e}");
                }
            }
        })
        .expect("spawn hotkey capture event thread");

    std::thread::Builder::new()
        .name("hotkeys".into())
        .spawn(move || {
            let mut down: HashSet<Key> = HashSet::new();
            // One dictation chord at a time, whichever it is. `Some(kind)`
            // both marks the chord held and remembers which binding to watch
            // for the release — the three chords all start a recording, so a
            // second one landing mid-dictation would be nothing but a way to
            // desync the route stamp from the session it describes.
            let mut chord_active: Option<ChordKind> = None;
            let mut transforms_active: HashSet<usize> = HashSet::new();
            let mut app_shortcuts_active: HashSet<usize> = HashSet::new();
            let mut capture = Capture::default();
            // Opt-in tracing for the hook. Global-hotkey faults are invisible
            // by nature -- there is no error, just nothing happening -- so keep
            // a way to see every event without shipping the noise. It names
            // modifiers and the bindings that fire, never another key
            // (`traced_press`), and says loudly that it is on.
            let trace = std::env::var_os("BS_DEBUG_HOTKEYS").is_some();
            tracing::info!(
                "hotkey hook installed (build {}, tracing {})",
                env!("CARGO_PKG_VERSION"),
                if trace { "on" } else { "off" }
            );
            if trace {
                tracing::warn!(
                    "BS_DEBUG_HOTKEYS is set: every key press in every app is traced to this log \
                     (modifiers by name, other keys unnamed); unset it when you are done"
                );
            }

            let emit_step = move |step: CaptureStep| {
                if step == CaptureStep::Idle {
                    return;
                }
                if capture_events_tx.send(step).is_err() {
                    tracing::warn!("hotkey capture event worker stopped");
                }
            };

            let cb = move |event: Event| {
                let capture_on = shared.capture.load(Ordering::Relaxed);
                capture.set_active(capture_on);

                // Drained here rather than in the `system_events` thread
                // itself: only the hook owns `down`/`chord_active`/the
                // active-shortcut sets, so the reset has to happen on this
                // thread, on whatever event happens to arrive next.
                if shared.session_lock_reset.swap(false, Ordering::Relaxed) {
                    if trace {
                        tracing::info!("session lock detected; resetting hook held-key state");
                    }
                    reset_after_session_lock(
                        &mut down,
                        &mut chord_active,
                        &mut transforms_active,
                        &mut app_shortcuts_active,
                    );
                }

                match event.event_type {
                    EventType::KeyPress(k) => {
                        let k = normalize(k);
                        if trace {
                            tracing::info!(
                                "hotkey press {} capture_on={capture_on}",
                                traced_press(k)
                            );
                        }
                        // Recording runs FIRST, on the recorder's own held-key
                        // set — see `record_press`.
                        if let Some(step) = record_press(&mut capture, &mut down, k) {
                            emit_step(step);
                            return;
                        }
                        // Outside capture, drop keys the OS says are no longer
                        // held before matching, so a lost KeyRelease can't pin
                        // `chord_active` and disable push-to-talk for the rest
                        // of the session.
                        prune_stale_modifiers(&mut down);
                        if !down.insert(k) {
                            return; // key-repeat
                        }
                        if shared.suppress.load(Ordering::Relaxed)
                            || shared.paused.load(Ordering::Relaxed)
                        {
                            return;
                        }
                        if k == Key::Escape {
                            let _ = tx.send(ControlMsg::Escape);
                            return;
                        }
                        if chord_active.is_none() {
                            let binding = shared.binding.read().expect("binding lock");
                            if chord_satisfied(&binding, &down) {
                                chord_active = Some(ChordKind::Dictation);
                            }
                            drop(binding);
                            // Checked only when the main chord did not match:
                            // these are the same gesture with a different
                            // intent, not a second thing to fire alongside it.
                            if chord_active.is_none() {
                                let routes = shared.route_chords.read().expect("route lock");
                                for (i, b) in routes.iter().enumerate() {
                                    if chord_satisfied(b, &down) {
                                        chord_active = route_chord_kind(i);
                                        if chord_active.is_some() {
                                            break;
                                        }
                                    }
                                }
                            }
                            if let Some(kind) = chord_active {
                                if trace {
                                    tracing::info!("hotkey chord down {kind:?}");
                                }
                                let _ = tx.send(ControlMsg::ChordDown(kind));
                            }
                        }
                        let transforms = shared.transforms.read().expect("transforms lock");
                        for (i, b) in transforms.iter().enumerate() {
                            if !transforms_active.contains(&i) && chord_satisfied(b, &down) {
                                if trace {
                                    tracing::info!("hotkey transform chord {i}");
                                }
                                transforms_active.insert(i);
                                let _ = tx.send(ControlMsg::TransformChord(i));
                            }
                        }
                        drop(transforms);
                        let shortcuts = shared.app_shortcuts.read().expect("shortcuts lock");
                        for (i, b) in shortcuts.iter().enumerate() {
                            if !app_shortcuts_active.contains(&i) && chord_satisfied(b, &down) {
                                if trace {
                                    tracing::info!("hotkey app shortcut {i}");
                                }
                                app_shortcuts_active.insert(i);
                                let _ = tx.send(ControlMsg::AppShortcut(i));
                            }
                        }
                    }
                    EventType::KeyRelease(k) => {
                        let k = normalize(k);
                        down.remove(&k);
                        if capture_on {
                            emit_step(capture.release(k));
                            return;
                        }
                        if let Some(kind) = chord_active {
                            // The release is watched on whichever binding
                            // started the dictation, so a translate chord
                            // that shares modifiers with the main one still
                            // ends when *it* is released.
                            let still_held = match route_chord_index(kind) {
                                None => {
                                    let binding = shared.binding.read().expect("binding lock");
                                    chord_satisfied(&binding, &down)
                                }
                                Some(i) => shared
                                    .route_chords
                                    .read()
                                    .expect("route lock")
                                    .get(i)
                                    .is_some_and(|b| chord_satisfied(b, &down)),
                            };
                            if !still_held {
                                if trace {
                                    tracing::info!("hotkey chord up {kind:?}");
                                }
                                chord_active = None;
                                if !shared.suppress.load(Ordering::Relaxed) {
                                    let _ = tx.send(ControlMsg::ChordUp);
                                }
                            }
                        }
                        if !transforms_active.is_empty() {
                            let transforms = shared.transforms.read().expect("transforms lock");
                            transforms_active.retain(|&i| {
                                transforms.get(i).is_some_and(|b| chord_satisfied(b, &down))
                            });
                        }
                        if !app_shortcuts_active.is_empty() {
                            let shortcuts = shared.app_shortcuts.read().expect("shortcuts lock");
                            app_shortcuts_active.retain(|&i| {
                                shortcuts.get(i).is_some_and(|b| chord_satisfied(b, &down))
                            });
                        }
                    }
                    _ => {}
                }
            };

            match listen(cb) {
                // rdev's Windows `listen` parks the thread in a single
                // `GetMessageA`, which is what keeps the low-level hook alive.
                // If it ever returns, the thread ends and Windows tears the
                // hook down with it — every global shortcut dies, silently.
                // Worth an error line, not a shrug.
                Ok(()) => {
                    tracing::error!("rdev listen returned; the hotkey hook is no longer installed")
                }
                Err(e) => tracing::error!("rdev listen error: {e:?}"),
            }
        })
        .expect("spawn hotkeys thread");
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every key the capture UI can name, for round-trip checking.
    const NAMEABLE: &[Key] = &[
        Key::Space,
        Key::Tab,
        Key::CapsLock,
        Key::Home,
        Key::End,
        Key::Insert,
        Key::Delete,
        Key::PageUp,
        Key::PageDown,
        Key::Backspace,
        Key::Return,
        Key::UpArrow,
        Key::DownArrow,
        Key::LeftArrow,
        Key::RightArrow,
        Key::PrintScreen,
        Key::ScrollLock,
        Key::Pause,
        Key::NumLock,
        Key::BackQuote,
        Key::Minus,
        Key::Equal,
        Key::LeftBracket,
        Key::RightBracket,
        Key::SemiColon,
        Key::Quote,
        Key::BackSlash,
        Key::Comma,
        Key::Dot,
        Key::Slash,
        Key::Kp0,
        Key::Kp9,
        Key::KpReturn,
        Key::KpMinus,
        Key::KpPlus,
        Key::KpMultiply,
        Key::KpDivide,
        Key::KpDelete,
        Key::F1,
        Key::F12,
        Key::KeyA,
        Key::KeyZ,
        Key::Num0,
        Key::Num9,
    ];

    /// `describe` writes the string that gets saved to settings, and
    /// `parse_binding` reads it back for the hook. If they ever disagree, a
    /// recorded shortcut saves fine and then silently never fires.
    #[test]
    fn every_display_name_parses_back_to_its_key() {
        for &k in NAMEABLE {
            let name = display_key(k).unwrap_or_else(|| panic!("{k:?} has no display name"));
            assert_eq!(
                parse_key(name),
                Some(BindKey::Key(k)),
                "{k:?} displayed as {name:?} but did not parse back"
            );
        }
    }

    /// Bindings are split on '+', so no key may render to a name containing it.
    #[test]
    fn no_display_name_contains_the_separator() {
        for &k in NAMEABLE {
            let name = display_key(k).unwrap();
            assert!(!name.contains('+'), "{k:?} renders as {name:?}");
        }
    }

    #[test]
    fn describe_round_trips_through_parse_binding() {
        let mut down = HashSet::new();
        down.insert(Key::ControlLeft);
        down.insert(Key::ShiftLeft);
        down.insert(Key::Slash);
        let text = describe(&down);
        assert_eq!(text, "Ctrl+Shift+/");
        assert!(chord_satisfied(&parse_binding(&text), &down));
    }

    /// `SHORTCUT_*` constants are positionally coupled to the Vec built by
    /// `app_shortcut_bindings` — correct today, invisible to the compiler.
    /// A future field inserted in the middle of `ShortcutSettings` (or a
    /// reordered push here) would silently fire the wrong action for every
    /// existing binding. Four distinct chords pin each constant to the
    /// binding parsed from its own named settings field, so a transposition
    /// shows up as a mismatch rather than a coincidentally-passing test.
    #[test]
    fn shortcut_indices_match_their_own_settings_field() {
        let mut settings = crate::settings::Settings::default();
        settings.shortcuts.paste_last = "Alt+Shift+Z".into();
        settings.shortcuts.copy_last = "Alt+Shift+X".into();
        settings.shortcuts.scratchpad = "Alt+Shift+S".into();
        settings.shortcuts.undo_ai_edit = "Alt+Shift+U".into();

        let bindings = app_shortcut_bindings(&settings);

        assert_eq!(
            bindings[SHORTCUT_PASTE_LAST],
            parse_binding(&settings.shortcuts.paste_last)
        );
        assert_eq!(
            bindings[SHORTCUT_COPY_LAST],
            parse_binding(&settings.shortcuts.copy_last)
        );
        assert_eq!(
            bindings[SHORTCUT_SCRATCHPAD],
            parse_binding(&settings.shortcuts.scratchpad)
        );
        assert_eq!(
            bindings[SHORTCUT_UNDO_AI],
            parse_binding(&settings.shortcuts.undo_ai_edit)
        );
    }

    /// The `ShortcutSettings` fields the two dictation-grade chords live on
    /// are appended after the app-shortcut ones, so they must not have
    /// disturbed the list `SHORTCUT_*` indexes into. Distinct chords again,
    /// so a transposition cannot pass by coincidence.
    #[test]
    fn the_new_chord_fields_did_not_shift_the_app_shortcut_indices() {
        let mut settings = crate::settings::Settings::default();
        settings.shortcuts.paste_last = "Alt+Shift+Z".into();
        settings.shortcuts.copy_last = "Alt+Shift+X".into();
        settings.shortcuts.scratchpad = "Alt+Shift+S".into();
        settings.shortcuts.undo_ai_edit = "Alt+Shift+U".into();
        settings.shortcuts.translate_dictation = "Alt+Shift+T".into();
        settings.shortcuts.voice_agent = "Alt+Shift+A".into();

        let bindings = app_shortcut_bindings(&settings);

        assert_eq!(bindings.len(), 4, "route chords are not app shortcuts");
        assert_eq!(
            bindings[SHORTCUT_PASTE_LAST],
            parse_binding(&settings.shortcuts.paste_last)
        );
        assert_eq!(
            bindings[SHORTCUT_UNDO_AI],
            parse_binding(&settings.shortcuts.undo_ai_edit)
        );
    }

    /// The route-chord twin of `shortcut_indices_match_their_own_settings_
    /// field`, and the one that matters most: these indices decide which
    /// *intent* gets stamped on a dictation, so a transposition would send
    /// every translate dictation to the voice agent (and vice versa) with
    /// nothing in the type system to notice.
    #[test]
    fn route_chord_indices_match_their_own_settings_field_and_intent() {
        let mut settings = crate::settings::Settings::default();
        settings.shortcuts.translate_dictation = "Alt+Shift+T".into();
        settings.shortcuts.voice_agent = "Alt+Shift+A".into();

        let bindings = route_chord_bindings(&settings);

        assert_eq!(
            bindings[ROUTE_CHORD_TRANSLATE],
            parse_binding(&settings.shortcuts.translate_dictation)
        );
        assert_eq!(
            bindings[ROUTE_CHORD_AGENT],
            parse_binding(&settings.shortcuts.voice_agent)
        );
        assert_eq!(
            route_chord_kind(ROUTE_CHORD_TRANSLATE),
            Some(ChordKind::Translate)
        );
        assert_eq!(route_chord_kind(ROUTE_CHORD_AGENT), Some(ChordKind::Agent));
        assert_eq!(route_chord_kind(bindings.len()), None, "no phantom slots");
    }

    /// `route_chord_kind` and `route_chord_index` are used on opposite edges
    /// of a press/release pair: press looks up the intent from the slot, and
    /// release looks the slot back up from the intent to find the binding to
    /// watch. If they ever disagree, a chord would be pressed and then never
    /// released — recording forever.
    #[test]
    fn route_chord_kind_and_index_round_trip() {
        for i in [ROUTE_CHORD_TRANSLATE, ROUTE_CHORD_AGENT] {
            let kind = route_chord_kind(i).expect("slot has an intent");
            assert_eq!(route_chord_index(kind), Some(i));
        }
        // The main push-to-talk chord is `HookShared::binding`, not a slot.
        assert_eq!(route_chord_index(ChordKind::Dictation), None);
    }

    // --- chord shadowing --------------------------------------------------

    /// The natural way a user would bind this:
    /// push-to-talk is Ctrl+Win, so "the dictation chord plus T" reads as the
    /// obvious translate chord. It cannot work. The hook's press check
    /// satisfies the main binding the instant Ctrl and Win are down, sends
    /// `ChordDown(Dictation)`, and never looks at the route list again — the
    /// T is never seen. Bound to the agent that is worse than not working: a
    /// spoken command becomes a plain cleanup dictation and gets pasted as
    /// prose, which is the silent-wrong-route class this module exists to
    /// prevent, reached through configuration instead of a missing key.
    ///
    /// So it is not registered at all. An unbound chord is a state the user
    /// can see; a bound chord that quietly does something else is not.
    #[test]
    fn a_route_chord_that_extends_the_main_chord_is_not_registered() {
        let mut settings = crate::settings::Settings::default();
        settings.hotkey.binding = "Ctrl+Win".into();
        settings.shortcuts.translate_dictation = "Ctrl+Win+T".into();
        settings.shortcuts.voice_agent = "Ctrl+Win+A".into();

        let bindings = route_chord_bindings(&settings);

        assert!(
            bindings[ROUTE_CHORD_TRANSLATE].is_empty(),
            "Ctrl+Win+T is swallowed by a Ctrl+Win push-to-talk"
        );
        assert!(
            bindings[ROUTE_CHORD_AGENT].is_empty(),
            "Ctrl+Win+A is swallowed by a Ctrl+Win push-to-talk"
        );
    }

    /// Exactly the same chord as push-to-talk: the main binding wins and the
    /// route chord is dead weight.
    #[test]
    fn a_route_chord_duplicating_the_main_chord_is_not_registered() {
        let mut settings = crate::settings::Settings::default();
        settings.hotkey.binding = "Ctrl+Win".into();
        settings.shortcuts.translate_dictation = "Win+Ctrl".into();

        assert!(route_chord_bindings(&settings)[ROUTE_CHORD_TRANSLATE].is_empty());
    }

    /// The other direction, which breaks the *main* feature: with
    /// push-to-talk on Ctrl+Win+T and translate on Ctrl+Win, pressing Ctrl
    /// and Win satisfies the route chord first (the main one needs a T that
    /// hasn't arrived), so every attempt at plain dictation would start a
    /// translation. Dropping the route chord is the resolution — ordinary
    /// dictation is the feature that must always work, and cleanup is the
    /// safe route to be left on.
    #[test]
    fn a_route_chord_the_main_chord_extends_is_not_registered_either() {
        let mut settings = crate::settings::Settings::default();
        settings.hotkey.binding = "Ctrl+Win+T".into();
        settings.shortcuts.translate_dictation = "Ctrl+Win".into();

        assert!(route_chord_bindings(&settings)[ROUTE_CHORD_TRANSLATE].is_empty());
    }

    /// Two route chords can shadow each other the same way; the loop keeps
    /// the first and drops the second, deterministically.
    #[test]
    fn route_chords_that_shadow_each_other_keep_only_the_first() {
        let mut settings = crate::settings::Settings::default();
        settings.hotkey.binding = "Ctrl+Win".into();
        settings.shortcuts.translate_dictation = "Alt+Shift+T".into();
        settings.shortcuts.voice_agent = "Alt+Shift+T+A".into();

        let bindings = route_chord_bindings(&settings);

        assert_eq!(
            bindings[ROUTE_CHORD_TRANSLATE],
            parse_binding("Alt+Shift+T")
        );
        assert!(bindings[ROUTE_CHORD_AGENT].is_empty());
    }

    /// The whole point is that a chord which cannot be confused with another
    /// still registers normally — the rejection must be narrow.
    #[test]
    fn chords_that_share_only_some_keys_are_left_alone() {
        let mut settings = crate::settings::Settings::default();
        settings.hotkey.binding = "Ctrl+Win".into();
        // Shares Ctrl with the main chord but adds Shift, which the main
        // chord does not require — neither is satisfied whenever the other is.
        settings.shortcuts.translate_dictation = "Ctrl+Shift+T".into();
        settings.shortcuts.voice_agent = "Alt+Shift+A".into();

        let bindings = route_chord_bindings(&settings);

        assert_eq!(
            bindings[ROUTE_CHORD_TRANSLATE],
            parse_binding("Ctrl+Shift+T")
        );
        assert_eq!(bindings[ROUTE_CHORD_AGENT], parse_binding("Alt+Shift+A"));
    }

    /// `shadows` is the predicate the whole rule rests on: "every key `a`
    /// needs is also needed by `b`", i.e. `b` can never be satisfied without
    /// `a` being satisfied too. An empty binding shadows nothing and is
    /// shadowed by nothing — that is how "unbound" stays a no-op.
    #[test]
    fn shadowing_is_subset_containment_and_ignores_the_empty_binding() {
        let ctrl_win = parse_binding("Ctrl+Win");
        let ctrl_win_t = parse_binding("Ctrl+Win+T");
        let ctrl_shift = parse_binding("Ctrl+Shift");

        assert!(shadows(&ctrl_win, &ctrl_win_t));
        assert!(!shadows(&ctrl_win_t, &ctrl_win));
        assert!(shadows(&ctrl_win, &ctrl_win));
        assert!(!shadows(&ctrl_win, &ctrl_shift));
        assert!(!shadows(&Vec::new(), &ctrl_win));
        assert!(!shadows(&ctrl_win, &Vec::new()));
    }

    /// An unbound chord is simply not registered — the shipped default for
    /// both of them. `chord_satisfied` rejects an empty binding, so the slot
    /// exists but can never match.
    #[test]
    fn unbound_route_chords_never_match() {
        let settings = crate::settings::Settings::default();
        let bindings = route_chord_bindings(&settings);
        let everything = HashSet::from([
            Key::ControlLeft,
            Key::MetaLeft,
            Key::Alt,
            Key::ShiftLeft,
            Key::KeyT,
        ]);
        for b in &bindings {
            assert!(b.is_empty(), "ships unbound");
            assert!(!chord_satisfied(b, &everything));
        }
    }

    /// The hook sees every key pressed in every app, passwords included, and
    /// the opt-in trace (`BS_DEBUG_HOTKEYS`) writes to the log file. So a
    /// press is named only when it is a modifier; any other key is traced
    /// without its name, so the log never records what was typed.
    #[test]
    fn the_hook_trace_names_modifiers_and_no_other_key() {
        for k in [
            Key::ControlLeft,
            Key::ControlRight,
            Key::MetaLeft,
            Key::MetaRight,
            Key::Alt,
            Key::AltGr,
            Key::ShiftLeft,
            Key::ShiftRight,
        ] {
            let line = traced_press(k);
            assert!(line.contains(&format!("{k:?}")), "modifier {k:?} not named: {line}");
        }
        for k in [
            Key::KeyA,
            Key::KeyZ,
            Key::Num1,
            Key::Space,
            Key::Return,
            Key::Backspace,
            Key::F5,
            Key::Escape,
            Key::SemiColon,
            Key::Unknown(200),
        ] {
            let line = traced_press(k);
            assert!(!line.contains(&format!("{k:?}")), "{k:?} named in the trace: {line}");
        }
        // Unnamed, but a key rdev cannot name (how the right Windows key
        // once showed up) is still told apart from an ordinary one.
        assert_ne!(traced_press(Key::KeyA), traced_press(Key::Unknown(200)));
    }

    /// Escape is the cancel gesture, so it must never become part of a chord.
    #[test]
    fn escape_is_not_bindable() {
        assert!(!is_bindable(Key::Escape));
        assert_eq!(display_key(Key::Escape), None);
    }

    /// The regression: these used to reach `capture_max`, whose `describe`
    /// rendered empty, which the UI reads as "cancel" — so pressing one
    /// silently ended the recording with nothing saved.
    #[test]
    fn formerly_unnameable_keys_are_bindable_now() {
        for k in [
            Key::Backspace,
            Key::Return,
            Key::UpArrow,
            Key::Minus,
            Key::Slash,
            Key::SemiColon,
        ] {
            assert!(is_bindable(k), "{k:?} should be bindable");
            assert!(!describe(&HashSet::from([k])).is_empty(), "{k:?} describes empty");
        }
    }

    #[test]
    fn unknown_keys_stay_unbindable() {
        assert!(!is_bindable(Key::Unknown(255)));
        assert!(!is_bindable(Key::Function));
    }

    fn recording() -> Capture {
        let mut c = Capture::default();
        c.set_active(true);
        c
    }

    /// THE REGRESSION. Recording used to run behind the hook's global `down`
    /// key-repeat guard, and that set goes stale (a KeyRelease lost to Win+L,
    /// a UAC prompt, or an elevated window is never recovered). A stale Ctrl
    /// or Win — precisely what push-to-talk holds — made the press vanish
    /// before the recorder saw it, so nothing was emitted and the dialog hung
    /// on "Press your combination…" forever. The recorder now owns its held
    /// set, so a dirty global `down` cannot reach it.
    #[test]
    fn recorder_is_unaffected_by_a_stale_global_down_set() {
        let mut stale: HashSet<Key> = HashSet::from([Key::ControlLeft, Key::MetaLeft]);

        let mut c = recording();
        // Exactly the chord the user reported, through the hook's own press
        // path, with the hook's set already holding a phantom Ctrl and Win.
        for (k, live) in [
            (Key::ControlLeft, "Ctrl"),
            (Key::ShiftLeft, "Ctrl+Shift"),
            (Key::KeyD, "Ctrl+Shift+D"),
        ] {
            assert_eq!(
                record_press(&mut c, &mut stale, k),
                Some(CaptureStep::Live(live.into()))
            );
        }
        assert_eq!(c.release(Key::KeyD), CaptureStep::Idle);
        assert_eq!(c.release(Key::ShiftLeft), CaptureStep::Idle);
        assert_eq!(
            c.release(Key::ControlLeft),
            CaptureStep::Done("Ctrl+Shift+D".into())
        );

        // With the recorder off, a press is the hook's to handle.
        let mut off = Capture::default();
        assert_eq!(record_press(&mut off, &mut stale, Key::KeyA), None);
    }

    /// A lone modifier tap used to commit itself: the reported session bound
    /// push-to-talk to a bare "Ctrl", which then fired on every copy/paste.
    #[test]
    fn a_lone_modifier_is_refused_with_a_hint() {
        let mut c = recording();
        assert_eq!(c.press(Key::ControlLeft), CaptureStep::Live("Ctrl".into()));
        assert_eq!(
            c.release(Key::ControlLeft),
            CaptureStep::Hint(HINT_NEEDS_MODIFIER)
        );
        // Still recording, so the user can simply try again.
        assert_eq!(c.press(Key::ControlLeft), CaptureStep::Live("Ctrl".into()));
        assert_eq!(c.press(Key::MetaLeft), CaptureStep::Live("Ctrl+Win".into()));
        c.release(Key::MetaLeft);
        assert_eq!(
            c.release(Key::ControlLeft),
            CaptureStep::Done("Ctrl+Win".into())
        );
    }

    #[test]
    fn two_modifiers_or_modifier_plus_key_are_both_usable() {
        assert!(is_usable_chord(&HashSet::from([Key::ControlLeft, Key::MetaLeft])));
        assert!(is_usable_chord(&HashSet::from([Key::Alt, Key::ShiftLeft, Key::KeyZ])));
        assert!(!is_usable_chord(&HashSet::from([Key::ControlLeft])));
        assert!(!is_usable_chord(&HashSet::from([Key::KeyZ])));
        // Two keys with no modifier can't be held reliably and would fire
        // while typing.
        assert!(!is_usable_chord(&HashSet::from([Key::KeyZ, Key::KeyX])));
    }

    /// rdev never produces `MetaRight` on Windows, so right-Win arrives as
    /// `Unknown(92)` and was invisible to both chord matching and recording.
    #[test]
    fn right_windows_key_is_normalized_and_bindable() {
        assert_eq!(normalize(Key::Unknown(92)), Key::MetaRight);
        assert!(is_bindable(normalize(Key::Unknown(92))));
        assert!(is_win(normalize(Key::Unknown(92))));

        let mut c = recording();
        assert_eq!(c.press(normalize(Key::Unknown(92))), CaptureStep::Live("Win".into()));
        assert_eq!(c.press(Key::ControlLeft), CaptureStep::Live("Ctrl+Win".into()));
        c.release(Key::ControlLeft);
        assert_eq!(
            c.release(normalize(Key::Unknown(92))),
            CaptureStep::Done("Ctrl+Win".into())
        );
    }

    /// Genuinely unnameable keys now explain themselves instead of being
    /// dropped in silence, which is what made the dialog look frozen.
    #[test]
    fn unnameable_keys_report_a_hint_and_keep_recording() {
        let mut c = recording();
        assert_eq!(c.press(Key::Unknown(255)), CaptureStep::Hint(HINT_UNKNOWN_KEY));
        assert_eq!(c.press(Key::ControlLeft), CaptureStep::Live("Ctrl".into()));
    }

    #[test]
    fn escape_cancels_with_an_empty_chord() {
        let mut c = recording();
        c.press(Key::ControlLeft);
        assert_eq!(c.press(Key::Escape), CaptureStep::Done(String::new()));
    }

    /// Held keys auto-repeat at ~30 ms; the recorder must not re-emit for each.
    #[test]
    fn auto_repeat_does_not_re_emit() {
        let mut c = recording();
        assert_eq!(c.press(Key::ControlLeft), CaptureStep::Live("Ctrl".into()));
        assert_eq!(c.press(Key::ControlLeft), CaptureStep::Idle);
        assert_eq!(c.press(Key::ControlLeft), CaptureStep::Idle);
    }

    /// Re-arming must not inherit the previous attempt's keys.
    #[test]
    fn toggling_capture_clears_previous_state() {
        let mut c = recording();
        c.press(Key::ControlLeft);
        c.press(Key::ShiftLeft);
        c.set_active(false);
        c.set_active(true);
        // No lingering Ctrl+Shift: the next press starts a fresh chord.
        assert_eq!(c.press(Key::Alt), CaptureStep::Live("Alt".into()));
    }

    /// Releasing in a different order than pressing still yields the full
    /// chord, which is why the recorder keeps a high-water mark.
    #[test]
    fn release_order_does_not_change_the_recorded_chord() {
        let mut c = recording();
        c.press(Key::ControlLeft);
        c.press(Key::ShiftLeft);
        c.press(Key::KeyD);
        assert_eq!(c.release(Key::ControlLeft), CaptureStep::Idle);
        assert_eq!(c.release(Key::KeyD), CaptureStep::Idle);
        assert_eq!(
            c.release(Key::ShiftLeft),
            CaptureStep::Done("Ctrl+Shift+D".into())
        );
    }

    // --- Session-lock reset -----------------------------------------------

    /// Every held key, chord and shortcut is dropped, so the first key after
    /// the unlock starts from a clean slate.
    #[test]
    fn a_session_lock_drops_everything_the_hook_holds() {
        let mut down = HashSet::from([Key::ControlLeft, Key::MetaLeft]);
        let mut chord_active = Some(ChordKind::Dictation);
        let mut transforms_active = HashSet::from([0usize]);
        let mut app_shortcuts_active = HashSet::from([1usize]);

        reset_after_session_lock(
            &mut down,
            &mut chord_active,
            &mut transforms_active,
            &mut app_shortcuts_active,
        );

        assert!(down.is_empty());
        assert!(chord_active.is_none());
        assert!(transforms_active.is_empty());
        assert!(app_shortcuts_active.is_empty());
    }

    /// A route chord is a dictation chord: the lock clears it exactly as it
    /// clears the main one, or it would stay held with no key left to
    /// release it.
    #[test]
    fn a_held_route_chord_is_dropped_too() {
        for kind in [ChordKind::Translate, ChordKind::Agent] {
            let mut down = HashSet::from([Key::ControlLeft, Key::KeyT]);
            let mut chord_active = Some(kind);
            reset_after_session_lock(
                &mut down,
                &mut chord_active,
                &mut HashSet::new(),
                &mut HashSet::new(),
            );
            assert!(chord_active.is_none(), "{kind:?}");
        }
    }

    /// The reset tells the controller nothing: the recording was cancelled at
    /// the lock (`ControlMsg::SessionLocked`), and a `ChordUp` sent from here,
    /// on the first key after the unlock, would finish it and paste instead.
    /// The hook sends from exactly one place in the reset's path, and the
    /// reset's own body has no channel to send on.
    #[test]
    fn a_session_lock_reset_sends_no_chord_up() {
        let source = include_str!("hotkeys.rs").replace("\r\n", "\n");
        let start = source
            .find("\nfn reset_after_session_lock(")
            .expect("reset_after_session_lock is defined here");
        let body = &source[start..start + source[start..].find("\n}\n").unwrap()];
        assert!(!body.contains("ChordUp"), "{body}");
        let drain = source
            .find("if shared.session_lock_reset.swap(false")
            .expect("the hook drains the lock flag");
        let arm = &source[drain..drain + source[drain..].find("match event.event_type").unwrap()];
        assert!(!arm.contains("tx.send"), "{arm}");
    }

    /// Whatever the recorder emits must parse back into a hook binding, or the
    /// shortcut saves fine and then never fires.
    #[test]
    fn recorded_chords_round_trip_into_a_working_binding() {
        let mut c = recording();
        c.press(Key::ControlLeft);
        c.press(Key::ShiftLeft);
        c.press(Key::Slash);
        let CaptureStep::Done(text) = ({
            c.release(Key::Slash);
            c.release(Key::ShiftLeft);
            c.release(Key::ControlLeft)
        }) else {
            panic!("chord did not complete");
        };
        let held = HashSet::from([Key::ControlLeft, Key::ShiftLeft, Key::Slash]);
        assert!(chord_satisfied(&parse_binding(&text), &held));
    }
}
