//! The layouts earlier builds of the app left in `history.db`, for the
//! upgrade tests. Nothing outside the tests creates these: `store::migrate`
//! only ever creates the current layout, and turns any of these into it.
//!
//! Each constant is the state of the file at one schema version, written out
//! from the column list, types and defaults that version used, because that
//! is what a user's file holds and what the upgrade has to read. Versions 1
//! to 5 were written by internal builds before the first public release;
//! 0.2.0, as released, writes version 6. The earlier layouts' table, column,
//! index and trigger names therefore appear here, in the upgrade's reads of
//! the old tables and in its drops of the old indexes, and nowhere a current
//! file can reach. That is also why the version 5 fixture the upgrade tests
//! share, and the reading of it in current terms, live here rather than in
//! `store`.

use rusqlite::types::Value;
use rusqlite::{params, Connection};

/// Version 1: dictation history and its search index.
const V1: &str = r#"
CREATE TABLE transcriptions (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    text        TEXT NOT NULL,
    raw_text    TEXT,
    created_at  TEXT NOT NULL DEFAULT (datetime('now')),
    status      TEXT NOT NULL DEFAULT 'completed',
    error_code  TEXT,
    provider    TEXT,
    model       TEXT,
    duration_ms INTEGER,
    app         TEXT,
    words       INTEGER,
    route_kind  TEXT
);
CREATE INDEX idx_transcriptions_created_at ON transcriptions(created_at);
CREATE VIRTUAL TABLE transcriptions_fts USING fts5(
    text, raw_text, content='transcriptions', content_rowid='id',
    tokenize='unicode61 remove_diacritics 0'
);
CREATE TRIGGER transcriptions_ai AFTER INSERT ON transcriptions BEGIN
    INSERT INTO transcriptions_fts(rowid, text, raw_text) VALUES (new.id, new.text, new.raw_text);
END;
CREATE TRIGGER transcriptions_ad AFTER DELETE ON transcriptions BEGIN
    INSERT INTO transcriptions_fts(transcriptions_fts, rowid, text, raw_text)
    VALUES ('delete', old.id, old.text, old.raw_text);
END;
CREATE TRIGGER transcriptions_au AFTER UPDATE ON transcriptions BEGIN
    INSERT INTO transcriptions_fts(transcriptions_fts, rowid, text, raw_text)
    VALUES ('delete', old.id, old.text, old.raw_text);
    INSERT INTO transcriptions_fts(rowid, text, raw_text) VALUES (new.id, new.text, new.raw_text);
END;
"#;

/// Version 3: the notes tables, and the history index rebuilt on the Indic
/// tokenizer.
const V3: &str = r#"
CREATE TABLE folders (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    name       TEXT    NOT NULL UNIQUE COLLATE NOCASE,
    sort_order INTEGER NOT NULL DEFAULT 0,
    created_at INTEGER NOT NULL DEFAULT (CAST(unixepoch('now','subsec') * 1000 AS INTEGER)),
    updated_at INTEGER NOT NULL DEFAULT (CAST(unixepoch('now','subsec') * 1000 AS INTEGER))
);
CREATE INDEX idx_folders_sort_order ON folders(sort_order);
CREATE TABLE notes (
    id                       INTEGER PRIMARY KEY AUTOINCREMENT,
    folder_id                INTEGER NULL REFERENCES folders(id) ON DELETE CASCADE,
    note_type                TEXT    NOT NULL DEFAULT 'note' CHECK (note_type IN ('note','upload')),
    title                    TEXT    NOT NULL DEFAULT '',
    content                  TEXT    NOT NULL DEFAULT '',
    enhanced_content         TEXT    NULL,
    enhancement_prompt       TEXT    NULL,
    enhanced_at_content_hash TEXT    NULL,
    transcript_json          TEXT    NULL,
    transcript_text          TEXT    NULL,
    source_file              TEXT    NULL,
    audio_duration_seconds   REAL    NULL,
    created_at               INTEGER NOT NULL DEFAULT (CAST(unixepoch('now','subsec') * 1000 AS INTEGER)),
    updated_at               INTEGER NOT NULL DEFAULT (CAST(unixepoch('now','subsec') * 1000 AS INTEGER))
);
CREATE INDEX idx_notes_updated_at ON notes(updated_at);
CREATE INDEX idx_notes_folder_id  ON notes(folder_id);
CREATE VIRTUAL TABLE notes_fts USING fts5(
    title, content, enhanced_content, transcript_text,
    content='notes', content_rowid='id',
    tokenize="unicode61 remove_diacritics 0 categories 'L* N* Co Mn Mc'"
);
CREATE TRIGGER notes_ai AFTER INSERT ON notes BEGIN
    INSERT INTO notes_fts(rowid, title, content, enhanced_content, transcript_text)
    VALUES (new.id, new.title, new.content, new.enhanced_content, new.transcript_text);
