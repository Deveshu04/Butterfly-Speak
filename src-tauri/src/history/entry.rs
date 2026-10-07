//! The row shapes `history` reads and writes. Kept separate from the SQL
//! (`store.rs`) and the thread plumbing (`mod.rs`) so the wire/DB shape is
//! easy to scan on its own.

use serde::Serialize;

/// How a filed dictation ended, stored in the `outcome` column. The table's
/// `CHECK` admits these two values and nothing else.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Outcome {
    /// The text was delivered. `error_code` may still say it stopped short.
    #[default]
    Done,
    /// Nothing was typed; `error_code` says why.
    Failed,
}

impl Outcome {
    pub(super) fn as_sql(self) -> &'static str {
        match self {
            Outcome::Done => "done",
            Outcome::Failed => "failed",
        }
    }

    pub(super) fn from_sql(s: &str) -> Self {
        match s {
            "failed" => Outcome::Failed,
            _ => Outcome::Done,
        }
    }
}

/// What the caller hands `Recorder::record` for a new row. `created_at` is
/// deliberately not here — the DB assigns it (`datetime('now')`) so every
/// row's clock reference is the same one, not whatever each caller's
/// `Instant`/`SystemTime` happened to read.
#[derive(Clone, Debug, Default)]
pub struct NewEntry {
    pub text: String,
    /// The verbatim transcript before rule cleanup or AI formatting: the text
    /// Undo AI Edit puts back. `None` and `Some` equal to `text` both mean
    /// nothing changed it, and History's detail view shows no before and
    /// after for either.
    pub raw_text: Option<String>,
    pub outcome: Outcome,
    pub error_code: Option<String>,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub duration_ms: Option<u64>,
    /// Lowercased, ".exe"-stripped process name (`foreground::Target::app`).
    pub app: Option<String>,
    pub words: Option<u32>,
    /// Which route produced this row, so a later retry can take the same
    /// one. `None` for a plain dictation.
    pub route: Option<String>,
}

/// A stored row, as handed back to Tauri commands (and the webview).
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Entry {
    pub id: i64,
    pub text: String,
    pub raw_text: Option<String>,
    /// `datetime('now')`-formatted UTC string ("YYYY-MM-DD HH:MM:SS").
    pub created_at: String,
    pub outcome: Outcome,
    pub error_code: Option<String>,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub duration_ms: Option<i64>,
    pub app: Option<String>,
    pub words: Option<i64>,
    pub route: Option<String>,
}
