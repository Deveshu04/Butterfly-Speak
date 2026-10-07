//! Notes and folders: storage, listing, paging and full-text search over the
//! tables [`schema`] creates. A note is a document the user typed, dictated or
//! imported; a folder is one flat level of grouping, and a note may have none.
//!
//! ## Where the tables live, and why
//!
//! In `history.db`, alongside `transcriptions` and `learn_candidates`, reached
//! through [`crate::history::Recorder::with_connection`]. `rusqlite::Connection`
//! is `!Sync`, so one thread owns it and everyone else talks to that thread
//! over a channel; opening a *second* database would mean a second such thread
//! for no gain, and would make a future "attach this note to that dictation"
//! a cross-database join. This is the door `learn::candidates` already comes
//! through, and the second module to use it.
//!
//! Two obligations come with that door, both binding on every caller:
//!
//! - **`None` is not an error.** `with_connection` answers `None` when the DB
//!   thread is gone (the file failed to open). Callers must read that as
//!   "notes are off this session", never surface it as a failure.
//! - **LOCK ORDERING.** No caller may hold the settings `RwLock` — read *or*
//!   write — across a `with_connection` call. Nothing in this module takes
//!   that lock, and nothing in it should start to; the Tauri commands in
//!   `commands.rs` take no settings guard either.
//!
//! ## Notes are not dictation history
//!
//! Retention does not reach them, deliberately. `history::store::sweep_expired`
//! names `transcriptions` and only `transcriptions`, and `Settings.history.enabled`
//! gates `Cmd::Record` and only `Cmd::Record` — a note written through
//! `Cmd::Task` is unaffected by both. A note is a document the user wrote and
//! expects to still be there; a transcription is a byproduct of dictating into
//! somebody else's window. Two tests below hold that line.
//!
//! ## Privacy
//!
//! Never log note, title or transcript text. Every `tracing` call reachable
//! from here carries counts, ids and errors only — the same rule
//! `history`'s module doc states for transcripts.
//!
//! ## Deletes are final
//!
//! Deleting a note or a folder removes the rows, and the search index loses
//! them in the same statement. Nothing keeps a deleted note for later: the
//! file on this PC is the only copy there is.

pub mod actions;
pub mod export;
pub mod mirror;
pub mod schema;
pub mod search;
pub mod title;

use rusqlite::types::Value;
use rusqlite::{params, params_from_iter, Connection, OptionalExtension, Row};
use serde::{Deserialize, Deserializer, Serialize};

/// Notes per page of [`list_notes`].
pub const PAGE_SIZE: u32 = 30;

/// How many results [`search_notes`] returns when the caller names no limit.
/// Search answers in one response with no "load more", so it returns two list
/// pages' worth; past that, typing another word finds a note faster than
/// scrolling.
pub const SEARCH_LIMIT: u32 = 2 * PAGE_SIZE;

/// Which folder a listing is scoped to.
///
/// Three states, and the third is the one a plain `Option<i64>` cannot say:
/// `None` is every note, `Some(None)` is the unfiled bucket (`folder_id IS
/// NULL`), `Some(Some(id))` is one folder. Over the Tauri boundary the field
/// is absent, `null`, or a number respectively — see [`double_option`].
pub type FolderFilter = Option<Option<i64>>;

/// Serde helper telling "the field was absent" apart from "the field was
/// explicitly `null`".
///
/// Without it there is no way for the webview to say *unfile this note* or
/// *list the unfiled ones*: `Option<T>` collapses both onto `None`. With
/// `#[serde(default, deserialize_with = "double_option")]`, an absent field is
/// `None` and a `null` is `Some(None)`.
fn double_option<'de, T, D>(de: D) -> Result<Option<Option<T>>, D::Error>
where
    T: Deserialize<'de>,
    D: Deserializer<'de>,
{
    Option::<T>::deserialize(de).map(Some)
}

/// A stored note, as the webview sees it.
///
/// `transcript_text` is deliberately absent: it is index fodder derived from
/// `transcript_json`, and shipping both would invite a caller to edit the
/// wrong one.
#[derive(Clone, Debug, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Note {
    pub id: i64,
    pub folder_id: Option<i64>,
    /// `"written"` or `"imported"`.
    pub kind: String,
    pub title: String,
    pub content: String,
    /// The last note action's result, shown beside `content`.
    pub polished_body: Option<String>,
    /// The action prompt that produced `polished_body`.
    pub polish_prompt: Option<String>,
    /// `actions::content_hash` of the content `polished_body` was made from,
    /// so the editor can tell when the content has moved on since.
    pub polished_from_hash: Option<String>,
    /// An import's transcript segments, stored as the JSON it produced.
    pub transcript_json: Option<String>,
    /// The name of the recording an import was made from.
    pub imported_file: Option<String>,
    /// That recording's length.
    pub audio_seconds: Option<f64>,
    /// Epoch **milliseconds** — `new Date(createdAt)` on the webview side,
    /// with none of the offset-naive un-doing `History.svelte` has to do for
    /// `transcriptions.created_at`.
    pub created_at: i64,
    pub updated_at: i64,
}

/// A folder, as the webview sees it.
#[derive(Clone, Debug, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Folder {
    pub id: i64,
    pub name: String,
    pub sort_order: i64,
    pub created_at: i64,
    pub updated_at: i64,
    /// Notes filed in this folder, counted when the row is read.
    pub note_count: i64,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct NewNote {
    pub folder_id: Option<i64>,
    /// `"written"` (the default) or `"imported"`; anything else is rejected
    /// by the table's CHECK.
    pub kind: Option<String>,
    pub title: Option<String>,
    pub content: Option<String>,
    pub transcript_json: Option<String>,
    pub imported_file: Option<String>,
    pub audio_seconds: Option<f64>,
    /// Epoch-millisecond timestamps an import carries over from its source.
    /// An absent `created_at` means now, and an absent `updated_at` means
    /// "as created", so a caller only has to say the one it knows.
    ///
    /// Deliberately *not* in [`NoteUpdate`]: an import states when a note was
    /// written, an edit never gets to rewrite it.
    pub created_at: Option<i64>,
    pub updated_at: Option<i64>,
}

/// A partial edit to a note, as the webview sends it to [`update_note`].
///
/// Every field is "absent means leave it alone". The four nullable ones use
/// [`double_option`], so `null` means *clear this column* and is distinct from
/// not mentioning it.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct NoteUpdate {
    pub title: Option<String>,
    pub content: Option<String>,
    #[serde(deserialize_with = "double_option")]
    pub polished_body: Option<Option<String>>,
    #[serde(deserialize_with = "double_option")]
    pub polish_prompt: Option<Option<String>>,
    #[serde(deserialize_with = "double_option")]
    pub polished_from_hash: Option<Option<String>>,
    #[serde(deserialize_with = "double_option")]
    pub folder_id: Option<Option<i64>>,
}

/// What [`list_notes`] is asked for over the Tauri boundary. A struct rather
/// than two parameters because `folder`'s three-state shape needs a serde
/// attribute, and Tauri command parameters cannot carry one.
#[derive(Clone, Copy, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ListNotesArgs {
    #[serde(deserialize_with = "double_option")]
    pub folder: FolderFilter,
    pub page: u32,
}