END;
CREATE TRIGGER notes_ad AFTER DELETE ON notes BEGIN
    INSERT INTO notes_fts(notes_fts, rowid, title, content, enhanced_content, transcript_text)
    VALUES ('delete', old.id, old.title, old.content, old.enhanced_content, old.transcript_text);
END;
CREATE TRIGGER notes_au AFTER UPDATE ON notes BEGIN
    INSERT INTO notes_fts(notes_fts, rowid, title, content, enhanced_content, transcript_text)
    VALUES ('delete', old.id, old.title, old.content, old.enhanced_content, old.transcript_text);
    INSERT INTO notes_fts(rowid, title, content, enhanced_content, transcript_text)
    VALUES (new.id, new.title, new.content, new.enhanced_content, new.transcript_text);
END;
CREATE TABLE note_actions (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    name        TEXT    NOT NULL,
    description TEXT    NOT NULL DEFAULT '',
    prompt      TEXT    NOT NULL,
    icon        TEXT    NOT NULL DEFAULT 'sparkles',
    sort_order  INTEGER NOT NULL DEFAULT 0,
    is_builtin  INTEGER NOT NULL DEFAULT 0,
    builtin_key TEXT    NULL UNIQUE,
    created_at  INTEGER NOT NULL DEFAULT (CAST(unixepoch('now','subsec') * 1000 AS INTEGER)),
    updated_at  INTEGER NOT NULL DEFAULT (CAST(unixepoch('now','subsec') * 1000 AS INTEGER))
);

DROP TABLE transcriptions_fts;
CREATE VIRTUAL TABLE transcriptions_fts USING fts5(
    text, raw_text, content='transcriptions', content_rowid='id',
    tokenize="unicode61 remove_diacritics 0 categories 'L* N* Co Mn Mc'"
);
INSERT INTO transcriptions_fts(transcriptions_fts) VALUES('rebuild');
"#;

/// Version 4: both indexes remove deleted terms in place. Version 5 changed
/// no table: it purged the free pages once.
const V4: &str = r#"
INSERT INTO transcriptions_fts(transcriptions_fts, rank) VALUES('secure-delete', 1);
INSERT INTO notes_fts(notes_fts, rank) VALUES('secure-delete', 1);
INSERT INTO transcriptions_fts(transcriptions_fts) VALUES('rebuild');
INSERT INTO notes_fts(notes_fts) VALUES('rebuild');
"#;

/// Lay out an empty file exactly as a build at `version` (1 to 5) left it.
pub(super) fn write(conn: &Connection, version: u32) {
    assert!((1..=5).contains(&version), "no layout for version {version}");
    conn.execute_batch(V1).unwrap();
    if version >= 2 {
        conn.execute_batch(crate::learn::candidates::SCHEMA).unwrap();
    }
    if version >= 3 {
        conn.execute_batch(V3).unwrap();
    }
    if version >= 4 {
        conn.execute_batch(V4).unwrap();
    }
    conn.pragma_update(None, "user_version", version).unwrap();
}

