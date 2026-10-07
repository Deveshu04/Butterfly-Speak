//! Schema, migrations, and every SQL statement history runs. Every function
//! here takes `&Connection` rather than owning one, so it's testable against
//! `Connection::open_in_memory()` with no thread involved — the thread
//! ownership lives one layer up, in `mod.rs`.
//!
//! ## Removed text leaves the file, not only the table
//!
//! A delete, a clear, a retention sweep or Home's Edit takes words out of
//! `transcriptions`, and without care they stay readable in three places:
//! the freed pages of `history.db` (the bundled SQLite is built without
//! `SQLITE_SECURE_DELETE`), the `-wal` file, and the FTS5 index itself, which
//! by default records an external-content delete as a tombstone and keeps the
//! original segment, words and all, until a merge. A `VACUUM` does not reach
//! that last one: the segment is a live row of `*_fts_data`.
//!
//! So there are three parts. `open_at` turns on `PRAGMA secure_delete`, which
//! zeroes what a delete frees. Both search indexes have FTS5's own
//! `secure-delete` option turned on ([`SECURE_DELETE`]; persistent, stored in
//! the index's config; SQLite 3.44 or later, the bundled one is 3.46), which
//! removes a deleted row's entries from the segment in place. And every
//! statement that removes text ends with [`scrub_removed_text`], which pushes
//! the zeroed pages through to the file and empties the WAL. Notes share the
//! file and get the same treatment: a deleted note's words should not outlive
//! it either.
//!
//! One known wrinkle: on the bundled SQLite 3.46.0, FTS5's `integrity-check`
//! (and so `PRAGMA integrity_check`) reports "malformed inverted index" for an
//! index with `secure-delete` on once any row has been deleted or changed,
//! although every search still returns the right rows (measured; SQLite 3.50
//! reports no problem for the same steps). A `'rebuild'` clears the report.

use super::entry::{Entry, NewEntry, Outcome};
use rusqlite::{params, Connection, Row};

/// Written to `PRAGMA user_version` (which SQLite keeps in the file header)
/// once a file is in the current layout. See [`migrate`] for what each
/// earlier number meant.
const SCHEMA_VERSION: u32 = 6;

/// The current layout of dictation history: one row per filed dictation,
/// and an external-content FTS5 index over its two texts.
///
/// `outcome` is `'done'` for a dictation whose text was delivered and
/// `'failed'` for one that typed nothing; `error_code` says why, and for a
/// `'done'` row it can say the text stopped short (see `history`'s module
/// doc). `route` is NULL for a plain dictation and names the route otherwise
/// (`routes::Route::history_label`).
///
/// The tokenizer takes the Indic combining marks as letters. `unicode61`'s
/// default token categories are `L* N* Co`, and combining marks are `Mn`/`Mc`,
/// so by default every Devanagari matra, virama, Tamil vowel sign and Bengali
/// kar is a separator: `किताब` indexes as `["क","त","ब"]`, and `"दिन"*`
/// matches दिन, दान, दीन and दुनिया alike. `remove_diacritics` does not help:
/// measured on bundled SQLite 3.46.0, `0` and `1` give identical terms for
/// every Indic word; the option folds precomposed Latin/Greek/Cyrillic
/// accents and nothing else, which is why it stays `0` and keeps `café` and
/// `cafe` apart. With `categories 'L* N* Co Mn Mc'` each word is one token and
/// `"दिन"*` matches दिन alone. `notes::schema` uses the same string.
const TRANSCRIPTIONS: &str = r#"
CREATE TABLE IF NOT EXISTS transcriptions (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    created_at  TEXT    NOT NULL DEFAULT (datetime('now')),
    outcome     TEXT    NOT NULL CHECK (outcome IN ('done', 'failed')),
    error_code  TEXT,
    text        TEXT    NOT NULL,
    raw_text    TEXT,
    words       INTEGER,
    duration_ms INTEGER,
    app         TEXT,
    route       TEXT,
    provider    TEXT,
    model       TEXT
);

-- Listing and the retention sweep both go by time.
CREATE INDEX IF NOT EXISTS transcriptions_by_created ON transcriptions(created_at);

CREATE VIRTUAL TABLE IF NOT EXISTS transcriptions_fts USING fts5(
    text, raw_text,
    content='transcriptions',
    content_rowid='id',
    tokenize="unicode61 remove_diacritics 0 categories 'L* N* Co Mn Mc'"
);

CREATE TRIGGER IF NOT EXISTS transcriptions_index_add AFTER INSERT ON transcriptions BEGIN
    INSERT INTO transcriptions_fts(rowid, text, raw_text) VALUES (new.id, new.text, new.raw_text);
END;

CREATE TRIGGER IF NOT EXISTS transcriptions_index_remove AFTER DELETE ON transcriptions BEGIN
    INSERT INTO transcriptions_fts(transcriptions_fts, rowid, text, raw_text)
    VALUES ('delete', old.id, old.text, old.raw_text);
END;

CREATE TRIGGER IF NOT EXISTS transcriptions_index_replace AFTER UPDATE ON transcriptions BEGIN
    INSERT INTO transcriptions_fts(transcriptions_fts, rowid, text, raw_text)
    VALUES ('delete', old.id, old.text, old.raw_text);
    INSERT INTO transcriptions_fts(rowid, text, raw_text) VALUES (new.id, new.text, new.raw_text);
END;
"#;

/// Both search indexes remove a deleted row's terms in place (see the module
/// doc). The option is stored in each index's own config, so setting it
/// again is harmless.
const SECURE_DELETE: &str = r#"
INSERT INTO transcriptions_fts(transcriptions_fts, rank) VALUES('secure-delete', 1);
INSERT INTO notes_fts(notes_fts, rank) VALUES('secure-delete', 1);
"#;

/// Open (creating if absent) the DB at `path`, set WAL, and bring it to the
/// current layout with [`migrate`].
pub fn open_at(path: &std::path::Path) -> anyhow::Result<Connection> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let conn = Connection::open(path)?;
    // WAL from day one: readers (list/search from a Tauri command) never
    // block a concurrent insert, and vice versa.
    conn.pragma_update(None, "journal_mode", "WAL")?;
    // Zero what a delete frees, so removed text does not linger in free
    // pages. Per connection, so it is set on every open. See the module doc.
    conn.pragma_update(None, "secure_delete", "ON")?;
    migrate(&conn)?;
    Ok(conn)
}

/// Call after any statement that removed or replaced text. `secure_delete`
/// has zeroed the freed space, but the WAL still holds the frames that
/// carried the words; a `TRUNCATE` checkpoint copies the zeroed pages into
/// the database file and empties the WAL. A reader holding the WAL open can
/// make the checkpoint stop short; the next removal tries again, and it
/// never fails the removal itself. A no-op on an in-memory database.
pub(crate) fn scrub_removed_text(conn: &Connection) {
    if let Err(e) = conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(())) {
        tracing::warn!("history checkpoint after a removal failed: {e}");
    }
}

/// Bring the file to [`SCHEMA_VERSION`].
///
/// What the earlier version numbers meant, since files at each of them exist:
///
/// 1. `transcriptions` and its index.
/// 2. Added `learn_candidates` (`learn::candidates::SCHEMA`).
/// 3. Added the notes tables, and rebuilt the history index on the Indic
///    tokenizer.
/// 4. Turned on FTS5 `secure-delete` for both indexes.
/// 5. Purged, once, the free pages older versions had left holding deleted
///    text.
/// 6. Rebuilt `transcriptions`, `notes` and `note_actions` in their current
///    layouts, with both search indexes and their triggers.
///
/// Every file below 6 takes the same path: one transaction that creates
/// whatever is missing and rebuilds whatever is in an older layout (each
/// table's own function decides which by looking at the table), and then,
/// for files from before version 5, the purge (a version 5 file gets the same
/// `VACUUM` to hand back the pages of its dropped tables, but nothing is
/// recorded as owed if that fails). A brand-new file takes it too
/// and simply gets everything created. If any statement in the transaction
/// fails, the file is left exactly as it was and the open fails, which turns
/// history and notes off for the session rather than leaving half an upgrade.
///
/// The purge is best effort, unlike the rest: a `VACUUM` needs room for a full
/// copy of the file, and a nearly full disk must not cost the user their
/// history for the session. So the file is recorded at 4 until a purge
/// succeeds, and the next start runs this again; the tables are already
/// current by then and are left alone. `VACUUM` cannot run inside a
/// transaction; none is open at that point.
fn migrate(conn: &Connection) -> rusqlite::Result<()> {
    let version: u32 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    if version > SCHEMA_VERSION {
        // A newer build wrote this file, and an older build is now reading
        // it (a reinstall of an earlier release). Version 6 renamed columns,
        // so statements written for an earlier layout can fail against a
        // later one. The file is left as it is: nothing here can safely move
        // it backwards, and the newer build will read it again.
        tracing::warn!(
            "history.db is at schema version {version}, newer than this build's \
             {SCHEMA_VERSION}; history and notes may not load until the newer build is reinstalled"
        );
    }
    if version >= SCHEMA_VERSION {
        return Ok(());
    }
    let purge_owed = (1..5).contains(&version);

    in_savepoint(conn, "upgrade", |c| {
        c.execute_batch(crate::learn::candidates::SCHEMA)?;
        // Both old indexes and their triggers go before either table is
        // renamed. A rename makes SQLite re-read every trigger in the file,
        // and an old trigger naming a damaged index (one that plain reads of
        // the file never touch) would fail that re-read and with it the
        // whole upgrade, at every start. A damaged index cannot be dropped
        // the ordinary way either, so it is removed by hand
        // (`notes::schema::drop_search_index`).
        drop_old_transcriptions_index(c)?;
        crate::notes::schema::drop_old_index(c)?;
        bring_transcriptions_current(c)?;
        crate::notes::schema::bring_current(c)?;
        c.execute_batch(SECURE_DELETE)?;
        c.pragma_update(None, "user_version", if purge_owed { 4 } else { SCHEMA_VERSION })
    })?;

    if purge_owed {
        match conn.execute_batch("VACUUM") {
            Ok(()) => conn.pragma_update(None, "user_version", SCHEMA_VERSION)?,
            Err(e) => {
                tracing::warn!("history purge of earlier deletions failed, will retry next start: {e}")
            }
        }
    } else if version == 5 {
        // The rebuild above copied each table and dropped the old one, and
        // those pages stay free inside the file until something compacts it.
        // Secure delete has already zeroed them, so this only gives the space
        // back; if it fails, the file works as it is and nothing is retried.
        if let Err(e) = conn.execute_batch("VACUUM") {
            tracing::warn!("history compaction after the upgrade failed: {e}")
        }
    }
    // Empties the WAL the upgrade (and the purge) filled. A new file has
    // nothing to scrub, but the checkpoint costs nothing there.
    scrub_removed_text(conn);
    Ok(())
}