const COLUMNS: &str = "id, folder_id, kind, title, content, polished_body, \
                       polish_prompt, polished_from_hash, transcript_json, \
                       imported_file, audio_seconds, created_at, updated_at";

fn row_to_note(row: &Row) -> rusqlite::Result<Note> {
    Ok(Note {
        id: row.get("id")?,
        folder_id: row.get("folder_id")?,
        kind: row.get("kind")?,
        title: row.get("title")?,
        content: row.get("content")?,
        polished_body: row.get("polished_body")?,
        polish_prompt: row.get("polish_prompt")?,
        polished_from_hash: row.get("polished_from_hash")?,
        transcript_json: row.get("transcript_json")?,
        imported_file: row.get("imported_file")?,
        audio_seconds: row.get("audio_seconds")?,
        created_at: row.get("created_at")?,
        updated_at: row.get("updated_at")?,
    })
}

fn row_to_folder(row: &Row) -> rusqlite::Result<Folder> {
    Ok(Folder {
        id: row.get("id")?,
        name: row.get("name")?,
        sort_order: row.get("sort_order")?,
        created_at: row.get("created_at")?,
        updated_at: row.get("updated_at")?,
        note_count: row.get("note_count")?,
    })
}

// ---------------------------------------------------------------------------
// Which text *is* the note.
// ---------------------------------------------------------------------------

/// The text that is the note: the polished body when there is a non-empty
/// one, otherwise the content. Export and the mirror both read the note
/// through this, so a file always holds the version the editor shows.
pub fn body_source(note: &Note) -> &str {
    note.polished_body
        .as_deref()
        .filter(|s| !s.is_empty())
        .unwrap_or(&note.content)
}

// ---------------------------------------------------------------------------
// The derived transcript column.
// ---------------------------------------------------------------------------

/// Flatten an import's transcript JSON into the text FTS5 indexes.
///
/// The obvious spelling — derive it in SQL so no writer can forget — does not
/// survive contact with FTS5: an external-content table whose `content=` is a
/// view using `json_each` cannot be rebuilt, because FTS5 prepares its content
/// query with `SQLITE_PREPARE_NO_VTAB` and `json_each` is a virtual table
/// (measured; see `schema.rs`). So the derivation lives here, and the
/// invariant "`transcript_json` and `transcript_text` are written by the same
/// statement" is enforced by there being exactly two writers —
/// [`create_note`] and [`set_transcript`] — and `transcript_json` being absent
/// from [`NoteUpdate`].
///
/// Tolerant about shape: a top-level array of `{"text": …}` objects, an array
/// of bare strings, or an object wrapping either under `"segments"` all work.
/// Anything it cannot read — including malformed JSON — yields `None` rather
/// than an error: a transcript that does not index is a smaller problem than
/// an import that refuses to save.
pub fn transcript_text(transcript_json: Option<&str>) -> Option<String> {
    let raw = transcript_json?;
    let parsed: serde_json::Value = serde_json::from_str(raw).ok()?;
    let segments = match &parsed {
        serde_json::Value::Array(items) => items.as_slice(),
        serde_json::Value::Object(map) => map.get("segments")?.as_array()?.as_slice(),
        _ => return None,
    };
    let joined = segments
        .iter()
        .filter_map(|seg| match seg {
            serde_json::Value::String(s) => Some(s.as_str()),
            serde_json::Value::Object(map) => map.get("text").and_then(|t| t.as_str()),
            _ => None,
        })
        .filter(|s| !s.trim().is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    (!joined.is_empty()).then_some(joined)
}

// ---------------------------------------------------------------------------
// Notes.
// ---------------------------------------------------------------------------

pub fn create_note(conn: &Connection, new: &NewNote) -> rusqlite::Result<i64> {
    conn.execute(
        &format!(
            "INSERT INTO notes
                (folder_id, kind, title, content, transcript_json, transcript_text,
                 imported_file, audio_seconds, created_at, updated_at)
             VALUES (?1, COALESCE(?2, 'written'), COALESCE(?3, ''), COALESCE(?4, ''), ?5, ?6, ?7, ?8,
                     COALESCE(?9, {NOW_MS}), COALESCE(?10, ?9, {NOW_MS}))"
        ),
        params![
            new.folder_id,
            new.kind,
            new.title,
            new.content,
            new.transcript_json,
            transcript_text(new.transcript_json.as_deref()),
            new.imported_file,
            new.audio_seconds,
            new.created_at,
            new.updated_at,
        ],
    )?;
    Ok(conn.last_insert_rowid())
}

pub fn get_note(conn: &Connection, id: i64) -> rusqlite::Result<Option<Note>> {
    conn.query_row(
        &format!("SELECT {COLUMNS} FROM notes WHERE id = ?1"),
        params![id],
        row_to_note,
    )
    .optional()
}

/// The `SET` fragment every note write appends: `updated_at` becomes now, in
/// epoch milliseconds. Writers set it themselves because a trigger on `notes`
/// that writes to `notes` corrupts the FTS index (see [`schema`]).
const TOUCH: &str = "updated_at = CAST(unixepoch('now','subsec') * 1000 AS INTEGER)";

/// The same clock as an expression, for [`create_note`]'s `COALESCE` fallback.
/// `schema.rs` spells it a third time in the two column defaults;
/// `touch_and_the_insert_default_read_the_same_clock` fails if these two drift.
const NOW_MS: &str = "CAST(unixepoch('now','subsec') * 1000 AS INTEGER)";

/// Apply the fields `update` carries. Returns whether a row changed.
///
/// An update naming no fields at all is `Ok(false)`, not an error — it is what
/// a UI sends when a debounced save fires with nothing dirty, and it must not
/// bump `updated_at` and reorder the note list for a no-op.
pub fn update_note(conn: &Connection, id: i64, update: &NoteUpdate) -> rusqlite::Result<bool> {
    let mut fields: Vec<&str> = Vec::new();
    let mut values: Vec<Value> = Vec::new();

    // The two NOT NULL text columns: absent leaves the column alone, and
    // there is no way to ask for NULL because the column forbids it.
    for (field, v) in [("title = ?", &update.title), ("content = ?", &update.content)] {
        if let Some(s) = v {
            fields.push(field);
            values.push(Value::Text(s.clone()));
        }
    }

    // The nullable ones: absent leaves the column alone, an explicit `null`
    // clears it. That is what `double_option` bought.
    for (field, v) in [
        ("polished_body = ?", &update.polished_body),
        ("polish_prompt = ?", &update.polish_prompt),
        ("polished_from_hash = ?", &update.polished_from_hash),
    ] {
        if let Some(inner) = v {
            fields.push(field);
            values.push(match inner {
                Some(s) => Value::Text(s.clone()),
                None => Value::Null,
            });
        }
    }
    if let Some(inner) = update.folder_id {
        fields.push("folder_id = ?");
        values.push(match inner {
            Some(f) => Value::Integer(f),
            None => Value::Null,
        });
    }

    if fields.is_empty() {
        return Ok(false);
    }
    fields.push(TOUCH);
    values.push(Value::Integer(id));
    let sql = format!("UPDATE notes SET {} WHERE id = ?", fields.join(", "));
    Ok(conn.execute(&sql, params_from_iter(values))? > 0)
}

/// The other half of the `transcript_json` / `transcript_text` invariant. Kept
/// out of [`NoteUpdate`] so the two columns can only ever be written together.
///
/// No command calls it today: an import stores its transcript in the same
/// [`create_note`] that stores the note.
///
/// So this stays for the writer that genuinely needs it — re-transcribing, or
/// attaching a transcript to a note that already exists. Remove the allow
/// then. It is deliberately not deleted: it is the only spelling that keeps
/// `transcript_json` and `transcript_text` moving together, and the next
/// caller to need one would otherwise hand-roll an `UPDATE` that moves just one.
#[allow(dead_code)]
pub fn set_transcript(
    conn: &Connection,
    id: i64,
    transcript_json: Option<&str>,
) -> rusqlite::Result<bool> {
    let changed = conn.execute(
        &format!(
            "UPDATE notes SET transcript_json = ?1, transcript_text = ?2, {TOUCH} WHERE id = ?3"
        ),
        params![transcript_json, transcript_text(transcript_json), id],
    )?;
    Ok(changed > 0)
}

/// Hard delete (see the module doc). `notes_index_remove` takes the row out of
/// the search index as a side effect, and the checkpoint takes its words out
/// of the file (`history::store`'s module doc).
pub fn delete_note(conn: &Connection, id: i64) -> rusqlite::Result<bool> {
    let removed = conn.execute("DELETE FROM notes WHERE id = ?1", params![id])? > 0;
    if removed {
        crate::history::scrub_removed_text(conn);
    }
    Ok(removed)
}

/// Most-recently-edited-first page.
pub fn list_notes(
    conn: &Connection,
    folder: FolderFilter,
    page: u32,
) -> rusqlite::Result<Vec<Note>> {
    let page_size = i64::from(PAGE_SIZE);
    let offset = i64::from(page) * page_size;
    let (scope, scope_param): (&str, Option<i64>) = match folder {
        None => ("", None),
        Some(None) => ("WHERE folder_id IS NULL", None),
        Some(Some(id)) => ("WHERE folder_id = ?3", Some(id)),
    };
    // `id DESC` breaks the tie the millisecond clock can still produce for two
    // notes created in the same instant, so a page boundary can't drop or
    // duplicate a row.
    let sql = format!(
        "SELECT {COLUMNS} FROM notes {scope}
         ORDER BY updated_at DESC, id DESC
         LIMIT ?1 OFFSET ?2"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = match scope_param {
        Some(id) => stmt.query_map(params![page_size, offset, id], row_to_note)?,
        None => stmt.query_map(params![page_size, offset], row_to_note)?,
    };
    rows.collect()
}

/// Full-text search over title, body, polished body and transcript.
///
/// Empty result (not an error) for a query with no searchable tokens, so an
/// all-punctuation search box shows nothing rather than a SQL error.
pub fn search_notes(conn: &Connection, raw_query: &str, limit: u32) -> rusqlite::Result<Vec<Note>> {
    let fts_query = search::sanitize_query(raw_query);
    if fts_query.is_empty() {
        return Ok(Vec::new());
    }
    let limit = limit.clamp(1, 500) as i64;
    // Deliberately unaliased, for the reason `history::store::search` states:
    // `tbl MATCH expr` / `bm25(tbl)` only resolve against the FTS5 table's own
    // name, and aliasing it raised "no such column" against this SQLite.
    let sql = format!(
        "SELECT {cols} FROM notes_fts
         JOIN notes n ON n.id = notes_fts.rowid
         WHERE notes_fts MATCH ?1
         ORDER BY bm25(notes_fts) ASC
         LIMIT ?2",
        cols = COLUMNS
            .split(", ")
            .map(|c| format!("n.{}", c.trim()))
            .collect::<Vec<_>>()
            .join(", "),
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params![fts_query, limit], row_to_note)?;
    rows.collect()
}

// ---------------------------------------------------------------------------
// Folders.
// ---------------------------------------------------------------------------

const FOLDER_COLUMNS: &str = "f.id AS id, f.name AS name, f.sort_order AS sort_order, \
     f.created_at AS created_at, f.updated_at AS updated_at, \
     (SELECT COUNT(*) FROM notes n WHERE n.folder_id = f.id) AS note_count";

fn folder_by_id(conn: &Connection, id: i64) -> rusqlite::Result<Option<Folder>> {
    conn.query_row(
        &format!("SELECT {FOLDER_COLUMNS} FROM folders f WHERE f.id = ?1"),
        params![id],
        row_to_folder,
    )
    .optional()
}

pub fn list_folders(conn: &Connection) -> rusqlite::Result<Vec<Folder>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {FOLDER_COLUMNS} FROM folders f ORDER BY f.sort_order ASC, f.id ASC"
    ))?;
    let rows = stmt.query_map([], row_to_folder)?;
    rows.collect()
}

