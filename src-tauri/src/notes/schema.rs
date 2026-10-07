//! The `notes` / `folders` / `note_actions` tables and their FTS5 shadow.
//!
//! The DDL lives here rather than in `history::store` for the reason
//! `store.rs`'s `migrate` states: the table and the only code that reads it
//! stay in one file, while `store.rs` remains the single place that decides
//! what `PRAGMA user_version` means. [`SCHEMA`] is the current layout, and
//! [`bring_current`] is the notes half of `migrate`: it creates the tables on
//! a file that has none and reshapes the ones an earlier layout left.
//!
//! Every statement in [`SCHEMA`] is `IF NOT EXISTS`, so running it again is
//! a no-op.

use rusqlite::{params, Connection};

/// The current notes layout, applied by [`bring_current`].
///
/// ## Timestamps
///
/// `created_at` / `updated_at` are **epoch milliseconds**, not the
/// `datetime('now')` strings `transcriptions` uses. Two reasons: `Date.now()`
/// is the webview's own clock format, so
/// `new Date(ms)` needs none of the offset-naive un-doing `History.svelte`'s
/// `parseUtc` does; and second-resolution strings tie constantly under
/// `ORDER BY updated_at DESC` when a 1 s autosave debounce is writing.
/// `unixepoch('now','subsec')` (SQLite 3.42+) is what gives the millisecond.
///
/// ## Why `updated_at` is bumped by the writer and NOT by a trigger
///
/// Keeping `updated_at` current with an `AFTER UPDATE` trigger is the obvious
/// design, and it does not work on this table. A trigger on `notes`
/// whose body updates its own table, sitting alongside the trigger
/// that keeps an external-content FTS5 index in sync, makes SQLite raise
/// `SQLITE_CORRUPT_VTAB` (extended code 267, "database disk image is
/// malformed") from the outer UPDATE.
///
/// Measured on bundled SQLite 3.46.0, with `recursive_triggers` at its default
/// of off. The worst part is that it is *selective*: with the trigger present,
/// an `UPDATE notes` of the body or the polished body failed while one of the
/// title or the folder succeeded, and a literal `SET content = 'x'` succeeded
/// where the bound-parameter form of the same statement failed. A schema that
/// is fine until the day a caller changes which column it writes is worse
/// than one that is honestly manual, so `notes::update_note` and
/// `notes::set_transcript` write `updated_at` themselves and
/// `notes::tests::every_write_path_moves_updated_at_forward` is what stops one
/// of them forgetting.
///
/// The rule this leaves behind, for anyone extending this schema: **no trigger
/// on `notes` may write to `notes`.**
///
/// ## Why `transcript_text` is a stored column
///
/// It is derived from `transcript_json`, and the obvious spelling — an FTS5
/// `content=` pointing at a view that computes it with `json_each` — was tried
/// and rejected: FTS5 prepares its content query with `SQLITE_PREPARE_NO_VTAB`,
/// `json_each` is an eponymous virtual table, and so
/// `INSERT INTO notes_fts(notes_fts) VALUES('rebuild')` fails with
/// `no such table: main.json_each` (measured, bundled SQLite 3.46.0 — see the
/// `the_index_can_be_rebuilt_and_integrity_checked` below). Deriving it in Rust and
/// storing it keeps `'rebuild'` and `'integrity-check'` working, and keeps
/// these triggers the same shape as `transcriptions`'.
///
/// ## Tokenizer — `categories` is the Indic fix, not `remove_diacritics`
///
/// `unicode61`'s default token categories are `L* N* Co`. Combining marks are
/// `Mn`/`Mc`, so by default every Devanagari matra, virama, Tamil vowel sign
/// and Bengali kar is a **separator**: `किताब` indexes as three tokens,
/// `["क","त","ब"]`, and a search for it also finds `कुतुब`, while `"दिन"*`
/// returns दिन, दान, दीन *and* दुनिया. That is skeleton matching, and it is
/// what this app is for.
///
/// `remove_diacritics` does not touch it. Measured on bundled SQLite 3.46.0,
/// `remove_diacritics 0` and `1` produce **identical** term lists for every
/// Indic word tested; the option only folds precomposed Latin/Greek/Cyrillic
/// accents, which is why it is still `0` — that is what keeps `café` and
/// `cafe` apart.
///
/// Adding `Mn Mc` to the category list is the actual fix. Each Indic word then
/// indexes as **one** token (`किताब` → `["किताब"]`), prefix recall stays at
/// 26/26 across Hindi, Tamil and Bengali, and precision becomes exact:
/// `"दिन"*` matches दिन alone, `"कि"*` matches किताब alone. Pure-ASCII English
/// tokenizes byte-identically to before.
///
/// The string is byte-identical to `transcriptions_fts`'s, deliberately — two
/// search boxes over the same Indic text must not disagree about what a token
/// is — and `the_tokenizer_is_byte_identical_to_the_history_index` is what
/// keeps them that way.
///
/// ## Secure delete
///
/// `history::store::migrate` turns on FTS5's `secure-delete` option for this
/// index, as for the history one, so a deleted or replaced row's terms are
/// removed in place rather than left in an old segment until a merge. The
/// store's module doc has the rest of that story.
pub const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS folders (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    -- NOCASE so "Work" and "work" are one folder: they would otherwise be two
    -- rows the user cannot tell apart, and one directory once a note-file
    -- mirror turns folder names into Windows paths. ASCII folding only, which
    -- is exactly right for the caseless Indic scripts (same reasoning as
    -- `learn_candidates`).
    name       TEXT    NOT NULL UNIQUE COLLATE NOCASE,
    sort_order INTEGER NOT NULL DEFAULT 0,
    created_at INTEGER NOT NULL DEFAULT (CAST(unixepoch('now','subsec') * 1000 AS INTEGER)),
    updated_at INTEGER NOT NULL DEFAULT (CAST(unixepoch('now','subsec') * 1000 AS INTEGER))
);