/// Run `f` inside a savepoint: everything it did is kept if it succeeds and
/// undone if it fails. A savepoint rather than `BEGIN`, so this also nests
/// inside a transaction the caller already holds.
fn in_savepoint(
    conn: &Connection,
    name: &str,
    f: impl FnOnce(&Connection) -> rusqlite::Result<()>,
) -> rusqlite::Result<()> {
    conn.execute_batch(&format!("SAVEPOINT {name}"))?;
    match f(conn) {
        Ok(()) => conn.execute_batch(&format!("RELEASE {name}")),
        Err(e) => {
            // An error that ends SQLite's own transaction (an interrupt, an
            // I/O error, a full disk) has already undone everything, savepoint
            // included.
            if !conn.is_autocommit() {
                let rollback = format!("ROLLBACK TO {name}; RELEASE {name}");
                if let Err(undo) = conn.execute_batch(&rollback) {
                    tracing::error!("history upgrade could not be rolled back: {undo}");
                }
            }
            Err(e)
        }
    }
}

/// Create `transcriptions` and its index, or rebuild a table in the layout
/// versions 1 to 5 used (recognised by its `route_kind` column).
///
/// The rebuild renames the old table aside, creates the current one under the
/// real name, and copies every row across with its id, its time and every
/// field. `status` becomes `outcome`: `'completed'` is `'done'`, and every
/// other value is `'failed'`. That covers `'failed'` itself and `'discarded'`,
/// a state the old schema allowed for dictations the app declined to type and
/// that no build ever filed; had one been, `'failed'` is what it was: nothing
/// was typed. The old index and its triggers are dropped first (see
/// [`drop_old_transcriptions_index`]), and the new index is filled row by row
/// as the copy goes in.
fn bring_transcriptions_current(conn: &Connection) -> rusqlite::Result<()> {
    use crate::notes::schema::{carry_sequence, has_column};
    let was_old = has_column(conn, "transcriptions", "route_kind")?;
    if was_old {
        drop_old_transcriptions_index(conn)?;
        conn.execute_batch("ALTER TABLE transcriptions RENAME TO transcriptions_before_v6;")?;
    }
    conn.execute_batch(TRANSCRIPTIONS)?;
    if was_old {
        conn.execute_batch(
            "INSERT INTO transcriptions
                 (id, created_at, outcome, error_code, text, raw_text, words,
                  duration_ms, app, route, provider, model)
             SELECT id, created_at,
                    CASE status WHEN 'completed' THEN 'done' ELSE 'failed' END,
                    error_code, text, raw_text, words,
                    duration_ms, app, route_kind, provider, model
             FROM transcriptions_before_v6
             ORDER BY id;",
        )?;
        carry_sequence(conn, "transcriptions_before_v6", "transcriptions")?;
        conn.execute_batch("DROP TABLE transcriptions_before_v6;")?;
    }
    Ok(())
}

/// Drop the search index and triggers of a `transcriptions` table in the
/// layout versions 1 to 5 used. Nothing is dropped from a current file, and
/// running it twice is harmless. An index too damaged to open is removed as
/// well (`notes::schema::drop_search_index` says how). The rebuild fills a
/// fresh index from the rows, so nothing the old index held is lost.
fn drop_old_transcriptions_index(conn: &Connection) -> rusqlite::Result<()> {
    use crate::notes::schema::{drop_search_index, has_column};
    if has_column(conn, "transcriptions", "route_kind")? {
        conn.execute_batch(
            "DROP TRIGGER IF EXISTS transcriptions_ai;
             DROP TRIGGER IF EXISTS transcriptions_ad;
             DROP TRIGGER IF EXISTS transcriptions_au;",
        )?;
        drop_search_index(conn, "transcriptions_fts")?;
    }
    Ok(())
}

fn row_to_entry(row: &Row) -> rusqlite::Result<Entry> {
    Ok(Entry {
        id: row.get("id")?,
        text: row.get("text")?,
        raw_text: row.get("raw_text")?,
        created_at: row.get("created_at")?,
        outcome: Outcome::from_sql(&row.get::<_, String>("outcome")?),
        error_code: row.get("error_code")?,
        provider: row.get("provider")?,
        model: row.get("model")?,
        duration_ms: row.get("duration_ms")?,
        app: row.get("app")?,
        words: row.get("words")?,
        route: row.get("route")?,
    })
}

const COLUMNS: &str =
    "id, text, raw_text, created_at, outcome, error_code, provider, model, duration_ms, app, words, route";

pub fn insert(conn: &Connection, entry: &NewEntry) -> rusqlite::Result<i64> {
    conn.execute(
        "INSERT INTO transcriptions
            (text, raw_text, outcome, error_code, provider, model, duration_ms, app, words, route)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        params![
            entry.text,
            entry.raw_text,
            entry.outcome.as_sql(),
            entry.error_code,
            entry.provider,
            entry.model,
            entry.duration_ms.map(|d| d as i64),
            entry.app,
            entry.words.map(|w| w as i64),
            entry.route,
        ],
    )?;
    Ok(conn.last_insert_rowid())
}