/// A dictation row in the old layout. Returns its id.
pub(super) struct OldRow<'a> {
    pub text: &'a str,
    pub raw_text: Option<&'a str>,
    pub status: &'a str,
    pub error_code: Option<&'a str>,
    pub provider: Option<&'a str>,
    pub model: Option<&'a str>,
    pub duration_ms: Option<i64>,
    pub app: Option<&'a str>,
    pub words: Option<i64>,
    pub route_kind: Option<&'a str>,
}

impl Default for OldRow<'_> {
    fn default() -> Self {
        OldRow {
            text: "",
            raw_text: None,
            status: "completed",
            error_code: None,
            provider: None,
            model: None,
            duration_ms: None,
            app: None,
            words: None,
            route_kind: None,
        }
    }
}

pub(super) fn insert_row(conn: &Connection, row: &OldRow) -> i64 {
    conn.execute(
        "INSERT INTO transcriptions
             (text, raw_text, status, error_code, provider, model, duration_ms, app, words, route_kind)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        params![
            row.text,
            row.raw_text,
            row.status,
            row.error_code,
            row.provider,
            row.model,
            row.duration_ms,
            row.app,
            row.words,
            row.route_kind
        ],
    )
    .unwrap();
    conn.last_insert_rowid()
}

/// A dictation row with nothing but its text.
pub(super) fn insert_text(conn: &Connection, text: &str) -> i64 {
    insert_row(conn, &OldRow { text, ..Default::default() })
}

// ---------------------------------------------------------------------------
// A version 5 file with something of everything in it.
// ---------------------------------------------------------------------------

/// Precomposed nukta letters, as Sarvam spells them: ज़रूरी बड़ा फ़ोन.
const NUKTA_STORED: &str =
    "\u{095B}\u{0930}\u{0942}\u{0930}\u{0940} \u{092C}\u{095C}\u{093E} \u{095E}\u{094B}\u{0928}";

/// Ids of the rows [`fill_v5`] files, so the tests can find each one again.
pub(super) struct V5 {
    pub dictation: i64,
    pub failed: i64,
    pub translated: i64,
    pub nukta_dictation: i64,
    pub deleted_dictation: i64,
    pub work: i64,
    pub filed: i64,
    pub unfiled_polished: i64,
    pub imported: i64,
    pub nukta_note: i64,
    pub deleted_note: i64,
    pub renamed_shipped: i64,
    pub user_action: i64,
    pub retired_edited: i64,
    pub retired_untouched: i64,
    pub deleted_action: i64,
}