CREATE INDEX IF NOT EXISTS idx_folders_sort_order ON folders(sort_order);

-- One row per note. `kind` says where the note came from: written in the
-- editor (typed or dictated), or made by importing a recording.
CREATE TABLE IF NOT EXISTS notes (
    id                 INTEGER PRIMARY KEY AUTOINCREMENT,
    kind               TEXT    NOT NULL DEFAULT 'written'
                               CHECK (kind IN ('written', 'imported')),
    -- NULL is the unfiled bucket. Deleting a folder deletes its notes:
    -- `notes::delete_folder` removes them itself, inside one transaction, so
    -- that holds whether or not the connection enforces foreign keys.
    folder_id          INTEGER REFERENCES folders(id) ON DELETE CASCADE,
    title              TEXT    NOT NULL DEFAULT '',
    content            TEXT    NOT NULL DEFAULT '',
    -- An import's recording: its file name, its length, its segments as the
    -- JSON the import produced, and the flattened text of those segments
    -- (written by `notes::transcript_text`, never by hand; see above).
    imported_file      TEXT,
    audio_seconds      REAL,
    transcript_json    TEXT,
    transcript_text    TEXT,
    -- The last note action's result, kept beside the text the person wrote
    -- and never over it, with the action's prompt and a SHA-256 of the
    -- content it was produced from (`notes::actions::content_hash`).
    polished_body      TEXT,
    polish_prompt      TEXT,
    polished_from_hash TEXT,
    created_at         INTEGER NOT NULL DEFAULT (CAST(unixepoch('now','subsec') * 1000 AS INTEGER)),
    updated_at         INTEGER NOT NULL DEFAULT (CAST(unixepoch('now','subsec') * 1000 AS INTEGER))
);

