//! Identifies and targets the app the user is dictating into.
//!
//! [`capture`] pins down *which window* — not just which process — at the
//! moment dictation starts (chord-down) or a manual re-paste fires, so a
//! paste that lands seconds or minutes later (after transcription,
//! formatting, an AI polish round-trip) can be aimed back at that exact
//! window even if the user has switched to another one since. The captured
//! process name (lowercased, no ".exe") also feeds per-app style rules.
//! [`restore_foreground`] does the aiming; [`is_terminal`] tells the paste
//! path and Undo's readback probe when the target is a console, where the
//! ordinary Ctrl+V/Ctrl+C chords don't behave like "paste"/"copy".

use std::thread;
use std::time::Duration;
use windows::Win32::Foundation::HWND;
use windows::Win32::System::Threading::{
    AttachThreadInput, GetCurrentThreadId, OpenProcess, QueryFullProcessImageNameW,
    PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::UI::Input::KeyboardAndMouse::SetFocus;
use windows::Win32::UI::WindowsAndMessaging::{
    BringWindowToTop, GetClassNameW, GetForegroundWindow, GetWindowThreadProcessId, IsIconic,
    IsWindow, SetForegroundWindow, ShowWindow, SW_RESTORE,
};

/// Wait after bringing the target window forward, before the paste keys go
/// out. Activation reaches the target's thread as messages it still has to
/// pump, and a browser then passes focus on to the page, which takes a frame
/// or two: 35 ms is two frames at 60 Hz with a little to spare. Spent only
/// when a switch actually happened. It is a term of
/// `routes::selection::REPLACE_BUDGET` and of both finalize ceilings in
/// `controller`, so it stays small.
pub(crate) const SETTLE_AFTER_SWITCH_MS: u64 = 35;

/// The window the user was looking at when dictation started, captured at
/// chord-down — the moment they pressed the hotkey is the moment they were
/// looking at their intended destination, which is not necessarily still
/// true once the paste is ready.
///
/// `hwnd` is stored as `isize` rather than `HWND`: `HWND` wraps a raw pointer
/// and is not `Send`, but a window handle is just an opaque id, good for the
/// life of the window regardless of which thread holds it, so the round trip
/// through `isize` costs nothing and lets a `Target` cross onto the
/// injection thread.
#[derive(Debug, Clone)]
pub struct Target {
    hwnd: isize,
    /// Owning process id. Cheap to re-derive for *any* window
    /// (`GetWindowThreadProcessId` alone, no process handle) — kept here so
    /// a caller that only needs "is this still the same process" doesn't
    /// have to pay for [`process_name`]'s `OpenProcess` +
    /// `QueryFullProcessImageNameW` just to get an answer that's usually
    /// the same. See [`current_foreground_pid`].
    pub pid: u32,
    /// Process name, lowercased without ".exe".
    pub app: Option<String>,
    /// Window class (`GetClassNameW`), captured once here rather than
    /// re-queried at paste time so [`is_terminal`] always judges the window
    /// that was actually targeted, not whatever happens to be foreground
    /// when someone asks.
    class: String,
}

impl Target {
    /// The captured window handle, as the opaque `isize` it is stored as.
    ///
    /// For callers that name a window to something outside this module —
    /// `uia::element_for_hwnd`, which binds the field a paste landed in.
    /// Deliberately **not** an `HWND`: `HWND` wraps a raw pointer and is not
    /// `Send`, so handing one out here would let a caller put it somewhere
    /// that quietly costs `Target` the Send-ness the struct doc promises. An
    /// `isize` is what a window handle actually is — an opaque id, good for
    /// the life of the window on any thread.
    pub(crate) fn hwnd(&self) -> isize {
        self.hwnd
    }
}

fn hwnd_of(target: &Target) -> HWND {
    HWND(target.hwnd as *mut core::ffi::c_void)
}

/// Resolve a process id to the lowercased, ".exe"-stripped name [`capture`]
/// hands out on `Target::app`.
fn process_name(pid: u32) -> Option<String> {
    if pid == 0 {
        return None;
    }
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut buf = [0u16; 512];
        let mut len = buf.len() as u32;
        let result = QueryFullProcessImageNameW(
            handle,
            PROCESS_NAME_WIN32,
            windows::core::PWSTR(buf.as_mut_ptr()),
            &mut len,
        );
        let _ = windows::Win32::Foundation::CloseHandle(handle);
        result.ok()?;
        let path = String::from_utf16_lossy(&buf[..len as usize]);
        let stem = std::path::Path::new(&path).file_stem()?.to_string_lossy();
        Some(stem.to_lowercase())
    }
}