/// Refusals the folder editor shows word for word.
const FOLDER_NAME_EMPTY: &str = "Give the folder a name.";
const FOLDER_NAME_TAKEN: &str = "You already have a folder called that. Pick another name.";
const FOLDER_GONE: &str = "This folder no longer exists. Choose another one.";

/// Create a folder at the end of the list. The name is trimmed, and a name
/// that is empty, or that matches an existing folder ignoring ASCII case, is
/// refused.
pub fn create_folder(conn: &Connection, name: &str) -> anyhow::Result<Folder> {
    let name = usable_folder_name(conn, name, None)?;
    let inserted = conn.execute(
        "INSERT INTO folders (name, sort_order)
         VALUES (?1, (SELECT COALESCE(MAX(sort_order), -1) + 1 FROM folders))",
        params![name],
    );
    refuse_duplicate(inserted)?;
    let id = conn.last_insert_rowid();
    folder_by_id(conn, id)?.ok_or_else(|| anyhow::anyhow!(FOLDER_GONE))
}

/// Rename a folder, under the same rules as [`create_folder`]. A folder may
/// keep its own name or change only its case.
pub fn rename_folder(conn: &Connection, id: i64, name: &str) -> anyhow::Result<Folder> {
    let name = usable_folder_name(conn, name, Some(id))?;
    if folder_by_id(conn, id)?.is_none() {
        anyhow::bail!(FOLDER_GONE);
    }
    let renamed = conn.execute(
        &format!("UPDATE folders SET name = ?1, {TOUCH} WHERE id = ?2"),
        params![name, id],
    );
    refuse_duplicate(renamed)?;
    folder_by_id(conn, id)?.ok_or_else(|| anyhow::anyhow!(FOLDER_GONE))
}