-- The note list orders by `updated_at DESC` and scopes by folder.
CREATE INDEX IF NOT EXISTS notes_by_updated ON notes(updated_at);
CREATE INDEX IF NOT EXISTS notes_in_folder  ON notes(folder_id);

-- External-content FTS5 index, built the same way as `transcriptions_fts`:
-- the text lives once, in `notes`, and this is the inverted index over it,
-- kept in step by the three triggers below. An external-content table cannot
-- read a row's old values for itself, so the triggers hand them over in a
-- `'delete'` command before a row changes or goes.
CREATE VIRTUAL TABLE IF NOT EXISTS notes_fts USING fts5(
    title, content, polished_body, transcript_text,
    content='notes',
    content_rowid='id',
    tokenize="unicode61 remove_diacritics 0 categories 'L* N* Co Mn Mc'"
);

CREATE TRIGGER IF NOT EXISTS notes_index_add AFTER INSERT ON notes BEGIN
    INSERT INTO notes_fts(rowid, title, content, polished_body, transcript_text)
    VALUES (new.id, new.title, new.content, new.polished_body, new.transcript_text);
END;

CREATE TRIGGER IF NOT EXISTS notes_index_remove AFTER DELETE ON notes BEGIN
    INSERT INTO notes_fts(notes_fts, rowid, title, content, polished_body, transcript_text)
    VALUES ('delete', old.id, old.title, old.content, old.polished_body, old.transcript_text);
END;

CREATE TRIGGER IF NOT EXISTS notes_index_replace AFTER UPDATE ON notes BEGIN
    INSERT INTO notes_fts(notes_fts, rowid, title, content, polished_body, transcript_text)
    VALUES ('delete', old.id, old.title, old.content, old.polished_body, old.transcript_text);
    INSERT INTO notes_fts(rowid, title, content, polished_body, transcript_text)
    VALUES (new.id, new.title, new.content, new.polished_body, new.transcript_text);
END;

-- NOTE: there is deliberately no `updated_at` touch trigger here. A trigger on
-- `notes` that writes to `notes` corrupts the external-content FTS5 index
-- these three triggers maintain — see the module doc for the measurement.
-- Writers set `updated_at` themselves.

-- The prompts a person can run over a note from the Enhance menu. A row the
-- app ships carries the `shipped_key` it is seeded under (see
-- `notes::actions::ensure_builtins`); a row the person made has none.
CREATE TABLE IF NOT EXISTS note_actions (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    shipped_key TEXT    UNIQUE CHECK (shipped_key IS NULL OR length(shipped_key) > 0),
    position    INTEGER NOT NULL DEFAULT 0,
    label       TEXT    NOT NULL,
    summary     TEXT    NOT NULL DEFAULT '',
    instruction TEXT    NOT NULL,
    glyph       TEXT    NOT NULL DEFAULT 'note',
    created_at  INTEGER NOT NULL DEFAULT (CAST(unixepoch('now','subsec') * 1000 AS INTEGER)),
    updated_at  INTEGER NOT NULL DEFAULT (CAST(unixepoch('now','subsec') * 1000 AS INTEGER))
);
"#;