fn class_name(hwnd: HWND) -> String {
    let mut buf = [0u16; 256];
    let len = unsafe { GetClassNameW(hwnd, &mut buf) };
    if len <= 0 {
        return String::new();
    }
    String::from_utf16_lossy(&buf[..len as usize])
}

/// Capture the current foreground window as a [`Target`]: its handle, owning
/// process name, and window class. `None` when there is no foreground window
/// (nothing to target) or its process can't be queried.
pub fn capture() -> Option<Target> {
    unsafe {
        let hwnd = GetForegroundWindow();
        if hwnd.is_invalid() {
            return None;
        }
        let mut pid = 0u32;
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        Some(Target {
            hwnd: hwnd.0 as isize,
            pid,
            app: process_name(pid),
            class: class_name(hwnd),
        })
    }
}

/// The current foreground window's owning process id, or `None` when there
/// is no foreground window. Just `GetForegroundWindow` +
/// `GetWindowThreadProcessId` — no process handle, no name lookup — so a
/// caller that only needs "is this still the same process as `target`" can
/// answer that without paying for a full [`capture`].
pub fn current_foreground_pid() -> Option<u32> {
    unsafe {
        let hwnd = GetForegroundWindow();
        if hwnd.is_invalid() {
            return None;
        }
        let mut pid = 0u32;
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        Some(pid)
    }
}

/// Whether `target`'s exact window holds the foreground *right now*.
///
/// Not `current_foreground_pid() == Some(target.pid)`: the two answer
/// different questions, and this one is asked where the difference matters.
/// `routes::selection` calls it on both sides of a synthetic Ctrl+C, and a
/// second window of the same process is a different document — a pid match
/// there would let a copy read one window and a replacement land in another.
/// (`start_injection` asks the pid question deliberately, because for a
/// *paste* a sibling window of the same app is close enough to pick a chord.)
///
/// Answers the window handle without handing it out: `hwnd` stays private so
/// no caller can hold a raw handle past the life of the `Target`.
pub fn is_foreground(target: &Target) -> bool {
    unsafe { GetForegroundWindow() == hwnd_of(target) }
}

/// Console window classes (`GetClassNameW`), in alphabetical order, each
/// read off the program's own window and compared whole, ignoring ASCII
/// case. A classic console needs this check most: its window names the
/// program running in it (python, node, a build tool) as its process, so
/// only the class shows that it is a console.
const CONSOLE_CLASSES: &[&str] = &[
    "CASCADIA_HOSTING_WINDOW_CLASS", // Windows Terminal
    "ConsoleWindowClass",            // conhost, the classic console
    "mintty",                        // Git Bash, MSYS2 and Cygwin
];