/// Delete a folder and every note in it, in one transaction, and return the
/// ids of the notes that went with it. The notes are deleted here rather than
/// left to the schema's cascade, so the ids are known and the delete does not
/// depend on `PRAGMA foreign_keys` (the bundled SQLite turns it on for every
/// connection; a build without that default would skip the cascade).
pub fn delete_folder(conn: &Connection, id: i64) -> anyhow::Result<Vec<i64>> {
    let tx = conn.unchecked_transaction()?;
    if folder_by_id(&tx, id)?.is_none() {
        anyhow::bail!(FOLDER_GONE);
    }
    let note_ids = tx
        .prepare("SELECT id FROM notes WHERE folder_id = ?1 ORDER BY id")?
        .query_map(params![id], |r| r.get::<_, i64>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    tx.execute("DELETE FROM notes WHERE folder_id = ?1", params![id])?;
    tx.execute("DELETE FROM folders WHERE id = ?1", params![id])?;
    tx.commit()?;
    if !note_ids.is_empty() {
        crate::history::scrub_removed_text(conn);
    }
    Ok(note_ids)
}

/// `name` trimmed, if a folder may have it: it is not empty, and no other
/// folder (`except` aside) has it, ignoring ASCII case as the column does.
fn usable_folder_name<'a>(
    conn: &Connection,
    name: &'a str,
    except: Option<i64>,
) -> anyhow::Result<&'a str> {
    let name = name.trim();
    if name.is_empty() {
        anyhow::bail!(FOLDER_NAME_EMPTY);
    }
    let taken: bool = conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM folders WHERE name = ?1 COLLATE NOCASE AND id IS NOT ?2)",
        params![name, except],
        |r| r.get(0),
    )?;
    if taken {
        anyhow::bail!(FOLDER_NAME_TAKEN);
    }
    Ok(name)
}