/// Bring the notes tables to [`SCHEMA`], whatever the file holds.
///
/// Called by `history::store::migrate` inside its upgrade transaction, so a
/// failure here leaves the file exactly as it was. Three cases per table:
///
/// - **Absent** (a new file, or one from before notes existed): created.
/// - **The earlier layout** (files up to schema version 5): rebuilt. The old
///   table is renamed aside, the current one is created under the real name,
///   every row is copied across with its id, and the old table is dropped.
///   The old search index and its triggers are dropped first (see
///   [`drop_old_index`]) and the new ones are filled row by row as the copy
///   goes in.
/// - **Current**: left alone, so a second run changes nothing.
///
/// The earlier layout is recognised by a column only it has, rather than by
/// the file's version number, because a file whose start-up purge failed is
/// recorded at an older version with these tables already current (see
/// `migrate`).
pub fn bring_current(conn: &Connection) -> rusqlite::Result<()> {
    let notes_were_old = has_column(conn, "notes", "enhanced_content")?;
    let actions_were_old = has_column(conn, "note_actions", "builtin_key")?;

    if notes_were_old {
        drop_old_index(conn)?;
        conn.execute_batch("ALTER TABLE notes RENAME TO notes_before_v6;")?;
    }
    if actions_were_old {
        conn.execute_batch("ALTER TABLE note_actions RENAME TO note_actions_before_v6;")?;
    }

    conn.execute_batch(SCHEMA)?;

    if notes_were_old {
        // A note filed under a folder that no longer exists (possible only
        // if foreign keys were ever off for this file) is unfiled rather
        // than allowed to fail the whole upgrade.
        conn.execute_batch(
            "INSERT INTO notes
                 (id, kind, folder_id, title, content,
                  imported_file, audio_seconds, transcript_json, transcript_text,
                  polished_body, polish_prompt, polished_from_hash,
                  created_at, updated_at)
             SELECT id,
                    CASE note_type WHEN 'upload' THEN 'imported' ELSE 'written' END,
                    CASE WHEN folder_id IN (SELECT id FROM folders) THEN folder_id END,
                    title, content,
                    source_file, audio_duration_seconds, transcript_json, transcript_text,
                    enhanced_content, enhancement_prompt, enhanced_at_content_hash,
                    created_at, updated_at
             FROM notes_before_v6
             ORDER BY id;",
        )?;
        carry_sequence(conn, "notes_before_v6", "notes")?;
        conn.execute_batch("DROP TABLE notes_before_v6;")?;
    }
    if actions_were_old {
        // A row only counts as shipped when it was flagged built-in and still
        // carried a key; anything else was the person's own.
        conn.execute_batch(
            "INSERT INTO note_actions
                 (id, shipped_key, position, label, summary, instruction, glyph,
                  created_at, updated_at)
             SELECT id,
                    CASE WHEN is_builtin = 1 THEN NULLIF(builtin_key, '') END,
                    sort_order, name, description, prompt, icon,
                    created_at, updated_at
             FROM note_actions_before_v6
             ORDER BY id;",
        )?;
        carry_sequence(conn, "note_actions_before_v6", "note_actions")?;
        conn.execute_batch("DROP TABLE note_actions_before_v6;")?;
    }
    Ok(())
}

/// Drop the search index and triggers of a `notes` table in the layout
/// versions 3 to 5 used. Nothing is dropped from a current file, and running
/// it twice is harmless.
///
/// `history::store::migrate` calls this before any table is renamed, because
/// a rename makes SQLite re-read every trigger in the file, and an old
/// trigger naming a damaged index would fail the rename. A damaged index is
/// dropped too (see [`drop_search_index`]). The rebuild fills a fresh index
/// from the rows, so nothing the old one held is lost.
pub fn drop_old_index(conn: &Connection) -> rusqlite::Result<()> {
    if has_column(conn, "notes", "enhanced_content")? {
        conn.execute_batch(
            "DROP TRIGGER IF EXISTS notes_ai;
             DROP TRIGGER IF EXISTS notes_ad;
             DROP TRIGGER IF EXISTS notes_au;",
        )?;
        drop_search_index(conn, "notes_fts")?;
    }
    Ok(())
}