/// Fill a version 5 file the way the app would have: every kind of row
/// each table could hold, plus a row deleted from the top of each table
/// so the id counters are ahead of the highest id.
pub(super) fn fill_v5(conn: &Connection) -> V5 {
    let row = |r: OldRow<'_>| insert_row(conn, &r);
    let dictation = row(OldRow {
        text: "Standup moved to ten.",
        raw_text: Some("um standup moved to ten"),
        provider: Some("sarvam"),
        duration_ms: Some(2_400),
        app: Some("slack"),
        words: Some(4),
        ..Default::default()
    });
    let failed = row(OldRow {
        text: "the budget for the harbour",
        raw_text: Some("the budget for the harbour"),
        status: "failed",
        error_code: Some("connection-lost"),
        provider: Some("sarvam"),
        duration_ms: Some(9_100),
        app: Some("outlook"),
        words: Some(5),
        ..Default::default()
    });
    let translated = row(OldRow {
        text: "Translated text arrives here",
        raw_text: Some("yahan anuvaad aata hai"),
        provider: Some("whisper-local"),
        model: Some("small-int8"),
        duration_ms: Some(1_200),
        app: Some("notepad"),
        words: Some(4),
        route_kind: Some("translation"),
        ..Default::default()
    });
    let nukta_dictation = row(OldRow {
        text: NUKTA_STORED,
        raw_text: Some(NUKTA_STORED),
        route_kind: Some("agent"),
        words: Some(3),
        ..Default::default()
    });
    let deleted_dictation = insert_text(conn, "removed before the upgrade");
    conn.execute("DELETE FROM transcriptions WHERE id = ?1", params![deleted_dictation])
        .unwrap();
    conn.execute(
        "UPDATE transcriptions SET created_at = '2026-03-01 08:15:00' WHERE id = ?1",
        params![dictation],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO learn_candidates (from_word, to_word, count, last_seen, session_id)
         VALUES ('Koramangla', 'Koramangala', 1, 1800000000, 's1')",
        [],
    )
    .unwrap();

    conn.execute("INSERT INTO folders (name, sort_order) VALUES ('Work', 0)", [])
        .unwrap();
    let work = conn.last_insert_rowid();
    conn.execute("INSERT INTO folders (name, sort_order) VALUES ('घर', 1)", [])
        .unwrap();
    let home = conn.last_insert_rowid();

    let note = |sql: &str, p: &[&dyn rusqlite::ToSql]| -> i64 {
        conn.execute(sql, p).unwrap();
        conn.last_insert_rowid()
    };
    let filed = note(
        "INSERT INTO notes (folder_id, title, content, created_at, updated_at)
         VALUES (?1, 'Standup', 'Ravi takes the budget review.', 1700000000000, 1700000500000)",
        &[&work],
    );
    let unfiled_polished = note(
        "INSERT INTO notes (title, content, enhanced_content, enhancement_prompt,
                            enhanced_at_content_hash, created_at, updated_at)
         VALUES ('Errands', 'uh milk and the harbour pass',
                 '- Milk\n- Tidied harbour pass', 'List every errand.', 'abc123',
                 1700000100000, 1700000200000)",
        &[],
    );
    let imported = note(
        "INSERT INTO notes (note_type, title, content, transcript_json, transcript_text,
                            source_file, audio_duration_seconds, created_at, updated_at)
         VALUES ('upload', 'Talk', 'Opening remarks follow.',
                 '[{\"text\":\"spoken opening\"},{\"text\":\"closing words\"}]',
                 'spoken opening closing words', 'talk.wav', 312.5,
                 1700000300000, 1700000300000)",
        &[],
    );
    let nukta_note = note(
        "INSERT INTO notes (folder_id, title, content) VALUES (?1, 'हिंदी', ?2)",
        &[&home, &NUKTA_STORED],
    );
    let deleted_note = note("INSERT INTO notes (title) VALUES ('gone before the upgrade')", &[]);
    conn.execute("DELETE FROM notes WHERE id = ?1", params![deleted_note])
        .unwrap();

    let action = |sql: &str| -> i64 {
        conn.execute(sql, []).unwrap();
        conn.last_insert_rowid()
    };
    let renamed_shipped = action(
        "INSERT INTO note_actions (name, description, prompt, icon, sort_order,
                                   is_builtin, builtin_key, created_at, updated_at)
         VALUES ('My tidy-up', 'Shipped, then renamed', 'Tidy it.', 'sparkles', 0,
                 1, 'tidy_up', 1700000000000, 1700000900000)",
    );
    let user_action = action(
        "INSERT INTO note_actions (name, description, prompt, icon, sort_order,
                                   created_at, updated_at)
         VALUES ('Dates', 'Every date in the note', 'List the dates.', 'clock', 3,
                 1700000400000, 1700000400000)",
    );
    let retired_edited = action(
        "INSERT INTO note_actions (name, prompt, icon, sort_order, is_builtin, builtin_key,
                                   created_at, updated_at)
         VALUES ('Old summary', 'Summarise it.', 'book', 1, 1, 'old_summary', 1000, 2000)",
    );
    let retired_untouched = action(
        "INSERT INTO note_actions (name, prompt, is_builtin, builtin_key, created_at, updated_at)
         VALUES ('Old outline', 'Outline it.', 1, 'old_outline', 1000, 1000)",
    );
    let deleted_action =
        action("INSERT INTO note_actions (name, prompt) VALUES ('Short-lived', 'Do nothing.')");
    conn.execute("DELETE FROM note_actions WHERE id = ?1", params![deleted_action])
        .unwrap();

    V5 {
        dictation,
        failed,
        translated,
        nukta_dictation,
        deleted_dictation,
        work,
        filed,
        unfiled_polished,
        imported,
        nukta_note,
        deleted_note,
        renamed_shipped,
        user_action,
        retired_edited,
        retired_untouched,
        deleted_action,
    }
}