/// Turn the `UNIQUE` column's refusal, which a concurrent write can still
/// reach after the check above, into the same sentence the check gives.
fn refuse_duplicate(written: rusqlite::Result<usize>) -> anyhow::Result<()> {
    match written {
        Ok(_) => Ok(()),
        Err(rusqlite::Error::SqliteFailure(e, _))
            if e.code == rusqlite::ErrorCode::ConstraintViolation =>
        {
            anyhow::bail!(FOLDER_NAME_TAKEN)
        }
        Err(e) => Err(e.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::history::{NewEntry, RetentionCfg};
    use std::path::PathBuf;

    fn db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(schema::SCHEMA).unwrap();
        conn
    }

    fn note(title: &str, content: &str) -> NewNote {
        NewNote {
            title: Some(title.into()),
            content: Some(content.into()),
            ..Default::default()
        }
    }

    fn ids(notes: &[Note]) -> Vec<i64> {
        notes.iter().map(|n| n.id).collect()
    }

    // -----------------------------------------------------------------------
    // CRUD.
    // -----------------------------------------------------------------------

    #[test]
    fn create_then_get_round_trips_with_defaults() {
        let conn = db();
        let id = create_note(&conn, &NewNote::default()).unwrap();
        let n = get_note(&conn, id).unwrap().expect("the note exists");
        assert_eq!(n.id, id);
        assert_eq!(n.kind, "written");
        assert_eq!(n.title, "");
        assert_eq!(n.content, "");
        assert_eq!(n.folder_id, None, "a note with no folder is unfiled");
        assert!(n.polished_body.is_none());
        assert!(n.created_at > 0 && n.updated_at > 0);
    }

    /// An import copies older notes across in one go. Each arrives as a new
    /// entry and must keep the day each was written — otherwise three weeks of
    /// notes collapse onto the migration's own timestamp and the list, which
    /// orders by `updated_at DESC`, tells the user a flat lie.
    #[test]
    fn create_can_carry_an_imported_timestamp() {
        let conn = db();
        let when = 1_600_000_000_000; // 2020-09-13, long before any test run
        let id = create_note(
            &conn,
            &NewNote {
                content: Some("from the scratchpad".into()),
                created_at: Some(when),
                updated_at: Some(when),
                ..Default::default()
            },
        )
        .unwrap();
        let n = get_note(&conn, id).unwrap().unwrap();
        assert_eq!(n.created_at, when);
        assert_eq!(n.updated_at, when);
    }

    /// `updated_at` alone is what an importer usually knows; `created_at`
    /// follows it rather than jumping to now, which would leave a note created
    /// after it was last edited.
    #[test]
    fn an_imported_created_at_stands_in_for_a_missing_updated_at() {
        let conn = db();
        let when = 1_600_000_000_000;
        let id = create_note(
            &conn,
            &NewNote {
                created_at: Some(when),
                ..Default::default()
            },
        )
        .unwrap();
        let n = get_note(&conn, id).unwrap().unwrap();
        assert_eq!(n.created_at, when);
        assert_eq!(
            n.updated_at, when,
            "updated_at falls back to created_at, not to now"
        );
    }

    /// Two spellings of one clock, in two statements that have to agree, in a
    /// file where a silent disagreement would look like a 1970 timestamp.
    #[test]
    fn touch_and_the_insert_default_read_the_same_clock() {
        assert_eq!(TOUCH, format!("updated_at = {NOW_MS}"));
    }

    #[test]
    fn get_of_a_missing_note_is_none_not_an_error() {
        let conn = db();
        assert!(get_note(&conn, 404).unwrap().is_none());
    }

    #[test]
    fn update_writes_only_the_allow_listed_fields_it_was_given() {
        let conn = db();
        let id = create_note(&conn, &note("Original", "body")).unwrap();
        let changed = update_note(
            &conn,
            id,
            &NoteUpdate {
                content: Some("edited body".into()),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(changed);
        let n = get_note(&conn, id).unwrap().unwrap();
        assert_eq!(n.content, "edited body");
        assert_eq!(n.title, "Original", "an unnamed field is left alone");
    }

    #[test]
    fn an_update_naming_nothing_changes_nothing_and_is_not_an_error() {
        let conn = db();
        let id = create_note(&conn, &note("Kept", "body")).unwrap();
        assert!(!update_note(&conn, id, &NoteUpdate::default()).unwrap());
        assert_eq!(get_note(&conn, id).unwrap().unwrap().title, "Kept");
    }

    /// The reason [`NoteUpdate`]'s nullable fields are `Option<Option<_>>`:
    /// clearing a column and not mentioning it are different requests.
    #[test]
    fn an_explicit_null_clears_a_column_and_an_absent_field_does_not() {
        let conn = db();
        let id = create_note(&conn, &note("t", "b")).unwrap();
        update_note(
            &conn,
            id,
            &NoteUpdate {
                polished_body: Some(Some("polished".into())),
                folder_id: None,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(
            get_note(&conn, id).unwrap().unwrap().polished_body.as_deref(),
            Some("polished")
        );

        // Absent: untouched.
        update_note(&conn, id, &NoteUpdate { title: Some("t2".into()), ..Default::default() })
            .unwrap();
        assert!(get_note(&conn, id).unwrap().unwrap().polished_body.is_some());

        // Explicit null: cleared.
        update_note(
            &conn,
            id,
            &NoteUpdate {
                polished_body: Some(None),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(get_note(&conn, id).unwrap().unwrap().polished_body.is_none());
    }

    /// The same three-state shape is what moves a note to the unfiled bucket.
    #[test]
    fn a_note_can_be_moved_between_folders_and_out_of_all_of_them() {
        let conn = db();
        let work = create_folder(&conn, "Work").unwrap();
        let id = create_note(&conn, &note("t", "b")).unwrap();

        update_note(&conn, id, &NoteUpdate { folder_id: Some(Some(work.id)), ..Default::default() })
            .unwrap();
        assert_eq!(get_note(&conn, id).unwrap().unwrap().folder_id, Some(work.id));

        update_note(&conn, id, &NoteUpdate { folder_id: Some(None), ..Default::default() })
            .unwrap();
        assert_eq!(get_note(&conn, id).unwrap().unwrap().folder_id, None);
    }

    /// Every mutating path in this module owns `updated_at`, because the
    /// trigger that would have owned it for them corrupts the FTS index
    /// (`schema.rs`). This is the test that notices when a path added later
    /// forgets — the note list is ordered by this column and nothing else.
    #[test]
    fn every_write_path_moves_updated_at_forward() {
        let conn = db();
        let id = create_note(&conn, &note("t", "b")).unwrap();
        let backdate = |c: &Connection| {
            c.execute("UPDATE notes SET updated_at = 1000 WHERE id = ?1", params![id])
                .unwrap();
        };
        let stamp = |c: &Connection| get_note(c, id).unwrap().unwrap().updated_at;

        for (label, write) in [
            (
                "update_note",
                Box::new(|c: &Connection| {
                    update_note(
                        c,
                        id,
                        &NoteUpdate {
                            content: Some("edited".into()),
                            ..Default::default()
                        },
                    )
                    .unwrap();
                }) as Box<dyn Fn(&Connection)>,
            ),
            (
                "set_transcript",
                Box::new(|c: &Connection| {
                    set_transcript(c, id, Some(r#"[{"text":"spoken"}]"#)).unwrap();
                }),
            ),
        ] {
            backdate(&conn);
            assert_eq!(stamp(&conn), 1000);
            write(&conn);
            assert!(stamp(&conn) > 1000, "{label} left updated_at behind");
        }
    }

    /// An update naming nothing must not reorder the note list.
    #[test]
    fn an_update_that_writes_nothing_does_not_move_updated_at() {
        let conn = db();
        let id = create_note(&conn, &note("t", "b")).unwrap();
        conn.execute("UPDATE notes SET updated_at = 1000 WHERE id = ?1", params![id])
            .unwrap();
        assert!(!update_note(&conn, id, &NoteUpdate::default()).unwrap());
        assert_eq!(get_note(&conn, id).unwrap().unwrap().updated_at, 1000);
    }

    /// The rule `schema.rs` leaves behind, asserted rather than hoped for: no
    /// trigger on `notes` may write to `notes`. One that does corrupts the
    /// external-content FTS5 index — selectively, which is how it would get
    /// past a thinner test than this one.
    #[test]
    fn no_trigger_on_notes_writes_back_to_notes() {
        let conn = db();
        let mut stmt = conn
            .prepare("SELECT sql FROM sqlite_master WHERE type = 'trigger' AND tbl_name = 'notes'")
            .unwrap();
        let bodies: Vec<String> = stmt
            .query_map([], |r| r.get::<_, String>(0))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        assert!(!bodies.is_empty(), "the FTS sync triggers should be there");
        for body in &bodies {
            let after_begin = body.split_once("BEGIN").map(|(_, b)| b).unwrap_or(body);
            assert!(
                !after_begin.contains("UPDATE notes")
                    && !after_begin.contains("INSERT INTO notes ")
                    && !after_begin.contains("DELETE FROM notes"),
                "a trigger on `notes` writes to `notes`, which corrupts notes_fts"
            );
        }
    }

    #[test]
    fn deleting_a_note_twice_answers_true_then_false() {
        let conn = db();
        let id = create_note(&conn, &note("Dhobi list", "four kurtas, two dupattas")).unwrap();
        assert!(delete_note(&conn, id).unwrap());
        assert!(!delete_note(&conn, id).unwrap());
        assert!(get_note(&conn, id).unwrap().is_none());
    }

    #[test]
    fn a_bad_kind_is_rejected_rather_than_stored() {
        let conn = db();
        assert!(create_note(
            &conn,
            &NewNote {
                kind: Some("bogus".into()),
                ..Default::default()
            }
        )
        .is_err());
    }

    /// `folder_id` is a real foreign key, and the bundled SQLite is built with
    /// `PRAGMA foreign_keys` on by default, so a note cannot be filed into a
    /// folder that does not exist.
    #[test]
    fn a_note_cannot_be_filed_into_a_folder_that_does_not_exist() {
        let conn = db();
        assert!(create_note(
            &conn,
            &NewNote {
                folder_id: Some(999),
                ..Default::default()
            }
        )
        .is_err());
    }

    // -----------------------------------------------------------------------
    // The wire contract. These types cross the Tauri boundary, and the three
    // states of a `double_option` field are a thing only serde can get wrong —
    // every other test here builds the structs directly and would keep passing
    // while the webview quietly lost the ability to unfile a note.
    // -----------------------------------------------------------------------

    #[test]
    fn the_update_allow_list_tells_absent_from_null_over_the_wire() {
        let absent: NoteUpdate = serde_json::from_str(r#"{"title":"t"}"#).unwrap();
        assert_eq!(absent.title.as_deref(), Some("t"));
        assert_eq!(absent.folder_id, None, "an absent folderId must not unfile the note");
        assert_eq!(absent.polished_body, None);

        let cleared: NoteUpdate =
            serde_json::from_str(r#"{"folderId":null,"polishedBody":null}"#).unwrap();
        assert_eq!(cleared.folder_id, Some(None), "an explicit null must clear");
        assert_eq!(cleared.polished_body, Some(None));

        let set: NoteUpdate =
            serde_json::from_str(r#"{"folderId":7,"polishedFromHash":"abc"}"#).unwrap();
        assert_eq!(set.folder_id, Some(Some(7)));
        assert_eq!(set.polished_from_hash, Some(Some("abc".into())));
    }

    #[test]
    fn the_list_args_have_the_same_three_states() {
        let all: ListNotesArgs = serde_json::from_str("{}").unwrap();
        assert_eq!(all.folder, None);
        assert_eq!(all.page, 0);

        let unfiled: ListNotesArgs = serde_json::from_str(r#"{"folder":null,"page":2}"#).unwrap();
        assert_eq!(unfiled.folder, Some(None));
        assert_eq!(unfiled.page, 2);

        let one: ListNotesArgs = serde_json::from_str(r#"{"folder":3}"#).unwrap();
        assert_eq!(one.folder, Some(Some(3)));
    }

    /// `src/lib/api.ts` declares these names; a `rename_all` slip would break
    /// the page silently rather than loudly.
    #[test]
    fn the_row_types_serialize_the_names_api_ts_declares() {
        let conn = db();
        let id = create_note(
            &conn,
            &serde_json::from_str::<NewNote>(
                r#"{"kind":"imported","title":"t","audioSeconds":1.5,"importedFile":"a.wav"}"#,
            )
            .unwrap(),
        )
        .unwrap();
        let note = serde_json::to_value(get_note(&conn, id).unwrap().unwrap()).unwrap();
        for key in [
            "id",
            "folderId",
            "kind",
            "title",
            "content",
            "polishedBody",
            "polishPrompt",
            "polishedFromHash",
            "transcriptJson",
            "importedFile",
            "audioSeconds",
            "createdAt",
            "updatedAt",
        ] {
            assert!(note.get(key).is_some(), "Note is missing {key}");
        }
        assert_eq!(note["kind"], "imported", "NewNote's camelCase did not land");
        assert_eq!(note["audioSeconds"], 1.5);
        assert!(
            note.get("transcriptText").is_none(),
            "the derived index column must not leak into the API"
        );

        let folder = serde_json::to_value(create_folder(&conn, "Work").unwrap()).unwrap();
        for key in ["id", "name", "sortOrder", "createdAt", "updatedAt", "noteCount"] {
            assert!(folder.get(key).is_some(), "Folder is missing {key}");
        }
    }

    // -----------------------------------------------------------------------
    // Listing.
    // -----------------------------------------------------------------------

    #[test]
    fn list_is_most_recently_edited_first() {
        let conn = db();
        let a = create_note(&conn, &note("a", "")).unwrap();
        let b = create_note(&conn, &note("b", "")).unwrap();
        let c = create_note(&conn, &note("c", "")).unwrap();
        for (id, when) in [(a, 300), (b, 100), (c, 200)] {
            conn.execute("UPDATE notes SET updated_at = ?2 WHERE id = ?1", params![id, when])
                .unwrap();
        }
        assert_eq!(ids(&list_notes(&conn, None, 0).unwrap()), vec![a, c, b]);
    }

    #[test]
    fn list_paginates_at_the_page_size() {
        let conn = db();
        for i in 0..(PAGE_SIZE + 5) {
            create_note(&conn, &note(&format!("n{i}"), "")).unwrap();
        }
        assert_eq!(list_notes(&conn, None, 0).unwrap().len(), PAGE_SIZE as usize);
        assert_eq!(list_notes(&conn, None, 1).unwrap().len(), 5);
        assert!(list_notes(&conn, None, 9).unwrap().is_empty());
    }

    /// The three states of [`FolderFilter`], which is the whole reason it is
    /// not an `Option<i64>`.
    #[test]
    fn list_scopes_to_all_notes_to_one_folder_or_to_the_unfiled_ones() {
        let conn = db();
        let work = create_folder(&conn, "Work").unwrap();
        let filed = create_note(
            &conn,
            &NewNote {
                folder_id: Some(work.id),
                ..note("filed", "")
            },
        )
        .unwrap();
        let loose = create_note(&conn, &note("loose", "")).unwrap();

        assert_eq!(list_notes(&conn, None, 0).unwrap().len(), 2);
        assert_eq!(ids(&list_notes(&conn, Some(Some(work.id)), 0).unwrap()), vec![filed]);
        assert_eq!(ids(&list_notes(&conn, Some(None), 0).unwrap()), vec![loose]);
    }

    // -----------------------------------------------------------------------
    // Search.
    // -----------------------------------------------------------------------

    #[test]
    fn search_finds_a_note_by_title_body_polished_body_or_transcript() {
        let conn = db();
        let id = create_note(
            &conn,
            &NewNote {
                title: Some("Quarterly review".into()),
                content: Some("headcount and runway".into()),
                transcript_json: Some(r#"[{"text":"opening remarks"}]"#.into()),
                ..Default::default()
            },
        )
        .unwrap();
        update_note(
            &conn,
            id,
            &NoteUpdate {
                polished_body: Some(Some("polished summary".into())),
                ..Default::default()
            },
        )
        .unwrap();

        for q in ["quarterly", "runway", "polished", "remarks"] {
            assert_eq!(
                ids(&search_notes(&conn, q, SEARCH_LIMIT).unwrap()),
                vec![id],
                "query {q:?}"
            );
        }
        assert!(search_notes(&conn, "unrelated", SEARCH_LIMIT).unwrap().is_empty());
    }

    #[test]
    fn search_is_prefix_matching() {
        let conn = db();
        let id = create_note(&conn, &note("Dictation", "is delightful")).unwrap();
        assert_eq!(ids(&search_notes(&conn, "dict", SEARCH_LIMIT).unwrap()), vec![id]);
    }

    /// The regression guard the whole tokenizer choice exists for. `नमस्ते`
    /// carries a virama and a vowel sign; `किताब` is two matras.
    #[test]
    fn search_finds_devanagari_tamil_and_bengali() {
        let conn = db();
        let hi = create_note(&conn, &note("हिंदी नोट", "नमस्ते दुनिया किताब")).unwrap();
        let ta = create_note(&conn, &note("தமிழ்", "வணக்கம் உலகம்")).unwrap();
        let bn = create_note(&conn, &note("বাংলা", "নমস্কার পৃথিবী")).unwrap();

        assert_eq!(ids(&search_notes(&conn, "नमस्ते", SEARCH_LIMIT).unwrap()), vec![hi]);
        assert_eq!(ids(&search_notes(&conn, "किताब", SEARCH_LIMIT).unwrap()), vec![hi]);
        assert_eq!(ids(&search_notes(&conn, "வணக்கம்", SEARCH_LIMIT).unwrap()), vec![ta]);
        assert_eq!(ids(&search_notes(&conn, "নমস্কার", SEARCH_LIMIT).unwrap()), vec![bn]);
        // A short prefix, which is what a search-as-you-type box actually
        // sends.
        assert_eq!(ids(&search_notes(&conn, "नम", SEARCH_LIMIT).unwrap()), vec![hi]);
        assert_eq!(ids(&search_notes(&conn, "வண", SEARCH_LIMIT).unwrap()), vec![ta]);
        assert_eq!(ids(&search_notes(&conn, "নম", SEARCH_LIMIT).unwrap()), vec![bn]);
    }

    /// Recall alone is a weak guard: under `unicode61`'s default categories the
    /// marks are separators, so the query gets shredded into the same
    /// consonant skeleton the document was and matches anyway. Precision is
    /// what actually pins `categories 'L* N* Co Mn Mc'` — these words differ
    /// from each other *only* in their matras.
    #[test]
    fn search_tells_apart_words_that_share_a_consonant_skeleton() {
        let conn = db();
        let din = create_note(&conn, &note("दिन", "")).unwrap();
        let daan = create_note(&conn, &note("दान", "")).unwrap();
        create_note(&conn, &note("दीन", "")).unwrap();
        let kitab = create_note(&conn, &note("किताब", "")).unwrap();
        create_note(&conn, &note("कुतुब", "")).unwrap();

        assert_eq!(
            ids(&search_notes(&conn, "दिन", SEARCH_LIMIT).unwrap()),
            vec![din],
            "दान and दीन are different words, not fuzzy matches for दिन"
        );
        assert_eq!(ids(&search_notes(&conn, "दान", SEARCH_LIMIT).unwrap()), vec![daan]);
        assert_eq!(
            ids(&search_notes(&conn, "किताब", SEARCH_LIMIT).unwrap()),
            vec![kitab],
            "कुतुब shares only the skeleton क-त-ब"
        );
        // A genuine prefix still matches: कि is the real opening of किताब.
        assert_eq!(ids(&search_notes(&conn, "कि", SEARCH_LIMIT).unwrap()), vec![kitab]);
    }

    /// The index follows the table: an edit replaces a note's indexed text, a
    /// delete removes it, and both leave `integrity-check` passing.
    #[test]
    fn the_index_follows_an_edit_and_a_delete() {
        let conn = db();
        let id = create_note(&conn, &note("Findable", "original wording")).unwrap();
        assert_eq!(search_notes(&conn, "original", SEARCH_LIMIT).unwrap().len(), 1);

        update_note(
            &conn,
            id,
            &NoteUpdate {
                content: Some("replacement wording".into()),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(
            search_notes(&conn, "original", SEARCH_LIMIT).unwrap().is_empty(),
            "the old text must leave the index"
        );
        assert_eq!(search_notes(&conn, "replacement", SEARCH_LIMIT).unwrap().len(), 1);
        conn.execute("INSERT INTO notes_fts(notes_fts) VALUES('integrity-check')", [])
            .expect("an edit must leave the index consistent");

        delete_note(&conn, id).unwrap();
        assert!(
            search_notes(&conn, "replacement", SEARCH_LIMIT).unwrap().is_empty(),
            "a deleted note must leave the index"
        );
        conn.execute("INSERT INTO notes_fts(notes_fts) VALUES('integrity-check')", [])
            .expect("the index is still consistent");
    }

    #[test]
    fn search_with_no_tokens_returns_empty_and_operator_text_is_literal() {
        let conn = db();
        create_note(&conn, &note("close the parenthesis)", "NEAR")).unwrap();
        assert!(search_notes(&conn, "   ---   ", SEARCH_LIMIT).unwrap().is_empty());
        assert!(search_notes(&conn, "NEAR", SEARCH_LIMIT).is_ok());
        assert!(search_notes(&conn, "\"unterminated", SEARCH_LIMIT).is_ok());
        assert!(search_notes(&conn, "(unbalanced", SEARCH_LIMIT).is_ok());
    }

    #[test]
    fn search_respects_its_limit() {
        let conn = db();
        for i in 0..5 {
            create_note(&conn, &note(&format!("shared word {i}"), "")).unwrap();
        }
        assert_eq!(search_notes(&conn, "shared", 2).unwrap().len(), 2);
    }

    // -----------------------------------------------------------------------
    // The derived transcript column.
    // -----------------------------------------------------------------------

    #[test]
    fn transcript_text_reads_the_shapes_an_import_might_write() {
        assert_eq!(
            transcript_text(Some(r#"[{"text":"one"},{"text":"two"}]"#)).as_deref(),
            Some("one two")
        );
        assert_eq!(
            transcript_text(Some(r#"{"segments":[{"text":"one"},{"text":"two"}]}"#)).as_deref(),
            Some("one two")
        );
        assert_eq!(transcript_text(Some(r#"["one","two"]"#)).as_deref(), Some("one two"));
        assert_eq!(
            transcript_text(Some(r#"[{"text":"क़लम","start":0.0}]"#)).as_deref(),
            Some("क़लम")
        );
    }

    /// A transcript that cannot be read must cost the note nothing: an import
    /// that refuses to save is a bigger problem than a transcript that does
    /// not index.
    #[test]
    fn an_unreadable_transcript_is_none_and_still_saves_the_note() {
        for bad in ["{not json", "[]", "[{}]", "null", "42", r#"["   "]"#] {
            assert_eq!(transcript_text(Some(bad)), None, "input {bad:?}");
        }
        assert_eq!(transcript_text(None), None);

        let conn = db();
        let id = create_note(
            &conn,
            &NewNote {
                title: Some("Broken transcript".into()),
                transcript_json: Some("{not json".into()),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(ids(&search_notes(&conn, "broken", SEARCH_LIMIT).unwrap()), vec![id]);
    }

    #[test]
    fn set_transcript_moves_both_columns_together() {
        let conn = db();
        let id = create_note(&conn, &note("t", "b")).unwrap();
        assert!(set_transcript(&conn, id, Some(r#"[{"text":"spoken aloud"}]"#)).unwrap());
        assert_eq!(ids(&search_notes(&conn, "spoken", SEARCH_LIMIT).unwrap()), vec![id]);
        assert!(get_note(&conn, id).unwrap().unwrap().transcript_json.is_some());

        assert!(set_transcript(&conn, id, None).unwrap());
        assert!(search_notes(&conn, "spoken", SEARCH_LIMIT).unwrap().is_empty());
        assert!(get_note(&conn, id).unwrap().unwrap().transcript_json.is_none());
    }

    // -----------------------------------------------------------------------
    // Folders.
    // -----------------------------------------------------------------------

    #[test]
    fn folders_are_created_trimmed_appended_and_counted() {
        let conn = db();
        let a = create_folder(&conn, "  Work  ").unwrap();
        let b = create_folder(&conn, "Personal").unwrap();
        assert_eq!(a.name, "Work");
        assert!(b.sort_order > a.sort_order, "new folders go at the end");
        assert_eq!(a.note_count, 0);

        create_note(&conn, &NewNote { folder_id: Some(a.id), ..note("filed", "") }).unwrap();
        let listed = list_folders(&conn).unwrap();
        assert_eq!(listed.iter().map(|f| f.name.as_str()).collect::<Vec<_>>(), vec!["Work", "Personal"]);
        assert_eq!(listed[0].note_count, 1);
        assert_eq!(listed[1].note_count, 0);
    }

    #[test]
    fn a_folder_name_must_be_non_empty_and_unique_ignoring_case() {
        let conn = db();
        create_folder(&conn, "Work").unwrap();
        assert!(create_folder(&conn, "   ").is_err());
        assert!(create_folder(&conn, "Work").is_err());
        assert!(
            create_folder(&conn, "work").is_err(),
            "two folders the user cannot tell apart are one folder"
        );
    }

    #[test]
    fn rename_applies_the_same_rules_and_leaves_the_folder_its_own_name() {
        let conn = db();
        let a = create_folder(&conn, "Work").unwrap();
        let b = create_folder(&conn, "Personal").unwrap();

        assert!(rename_folder(&conn, a.id, "Personal").is_err());
        assert!(rename_folder(&conn, a.id, "  ").is_err());
        assert!(rename_folder(&conn, 999, "Anything").is_err());
        // Renaming to its own name is not a duplicate.
        assert_eq!(rename_folder(&conn, a.id, "Work").unwrap().name, "Work");
        assert_eq!(rename_folder(&conn, b.id, " Home ").unwrap().name, "Home");
    }

    /// A folder delete takes the folder's notes with it, and their index rows
    /// with them.
    #[test]
    fn deleting_a_folder_takes_its_notes_and_their_index_rows_with_it() {
        let conn = db();
        let work = create_folder(&conn, "Work").unwrap();
        let inside = create_note(
            &conn,
            &NewNote { folder_id: Some(work.id), ..note("inside", "cascaded away") },
        )
        .unwrap();
        let outside = create_note(&conn, &note("outside", "kept")).unwrap();

        let cascaded = delete_folder(&conn, work.id).unwrap();
        assert_eq!(cascaded, vec![inside]);
        assert!(get_note(&conn, inside).unwrap().is_none());
        assert!(get_note(&conn, outside).unwrap().is_some());
        assert!(list_folders(&conn).unwrap().is_empty());
        assert!(
            search_notes(&conn, "cascaded", SEARCH_LIMIT).unwrap().is_empty(),
            "the cascade must reach the FTS index too"
        );
        conn.execute("INSERT INTO notes_fts(notes_fts) VALUES('integrity-check')", [])
            .expect("the index is still consistent");
        assert!(delete_folder(&conn, work.id).is_err(), "already gone");
    }

    #[test]
    fn a_folder_can_be_renamed_to_a_case_variant_of_its_own_name() {
        let conn = db();
        let work = create_folder(&conn, "work").unwrap();
        let before = work.updated_at;
        std::thread::sleep(std::time::Duration::from_millis(5));

        let renamed = rename_folder(&conn, work.id, "Work").unwrap();
        assert_eq!(renamed.name, "Work");
        assert_eq!(renamed.id, work.id);
        assert!(renamed.updated_at > before, "a rename moves updated_at");
        assert_eq!(list_folders(&conn).unwrap(), vec![renamed]);
    }

    /// Both or neither: if the folder row cannot go, its notes stay too. The
    /// notes are deleted before the folder row, so a refused folder delete
    /// has to roll their delete back.
    #[test]
    fn a_folder_delete_that_fails_keeps_every_note() {
        let conn = db();
        let work = create_folder(&conn, "Work").unwrap();
        let inside = create_note(
            &conn,
            &NewNote { folder_id: Some(work.id), ..note("inside", "still here") },
        )
        .unwrap();
        conn.execute_batch(
            "CREATE TEMP TRIGGER keep_folders BEFORE DELETE ON folders
             BEGIN SELECT RAISE(ABORT, 'refused'); END;",
        )
        .unwrap();

        assert!(delete_folder(&conn, work.id).is_err());
        assert!(get_note(&conn, inside).unwrap().is_some(), "the note must survive");
        assert_eq!(ids(&search_notes(&conn, "still", SEARCH_LIMIT).unwrap()), vec![inside]);

        conn.execute_batch("DROP TRIGGER keep_folders").unwrap();
        assert_eq!(delete_folder(&conn, work.id).unwrap(), vec![inside]);
        assert!(get_note(&conn, inside).unwrap().is_none());
    }

    #[test]
    fn folder_refusals_are_sentences_the_editor_can_show() {
        let conn = db();
        let work = create_folder(&conn, "Work").unwrap();
        let refusals = [
            create_folder(&conn, "  ").unwrap_err().to_string(),
            create_folder(&conn, "WORK").unwrap_err().to_string(),
            rename_folder(&conn, work.id, "").unwrap_err().to_string(),
            rename_folder(&conn, 999, "Home").unwrap_err().to_string(),
            delete_folder(&conn, 999).unwrap_err().to_string(),
        ];
        assert_eq!(refusals[0], FOLDER_NAME_EMPTY);
        assert_eq!(refusals[1], FOLDER_NAME_TAKEN);
        assert_eq!(refusals[2], FOLDER_NAME_EMPTY);
        assert_eq!(refusals[3], FOLDER_GONE);
        assert_eq!(refusals[4], FOLDER_GONE);
        for text in refusals {
            assert!(text.ends_with('.'), "{text:?} is not a sentence");
            let lower = text.to_lowercase();
            for internal in ["sqlite", "constraint", "unique", "database", "row"] {
                assert!(!lower.contains(internal), "{text:?} names {internal:?}");
            }
        }
    }

    #[test]
    fn a_new_folder_goes_after_every_existing_one() {
        let conn = db();
        let a = create_folder(&conn, "A").unwrap();
        let b = create_folder(&conn, "B").unwrap();
        // Reorder so the first folder sorts last, then add another.
        conn.execute("UPDATE folders SET sort_order = 50 WHERE id = ?1", params![a.id])
            .unwrap();
        let c = create_folder(&conn, "C").unwrap();
        let order: Vec<i64> = list_folders(&conn).unwrap().iter().map(|f| f.id).collect();
        assert_eq!(order, vec![b.id, a.id, c.id]);
    }

    // -----------------------------------------------------------------------
    // Retention isolation. Notes are documents, not dictations.
    // -----------------------------------------------------------------------

    fn temp_db_path(name: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "bs-notes-test-{name}-{}-{}.db",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        p
    }

    /// Two claims in one, both through the real DB thread:
    ///
    /// 1. A note is written while `history.enabled` is **false**. That switch
    ///    gates `Cmd::Record`, not `Cmd::Task`, and a user who turns dictation
    ///    history off has not asked to lose their documents.
    /// 2. The retention sweep purges an expired *transcription* and leaves an
    ///    equally old *note* alone. `sweep_expired` names `transcriptions`;
    ///    this is the test that notices if it ever stops doing only that.
    ///
    /// The ordering is the channel's: `set_retention` is fire-and-forget on
    /// the same channel the following `with_connection` round-trips on, and it
    /// sweeps because the config actually changed — so by the time the
    /// assertions run, the sweep has already happened.
    #[test]
    fn retention_sweeps_transcriptions_and_never_touches_notes() {
        let path = temp_db_path("retention");
        let rec = crate::history::spawn(path, RetentionCfg { enabled: false, keep_days: 0 });

        let note_id = rec
            .with_connection(|conn| {
                let id = create_note(conn, &note("Old note", "still wanted")).unwrap();
                conn.execute(
                    "UPDATE notes SET created_at = 0, updated_at = 0 WHERE id = ?1",
                    params![id],
                )
                .unwrap();
                conn.execute(
                    "INSERT INTO transcriptions (text, outcome, created_at)
                     VALUES ('ancient dictation', 'done', datetime('now', '-400 days'))",
                    [],
                )
                .unwrap();
                id
            })
            .expect("the DB thread answered while history recording was disabled");

        rec.set_retention(RetentionCfg { enabled: false, keep_days: 7 });

        let (notes_left, transcriptions_left) = rec
            .with_connection(move |conn| {
                let n = list_notes(conn, None, 0).unwrap().len();
                let t: i64 = conn
                    .query_row("SELECT COUNT(*) FROM transcriptions", [], |r| r.get(0))
                    .unwrap();
                (n, t)
            })
            .expect("the DB thread answered");

        assert_eq!(transcriptions_left, 0, "the sweep should have purged the transcription");
        assert_eq!(notes_left, 1, "a note is not under history retention");
        assert!(rec
            .with_connection(move |conn| get_note(conn, note_id).unwrap().is_some())
            .unwrap());
    }

    /// The write-time master switch is about dictation, not documents: a
    /// `Cmd::Record` is dropped while it is off, a note written through
    /// `Cmd::Task` is not.
    #[test]
    fn history_disabled_drops_a_dictation_but_not_a_note() {
        let path = temp_db_path("disabled");
        let rec = crate::history::spawn(path, RetentionCfg { enabled: false, keep_days: 0 });
        rec.record(NewEntry {
            text: "should not be stored".into(),
            ..Default::default()
        });
        rec.with_connection(|conn| create_note(conn, &note("kept", "body")).unwrap())
            .expect("the DB thread answered");

        assert!(rec.list(0, 10).is_empty());
        assert_eq!(
            rec.with_connection(|conn| list_notes(conn, None, 0).unwrap().len())
                .unwrap(),
            1
        );
    }
}