/// Drop the FTS5 table `index`, even when it is too damaged to open.
///
/// Dropping an FTS5 table means opening it first, and opening it reads the
/// index's config and structure records. With one of those gone (a deleted
/// `_data` row, a dropped `_data` or `_config` table) SQLite refuses the drop
/// with "vtable constructor failed", on every connection that has not already
/// opened the table, so on every start. An index holds nothing the rows it
/// was built from do not, so when the drop fails that way the table's schema
/// row is deleted by hand and its shadow tables are dropped as the plain
/// tables they then are.
///
/// That happens only inside a transaction that is still open (the upgrade's
/// savepoint), so a failure anywhere after it puts the schema row back with
/// everything else, and only for the errors FTS5 gives when it cannot open
/// an index ([`cannot_open`]), which leave that transaction as it was. Any
/// other failure is returned as it is. An interrupt, an I/O error or a full
/// disk can end SQLite's transaction, and past that point each statement
/// here would commit on its own.
///
/// Deleting a schema row needs `PRAGMA writable_schema`, which a connection
/// in defensive mode (`SQLITE_DBCONFIG_DEFENSIVE`) silently ignores; there
/// the delete fails and so does the upgrade, leaving the file as it was. The
/// bundled SQLite is not built with defensive mode on and `open_at` does not
/// turn it on. `RESET` turns schema writing off again and makes this
/// connection read the schema afresh, so it no longer knows the table. A
/// write to the schema table does not change the schema cookie, but dropping
/// the shadow tables does, and so does the table rebuild that follows in the
/// same savepoint, so any other connection reads the new schema once the
/// upgrade commits.
pub(crate) fn drop_search_index(conn: &Connection, index: &str) -> rusqlite::Result<()> {
    let Err(refused) = conn.execute_batch(&format!("DROP TABLE IF EXISTS {index};")) else {
        return Ok(());
    };
    if conn.is_autocommit() || !cannot_open(&refused) {
        return Err(refused);
    }
    tracing::warn!("old search index {index} cannot be opened ({refused}); removing it by hand");
    conn.execute_batch("PRAGMA writable_schema = ON;")?;
    // `rootpage = 0` is a virtual table: it owns no pages, so deleting its
    // row leaves nothing behind but the shadow tables dropped below.
    let unlinked = conn.execute(
        "DELETE FROM sqlite_master WHERE type = 'table' AND name = ?1 AND rootpage = 0",
        params![index],
    );
    conn.execute_batch("PRAGMA writable_schema = OFF; PRAGMA writable_schema = RESET;")?;
    if unlinked? == 0 {
        return Err(refused);
    }
    // Every old index read another table (`content=`), so it never had a
    // `_content` table; a table by that name is not the index's to drop.
    for shadow in ["data", "idx", "docsize", "config"] {
        conn.execute_batch(&format!("DROP TABLE IF EXISTS {index}_{shadow};"))?;
    }
    Ok(())
}

/// Whether `error` is FTS5 failing to open an index whose records are
/// missing or unreadable: "vtable constructor failed", which is
/// `SQLITE_CORRUPT_VTAB` when the structure record or the `_data` table is
/// gone and plain `SQLITE_ERROR` when the `_config` table is, or "invalid
/// fts5 file format", plain `SQLITE_ERROR` (measured on the bundled 3.46.0).
/// Both come from opening the table, before the drop runs, and neither ends
/// a transaction.
fn cannot_open(error: &rusqlite::Error) -> bool {
    use rusqlite::ffi::{SQLITE_CORRUPT_VTAB, SQLITE_ERROR};
    matches!(
        error,
        rusqlite::Error::SqliteFailure(e, _)
            if e.extended_code == SQLITE_ERROR || e.extended_code == SQLITE_CORRUPT_VTAB
    )
}

/// Whether `table` exists and has a column called `column`.
pub(crate) fn has_column(conn: &Connection, table: &str, column: &str) -> rusqlite::Result<bool> {
    conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM pragma_table_info(?1) WHERE name = ?2)",
        params![table, column],
        |r| r.get(0),
    )
}