/// Process stems (lowercased, no ".exe", the shape of [`Target::app`]) that
/// are terminals whatever window class they draw with.
///
/// Some emulators use a class too ordinary to list in [`CONSOLE_CLASSES`].
/// A terminal drawn by Chromium has a browser's window class, so its process
/// name is what identifies it. The whole stem has to match:
/// `powershell_ise` is an editor where Ctrl+C copies, and it must not match
/// `powershell`.
const TERMINAL_PROCESSES: &[&str] = &[
    // Windows Terminal, the Windows 11 default. It owns the foreground
    // window itself, so this one entry covers every shell, REPL and TUI run
    // inside it.
    "windowsterminal",
    // Shells in a classic conhost window (reported as the client, see above).
    "cmd",
    "powershell",
    "pwsh",
    "wsl",
    // Console hosts, for arrangements that surface the host itself.
    "conhost",
    "openconsole",
    // Common third-party emulators.
    "mintty", // Git Bash / MSYS2 / Cygwin
    "alacritty",
    "wezterm-gui",
    "conemu",
    "conemu64", // Cmder runs on ConEmu
    "tabby",
    "hyper",
    // Built on Chromium, so known only by the executable.
    "electerm", // electerm (app\electerm.exe in its Windows package)
];

/// Whether `target` is a terminal, where the ordinary paste and copy chords
/// mean something else. True when its window class is in
/// [`CONSOLE_CLASSES`] or its process stem is in [`TERMINAL_PROCESSES`],
/// each compared as a whole string. With no process name (`app: None`), only
/// the class can make it one.
pub fn is_terminal(target: &Target) -> bool {
    let by_class = CONSOLE_CLASSES
        .iter()
        .any(|class| class.eq_ignore_ascii_case(&target.class));
    let by_process = target
        .app
        .as_deref()
        .is_some_and(|app| TERMINAL_PROCESSES.contains(&app));
    by_class || by_process
}

/// This thread's input state joined to other threads' for the length of a
/// foreground switch. Dropping it undoes every join it made.
struct InputJoin {
    me: u32,
    joined: Vec<u32>,
}

impl InputJoin {
    /// Joins this thread to each of `threads`, skipping zero (no thread),
    /// this thread itself and repeats.
    fn new(threads: &[u32]) -> Self {
        let me = unsafe { GetCurrentThreadId() };
        let mut joined = Vec::with_capacity(threads.len());
        for &tid in threads {
            if tid == 0 || tid == me || joined.contains(&tid) {
                continue;
            }
            if unsafe { AttachThreadInput(me, tid, true) }.as_bool() {
                joined.push(tid);
            }
        }
        Self { me, joined }
    }
}

impl Drop for InputJoin {
    fn drop(&mut self) {
        for &tid in self.joined.iter().rev() {
            unsafe {
                let _ = AttachThreadInput(self.me, tid, false);
            }
        }
    }
}

/// Brings `target`'s window back to the front as the active, focused window,
/// and says whether it is the foreground window afterwards.
///
/// A missing or closed window is `false` at once, and a window already in
/// front is `true` at once; neither waits. A minimised window is restored
/// first. Windows lets a background process take the foreground only under
/// the conditions listed for `SetForegroundWindow`, and a dictation that
/// finishes while the user is elsewhere usually meets none of them. Joining
/// this thread's input to the target's thread and to the current foreground
/// thread for the length of the switch meets them, because the joined threads
/// share one activation state; every join is undone before the answer is
/// read.
///
/// The answer comes from asking Windows which window is in front afterwards,
/// never from what the calls returned. After a real switch it waits
/// [`SETTLE_AFTER_SWITCH_MS`] so the first synthetic key is not lost to the
/// activation. Callers paste whatever the answer.
pub fn restore_foreground(target: &Target) -> bool {
    let hwnd = hwnd_of(target);
    if hwnd.is_invalid() || !unsafe { IsWindow(Some(hwnd)) }.as_bool() {
        return false;
    }
    if is_foreground(target) {
        return true;
    }
    unsafe {
        if IsIconic(hwnd).as_bool() {
            let _ = ShowWindow(hwnd, SW_RESTORE);
        }
        let _join = InputJoin::new(&[
            GetWindowThreadProcessId(hwnd, None),
            GetWindowThreadProcessId(GetForegroundWindow(), None),
        ]);
        let _ = BringWindowToTop(hwnd);
        let _ = SetForegroundWindow(hwnd);
        let _ = SetFocus(Some(hwnd));
    }
    let in_front = is_foreground(target);
    if in_front {
        thread::sleep(Duration::from_millis(SETTLE_AFTER_SWITCH_MS));
    }
    in_front
}