/// Most-recent-first page of every filed dictation, failed ones included:
/// History shows what did not work as well as what did.
pub fn list(conn: &Connection, page: u32, page_size: u32) -> rusqlite::Result<Vec<Entry>> {
    let page_size = page_size.clamp(1, 500) as i64;
    let offset = i64::from(page) * page_size;
    let sql = format!(
        "SELECT {COLUMNS} FROM transcriptions
         ORDER BY created_at DESC, id DESC
         LIMIT ?1 OFFSET ?2"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params![page_size, offset], row_to_entry)?;
    rows.collect()
}

/// The FTS5 `MATCH` expression for a search-box string, or `None` when the
/// string holds nothing searchable. History search shares the notes builder,
/// so both search boxes read the same input the same way.
pub fn build_search_query(input: &str) -> Option<String> {
    let query = crate::notes::search::sanitize_query(input);
    (!query.is_empty()).then_some(query)
}

/// Full-text search, most-relevant-first. Returns an empty result (not an
/// error) for a query with no searchable tokens, so an all-punctuation
/// search box just shows nothing instead of surfacing a SQL error to the UI.
pub fn search(conn: &Connection, raw_query: &str, limit: u32) -> rusqlite::Result<Vec<Entry>> {
    let Some(fts_query) = build_search_query(raw_query) else {
        return Ok(Vec::new());
    };
    let limit = limit.clamp(1, 500) as i64;
    // Deliberately unaliased: SQLite's `tbl MATCH expr` / `bm25(tbl)` special
    // forms only reliably resolve `tbl` against the FTS5 virtual table's own
    // name in a join, not an arbitrary alias for it (aliasing it, e.g. `f`,
    // raised "no such column: f" here against the bundled SQLite version).
    let sql = format!(
        "SELECT {cols} FROM transcriptions_fts
         JOIN transcriptions t ON t.id = transcriptions_fts.rowid
         WHERE transcriptions_fts MATCH ?1
         ORDER BY bm25(transcriptions_fts) ASC
         LIMIT ?2",
        cols = COLUMNS
            .split(", ")
            .map(|c| format!("t.{c}"))
            .collect::<Vec<_>>()
            .join(", "),
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params![fts_query, limit], row_to_entry)?;
    rows.collect()
}

/// [`delete_returning`] as a yes or no, for the tests.
#[cfg(test)]
pub fn delete(conn: &Connection, id: i64) -> rusqlite::Result<bool> {
    Ok(delete_returning(conn, id)?.is_some())
}

/// Every function below that removes or replaces text ends with
/// [`scrub_removed_text`], so the words leave the file too (see the module
/// doc).
///
/// Deletes one row and hands back its `text` and `raw_text`, so the caller
/// can forget other copies of those words. `None` when no row had that id.
/// A delete removes the row; there is nothing else to filter.
pub fn delete_returning(
    conn: &Connection,
    id: i64,
) -> rusqlite::Result<Option<(String, Option<String>)>> {
    let removed = {
        let mut stmt =
            conn.prepare("DELETE FROM transcriptions WHERE id = ?1 RETURNING text, raw_text")?;
        let mut rows = stmt.query_map(params![id], |r| Ok((r.get(0)?, r.get(1)?)))?;
        // Stepped to the end, so the delete itself is finished before the
        // statement is dropped and the scrub below runs.
        let first = rows.next().transpose()?;
        for rest in rows {
            rest?;
        }
        first
    };
    if removed.is_some() {
        scrub_removed_text(conn);
    }
    Ok(removed)
}

/// Replace a row's text with the user's correction (Home's Edit). `words` is
/// recounted from the new text the same way `Controller::record_history`
/// counts it; `raw_text`, the verbatim transcript, is left as it was. The
/// `transcriptions_index_replace` trigger re-indexes the row. `false` when no
/// row has that id (deleted from History, or swept by retention, in the
/// meantime).
pub fn update_text(conn: &Connection, id: i64, text: &str) -> rusqlite::Result<bool> {
    let words = text.split_whitespace().count() as i64;
    let changed = conn.execute(
        "UPDATE transcriptions SET text = ?1, words = ?2 WHERE id = ?3",
        params![text, words, id],
    )?;
    if changed > 0 {
        scrub_removed_text(conn);
    }
    Ok(changed > 0)
}

pub fn clear(conn: &Connection) -> rusqlite::Result<u32> {
    let changed = conn.execute("DELETE FROM transcriptions", [])?;
    if changed > 0 {
        scrub_removed_text(conn);
    }
    Ok(changed as u32)
}

/// Hard-deletes every row older than `keep_days`. The cutoff is computed once
/// and reused for the delete, so what gets purged can't drift from what a
/// caller might have already queried against the same cutoff. `keep_days == 0`
/// means "forever" (the settled default) and is always a no-op — never call
/// this without checking that first.
pub fn sweep_expired(conn: &Connection, keep_days: u32) -> rusqlite::Result<u32> {
    if keep_days == 0 {
        return Ok(0);
    }
    let cutoff: String = conn.query_row(
        "SELECT datetime('now', ?1)",
        params![format!("-{keep_days} days")],
        |row| row.get(0),
    )?;
    let changed = conn.execute(
        "DELETE FROM transcriptions WHERE created_at < ?1",
        params![cutoff],
    )?;
    if changed > 0 {
        scrub_removed_text(conn);
    }
    Ok(changed as u32)
}

#[cfg(test)]
mod tests {
    use super::super::legacy::{self, rows};
    use super::*;
    use rusqlite::types::Value;

    fn db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        conn
    }

    fn entry(text: &str) -> NewEntry {
        NewEntry {
            text: text.into(),
            ..Default::default()
        }
    }

    fn user_version(conn: &Connection) -> u32 {
        conn.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap()
    }

    #[test]
    fn migrate_sets_user_version_and_is_idempotent() {
        let conn = db();
        assert_eq!(user_version(&conn), SCHEMA_VERSION);
        // Running again against an already-migrated connection must not error
        // or re-create anything.
        migrate(&conn).unwrap();
    }

    /// A history file written by the first build must reach the current
    /// layout without losing a single transcription, its search index
    /// included.
    #[test]
    fn a_version_1_file_migrates_losslessly_to_the_current_version() {
        let conn = Connection::open_in_memory().unwrap();
        legacy::write(&conn, 1);
        legacy::insert_text(&conn, "written before the upgrade");

        migrate(&conn).unwrap();

        assert_eq!(user_version(&conn), SCHEMA_VERSION);
        let rows = list(&conn, 0, 10).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].text, "written before the upgrade");
        assert_eq!(rows[0].outcome, Outcome::Done);
        assert_eq!(
            search(&conn, "upgrade", 10).unwrap().len(),
            1,
            "the search index must survive the migration too"
        );
        // The tables later versions added exist and are empty.
        for table in ["learn_candidates", "notes", "folders", "note_actions"] {
            let n: i64 = conn
                .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
                .unwrap_or_else(|e| panic!("{table} missing after the upgrade: {e}"));
            assert_eq!(n, 0);
        }

        // And re-running changes nothing.
        migrate(&conn).unwrap();
        assert_eq!(list(&conn, 0, 10).unwrap().len(), 1);
    }

    /// The same guarantee one rung up: a file left by a version 2 build
    /// gains the whole notes schema without losing a transcription, its
    /// search index, or a learn candidate.
    #[test]
    fn a_version_2_file_migrates_losslessly_to_the_current_version() {
        let conn = Connection::open_in_memory().unwrap();
        legacy::write(&conn, 2);
        legacy::insert_text(&conn, "written before the notes upgrade");
        conn.execute(
            "INSERT INTO learn_candidates (from_word, to_word, count, last_seen, session_id)
             VALUES ('Vaibav', 'Vaibhav', 1, 1800000000, 's1')",
            [],
        )
        .unwrap();

        migrate(&conn).unwrap();

        assert_eq!(user_version(&conn), SCHEMA_VERSION);
        let rows = list(&conn, 0, 10).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].text, "written before the notes upgrade");
        assert_eq!(
            search(&conn, "upgrade", 10).unwrap().len(),
            1,
            "the search index must survive the migration too"
        );
        let candidates: i64 = conn
            .query_row("SELECT COUNT(*) FROM learn_candidates", [], |r| r.get(0))
            .unwrap();
        assert_eq!(candidates, 1, "v2's rows are not the notes migration's to touch");

        // The new tables exist, are empty, and the notes index works.
        for table in ["notes", "folders", "note_actions"] {
            let n: i64 = conn
                .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
                .unwrap_or_else(|e| panic!("{table} missing after the upgrade: {e}"));
            assert_eq!(n, 0);
        }
        let id = crate::notes::create_note(
            &conn,
            &crate::notes::NewNote {
                title: Some("first note on an upgraded file".into()),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(
            crate::notes::search_notes(&conn, "upgraded", 10).unwrap().len(),
            1
        );

        // And re-running changes nothing.
        migrate(&conn).unwrap();
        assert_eq!(list(&conn, 0, 10).unwrap().len(), 1);
        assert!(crate::notes::get_note(&conn, id).unwrap().is_some());
    }

    /// A version 1 file's index was built on the old tokenizer, which split
    /// Indic words at every vowel sign. After the upgrade it is the current
    /// one, so a search tells दिन from दान.
    #[test]
    fn a_version_1_index_is_rebuilt_on_the_current_tokenizer() {
        let conn = Connection::open_in_memory().unwrap();
        legacy::write(&conn, 1);
        legacy::insert_text(&conn, "दिन");
        legacy::insert_text(&conn, "दान");

        migrate(&conn).unwrap();

        let found: Vec<String> = search(&conn, "दिन", 10)
            .unwrap()
            .into_iter()
            .map(|e| e.text)
            .collect();
        assert_eq!(found, vec!["दिन".to_string()]);
    }

    // -----------------------------------------------------------------------
    // Version 5 to 6, on a file with something of everything in it.
    // -----------------------------------------------------------------------

    /// What a person might search for after the upgrade, typed or pasted.
    const QUERIES: &[&str] = &[
        "standup",
        "budget",
        "tidied",
        "harbour",
        "spoken",
        "translated",
        "remarks",
        "\u{091C}\u{093C}\u{0930}\u{0942}\u{0930}\u{0940}", // ज़रूरी typed: ज + nukta
        "\u{095B}\u{0930}\u{0942}\u{0930}\u{0940}",         // ज़रूरी pasted: one code point
        "\u{092B}\u{093C}\u{094B}\u{0928}",                 // फ़ोन typed
        "nothing matches this",
    ];

    /// The ids each index finds for each of [`QUERIES`], straight from the
    /// index, so the same function reads a version 5 file and a current one.
    fn index_hits(conn: &Connection) -> Vec<(Vec<i64>, Vec<i64>)> {
        let hits = |table: &str, q: &str| -> Vec<i64> {
            let query = crate::notes::search::sanitize_query(q);
            if query.is_empty() {
                return Vec::new();
            }
            let mut stmt = conn
                .prepare(&format!(
                    "SELECT rowid FROM {table} WHERE {table} MATCH ?1 ORDER BY rowid"
                ))
                .unwrap();
            stmt.query_map(params![query], |r| r.get(0))
                .unwrap()
                .collect::<Result<_, _>>()
                .unwrap()
        };
        QUERIES
            .iter()
            .map(|q| (hits("transcriptions_fts", q), hits("notes_fts", q)))
            .collect()
    }

    fn sorted(mut ids: Vec<i64>) -> Vec<i64> {
        ids.sort_unstable();
        ids
    }

    /// The whole schema, as SQLite stores it.
    fn schema(conn: &Connection) -> Vec<Vec<Value>> {
        rows(
            conn,
            "SELECT type, name, tbl_name, COALESCE(sql, '') FROM sqlite_master ORDER BY type, name",
        )
    }

    /// Foreign keys, the file and both search indexes all check clean.
    fn assert_file_is_sound(conn: &Connection) {
        let problems = rows(conn, "PRAGMA foreign_key_check");
        assert!(problems.is_empty(), "foreign key problems: {problems:?}");
        let integrity: String = conn
            .query_row("PRAGMA integrity_check", [], |r| r.get(0))
            .unwrap();
        assert_eq!(integrity, "ok");
        for index in ["transcriptions_fts", "notes_fts"] {
            conn.execute(&format!("INSERT INTO {index}({index}) VALUES('integrity-check')"), [])
                .unwrap_or_else(|e| panic!("{index} is out of step with its table: {e}"));
        }
    }

    /// Every row of a current file, column for column as
    /// [`legacy::v5_rows_in_current_terms`] reads a version 5 one.
    fn current_rows(conn: &Connection) -> legacy::Tables {
        legacy::Tables {
            dictations: rows(
                conn,
                "SELECT id, created_at, outcome, error_code, text, raw_text, words,
                        duration_ms, app, route, provider, model
                 FROM transcriptions ORDER BY id",
            ),
            notes: rows(
                conn,
                "SELECT id, kind, folder_id, title, content, imported_file,
                        audio_seconds, transcript_json, transcript_text,
                        polished_body, polish_prompt, polished_from_hash,
                        created_at, updated_at
                 FROM notes ORDER BY id",
            ),
            actions: rows(
                conn,
                "SELECT id, shipped_key, position, label, summary, instruction, glyph,
                        created_at, updated_at
                 FROM note_actions ORDER BY id",
            ),
            folders: rows(conn, "SELECT * FROM folders ORDER BY id"),
            candidates: rows(conn, "SELECT * FROM learn_candidates ORDER BY from_word, to_word"),
        }
    }

    /// Every table, index and trigger, with every column of every table.
    fn layout(conn: &Connection) -> Vec<Vec<Value>> {
        rows(
            conn,
            "SELECT m.type, m.name, m.tbl_name, p.name, p.type, p.\"notnull\", p.dflt_value, p.pk
             FROM sqlite_master m
             LEFT JOIN pragma_table_info(m.name) p ON m.type = 'table'
             WHERE m.name NOT LIKE 'sqlite_%'
             ORDER BY m.type, m.name, p.cid",
        )
    }

    /// The upgrade a real file takes: every row survives with every field,
    /// renamed where the column was renamed; every search finds the same rows
    /// it found before; foreign keys and both indexes check out; id counters
    /// stay ahead of every id ever used; and opening the file again changes
    /// nothing.
    #[test]
    fn a_version_5_file_upgrades_with_every_row_and_every_search_intact() {
        let dir = temp_db_dir("v5-upgrade");
        let path = dir.join("history.db");
        let (ids, expected, before_hits) = {
            let conn = Connection::open(&path).unwrap();
            conn.pragma_update(None, "journal_mode", "WAL").unwrap();
            legacy::write(&conn, 5);
            let ids = legacy::fill_v5(&conn);
            (ids, legacy::v5_rows_in_current_terms(&conn), index_hits(&conn))
        };
        assert!(
            before_hits.iter().any(|(t, n)| !t.is_empty() && !n.is_empty()),
            "control: the queries find something before the upgrade"
        );
        assert_eq!(expected.dictations.len(), 4, "the fixture files four dictations");
        assert_eq!(expected.notes.len(), 4, "the fixture keeps four notes");
        assert_eq!(expected.actions.len(), 4, "the fixture keeps four actions");

        let conn = open_at(&path).unwrap();
        assert_eq!(user_version(&conn), SCHEMA_VERSION);

        // Every row, every field: `status` read as `outcome`, a note's type as
        // its `kind`, and an action's key kept only where it was shipped.
        assert_eq!(current_rows(&conn), expected);

        // Every search finds what it found before.
        assert_eq!(index_hits(&conn), before_hits);
        for (q, (dictation_hits, note_hits)) in QUERIES.iter().zip(&before_hits) {
            let found = search(&conn, q, 50).unwrap().into_iter().map(|e| e.id).collect();
            assert_eq!(&sorted(found), dictation_hits, "History, query {q:?}");
            let found = crate::notes::search_notes(&conn, q, 50)
                .unwrap()
                .into_iter()
                .map(|n| n.id)
                .collect();
            assert_eq!(&sorted(found), note_hits, "Notes, query {q:?}");
        }

        assert_file_is_sound(&conn);

        // The rows read back through the app's own code as they should.
        let failed = list(&conn, 0, 50).unwrap().into_iter().find(|e| e.id == ids.failed).unwrap();
        assert_eq!(failed.outcome, Outcome::Failed);
        assert_eq!(failed.error_code.as_deref(), Some("connection-lost"));
        let translated =
            list(&conn, 0, 50).unwrap().into_iter().find(|e| e.id == ids.translated).unwrap();
        assert_eq!(translated.route.as_deref(), Some("translation"));
        assert_eq!(translated.outcome, Outcome::Done);
        let polished = crate::notes::get_note(&conn, ids.unfiled_polished).unwrap().unwrap();
        assert_eq!(polished.folder_id, None);
        assert_eq!(polished.polished_body.as_deref(), Some("- Milk\n- Tidied harbour pass"));
        assert_eq!(polished.polish_prompt.as_deref(), Some("List every errand."));
        assert_eq!(polished.polished_from_hash.as_deref(), Some("abc123"));
        let imported = crate::notes::get_note(&conn, ids.imported).unwrap().unwrap();
        assert_eq!(imported.kind, "imported");
        assert_eq!(imported.imported_file.as_deref(), Some("talk.wav"));
        assert_eq!(imported.audio_seconds, Some(312.5));
        let filed = crate::notes::get_note(&conn, ids.filed).unwrap().unwrap();
        assert_eq!((filed.kind.as_str(), filed.folder_id), ("written", Some(ids.work)));
        assert_eq!(
            crate::notes::list_notes(&conn, Some(None), 0)
                .unwrap()
                .iter()
                .map(|n| n.id)
                .collect::<Vec<_>>(),
            vec![ids.imported, ids.unfiled_polished],
            "the unfiled bucket holds the same notes"
        );
        assert!(crate::notes::get_note(&conn, ids.nukta_note).unwrap().is_some());

        // The menu treats each action as it did before: the renamed shipped
        // row is still the shipped one, the person's own is theirs, and the
        // retired ones go the way retirement always sent them.
        let menu = crate::notes::actions::list_actions(&conn).unwrap();
        let find = |id: i64| menu.iter().find(|a| a.id == id);
        let shipped = find(ids.renamed_shipped).expect("the renamed shipped action");
        assert!(shipped.shipped);
        assert_eq!(shipped.shipped_key.as_deref(), Some("tidy_up"));
        assert_eq!((shipped.label.as_str(), shipped.glyph.as_str()), ("My tidy-up", "sparkles"));
        let own = find(ids.user_action).expect("the person's own action");
        assert!(!own.shipped && own.shipped_key.is_none());
        assert_eq!((own.glyph.as_str(), own.position), ("clock", 3));
        let kept = find(ids.retired_edited).expect("an edited retired action is kept");
        assert!(!kept.shipped);
        assert!(find(ids.retired_untouched).is_none(), "an untouched retired action goes");
        assert_eq!(menu.iter().filter(|a| a.shipped).count(), 1, "no second tidy-up");

        // An id is never handed out twice, even one whose row was deleted.
        let next = insert(&conn, &entry("after the upgrade")).unwrap();
        assert!(next > ids.deleted_dictation, "{next} reuses a deleted dictation's id");
        let next = crate::notes::create_note(&conn, &Default::default()).unwrap();
        assert!(next > ids.deleted_note, "{next} reuses a deleted note's id");
        let next = crate::notes::actions::create_action(
            &conn,
            &crate::notes::actions::NewAction {
                label: "New".into(),
                instruction: "Do it.".into(),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(next.id > ids.deleted_action, "{} reuses a deleted action's id", next.id);
        assert_eq!(next.glyph, crate::notes::actions::DEFAULT_GLYPH);
        assert_ne!(next.glyph, "sparkles", "a new action never wears the Enhance glyph");
        let plain = list(&conn, 0, 50).unwrap().into_iter().find(|e| e.id == ids.dictation).unwrap();
        assert_eq!(plain.created_at, "2026-03-01 08:15:00");
        assert_eq!((plain.outcome, plain.route.as_deref()), (Outcome::Done, None));
        let typed_nukta = "\u{091C}\u{093C}\u{0930}\u{0942}\u{0930}\u{0940}";
        let found: Vec<i64> =
            search(&conn, typed_nukta, 10).unwrap().into_iter().map(|e| e.id).collect();
        assert_eq!(found, vec![ids.nukta_dictation]);

        // Opening it again is a no-op.
        let schema_after = schema(&conn);
        let everything = |c: &Connection| {
            (
                rows(c, "SELECT * FROM transcriptions ORDER BY id"),
                rows(c, "SELECT * FROM notes ORDER BY id"),
                rows(c, "SELECT * FROM note_actions ORDER BY id"),
                rows(c, "SELECT * FROM sqlite_sequence ORDER BY name"),
            )
        };
        let data_after = everything(&conn);
        drop(conn);
        let conn = open_at(&path).unwrap();
        assert_eq!(schema(&conn), schema_after);
        assert_eq!(everything(&conn), data_after);
        drop(conn);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// No table, index or trigger is named the way an earlier layout named
    /// it, and nothing from the rebuild is left behind.
    #[test]
    fn an_upgraded_file_has_the_same_schema_as_a_new_one() {
        let upgraded = Connection::open_in_memory().unwrap();
        legacy::write(&upgraded, 5);
        legacy::fill_v5(&upgraded);
        migrate(&upgraded).unwrap();
        let fresh = db();

        assert_eq!(layout(&upgraded), layout(&fresh));
        let names = |c: &Connection, which: &str| -> Vec<String> {
            rows(c, &format!("SELECT name FROM sqlite_master WHERE {which} ORDER BY name"))
                .into_iter()
                .map(|r| match &r[0] {
                    Value::Text(s) => s.clone(),
                    other => panic!("{other:?}"),
                })
                .collect()
        };
        // Every name a version 5 file has and a new file does not is gone.
        let old_layout = Connection::open_in_memory().unwrap();
        legacy::write(&old_layout, 5);
        let current = names(&fresh, "1");
        let retired: Vec<String> =
            names(&old_layout, "1").into_iter().filter(|n| !current.contains(n)).collect();
        assert!(!retired.is_empty(), "control: version 6 renames indexes and triggers");
        let after = names(&upgraded, "1");
        for gone in &retired {
            assert!(!after.contains(gone), "{gone} is still there");
        }
        let ours = [
            "notes_index_add",
            "notes_index_remove",
            "notes_index_replace",
            "transcriptions_index_add",
            "transcriptions_index_remove",
            "transcriptions_index_replace",
        ];
        assert_eq!(names(&fresh, "type = 'trigger'"), ours);
        assert_eq!(names(&upgraded, "type = 'trigger'"), ours);
        assert!(
            !after.iter().any(|n| n.ends_with("_before_v6")),
            "a rebuild left its old table behind: {after:?}"
        );
    }

    /// If any step of the upgrade fails, the file is the version 5 file it
    /// was: same schema, same rows, same searches, same integrity report.
    /// Here the old actions table cannot be moved aside because something
    /// already has the name, which fails the upgrade after both other tables
    /// were already rebuilt.
    ///
    /// The integrity report is compared rather than required to be clean:
    /// the bundled SQLite (3.46.0) reports "malformed inverted index" for an
    /// index with `secure-delete` on once any row has been deleted or
    /// changed, although every search still answers correctly, and the
    /// fixture deletes rows the way a real version 5 file has. The upgrade
    /// rebuilds both indexes, so the upgraded file checks clean.
    #[test]
    fn a_failed_upgrade_leaves_the_version_5_file_as_it_was() {
        let dir = temp_db_dir("v5-rollback");
        let path = dir.join("history.db");
        let snapshot = |c: &Connection| {
            (
                user_version(c),
                schema(c),
                rows(c, "SELECT * FROM transcriptions ORDER BY id"),
                rows(c, "SELECT * FROM notes ORDER BY id"),
                rows(c, "SELECT * FROM note_actions ORDER BY id"),
                rows(c, "SELECT * FROM sqlite_sequence ORDER BY name"),
                index_hits(c),
                rows(c, "PRAGMA integrity_check"),
                rows(c, "PRAGMA foreign_key_check"),
            )
        };
        let before = {
            let conn = Connection::open(&path).unwrap();
            conn.pragma_update(None, "journal_mode", "WAL").unwrap();
            legacy::write(&conn, 5);
            legacy::fill_v5(&conn);
            conn.execute_batch("CREATE TABLE note_actions_before_v6 (x)").unwrap();
            snapshot(&conn)
        };

        assert!(open_at(&path).is_err(), "the upgrade should have failed");

        let conn = Connection::open(&path).unwrap();
        assert_eq!(snapshot(&conn), before);
        assert_eq!(user_version(&conn), 5);
        assert!(before.8.is_empty(), "control: the version 5 file has no foreign key problems");

        // With the obstacle gone, the next start upgrades it.
        conn.execute_batch("DROP TABLE note_actions_before_v6").unwrap();
        drop(conn);
        let conn = open_at(&path).unwrap();
        assert_eq!(user_version(&conn), SCHEMA_VERSION);
        assert_file_is_sound(&conn);
        drop(conn);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A version 5 file whose search index is damaged still answers plain
    /// reads, so the upgrade must not be what turns history and notes off.
    ///
    /// The app opens the file on a new connection, and there dropping an FTS5
    /// table means opening it first, which a damaged one cannot be. So each
    /// damage is done to a file on disk, the file is closed, and it is
    /// upgraded the way a start upgrades it: through `open_at`. Every row
    /// arrives, and every search finds what it found before the damage, from
    /// new indexes built from the rows.
    #[test]
    fn a_version_5_file_with_a_damaged_search_index_still_upgrades() {
        for index in ["notes_fts", "transcriptions_fts"] {
            for damage in [
                format!("DELETE FROM {index}_data"),
                format!("DROP TABLE {index}_data"),
                format!("DROP TABLE {index}_config"),
            ] {
                let dir = temp_db_dir("v5-damaged");
                let path = dir.join("history.db");
                let (ids, expected, hits) = {
                    let conn = Connection::open(&path).unwrap();
                    conn.pragma_update(None, "journal_mode", "WAL").unwrap();
                    legacy::write(&conn, 5);
                    let ids = legacy::fill_v5(&conn);
                    let before = (ids, legacy::v5_rows_in_current_terms(&conn), index_hits(&conn));
                    conn.execute_batch(&damage).unwrap();
                    before
                };
                {
                    let conn = Connection::open(&path).unwrap();
                    let opened = conn.prepare(&format!("SELECT rowid FROM {index}"));
                    assert!(opened.is_err(), "control, {damage}: a new connection opens {index}");
                }

                let conn = open_at(&path).unwrap_or_else(|e| panic!("{damage}: {e}"));

                assert_eq!(user_version(&conn), SCHEMA_VERSION, "{damage}");
                assert_eq!(current_rows(&conn), expected, "{damage}");
                assert_eq!(index_hits(&conn), hits, "{damage}");
                let found: Vec<i64> =
                    search(&conn, "standup", 10).unwrap().into_iter().map(|e| e.id).collect();
                assert_eq!(found, vec![ids.dictation], "{damage}");
                let found: Vec<i64> = crate::notes::search_notes(&conn, "budget", 10)
                    .unwrap()
                    .into_iter()
                    .map(|n| n.id)
                    .collect();
                assert_eq!(found, vec![ids.filed], "{damage}");
                assert_file_is_sound(&conn);
                assert_eq!(layout(&conn), layout(&db()), "{damage}: part of the old index is left");
                drop(conn);

                // The next start finds a current file and leaves it as it is.
                let conn = open_at(&path).unwrap_or_else(|e| panic!("{damage}, reopened: {e}"));
                assert_eq!(index_hits(&conn), hits, "{damage}, reopened");
                drop(conn);
                let _ = std::fs::remove_dir_all(&dir);
            }
        }
    }

    /// Interrupts the statement that drops the FTS5 table `index`, at the
    /// progress handler's `at`-th call once that statement is prepared. Where
    /// the interrupt lands in the drop's own write, SQLite ends the whole
    /// transaction and leaves the connection in autocommit, as it does after
    /// an I/O error or a full disk; where it lands in a read the drop runs
    /// first, the transaction stays open.
    struct InterruptDrop {
        index: std::ffi::CString,
        at: u32,
        armed: std::cell::Cell<bool>,
        calls: std::cell::Cell<u32>,
        fired: std::cell::Cell<bool>,
    }

    impl InterruptDrop {
        fn new(index: &str, at: u32) -> Self {
            InterruptDrop {
                index: std::ffi::CString::new(index).unwrap(),
                at,
                armed: Default::default(),
                calls: Default::default(),
                fired: Default::default(),
            }
        }

        /// Arms on the authorizer's report of the drop, then counts the
        /// progress handler's calls and interrupts once, at the `at`-th.
        /// `self` must outlive `conn`.
        fn install(&self, conn: &Connection) {
            use rusqlite::ffi;
            use std::os::raw::{c_char, c_int, c_void};

            unsafe extern "C" fn arm(
                ctx: *mut c_void,
                action: c_int,
                table: *const c_char,
                _: *const c_char,
                _: *const c_char,
                _: *const c_char,
            ) -> c_int {
                let hook = &*(ctx as *const InterruptDrop);
                if action == ffi::SQLITE_DROP_VTABLE
                    && !table.is_null()
                    && std::ffi::CStr::from_ptr(table) == hook.index.as_c_str()
                {
                    hook.armed.set(true);
                }
                ffi::SQLITE_OK
            }

            unsafe extern "C" fn interrupt(ctx: *mut c_void) -> c_int {
                let hook = &*(ctx as *const InterruptDrop);
                if !hook.armed.get() || hook.fired.get() {
                    return 0;
                }
                hook.calls.set(hook.calls.get() + 1);
                hook.fired.set(hook.calls.get() >= hook.at);
                c_int::from(hook.fired.get())
            }

            let ctx = self as *const InterruptDrop as *mut c_void;
            // SAFETY: both callbacks only read `self` through `ctx`, and the
            // caller keeps `self` alive for as long as `conn` is open.
            unsafe {
                ffi::sqlite3_set_authorizer(conn.handle(), Some(arm), ctx);
                ffi::sqlite3_progress_handler(conn.handle(), 1, Some(interrupt), ctx);
            }
        }
    }

    /// An interrupt in the drop of an old index fails the upgrade, wherever
    /// in the drop it lands. Where it ends SQLite's transaction (as an I/O
    /// error or a full disk would), the upgrade must not carry on one
    /// committed statement at a time; where it does not, it is still no
    /// reason to remove the index by hand. Either way the open fails, the file
    /// is the version 5 file it was, and the next start upgrades it with
    /// every row.
    #[test]
    fn an_interrupted_index_drop_leaves_the_version_5_file_as_it_was() {
        let snapshot = |c: &Connection| {
            (
                user_version(c),
                schema(c),
                rows(c, "SELECT * FROM transcriptions ORDER BY id"),
                rows(c, "SELECT * FROM notes ORDER BY id"),
                rows(c, "SELECT * FROM note_actions ORDER BY id"),
                rows(c, "SELECT * FROM folders ORDER BY id"),
                rows(c, "SELECT * FROM sqlite_sequence ORDER BY name"),
                index_hits(c),
            )
        };
        for index in ["transcriptions_fts", "notes_fts"] {
            let dir = temp_db_dir("v5-interrupted");
            let template = dir.join("template.db");
            let (expected, before) = {
                let conn = Connection::open(&template).unwrap();
                conn.pragma_update(None, "journal_mode", "WAL").unwrap();
                legacy::write(&conn, 5);
                legacy::fill_v5(&conn);
                (legacy::v5_rows_in_current_terms(&conn), snapshot(&conn))
            };
            let mut ended = 0;
            for at in 1..=6 {
                let copy = |name: &str| {
                    let path = dir.join(format!("{name}-{at}.db"));
                    std::fs::copy(&template, &path).unwrap();
                    path
                };

                // Whether an interrupt at this point ends SQLite's transaction.
                let probe = InterruptDrop::new(index, at);
                {
                    let conn = Connection::open(copy("probe")).unwrap();
                    probe.install(&conn);
                    conn.execute_batch("SAVEPOINT probe").unwrap();
                    assert!(conn.execute_batch(&format!("DROP TABLE {index}")).is_err());
                    ended += usize::from(conn.is_autocommit());
                }

                let path = copy("history");
                let hook = InterruptDrop::new(index, at);
                {
                    let conn = Connection::open(&path).unwrap();
                    conn.pragma_update(None, "secure_delete", "ON").unwrap();
                    hook.install(&conn);
                    let Err(failed) = migrate(&conn) else {
                        panic!("{index} at {at}: the interrupted upgrade went through");
                    };
                    assert!(hook.fired.get(), "control: the drop of {index} was not interrupted");
                    assert_eq!(
                        failed.sqlite_error_code(),
                        Some(rusqlite::ErrorCode::OperationInterrupted),
                        "{index} at {at}: {failed}"
                    );
                }
                assert_eq!(snapshot(&Connection::open(&path).unwrap()), before, "{index} at {at}");

                let conn =
                    open_at(&path).unwrap_or_else(|e| panic!("{index} at {at}, next start: {e}"));
                assert_eq!(user_version(&conn), SCHEMA_VERSION, "{index} at {at}");
                assert_eq!(current_rows(&conn), expected, "{index} at {at}");
                assert_file_is_sound(&conn);
            }
            assert!(ended > 0, "control: no interrupt in the {index} drop ended the transaction");
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    /// A note filed under a folder that is gone can only exist if foreign
    /// keys were off at some point. It must not fail the upgrade (which would
    /// turn notes off at every start); it arrives unfiled.
    #[test]
    fn a_note_whose_folder_is_gone_arrives_unfiled() {
        let conn = Connection::open_in_memory().unwrap();
        legacy::write(&conn, 5);
        conn.pragma_update(None, "foreign_keys", false).unwrap();
        conn.execute("INSERT INTO notes (folder_id, title) VALUES (77, 'orphan')", [])
            .unwrap();
        conn.pragma_update(None, "foreign_keys", true).unwrap();

        migrate(&conn).unwrap();

        let notes = crate::notes::list_notes(&conn, Some(None), 0).unwrap();
        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0].title, "orphan");
        assert_file_is_sound(&conn);
    }

    /// Retention is a dictation-history feature. A note is a document the user
    /// wrote; nothing about "keep transcriptions for N days" says anything
    /// about it, and this statement must keep naming `transcriptions` alone.
    #[test]
    fn the_retention_sweep_never_touches_notes() {
        let conn = db();
        insert(&conn, &entry("old dictation")).unwrap();
        conn.execute(
            "UPDATE transcriptions SET created_at = datetime('now', '-400 days')",
            [],
        )
        .unwrap();
        let note_id = crate::notes::create_note(
            &conn,
            &crate::notes::NewNote {
                title: Some("older still".into()),
                ..Default::default()
            },
        )
        .unwrap();
        conn.execute("UPDATE notes SET created_at = 0, updated_at = 0", [])
            .unwrap();

        assert_eq!(sweep_expired(&conn, 7).unwrap(), 1);
        assert!(list(&conn, 0, 10).unwrap().is_empty());
        assert!(
            crate::notes::get_note(&conn, note_id).unwrap().is_some(),
            "the sweep reached a table it has no business in"
        );
    }

    #[test]
    fn insert_then_list_round_trips() {
        let conn = db();
        let id = insert(&conn, &entry("hello world")).unwrap();
        assert!(id > 0);
        let rows = list(&conn, 0, 10).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, id);
        assert_eq!(rows[0].text, "hello world");
        assert_eq!(rows[0].outcome, Outcome::Done);
    }

    /// The table takes the two outcomes and nothing else.
    #[test]
    fn the_outcome_column_is_constrained() {
        let conn = db();
        for (outcome, ok) in [("done", true), ("failed", true), ("completed", false), ("", false)] {
            let result = conn.execute(
                "INSERT INTO transcriptions (text, outcome) VALUES ('x', ?1)",
                params![outcome],
            );
            assert_eq!(result.is_ok(), ok, "outcome {outcome:?}");
        }
    }

    #[test]
    fn list_orders_most_recent_first() {
        let conn = db();
        insert(&conn, &entry("first")).unwrap();
        insert(&conn, &entry("second")).unwrap();
        insert(&conn, &entry("third")).unwrap();
        let rows = list(&conn, 0, 10).unwrap();
        assert_eq!(
            rows.iter().map(|r| r.text.as_str()).collect::<Vec<_>>(),
            vec!["third", "second", "first"]
        );
    }

    #[test]
    fn list_paginates() {
        let conn = db();
        for i in 0..5 {
            insert(&conn, &entry(&format!("item{i}"))).unwrap();
        }
        let page0 = list(&conn, 0, 2).unwrap();
        let page1 = list(&conn, 1, 2).unwrap();
        assert_eq!(page0.len(), 2);
        assert_eq!(page1.len(), 2);
        assert_ne!(page0[0].id, page1[0].id);
    }

    /// A failed dictation stays in the list and in search, so the user can
    /// see what did not work.
    #[test]
    fn failed_rows_are_listed_and_found() {
        let conn = db();
        insert(
            &conn,
            &NewEntry {
                outcome: Outcome::Failed,
                ..entry("oops lighthouse")
            },
        )
        .unwrap();
        let rows = list(&conn, 0, 10).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].outcome, Outcome::Failed);
        assert_eq!(search(&conn, "lighthouse", 10).unwrap().len(), 1);
    }

    #[test]
    fn search_finds_substring_matches_by_word() {
        let conn = db();
        insert(&conn, &entry("the quick brown fox")).unwrap();
        insert(&conn, &entry("a slow red hen")).unwrap();
        let rows = search(&conn, "quick", 10).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].text, "the quick brown fox");
    }

    #[test]
    fn search_is_prefix_matching() {
        let conn = db();
        insert(&conn, &entry("dictation is delightful")).unwrap();
        let rows = search(&conn, "dict", 10).unwrap();
        assert_eq!(rows.len(), 1);
    }

    #[test]
    fn search_matches_raw_text_too() {
        let conn = db();
        insert(
            &conn,
            &NewEntry {
                raw_text: Some("um so basically hello".into()),
                ..entry("hello")
            },
        )
        .unwrap();
        let rows = search(&conn, "basically", 10).unwrap();
        assert_eq!(rows.len(), 1);
    }

    /// Devanagari text with matras must survive the round trip and be findable
    /// — and findable *precisely*.
    ///
    /// The recall half of this passed even under the old tokenizer, for a
    /// reason that made it a poor guard: with marks treated as separators, the
    /// query was shredded into the same consonant skeleton the document was,
    /// so it matched. The precision half below is what actually pins
    /// `categories 'L* N* Co Mn Mc'`. दिन (day), दान (donation) and दीन
    /// (faith) share the skeleton द-न and differ only in their matras; under
    /// the old tokenizer a search for one returned all three.
    #[test]
    fn search_finds_devanagari_with_matras() {
        let conn = db();
        insert(&conn, &entry("नमस्ते दुनिया")).unwrap();
        let rows = search(&conn, "नमस्ते", 10).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].text, "नमस्ते दुनिया");
    }

    #[test]
    fn search_tells_devanagari_words_apart_that_share_a_consonant_skeleton() {
        let conn = db();
        insert(&conn, &entry("दिन")).unwrap();
        insert(&conn, &entry("दान")).unwrap();
        insert(&conn, &entry("दीन")).unwrap();
        insert(&conn, &entry("किताब")).unwrap();
        insert(&conn, &entry("कुतुब")).unwrap();

        let texts = |q: &str| -> Vec<String> {
            search(&conn, q, 10)
                .unwrap()
                .into_iter()
                .map(|e| e.text)
                .collect()
        };
        assert_eq!(texts("दिन"), vec!["दिन".to_string()], "दान/दीन are different words");
        assert_eq!(texts("दान"), vec!["दान".to_string()]);
        assert_eq!(texts("किताब"), vec!["किताब".to_string()], "कुतुब is a different word");
    }

    /// Sarvam's transcripts spell the nukta letters precomposed (ड़ as
    /// U+095C), and the index keeps text as it was stored. A query is
    /// normalised to NFC, which splits those letters into consonant + nukta,
    /// so a typed or a pasted word has to be looked up in both spellings.
    /// History and Notes share the query builder; both are checked here.
    #[test]
    fn a_precomposed_nukta_word_is_found_whether_typed_or_pasted() {
        let conn = db();
        // बड़ा लड़की ज़रूर फ़ोन, each nukta letter one code point.
        let stored = "\u{092C}\u{095C}\u{093E} \u{0932}\u{095C}\u{0915}\u{0940} \
                      \u{095B}\u{0930}\u{0942}\u{0930} \u{095E}\u{094B}\u{0928}";
        insert(&conn, &entry(stored)).unwrap();
        let note = crate::notes::create_note(
            &conn,
            &crate::notes::NewNote {
                content: Some(stored.into()),
                ..Default::default()
            },
        )
        .unwrap();

        // (typed: consonant + nukta, pasted: precomposed)
        for (typed, pasted) in [
            ("\u{092C}\u{0921}\u{093C}\u{093E}", "\u{092C}\u{095C}\u{093E}"),
            ("\u{0932}\u{0921}\u{093C}", "\u{0932}\u{095C}"),
            ("\u{091C}\u{093C}\u{0930}\u{0942}\u{0930}", "\u{095B}\u{0930}\u{0942}\u{0930}"),
            ("\u{092B}\u{093C}\u{094B}\u{0928}", "\u{095E}\u{094B}\u{0928}"),
        ] {
            for query in [typed, pasted] {
                assert_eq!(
                    search(&conn, query, 10).unwrap().len(),
                    1,
                    "History, query {query:?}"
                );
                let notes = crate::notes::search_notes(&conn, query, 10).unwrap();
                assert_eq!(
                    notes.iter().map(|n| n.id).collect::<Vec<_>>(),
                    vec![note],
                    "Notes, query {query:?}"
                );
            }
        }
    }

    #[test]
    fn search_with_no_tokens_returns_empty_not_error() {
        let conn = db();
        insert(&conn, &entry("hello")).unwrap();
        assert!(search(&conn, "   ---   ", 10).unwrap().is_empty());
    }

    /// A bare FTS5 operator typed as search text (not escaped) must not blow
    /// up as a syntax error — `build_search_query` quotes every token.
    #[test]
    fn search_treats_operator_looking_input_as_literal() {
        let conn = db();
        insert(&conn, &entry("close the parenthesis)")).unwrap();
        // `NEAR`, quotes, and parens are all FTS5 syntax; none of these should error.
        assert!(search(&conn, "NEAR", 10).is_ok());
        assert!(search(&conn, "\"unterminated", 10).is_ok());
        assert!(search(&conn, "(unbalanced", 10).is_ok());
    }

    #[test]
    fn the_query_adapter_turns_nothing_searchable_into_none() {
        assert_eq!(build_search_query(""), None);
        assert_eq!(build_search_query("  ?! -- "), None);
        let query = build_search_query("hello there").expect("two words are searchable");
        assert_eq!(query.split(" AND ").count(), 2, "{query}");
        // The index splits at the underscore, so the query does too.
        let query = build_search_query("snake_case").expect("searchable");
        assert_eq!(query.split(" AND ").count(), 2, "{query}");
    }

    #[test]
    fn history_and_notes_build_the_same_query() {
        for input in [
            "",
            "   ",
            "---",
            "_",
            "\u{093F}\u{0942}",
            "hello",
            "Hello World",
            "snake_case",
            "NEAR(a b)",
            "title:plan",
            "\"quoted\" (bracketed",
            "नमस्ते दुनिया",
            "日本語 text",
            "Cafe\u{0301}",
        ] {
            let notes = crate::notes::search::sanitize_query(input);
            let history = build_search_query(input);
            if notes.is_empty() {
                assert_eq!(history, None, "input {input:?}");
            } else {
                assert_eq!(history, Some(notes), "input {input:?}");
            }
        }
    }

    #[test]
    fn search_finds_both_halves_of_a_snake_case_word_anywhere() {
        let conn = db();
        insert(&conn, &entry("case study of a snake")).unwrap();
        assert_eq!(search(&conn, "snake_case", 10).unwrap().len(), 1);
    }

    #[test]
    fn only_the_first_delete_of_a_dictation_finds_a_row() {
        let conn = db();
        let id = insert(&conn, &entry("Nikhil ko kal subah call karna")).unwrap();
        assert!(delete(&conn, id).unwrap());
        assert!(!delete(&conn, id).unwrap(), "the row was already gone the second time");
        assert!(list(&conn, 0, 10).unwrap().is_empty());
    }

    /// The index must stay in step with a delete through the delete trigger:
    /// a deleted row must not still be findable by search.
    #[test]
    fn delete_removes_row_from_fts_index() {
        let conn = db();
        let id = insert(&conn, &entry("findable until deleted")).unwrap();
        assert!(delete(&conn, id).unwrap());
        assert!(search(&conn, "findable", 10).unwrap().is_empty());
    }

    /// History's delete hands back what the row held, so the controller can
    /// forget its in-memory copy of the same words.
    #[test]
    fn delete_returning_hands_back_the_rows_text_once() {
        let conn = db();
        let id = insert(
            &conn,
            &NewEntry {
                text: "The meeting is at noon.".into(),
                raw_text: Some("the meeting is at noon".into()),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(
            delete_returning(&conn, id).unwrap(),
            Some(("The meeting is at noon.".to_string(), Some("the meeting is at noon".to_string())))
        );
        assert_eq!(delete_returning(&conn, id).unwrap(), None);
        assert!(list(&conn, 0, 10).unwrap().is_empty());
    }

    /// Home's Edit: the corrected text replaces `text`, `words` is recounted
    /// from it, and `raw_text` (the verbatim transcript) is left alone.
    #[test]
    fn update_text_replaces_the_text_and_recounts_words() {
        let conn = db();
        let id = insert(
            &conn,
            &NewEntry {
                text: "their going home".into(),
                raw_text: Some("their going home".into()),
                words: Some(3),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(update_text(&conn, id, "they're going home now").unwrap());
        let rows = list(&conn, 0, 10).unwrap();
        assert_eq!(rows[0].text, "they're going home now");
        assert_eq!(rows[0].words, Some(4));
        assert_eq!(rows[0].raw_text.as_deref(), Some("their going home"));
    }

    /// The update trigger keeps the index in step: the new word matches, and
    /// the old one stops matching unless `raw_text` still holds it. The index
    /// covers `raw_text` too, so after a real Home edit (where `raw_text` is
    /// the verbatim transcript) a search for the pre-edit wording still finds
    /// the row, by design.
    #[test]
    fn update_text_reindexes_the_row() {
        let conn = db();
        let id = insert(&conn, &entry("meet at the harbour")).unwrap();
        assert!(update_text(&conn, id, "meet at the station").unwrap());
        assert!(search(&conn, "harbour", 10).unwrap().is_empty());
        assert_eq!(search(&conn, "station", 10).unwrap().len(), 1);

        let with_raw = insert(
            &conn,
            &NewEntry {
                raw_text: Some("see you at the pier".into()),
                ..entry("see you at the pier")
            },
        )
        .unwrap();
        assert!(update_text(&conn, with_raw, "see you at the dock").unwrap());
        let found = search(&conn, "pier", 10).unwrap();
        assert_eq!(found.len(), 1, "raw_text keeps the pre-edit wording searchable");
        assert_eq!(found[0].text, "see you at the dock");
    }

    #[test]
    fn update_text_reports_false_for_a_missing_row() {
        let conn = db();
        assert!(!update_text(&conn, 4242, "nothing to change").unwrap());
        assert!(list(&conn, 0, 10).unwrap().is_empty());
    }

    #[test]
    fn clear_removes_everything_and_reports_count() {
        let conn = db();
        insert(&conn, &entry("one")).unwrap();
        insert(&conn, &entry("two")).unwrap();
        assert_eq!(clear(&conn).unwrap(), 2);
        assert!(list(&conn, 0, 10).unwrap().is_empty());
    }

    /// Clear all history empties `transcriptions` only. The words the user
    /// corrected stay until their own 30-day sweep, and notes stay until the
    /// user deletes them; the privacy page, the README and the Clear all
    /// dialog say so. Change those three with this.
    #[test]
    fn clear_all_leaves_the_corrected_words_and_the_notes() {
        let conn = db();
        insert(&conn, &entry("one")).unwrap();
        conn.execute(
            "INSERT INTO learn_candidates (from_word, to_word, count, last_seen, session_id)
             VALUES ('Shrishti', 'Srishti', 1, 1800000000, 's1')",
            [],
        )
        .unwrap();
        crate::notes::create_note(
            &conn,
            &crate::notes::NewNote {
                title: Some("an imported recording".into()),
                ..Default::default()
            },
        )
        .unwrap();

        assert_eq!(clear(&conn).unwrap(), 1);

        let candidates: i64 = conn
            .query_row("SELECT COUNT(*) FROM learn_candidates", [], |r| r.get(0))
            .unwrap();
        assert_eq!(candidates, 1, "Clear all history now removes the corrected words");
        let notes: i64 = conn.query_row("SELECT COUNT(*) FROM notes", [], |r| r.get(0)).unwrap();
        assert_eq!(notes, 1, "Clear all history now removes notes");
    }

    /// A fresh folder under the temp dir for a test that needs a real file
    /// (and so a real WAL) rather than `open_in_memory`.
    fn temp_db_dir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "bs-store-test-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Whether `needle`'s bytes appear anywhere in the database file or its
    /// WAL: what `strings history.db` or a copy of the folder would recover.
    /// Every needle in these tests is one lowercase word whose first letter
    /// no other indexed word shares, so FTS5's prefix compression of terms
    /// cannot hide a copy of it from this search.
    fn on_disk(db: &std::path::Path, needle: &str) -> bool {
        let wal = db.with_file_name(format!(
            "{}-wal",
            db.file_name().unwrap().to_string_lossy()
        ));
        [db.to_path_buf(), wal].iter().any(|p| {
            std::fs::read(p)
                .map(|bytes| bytes.windows(needle.len()).any(|w| w == needle.as_bytes()))
                .unwrap_or(false)
        })
    }

    /// History's delete, Home's Edit, the retention sweep and History's clear
    /// must remove the words from the file, not only from the table: no
    /// free-page copy, no WAL frame, no FTS5 tombstoned segment left for
    /// anyone who copies `history.db` to read. A deleted note's words go too.
    /// Checked with the connection still open (the WAL at its fullest) and
    /// again after it closes.
    #[test]
    fn removed_text_leaves_no_copy_in_the_database_file_or_its_wal() {
        let dir = temp_db_dir("residue");
        let path = dir.join("history.db");
        let conn = open_at(&path).unwrap();

        let deleted = insert(&conn, &entry("kumquatzebra")).unwrap();
        let edited = insert(&conn, &entry("vermilionyak")).unwrap();
        let swept = insert(&conn, &entry("quokkaxylem")).unwrap();
        let _cleared = insert(&conn, &entry("jackalopeowl")).unwrap();
        conn.execute(
            "UPDATE transcriptions SET created_at = datetime('now', '-40 days') WHERE id = ?1",
            params![swept],
        )
        .unwrap();
        let note = crate::notes::create_note(
            &conn,
            &crate::notes::NewNote {
                title: Some("pelicanumbra".into()),
                ..Default::default()
            },
        )
        .unwrap();
        for word in ["kumquatzebra", "vermilionyak", "quokkaxylem", "jackalopeowl", "pelicanumbra"] {
            assert!(on_disk(&path, word), "control: {word} should be on disk before removal");
        }

        assert!(delete(&conn, deleted).unwrap());
        assert!(!on_disk(&path, "kumquatzebra"), "History's delete left the text on disk");

        assert!(update_text(&conn, edited, "harbourmaster").unwrap());
        assert!(!on_disk(&path, "vermilionyak"), "Home's Edit left the old wording on disk");
        assert!(on_disk(&path, "harbourmaster"));

        assert_eq!(sweep_expired(&conn, 30).unwrap(), 1);
        assert!(!on_disk(&path, "quokkaxylem"), "the retention sweep left the text on disk");

        assert!(crate::notes::delete_note(&conn, note).unwrap());
        assert!(!on_disk(&path, "pelicanumbra"), "deleting a note left its text on disk");

        assert_eq!(clear(&conn).unwrap(), 2);
        for word in ["jackalopeowl", "harbourmaster"] {
            assert!(!on_disk(&path, word), "History's clear left {word} on disk");
        }

        drop(conn);
        for word in ["kumquatzebra", "vermilionyak", "quokkaxylem", "jackalopeowl", "harbourmaster", "pelicanumbra"] {
            assert!(!on_disk(&path, word), "{word} is back on disk after the connection closed");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A file written before secure delete existed still holds the words of
    /// every row and note deleted from it. The upgrade purges them once.
    #[test]
    fn upgrading_a_version_3_file_purges_text_it_had_already_deleted() {
        let dir = temp_db_dir("v3-residue");
        let path = dir.join("history.db");
        write_v3_file_with_deletes(&path);
        assert!(
            on_disk(&path, "xylophonewombat") && on_disk(&path, "nightingaleoboe"),
            "control: a v3 file keeps the words it deleted"
        );

        let conn = open_at(&path).unwrap();
        assert_eq!(user_version(&conn), SCHEMA_VERSION);
        assert!(!on_disk(&path, "xylophonewombat"), "a deleted dictation survived the upgrade");
        assert!(!on_disk(&path, "nightingaleoboe"), "a deleted note survived the upgrade");
        assert_eq!(search(&conn, "gardenias", 10).unwrap().len(), 1, "the kept row is still found");
        drop(conn);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The one-time purge needs room for a full copy of the file. If it
    /// fails (a nearly full disk), the file must still open, with the rest
    /// of the upgrade in place, and the next open tries again. A `VACUUM`
    /// inside an open transaction is refused, which stands in for any failure
    /// here.
    #[test]
    fn a_failed_purge_still_opens_the_file_and_the_next_open_retries_it() {
        let dir = temp_db_dir("v3-purge-fails");
        let path = dir.join("history.db");
        write_v3_file_with_deletes(&path);
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch("BEGIN").unwrap();
            migrate(&conn).expect("a failed purge must not fail the upgrade");
            conn.execute_batch("COMMIT").unwrap();
            assert!(
                user_version(&conn) < SCHEMA_VERSION,
                "the purge is still owed, so the version must say so"
            );
            assert_eq!(search(&conn, "gardenias", 10).unwrap().len(), 1);
        }
        assert!(on_disk(&path, "xylophonewombat"), "control: the failed purge left the residue");

        let conn = open_at(&path).unwrap();
        assert_eq!(user_version(&conn), SCHEMA_VERSION);
        assert!(!on_disk(&path, "xylophonewombat"), "the retried purge left a deleted dictation");
        assert!(!on_disk(&path, "nightingaleoboe"), "the retried purge left a deleted note");
        assert_eq!(search(&conn, "gardenias", 10).unwrap().len(), 1);
        drop(conn);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The version 6 rebuild copies every table and drops the old one, which
    /// leaves the old tables' pages free inside the file. A version 5 file
    /// owes no purge, so it gets one compaction of its own after the upgrade,
    /// or the file would keep that dead space (about half its size again).
    #[test]
    fn upgrading_a_version_5_file_leaves_no_free_pages_behind() {
        let dir = temp_db_dir("v5-compact");
        let path = dir.join("history.db");
        {
            let conn = Connection::open(&path).unwrap();
            conn.pragma_update(None, "journal_mode", "WAL").unwrap();
            legacy::write(&conn, 5);
            legacy::fill_v5(&conn);
        }

        let conn = open_at(&path).unwrap();
        assert_eq!(user_version(&conn), SCHEMA_VERSION);
        let free: i64 = conn.query_row("PRAGMA freelist_count", [], |r| r.get(0)).unwrap();
        assert_eq!(free, 0, "the dropped version 5 tables are still taking up room in the file");
        assert_eq!(search(&conn, "standup", 10).unwrap().len(), 1, "history search still works");
        drop(conn);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Exactly what a v3 build left on disk, deletes included: the words
    /// `xylophonewombat` (a deleted dictation) and `nightingaleoboe` (a
    /// deleted note) are still in the file, and `gardenias` is a kept row.
    fn write_v3_file_with_deletes(path: &std::path::Path) {
        let conn = Connection::open(path).unwrap();
        conn.pragma_update(None, "journal_mode", "WAL").unwrap();
        legacy::write(&conn, 3);
        let id = legacy::insert_text(&conn, "xylophonewombat");
        let kept = legacy::insert_text(&conn, "gardenias");
        conn.execute("INSERT INTO notes(title) VALUES ('nightingaleoboe')", [])
            .unwrap();
        conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(())).unwrap();
        conn.execute("DELETE FROM transcriptions WHERE id = ?1", params![id])
            .unwrap();
        conn.execute("DELETE FROM notes", []).unwrap();
        assert!(kept > id);
    }

    #[test]
    fn sweep_expired_is_noop_when_keep_forever() {
        let conn = db();
        insert(&conn, &entry("ancient")).unwrap();
        conn.execute(
            "UPDATE transcriptions SET created_at = datetime('now', '-999 days')",
            [],
        )
        .unwrap();
        assert_eq!(sweep_expired(&conn, 0).unwrap(), 0);
        assert_eq!(list(&conn, 0, 10).unwrap().len(), 1);
    }

    /// A 30-day setting removes a dictation from 45 days ago and keeps one
    /// from 20 days ago and one from today, and says how many it removed.
    #[test]
    fn a_30_day_setting_removes_only_the_dictation_past_it() {
        let conn = db();
        for (text, age) in [
            ("Diwali shopping list", "-45 days"),
            ("Bijli ka bill bharna hai", "-20 days"),
            ("Aaj Kochi ki flight hai", "+0 days"),
        ] {
            let id = insert(&conn, &entry(text)).unwrap();
            conn.execute(
                "UPDATE transcriptions SET created_at = datetime('now', ?1) WHERE id = ?2",
                params![age, id],
            )
            .unwrap();
        }
        assert_eq!(sweep_expired(&conn, 30).unwrap(), 1);
        let mut kept: Vec<String> =
            list(&conn, 0, 10).unwrap().into_iter().map(|r| r.text).collect();
        kept.sort();
        assert_eq!(kept, ["Aaj Kochi ki flight hai", "Bijli ka bill bharna hai"]);
    }
}