/// Give the rebuilt `to` table the id counter `from` had reached.
///
/// Both are `AUTOINCREMENT` tables, which promise never to hand out an id
/// again, even one whose row was deleted. Copying rows with their ids only
/// moves the counter up to the highest id copied, so without this a row
/// deleted from the top of the old table would have its id reused.
pub(crate) fn carry_sequence(conn: &Connection, from: &str, to: &str) -> rusqlite::Result<()> {
    let reached: Option<i64> = conn.query_row(
        "SELECT MAX(seq) FROM sqlite_sequence WHERE name = ?1",
        params![from],
        |r| r.get(0),
    )?;
    let Some(reached) = reached else {
        return Ok(());
    };
    let updated = conn.execute(
        "UPDATE sqlite_sequence SET seq = MAX(seq, ?2) WHERE name = ?1",
        params![to, reached],
    )?;
    if updated == 0 {
        conn.execute(
            "INSERT INTO sqlite_sequence (name, seq) VALUES (?1, ?2)",
            params![to, reached],
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    #[test]
    fn the_schema_applies_and_is_idempotent() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(SCHEMA).unwrap();
        conn.execute_batch(SCHEMA).unwrap();
        for table in ["folders", "notes", "notes_fts", "note_actions"] {
            let n: i64 = conn
                .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
                .unwrap_or_else(|e| panic!("{table} missing after migration: {e}"));
            assert_eq!(n, 0, "{table} should be empty");
        }
    }

    /// Two search boxes over the same Indic text must not disagree about what
    /// a token is. Pinned against `history/store.rs`'s source text, the same
    /// way `chat.rs` pins the shipped agent name against `settings.rs`.
    #[test]
    fn the_tokenizer_is_byte_identical_to_the_history_index() {
        const TOKENIZER: &str =
            "tokenize=\"unicode61 remove_diacritics 0 categories 'L* N* Co Mn Mc'\"";
        assert!(SCHEMA.contains(TOKENIZER));
        assert!(
            include_str!("../history/store.rs").contains(TOKENIZER),
            "history's FTS tokenizer changed; notes must move with it"
        );
    }

    /// `'rebuild'` and `'integrity-check'` are the maintenance commands a
    /// stale index is repaired with. They only work while every FTS column is
    /// a real column of the `content=` table — which is why `transcript_text`
    /// is stored rather than derived in a view (see the module doc).
    #[test]
    fn the_index_can_be_rebuilt_and_integrity_checked() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(SCHEMA).unwrap();
        conn.execute(
            "INSERT INTO notes(title, content, polished_body, transcript_text)
             VALUES ('t', 'body', 'tidied body', 'spoken words')",
            [],
        )
        .unwrap();
        conn.execute("INSERT INTO notes_fts(notes_fts) VALUES('rebuild')", [])
            .expect("rebuild");
        conn.execute("INSERT INTO notes_fts(notes_fts) VALUES('integrity-check')", [])
            .expect("integrity-check");
    }

    /// `kind` is a closed vocabulary; anything else is a bug in a caller,
    /// not a value to store and puzzle over later.
    #[test]
    fn kind_is_constrained() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(SCHEMA).unwrap();
        conn.execute("INSERT INTO notes(kind) VALUES ('imported')", [])
            .unwrap();
        assert!(conn
            .execute("INSERT INTO notes(kind) VALUES ('bogus')", [])
            .is_err());
    }

    /// A shipped action is found again by its key, so an empty key would be
    /// a shipped action nothing can find. The table refuses one.
    #[test]
    fn a_shipped_key_is_never_empty() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(SCHEMA).unwrap();
        assert!(conn
            .execute(
                "INSERT INTO note_actions(shipped_key, label, instruction) VALUES ('', 'a', 'b')",
                [],
            )
            .is_err());
        conn.execute(
            "INSERT INTO note_actions(label, instruction) VALUES ('mine', 'do it')",
            [],
        )
        .expect("a person's own action has no key");
    }

    /// A row that names no glyph gets the column's default, which is not the
    /// glyph the Enhance button itself uses.
    #[test]
    fn a_new_action_without_a_glyph_gets_the_default() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(SCHEMA).unwrap();
        conn.execute(
            "INSERT INTO note_actions(label, instruction) VALUES ('mine', 'do it')",
            [],
        )
        .unwrap();
        let glyph: String = conn
            .query_row("SELECT glyph FROM note_actions", [], |r| r.get(0))
            .unwrap();
        assert_eq!(glyph, crate::notes::actions::DEFAULT_GLYPH);
    }

    #[test]
    fn has_column_tells_a_missing_table_from_a_missing_column() {
        let conn = Connection::open_in_memory().unwrap();
        assert!(!has_column(&conn, "notes", "kind").unwrap());
        conn.execute_batch(SCHEMA).unwrap();
        assert!(has_column(&conn, "notes", "kind").unwrap());
        assert!(!has_column(&conn, "notes", "no_such_column").unwrap());
    }

    /// A file of its own under the temp dir, for a test that needs a second
    /// connection to it.
    fn temp_file(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "bs-schema-test-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("index.db")
    }

    /// A damaged index is removed by hand only inside a transaction, where a
    /// failure later on puts it back. Outside one every statement of the
    /// removal would commit by itself, and that is also where SQLite leaves a
    /// connection after an error that ends its transaction. The removal takes
    /// the index's own shadow tables and nothing else: an index over another
    /// table has no `_content` table, so one by that name is not the index's.
    #[test]
    fn a_damaged_index_is_removed_by_hand_only_inside_a_transaction() {
        let path = temp_file("fallback");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE docs (id INTEGER PRIMARY KEY, body TEXT NOT NULL);
                 CREATE VIRTUAL TABLE old_fts USING fts5(body, content='docs', content_rowid='id');
                 INSERT INTO docs (body) VALUES ('kept words');
                 INSERT INTO old_fts(old_fts) VALUES ('rebuild');
                 CREATE TABLE old_fts_content (note TEXT NOT NULL);
                 INSERT INTO old_fts_content (note) VALUES ('not part of the index');
                 DELETE FROM old_fts_data;",
            )
            .unwrap();
        }
        let conn = Connection::open(&path).unwrap();
        let names = |c: &Connection| -> Vec<String> {
            let mut stmt = c
                .prepare("SELECT name FROM sqlite_master WHERE name LIKE 'old_fts%' ORDER BY name")
                .unwrap();
            stmt.query_map([], |r| r.get(0)).unwrap().collect::<Result<_, _>>().unwrap()
        };
        let before = names(&conn);
        assert!(before.iter().any(|n| n == "old_fts"), "control: {before:?}");
        assert!(
            conn.prepare("SELECT rowid FROM old_fts").is_err(),
            "control: the damaged index still opens"
        );

        assert!(drop_search_index(&conn, "old_fts").is_err(), "removed outside a transaction");
        assert_eq!(names(&conn), before, "something was removed outside a transaction");

        conn.execute_batch("BEGIN").unwrap();
        drop_search_index(&conn, "old_fts").unwrap();
        conn.execute_batch("COMMIT").unwrap();
        assert_eq!(names(&conn), ["old_fts_content"]);
        let note: String =
            conn.query_row("SELECT note FROM old_fts_content", [], |r| r.get(0)).unwrap();
        assert_eq!(note, "not part of the index");
        let integrity: String =
            conn.query_row("PRAGMA integrity_check", [], |r| r.get(0)).unwrap();
        assert_eq!(integrity, "ok");
        drop(conn);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    /// Running the reshape on a file that is already current changes no
    /// table, index or trigger.
    #[test]
    fn bringing_a_current_file_current_changes_nothing() {
        let conn = Connection::open_in_memory().unwrap();
        bring_current(&conn).unwrap();
        let schema = |c: &Connection| -> Vec<(String, String)> {
            let mut stmt = c
                .prepare("SELECT name, COALESCE(sql, '') FROM sqlite_master ORDER BY name")
                .unwrap();
            stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
                .unwrap()
                .collect::<Result<_, _>>()
                .unwrap()
        };
        let before = schema(&conn);
        bring_current(&conn).unwrap();
        assert_eq!(schema(&conn), before);
    }
}