/// A `Target` with no window behind it, for tests in other modules that need
/// one to pass around ([`is_terminal`]'s tables, `routes::selection`'s session
/// store). `hwnd: 0` is deliberate: nothing that touches a real window —
/// [`restore_foreground`], [`is_foreground`] — can be fooled by it.
#[cfg(test)]
pub fn test_target(app: &str, class: &str) -> Target {
    Target {
        hwnd: 0,
        pid: 0,
        app: Some(app.to_string()),
        class: class.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(app: &str, class: &str) -> Target {
        test_target(app, class)
    }

    /// A classic console window reports its client process (python, node, a
    /// build tool) rather than a terminal, so the class has to catch it; the
    /// same goes for the other listed classes.
    #[test]
    fn the_window_class_alone_can_mark_a_terminal() {
        assert!(is_terminal(&target("python", "ConsoleWindowClass")));
        assert!(is_terminal(&target("bash", "mintty")));
        assert!(is_terminal(&target("someshell", "CASCADIA_HOSTING_WINDOW_CLASS")));
    }

    /// Classes match whole and without regard to ASCII case, never as a
    /// substring.
    #[test]
    fn a_listed_class_matches_in_either_case_but_not_as_a_substring() {
        assert!(is_terminal(&target("python", "CONSOLEWINDOWCLASS")));
        assert!(is_terminal(&target("python", "consolewindowclass")));
        assert!(!is_terminal(&target("python", "ConsoleWindowClassHost")));
        assert!(!is_terminal(&target("python", "Console")));
    }

    /// A window handle of zero is no window at all: there is nothing to
    /// bring forward, so the answer is false.
    #[test]
    fn restoring_a_target_with_no_window_fails() {
        assert!(!restore_foreground(&target("notepad", "Notepad")));
    }

    /// Windows Terminal's own process stem, not just its class, must match —
    /// this app's original console list, folded in.
    #[test]
    fn a_known_terminal_process_is_a_terminal_even_with_an_ordinary_class() {
        assert!(is_terminal(&target("windowsterminal", "SomeOrdinaryClass")));
        assert!(is_terminal(&target("powershell", "ConsoleClassButNotListed")));
    }

    /// A terminal built on Chromium draws in the same window class as a
    /// browser or a chat app, so its executable decides, and the class on
    /// its own never makes a window a terminal.
    #[test]
    fn chromium_based_terminals_are_known_by_executable_name() {
        for app in ["tabby", "hyper", "electerm"] {
            assert!(is_terminal(&target(app, "Chrome_WidgetWin_1")), "{app}");
        }
        for app in ["slack", "discord", "code"] {
            assert!(!is_terminal(&target(app, "Chrome_WidgetWin_1")), "{app}");
        }
    }

    /// Matching is exact on the whole stem, never substring: a name that
    /// merely contains a known one ("powershell_ise" is a GUI editor where
    /// Ctrl+C is copy) must not be treated as a terminal.
    #[test]
    fn process_matching_is_exact_not_substring() {
        assert!(!is_terminal(&target("powershell_ise", "SomeWindow")));
    }

    #[test]
    fn an_ordinary_app_is_not_a_terminal() {
        assert!(!is_terminal(&target("notepad", "Notepad")));
        assert!(!is_terminal(&target("chrome", "Chrome_WidgetWin_1")));
    }

    /// Unknown process (`app: None`, e.g. `OpenProcess` failed) falls through
    /// to "not a terminal" rather than panicking or matching everything.
    #[test]
    fn unknown_process_name_is_not_a_terminal() {
        let t = Target {
            hwnd: 0,
            pid: 0,
            app: None,
            class: "SomeWindow".to_string(),
        };
        assert!(!is_terminal(&t));
    }
}
