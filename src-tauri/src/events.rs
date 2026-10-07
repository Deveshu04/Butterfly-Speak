//! Event names + payloads shared with the frontend. Keep in sync with
//! `src/lib/events.ts`.

pub const STATE_CHANGED: &str = "state://changed";
/// Short status line for the pill ("Polish…"), cleared with an empty string.
/// Never carries transcript text: the pill deliberately shows no live
/// transcription, so a half-decoded phrase can't make the user second-guess
/// what they just said.
pub const OVERLAY_STATUS: &str = "overlay://status";
pub const TRANSCRIPT_FINAL: &str = "transcript://final";
pub const LEVEL: &str = "overlay://level";
pub const NOTICE_ERROR: &str = "notice://error";
pub const HOTKEY_CAPTURE: &str = "hotkey://capture";
pub const NAVIGATE: &str = "nav://page";
/// The backend changed the settings file on its own initiative, so any open
/// window is holding a stale copy and must re-read before it writes again.
///
/// Emitted only for writes the user did not ask for — today that means
/// `learn::candidates` promoting a correction to a `Replacement`. Writes the
/// frontend initiates do not need it: the caller already knows, and
/// `Dictionary.svelte`'s undo path reloads for itself.
///
/// No payload. A window that hears this reloads the whole object rather than
/// patching a field, because the point is that it does not know what changed.
pub const SETTINGS_CHANGED: &str = "settings://changed";

/// Updater state, re-emitted on every transition; `update_status` returns
/// the same shape. Payload is `updater::UpdateState` (serde-tagged on `kind`).
pub const UPDATE_STATE: &str = "update://state";

/// A sign-in finished, failed, or was signed out. Payload is
/// `auth::session::CloudStatus` — the same shape `cloud_status` returns, so a
/// listener never has to ask again.
///
/// No `://` prefix, unlike every name above: the sign-in flow is full of
/// real URLs, and a scheme separator in an event name would read as one
/// more.
pub const CLOUD_AUTH_CHANGED: &str = "cloud-auth-changed";

#[derive(Clone, serde::Serialize)]
pub struct StatePayload {
    pub state: &'static str,
    /// "pushToTalk" | "handsFree" while recording, otherwise None.
    pub mode: Option<&'static str>,
}

#[derive(Clone, serde::Serialize)]
pub struct StatusPayload {
    pub text: String,
}

#[derive(Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FinalPayload {
    pub text: String,
    /// Length of the captured audio, for words-per-minute stats.
    pub duration_ms: u64,
    /// The program the text was pasted into, by its lowercased executable
    /// name without ".exe" (`notepad`), for the per-app usage figures.
    pub app: Option<String>,
    /// Word-level edits made by cleanup + polish (fixes insights).
    pub words_corrected: u32,
    /// Dictionary rules + snippet expansions that fired.
    pub dict_fixes: u32,
}

#[derive(Clone, serde::Serialize)]
pub struct LevelPayload {
    pub level: f32,
}

#[derive(Clone, serde::Serialize)]
pub struct NoticePayload {
    pub message: String,
}

#[derive(Clone, serde::Serialize)]
pub struct HotkeyCapturePayload {
    pub keys: String,
    pub done: bool,
    /// Why a press was ignored, when it was. The recorder keeps listening —
    /// the dialog shows this instead of appearing frozen, which is what a
    /// silent drop looked like to the user.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hint: Option<&'static str>,
}

impl HotkeyCapturePayload {
    pub fn live(keys: String) -> Self {
        Self { keys, done: false, hint: None }
    }

    pub fn done(keys: String) -> Self {
        Self { keys, done: true, hint: None }
    }

    pub fn hint(message: &'static str) -> Self {
        Self { keys: String::new(), done: false, hint: Some(message) }
    }
}

#[derive(Clone, serde::Serialize)]
pub struct NavigatePayload {
    pub page: &'static str,
}

/// The whole import queue, re-emitted on every state change.
///
/// A snapshot rather than a per-item delta: the queue is at most a few dozen
/// rows, and a delta stream has to be reassembled correctly by a page that may
/// have mounted halfway through a run. `import_status` returns this same shape
/// for exactly that case.
pub const IMPORT_PROGRESS: &str = "import://progress";

/// Whether files are currently being dragged over the main window.
///
/// The drag-and-drop *paths* never reach the webview — `lib.rs` handles
/// `WindowEvent::DragDrop` and enqueues in Rust (see `import`'s module doc for
/// why). This event carries only the highlight state, so the drop target can
/// light up without the page ever learning what is being dropped.
pub const IMPORT_DROP_HOVER: &str = "import://drop-hover";

/// One row of the import queue.
///
/// `name` is the file's own name and never its directory — the queue has to
/// say which recording it is working on. It is emitted, never logged; see
/// `import`'s module doc for the split.
#[derive(Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportItemPayload {
    pub id: u64,
    pub name: String,
    /// `crate::import::ItemState::as_str`.
    pub state: &'static str,
    /// A finished sentence, set only alongside `state: "failed"`.
    pub error: Option<String>,
    /// A short remark about the file that is not a failure — today only
    /// "Length unknown", when the container states no duration.
    ///
    /// It matters enough to show because two things follow from it: the
    /// duration ceiling could not be applied to this file at all, and the job
    /// gets the 30-minute wait ceiling rather than a length-scaled deadline.
    /// A user watching an unexpectedly long import deserves to know which of
    /// their files the app could not measure.
    pub detail: Option<&'static str>,
    /// The note this import became, once it has become one.
    ///
    /// A known limit: the Import page carries this and does nothing with it.
    /// A finished row offers no way to open its note, which is found in
    /// Notes.
    pub note_id: Option<i64>,
}

/// Counts and percent — never bytes, never a timer, and never any part of a
/// transcript.
///
/// `percent` counts finished rows of every outcome (done, failed and
/// cancelled alike), so a finished run always shows 100%. It changes only
/// when a row finishes; nothing is estimated or animated in between.
#[derive(Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportProgressPayload {
    pub running: bool,
    pub total: u32,
    pub done: u32,
    pub failed: u32,
    pub cancelled: u32,
    pub percent: u32,
    pub items: Vec<ImportItemPayload>,
}

#[derive(Clone, serde::Serialize)]
pub struct ImportDropHoverPayload {
    pub over: bool,
}