/// Every row of `sql`'s result, every column, as SQLite values.
pub(super) fn rows(conn: &Connection, sql: &str) -> Vec<Vec<Value>> {
    let mut stmt = conn.prepare(sql).unwrap();
    let width = stmt.column_count();
    stmt.query_map([], |r| (0..width).map(|i| r.get::<_, Value>(i)).collect())
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

/// Every row of the tables the upgrade carries across.
#[derive(Debug, PartialEq)]
pub(super) struct Tables {
    pub dictations: Vec<Vec<Value>>,
    pub notes: Vec<Vec<Value>>,
    pub actions: Vec<Vec<Value>>,
    pub folders: Vec<Vec<Value>>,
    pub candidates: Vec<Vec<Value>>,
}

/// Every row of a version 5 file as the current layout should hold it, in
/// the column order `store`'s tests read a current file in. A dictation's
/// status becomes its outcome (`'completed'` is `'done'`, anything else
/// `'failed'`), a note's type its kind (`'upload'` is `'imported'`, anything
/// else `'written'`), and an action keeps its key only if it was flagged as
/// shipped and the key is not empty.
///
/// Worked out here in Rust rather than with the upgrade's own SQL, so a test
/// that compares the two is comparing the upgrade with a separate account of
/// what it should do.
pub(super) fn v5_rows_in_current_terms(conn: &Connection) -> Tables {
    let text = |s: &str| Value::Text(s.to_string());
    let dictations = rows(
        conn,
        "SELECT id, created_at, status, error_code, text, raw_text, words,
                duration_ms, app, route_kind, provider, model
         FROM transcriptions ORDER BY id",
    )
    .into_iter()
    .map(|mut r| {
        r[2] = match &r[2] {
            Value::Text(s) if s == "completed" => text("done"),
            _ => text("failed"),
        };
        r
    })
    .collect();
    let notes = rows(
        conn,
        "SELECT id, note_type, folder_id, title, content, source_file,
                audio_duration_seconds, transcript_json, transcript_text,
                enhanced_content, enhancement_prompt, enhanced_at_content_hash,
                created_at, updated_at
         FROM notes ORDER BY id",
    )
    .into_iter()
    .map(|mut r| {
        r[1] = match &r[1] {
            Value::Text(s) if s == "upload" => text("imported"),
            _ => text("written"),
        };
        r
    })
    .collect();
    let actions = rows(
        conn,
        "SELECT id, is_builtin, builtin_key, sort_order, name, description, prompt, icon,
                created_at, updated_at
         FROM note_actions ORDER BY id",
    )
    .into_iter()
    .map(|mut r| {
        let flagged = r.remove(1) == Value::Integer(1);
        if !flagged || r[1] == text("") {
            r[1] = Value::Null;
        }
        r
    })
    .collect();
    Tables {
        dictations,
        notes,
        actions,
        folders: rows(conn, "SELECT * FROM folders ORDER BY id"),
        candidates: rows(conn, "SELECT * FROM learn_candidates ORDER BY from_word, to_word"),
    }
}
