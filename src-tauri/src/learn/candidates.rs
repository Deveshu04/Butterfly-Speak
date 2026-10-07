//! Turns observed corrections into replacement rules, once the same
//! correction has turned up in more than one dictation.
//!
//! The field monitor reports every (heard, corrected) pair it sees, one batch
//! per paste. One sighting is weak evidence: the user may have been
//! rewording, or fixing a word for this sentence only. So each pair is a
//! candidate in the `learn_candidates` table, counted once per paste, and it
//! becomes an automatic [`Replacement`] only when [`PROMOTION_THRESHOLD`]
//! different pastes have shown it within [`CANDIDATE_WINDOW_SECS`]. A
//! promotion writes the rule into settings, tells the webview that settings
//! changed, and shows a notification that says where to undo it.
//! [`Guard::undo`] takes the rule and the candidate away together.
//!
//! Candidates are keyed by the whole pair. The diff engine reports a pair
//! only where the user replaced words in place, and when several
//! neighbouring words were replaced together it pairs them first with first
//! and second with second, or reports nothing when the counts differ. Each
//! pair from such a stretch is its own evidence: a pairing that was wrong
//! for one edit has to come back from another paste, pairing the same two
//! words, before it can be promoted, and two different mishearings fixed to
//! the same word each earn their own rule.
//!
//! A pair whose heard word already has a rule, automatic or typed by the
//! user, is not counted: once a rule exists, the heard word no longer
//! reaches the field, so further sightings can only be noise.
//!
//! Words are stored in canonical composed form and compared without regard
//! to case, so one word in two spellings is one candidate.
//!
//! PRIVACY: the table holds single corrected words, never the text around
//! them, and no log line here names a word.

use crate::canonical;
use crate::cleanup::CleanupSettings;
use crate::history::Recorder;
use crate::learn::diff::CorrectionPair;
use crate::settings::{self, Replacement, Settings};
use rusqlite::{params, Connection};
use std::sync::{Arc, RwLock};
use std::time::{SystemTime, UNIX_EPOCH};
use tauri::{AppHandle, Emitter};

/// Schema for this module's table, applied by `history::store::migrate` (it
/// arrived with schema version 2). The DDL lives here rather than in
/// `store.rs` so the table and the only code that reads it stay in one file;
/// `store.rs` remains the only place that may decide what
/// `PRAGMA user_version` means.
///
/// `from`/`to` are SQL keywords, hence `from_word`/`to_word`.
///
/// `COLLATE NOCASE` on both words makes `Sidharth → Siddharth` and
/// `sidharth → SIDDHARTH` one row, which keeps the casing stored first. It
/// folds ASCII case only; other scripts rely on [`key`] storing one composed
/// form (Devanagari has no case, so the collation changes nothing there).
pub const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS learn_candidates (
    from_word  TEXT    NOT NULL COLLATE NOCASE,
    to_word    TEXT    NOT NULL COLLATE NOCASE,
    count      INTEGER NOT NULL,
    last_seen  INTEGER NOT NULL,
    session_id TEXT    NOT NULL,
    PRIMARY KEY (from_word, to_word)
);
"#;

/// How many *distinct paste sessions* must show the same pair before it
/// becomes a rule. One paste is one edit, however often the monitor reports
/// it; the same fix in a second paste is a habit worth acting on.
pub const PROMOTION_THRESHOLD: i64 = 2;

/// How long a candidate keeps its count, in seconds. A pair not seen again
/// within this starts over at one, so fixes made months apart never add up
/// to a rule.
pub const CANDIDATE_WINDOW_SECS: i64 = 30 * 24 * 60 * 60;

/// Unix seconds. A clock that has never been set reads as 0, which just makes
/// every candidate look brand new — the safe direction.
fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Count one sighting of `pair` from the paste `session_id`.
///
/// A new pair starts at one. A known pair goes up by one when the sighting
/// comes from a different paste, or starts over at one when its last
/// sighting has fallen out of the window. The same paste reporting the same
/// pair again changes nothing, not even `last_seen`: the monitor can report
/// a pair more than once while it watches, and that is still one edit.
fn observe_pair(
    conn: &Connection,
    pair: &CorrectionPair,
    session_id: &str,
    now: i64,
) -> rusqlite::Result<()> {
    let cutoff = now - CANDIDATE_WINDOW_SECS;
    conn.execute(
        "INSERT INTO learn_candidates (from_word, to_word, count, last_seen, session_id)
         VALUES (?1, ?2, 1, ?3, ?4)
         ON CONFLICT (from_word, to_word) DO UPDATE SET
             count = CASE
                 WHEN learn_candidates.last_seen < ?5 THEN 1
                 ELSE learn_candidates.count + 1
             END,
             last_seen  = excluded.last_seen,
             session_id = excluded.session_id
         WHERE learn_candidates.session_id <> excluded.session_id",
        params![key(&pair.from), key(&pair.to), now, session_id, cutoff],
    )?;
    Ok(())
}

/// The form a word is stored and looked up in.
///
/// SQLite has no notion of canonical equivalence — `COLLATE NOCASE` folds
/// ASCII case and nothing else — so the only way the primary key can mean
/// "the same word" is for every value crossing this boundary to already be in
/// one form. Composition on the way in and on every lookup is that guarantee.
/// Case is deliberately left alone: `COLLATE NOCASE` still owns that, and the
/// observed casing is what gets promoted into the user's rule.
///
/// Today's rows are all NFC already, because they can only have come from the
/// field monitor, which normalizes before it diffs. This makes that an
/// invariant of the table rather than a property of its one caller — which is
/// what `Guard::undo` needs, since its arguments come back from the UI.
fn key(word: &str) -> String {
    canonical::normalized(word)
}

/// The rows among `pairs` that have crossed the threshold and are still
/// inside the window, reported with the casing that was stored first.
///
/// The `last_seen` test is redundant immediately after [`observe_pair`] (which
/// has just written `now`), and deliberately kept: it is what makes this
/// function a correct answer to "is this promotable?" on its own, rather than
/// a fragment that only works when called in one particular order.
fn promotable(
    conn: &Connection,
    pairs: &[CorrectionPair],
    now: i64,
) -> rusqlite::Result<Vec<CorrectionPair>> {
    let cutoff = now - CANDIDATE_WINDOW_SECS;
    let mut stmt = conn.prepare(
        "SELECT from_word, to_word FROM learn_candidates
         WHERE from_word = ?1 AND to_word = ?2 AND count >= ?3 AND last_seen >= ?4",
    )?;
    let mut out: Vec<CorrectionPair> = Vec::new();
    for pair in pairs {
        let row = stmt
            .query_row(
                params![key(&pair.from), key(&pair.to), PROMOTION_THRESHOLD, cutoff],
                |r| {
                    Ok(CorrectionPair {
                        from: r.get(0)?,
                        to: r.get(1)?,
                    })
                },
            )
            .map(Some)
            .or_else(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => Ok(None),
                other => Err(other),
            })?;
        // A batch can name the same row twice under a different casing or a
        // different spelling of the same word; the table folds those
        // together, so the answer must too or the pair would be promoted
        // (and toasted) twice.
        if let Some(found) = row {
            let already = out.iter().any(|p| {
                canonical::same_word(&p.from, &found.from)
                    && canonical::same_word(&p.to, &found.to)
            });
            if !already {
                out.push(found);
            }
        }
    }
    Ok(out)
}

fn delete_candidate(conn: &Connection, from: &str, to: &str) -> rusqlite::Result<usize> {
    conn.execute(
        "DELETE FROM learn_candidates WHERE from_word = ?1 AND to_word = ?2",
        params![key(from), key(to)],
    )
}

/// Drop candidates that fell out of the window. Without this the table only
/// ever grows: a pair seen once and never again is dead weight forever. The
/// rows are corrected words, so the removal is pushed through to the file
/// like any other removal of the user's text.
fn sweep_stale(conn: &Connection, now: i64) -> rusqlite::Result<usize> {
    let removed = conn.execute(
        "DELETE FROM learn_candidates WHERE last_seen < ?1",
        params![now - CANDIDATE_WINDOW_SECS],
    )?;
    if removed > 0 {
        crate::history::scrub_removed_text(conn);
    }
    Ok(removed)
}

/// The history thread's start and hourly sweep calls this, whatever the
/// learning switch says, so a corrected word is kept for at most
/// [`CANDIDATE_WINDOW_SECS`] (plus the sweep interval) even when no later
/// correction ever arrives. A failure only logs.
pub(crate) fn sweep_stale_now(conn: &Connection) {
    match sweep_stale(conn, now_unix()) {
        Ok(0) => {}
        Ok(n) => tracing::info!("swept {n} stale learn candidate(s)"),
        Err(e) => tracing::warn!("learn candidate sweep failed: {e}"),
    }
}

/// Count one paste session's observations and promote whatever crosses the
/// threshold.
///
/// Two transactions, because they are two different facts and only one of
/// them is a decision:
///
/// 1. **The observations always land.** The user really did make that
///    correction; nothing downstream should be able to un-see it.
/// 2. **The promotion is all-or-nothing.** `commit` writes the promoted pairs
///    into settings and runs *inside* the second transaction, so the
///    candidate deletes commit only if it returns `Ok`. The settings file is
///    the fallible, external half, so it goes last and SQLite is what rolls
///    back.
///
/// A failed save therefore leaves the counts at the threshold with their rows
/// intact, and the next observation retries the promotion — rather than
/// throwing the evidence away and making the user correct the same word twice
/// more because a disk was full.
fn record_batch(
    conn: &Connection,
    pairs: &[CorrectionPair],
    session_id: &str,
    now: i64,
    commit: impl FnOnce(&[CorrectionPair]) -> anyhow::Result<()>,
) -> anyhow::Result<Vec<CorrectionPair>> {
    let counting = conn.unchecked_transaction()?;
    for pair in pairs {
        observe_pair(&counting, pair, session_id, now)?;
    }
    let promoted = promotable(&counting, pairs, now)?;
    counting.commit()?;

    if promoted.is_empty() {
        return Ok(Vec::new());
    }

    let promoting = conn.unchecked_transaction()?;
    for pair in &promoted {
        delete_candidate(&promoting, &pair.from, &pair.to)?;
    }
    match commit(&promoted) {
        Ok(()) => {
            promoting.commit()?;
            // The promoted rows are corrected words, removed: push the
            // removal through to the file, as `sweep_stale` does.
            crate::history::scrub_removed_text(conn);
            Ok(promoted)
        }
        Err(e) => {
            promoting.rollback()?;
            // No words in the log line — only how many failed.
            tracing::warn!(
                "couldn't persist {} learned correction(s); candidates kept: {e:#}",
                promoted.len()
            );
            Ok(Vec::new())
        }
    }
}

/// Delete the candidate for `from → to` and run `commit`, which removes the
/// learned rule from settings, in one transaction: both happen or neither
/// does. The candidate has to go even though promotion already deleted it,
/// because a later sighting may have created it again, and leaving it would
/// let the next single sighting promote what the user has just rejected.
fn undo_in_db(
    conn: &Connection,
    from: &str,
    to: &str,
    commit: impl FnOnce() -> anyhow::Result<()>,
) -> anyhow::Result<bool> {
    let tx = conn.unchecked_transaction()?;
    delete_candidate(&tx, from, to)?;
    match commit() {
        Ok(()) => {
            tx.commit()?;
            // The user rejected the rule: its words leave the file too.
            crate::history::scrub_removed_text(conn);
            Ok(true)
        }
        Err(e) => {
            tx.rollback()?;
            tracing::warn!("couldn't undo a learned correction: {e:#}");
            Ok(false)
        }
    }
}

/// Read-modify-save against the live settings, so that nothing changes in
/// memory unless the disk write succeeded.
///
/// The write lock is held across the file write on purpose: it is the only
/// way the in-memory copy, the cleanup pipeline's copy and the file can't
/// disagree. Lock ordering to preserve — this runs *on the history DB
/// thread*, so the settings lock is acquired inside a DB round trip. Nothing
/// may hold the settings lock while making a blocking `Recorder` call, or the
/// two deadlock. (Nothing does today: `commands::set_settings` only ever
/// fire-and-forgets `set_retention`.)
fn apply_settings(
    settings: &RwLock<Settings>,
    cleanup: &RwLock<CleanupSettings>,
    mutate: impl FnOnce(&mut Settings),
) -> anyhow::Result<()> {
    let mut live = settings.write().unwrap_or_else(|e| e.into_inner());
    let mut next = live.clone();
    mutate(&mut next);
    settings::save(&next)?;
    // Both in-memory consumers only move after the save committed.
    //
    // Deliberately NOT `endpoint::set_config`, unlike `commands::set_settings`:
    // the only mutations reaching here are replacement promotions and rule
    // removals, so `custom_endpoint` is byte-identical across `mutate` and the
    // slot cannot go stale. A future mutation that touches it has to refresh
    // the slot here too, or the endpoint keeps serving the previous config
    // until the next settings write from the UI.
    *cleanup.write().unwrap_or_else(|e| e.into_inner()) = (&next).into();
    *live = next;
    Ok(())
}

/// The pairs worth counting: the ones the user does not already have a rule
/// for.
///
/// This is the analogue of the engine's already-in-the-dictionary gate, for
/// the store this module writes to: once a rule exists, every later transcript
/// is rewritten before the user sees it, so any further "evidence" for the
/// same `from` is an artefact rather than a second opinion.
///
/// Matched with [`canonical::same_word`], not `to_lowercase`. The rule may
/// have been typed by the user and the pair comes from the field monitor,
/// which NFC-normalizes what it reads — so for the nukta letters the two
/// arrive spelled differently by construction, and a scalar comparison would
/// let a word the user has already corrected by hand go on accumulating
/// evidence forever.
fn unknown_pairs(pairs: Vec<CorrectionPair>, replacements: &[Replacement]) -> Vec<CorrectionPair> {
    pairs
        .into_iter()
        .filter(|p| {
            !replacements
                .iter()
                .any(|r| canonical::same_word(&r.from, &p.from))
        })
        .collect()
}

/// Add an `auto` rule per promoted pair, unless the user already has a rule
/// for that word. Theirs wins: a promotion is the app's guess, and it must
/// never quietly rewrite a spelling the user chose by hand.
///
/// Pure, so it can be tested without going near the settings file.
fn apply_promotions(s: &mut Settings, promoted: &[CorrectionPair]) {
    for pair in promoted {
        let exists = s
            .replacements
            .iter()
            .any(|r| canonical::same_word(&r.from, &pair.from));
        if !exists {
            s.replacements.push(Replacement {
                from: pair.from.clone(),
                to: pair.to.clone(),
                auto: true,
            });
        }
    }
}

/// Remove the automatic rule for `from → to`, matching words the way
/// [`canonical::same_word`] does. A rule the user wrote is never removed,
/// even when it says the same thing: Undo takes back only what the app
/// added.
fn drop_learned_rule(s: &mut Settings, from: &str, to: &str) {
    s.replacements.retain(|r| {
        !(r.auto && canonical::same_word(&r.from, from) && canonical::same_word(&r.to, to))
    });
}

fn write_promotions(
    settings: &RwLock<Settings>,
    cleanup: &RwLock<CleanupSettings>,
    promoted: &[CorrectionPair],
) -> anyhow::Result<()> {
    apply_settings(settings, cleanup, |s| apply_promotions(s, promoted))
}

fn remove_learned_replacement(
    settings: &RwLock<Settings>,
    cleanup: &RwLock<CleanupSettings>,
    from: &str,
    to: &str,
) -> anyhow::Result<()> {
    apply_settings(settings, cleanup, |s| drop_learned_rule(s, from, to))
}

/// Tell the user a rule was just learned and where to take it back. Outside
/// Settings this is the only sign of a promotion, so it names both words; a
/// failure to show it is logged without them.
fn notify_learned(app: &AppHandle, pair: &CorrectionPair) {
    use tauri_plugin_notification::NotificationExt;

    if let Err(e) = app
        .notification()
        .builder()
        .title(format!("Learned: {} → {}", pair.from, pair.to))
        .body("Butterfly Speak will fix this automatically from now on. Undo it in Dictionary → Corrections.")
        .show()
    {
        tracing::warn!("learned-correction notice failed: {e}");
    }
}

/// What the field monitor's sink talks to, and what the Undo command calls.
///
/// Cheap to clone — it is three shared handles and a channel sender.
#[derive(Clone)]
pub struct Guard {
    history: Recorder,
    settings: Arc<RwLock<Settings>>,
    cleanup: Arc<RwLock<CleanupSettings>>,
    app: AppHandle,
}

impl Guard {
    pub fn new(
        history: Recorder,
        settings: Arc<RwLock<Settings>>,
        cleanup: Arc<RwLock<CleanupSettings>>,
        app: AppHandle,
    ) -> Self {
        Self {
            history,
            settings,
            cleanup,
            app,
        }
    }

    /// Record everything one paste session observed, and promote whatever has
    /// now been seen twice. **This is where the field monitor's sink sends
    /// its batches.**
    ///
    /// - `pairs` is exactly what `diff::corrections_in` returned for this
    ///   paste. Pass them all; the filtering below is this module's job.
    /// - `session_id` identifies *one paste*, not one dictation and not one
    ///   edit: the monitor's window can report the same correction more
    ///   than once, and a pair repeated under the same `session_id` is
    ///   counted once no matter how often it arrives. Any per-paste unique
    ///   string will do (a UUID, or the paste's timestamp); an empty string
    ///   is rejected, because "no session" would defeat the guard.
    ///
    /// Returns the pairs that were promoted — already written to settings,
    /// already toasted, candidate rows already gone. An empty vector is the
    /// normal outcome and is not an error; there is no error channel, for the
    /// same reason the diff engine has none.
    ///
    /// Blocking: one round trip to the history DB thread (microseconds
    /// against a local WAL file). Call it off the UI thread.
    ///
    /// LOCK ORDERING — binding on every caller: **do not hold the settings
    /// lock (`RwLock<Settings>`), read or write, across this call.** A
    /// promotion takes the settings *write* lock on the DB thread
    /// ([`apply_settings`]), so a caller still holding a guard of either kind
    /// is waiting for a thread that is waiting for it. A read guard deadlocks
    /// exactly as surely as a write guard, because it blocks the writer. Clone
    /// what you need out of the settings and drop the guard first — which is
    /// what the field monitor's call site does (`Controller::settings` returns
    /// an owned snapshot, and the sink closure captures only this guard).
    pub fn observe(&self, pairs: Vec<CorrectionPair>, session_id: &str) -> Vec<CorrectionPair> {
        if pairs.is_empty() || session_id.is_empty() {
            return Vec::new();
        }

        let pairs: Vec<CorrectionPair> = {
            let s = self.settings.read().unwrap_or_else(|e| e.into_inner());
            if !s.learn.field_monitor_enabled {
                // Belt and braces: the monitor does not start at all while
                // this is off. Dropping observations here too means a
                // monitor that is already mid-window when the user flips the
                // switch cannot still teach the app something.
                return Vec::new();
            }
            unknown_pairs(pairs, &s.replacements)
        };
        if pairs.is_empty() {
            return Vec::new();
        }

        let settings = self.settings.clone();
        let cleanup = self.cleanup.clone();
        let session_id = session_id.to_string();
        let now = now_unix();

        let promoted = self
            .history
            .with_connection(move |conn| {
                if let Err(e) = sweep_stale(conn, now) {
                    tracing::warn!("learn candidate sweep failed: {e}");
                }
                record_batch(conn, &pairs, &session_id, now, |promoted| {
                    write_promotions(&settings, &cleanup, promoted)
                })
                .unwrap_or_else(|e| {
                    tracing::warn!("learn candidate store failed: {e:#}");
                    Vec::new()
                })
            })
            .unwrap_or_default();

        if !promoted.is_empty() {
            // A promotion is the only settings write the user did not
            // initiate, which makes it the only one no open window can know
            // about. The webview loads settings once per life and
            // `set_settings` is a whole-object write, so without this the next
            // toggle the user flips anywhere in Settings would persist its
            // pre-promotion snapshot and silently delete the rule — along with
            // the evidence that earned it, which this function has already
            // consumed. Emitted once for the batch, and after the DB round
            // trip returned, so it can only announce a promotion that
            // committed.
            let _ = self.app.emit(crate::events::SETTINGS_CHANGED, ());
        }
        for pair in &promoted {
            notify_learned(&self.app, pair);
        }
        promoted
    }

    /// Take back a promotion: remove the learned `Replacement` **and** zero
    /// the candidate, both or neither. Returns whether it committed.
    ///
    /// Zeroing matters even though promotion already deleted the row: a fresh
    /// observation can have re-created it in the meantime, and leaving that
    /// behind would let the next single observation re-promote exactly what
    /// the user just rejected.
    pub fn undo(&self, from: &str, to: &str) -> bool {
        let settings = self.settings.clone();
        let cleanup = self.cleanup.clone();
        let from = from.to_string();
        let to = to.to_string();
        self.history
            .with_connection(move |conn| {
                let (rule_from, rule_to) = (from.clone(), to.clone());
                undo_in_db(conn, &from, &to, || {
                    remove_learned_replacement(&settings, &cleanup, &rule_from, &rule_to)
                })
                .unwrap_or_else(|e| {
                    tracing::warn!("learn candidate undo failed: {e:#}");
                    false
                })
            })
            .unwrap_or(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    fn db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(SCHEMA).unwrap();
        conn
    }

    fn pair(from: &str, to: &str) -> CorrectionPair {
        CorrectionPair {
            from: from.into(),
            to: to.into(),
        }
    }

    /// `(count, last_seen, session_id)` for a pair, or None when no row.
    fn row(conn: &Connection, from: &str, to: &str) -> Option<(i64, i64, String)> {
        conn.query_row(
            "SELECT count, last_seen, session_id FROM learn_candidates
             WHERE from_word = ?1 AND to_word = ?2",
            params![from, to],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .ok()
    }

    fn rows(conn: &Connection) -> i64 {
        conn.query_row("SELECT COUNT(*) FROM learn_candidates", [], |r| r.get(0))
            .unwrap()
    }

    /// A `commit` that always succeeds and records what it was handed.
    fn recording_commit(
        seen: &RefCell<Vec<CorrectionPair>>,
    ) -> impl FnOnce(&[CorrectionPair]) -> anyhow::Result<()> + '_ {
        move |promoted| {
            seen.borrow_mut().extend_from_slice(promoted);
            Ok(())
        }
    }

    fn failing_commit(_: &[CorrectionPair]) -> anyhow::Result<()> {
        Err(anyhow::anyhow!("settings file is read-only"))
    }

    const NOW: i64 = 1_800_000_000;

    // ---------------------------------------------------------------------
    // Upsert and per-session dedupe.
    // ---------------------------------------------------------------------

    #[test]
    fn a_first_observation_creates_a_candidate_at_one() {
        let conn = db();
        observe_pair(&conn, &pair("Sidharth", "Siddharth"), "s1", NOW).unwrap();
        assert_eq!(row(&conn, "Sidharth", "Siddharth").unwrap().0, 1);
    }

    #[test]
    fn a_second_session_increments_the_same_pair() {
        let conn = db();
        observe_pair(&conn, &pair("Sidharth", "Siddharth"), "s1", NOW).unwrap();
        observe_pair(&conn, &pair("Sidharth", "Siddharth"), "s2", NOW + 60).unwrap();
        let (count, last_seen, session) = row(&conn, "Sidharth", "Siddharth").unwrap();
        assert_eq!(count, 2);
        assert_eq!(last_seen, NOW + 60);
        assert_eq!(session, "s2");
    }

    /// The monitor's window can report the same correction more than once
    /// from a single paste. Counting those would let one dictation promote
    /// itself, which is the whole failure the guard exists to prevent.
    #[test]
    fn the_same_session_never_increments_the_same_pair_twice() {
        let conn = db();
        for _ in 0..5 {
            observe_pair(&conn, &pair("Sidharth", "Siddharth"), "s1", NOW).unwrap();
        }
        let (count, last_seen, _) = row(&conn, "Sidharth", "Siddharth").unwrap();
        assert_eq!(count, 1);
        assert_eq!(last_seen, NOW, "a repeat must not even refresh last_seen");
    }

    /// Casing is not identity. A user who writes the name capitalised once
    /// and lowercase the next time is giving the same evidence twice.
    #[test]
    fn casing_does_not_split_a_candidate_in_two() {
        let conn = db();
        observe_pair(&conn, &pair("Sidharth", "Siddharth"), "s1", NOW).unwrap();
        observe_pair(&conn, &pair("sidharth", "siddharth"), "s2", NOW).unwrap();
        assert_eq!(rows(&conn), 1);
        // The casing stored first is the casing kept.
        let promoted = promotable(&conn, &[pair("SIDHARTH", "SIDDHARTH")], NOW).unwrap();
        assert_eq!(promoted, vec![pair("Sidharth", "Siddharth")]);
    }

    /// Two mishearings of the same word are two separate candidates: the pair
    /// is the key, so evidence for one cannot promote the other.
    #[test]
    fn different_originals_are_different_candidates() {
        let conn = db();
        observe_pair(&conn, &pair("Sidharth", "Siddharth"), "s1", NOW).unwrap();
        observe_pair(&conn, &pair("Sidarth", "Siddharth"), "s2", NOW).unwrap();
        assert_eq!(rows(&conn), 2);
        assert_eq!(row(&conn, "Sidharth", "Siddharth").unwrap().0, 1);
        assert_eq!(row(&conn, "Sidarth", "Siddharth").unwrap().0, 1);
    }

    // ---------------------------------------------------------------------
    // The promotion threshold.
    // ---------------------------------------------------------------------

    #[test]
    fn one_observation_does_not_promote() {
        let conn = db();
        let seen = RefCell::new(Vec::new());
        let promoted = record_batch(
            &conn,
            &[pair("Sidharth", "Siddharth")],
            "s1",
            NOW,
            recording_commit(&seen),
        )
        .unwrap();
        assert!(promoted.is_empty());
        assert!(seen.borrow().is_empty(), "nothing may be written yet");
        assert_eq!(row(&conn, "Sidharth", "Siddharth").unwrap().0, 1);
    }

    #[test]
    fn two_observations_inside_the_window_promote_and_clear_the_candidate() {
        let conn = db();
        let seen = RefCell::new(Vec::new());
        record_batch(
            &conn,
            &[pair("Sidharth", "Siddharth")],
            "s1",
            NOW,
            recording_commit(&seen),
        )
        .unwrap();
        let promoted = record_batch(
            &conn,
            &[pair("Sidharth", "Siddharth")],
            "s2",
            NOW + CANDIDATE_WINDOW_SECS - 1,
            recording_commit(&seen),
        )
        .unwrap();

        assert_eq!(promoted, vec![pair("Sidharth", "Siddharth")]);
        assert_eq!(*seen.borrow(), vec![pair("Sidharth", "Siddharth")]);
        assert!(
            row(&conn, "Sidharth", "Siddharth").is_none(),
            "a promoted candidate must not stay behind to promote again"
        );
    }

    /// Without decay, two typos a year apart would add up to a permanent rule.
    #[test]
    fn two_observations_outside_the_window_do_not_promote() {
        let conn = db();
        let seen = RefCell::new(Vec::new());
        record_batch(
            &conn,
            &[pair("Sidharth", "Siddharth")],
            "s1",
            NOW,
            recording_commit(&seen),
        )
        .unwrap();
        let promoted = record_batch(
            &conn,
            &[pair("Sidharth", "Siddharth")],
            "s2",
            NOW + CANDIDATE_WINDOW_SECS + 1,
            recording_commit(&seen),
        )
        .unwrap();

        assert!(promoted.is_empty(), "a stale candidate must restart, not add");
        assert!(seen.borrow().is_empty());
        assert_eq!(
            row(&conn, "Sidharth", "Siddharth").unwrap().0,
            1,
            "the late observation is evidence, just the first piece of it"
        );
    }

    /// Promotion is per pair: reaching the threshold on one says nothing
    /// about the other, even when they share a corrected word.
    #[test]
    fn only_the_pair_that_crossed_the_threshold_is_promoted() {
        let conn = db();
        let seen = RefCell::new(Vec::new());
        record_batch(
            &conn,
            &[pair("Sidharth", "Siddharth")],
            "s1",
            NOW,
            recording_commit(&seen),
        )
        .unwrap();
        let promoted = record_batch(
            &conn,
            &[pair("Sidharth", "Siddharth"), pair("Sidarth", "Siddharth")],
            "s2",
            NOW,
            recording_commit(&seen),
        )
        .unwrap();

        assert_eq!(promoted, vec![pair("Sidharth", "Siddharth")]);
        assert_eq!(row(&conn, "Sidarth", "Siddharth").unwrap().0, 1);
    }

    /// A settings write that fails must leave the candidate sitting at the
    /// threshold, so the next observation retries the promotion instead of
    /// making the user earn the same rule from scratch.
    #[test]
    fn a_failed_settings_write_leaves_the_candidate_at_the_threshold() {
        let conn = db();
        let seen = RefCell::new(Vec::new());
        record_batch(
            &conn,
            &[pair("Sidharth", "Siddharth")],
            "s1",
            NOW,
            recording_commit(&seen),
        )
        .unwrap();
        let promoted = record_batch(
            &conn,
            &[pair("Sidharth", "Siddharth")],
            "s2",
            NOW,
            failing_commit,
        )
        .unwrap();

        assert!(promoted.is_empty(), "nothing was promoted");
        assert_eq!(
            row(&conn, "Sidharth", "Siddharth").unwrap().0,
            2,
            "the candidate must survive the failed write, count intact"
        );

        // And the retry works: a third session promotes it for real.
        let retried = record_batch(
            &conn,
            &[pair("Sidharth", "Siddharth")],
            "s3",
            NOW,
            recording_commit(&seen),
        )
        .unwrap();
        assert_eq!(retried, vec![pair("Sidharth", "Siddharth")]);
        assert!(row(&conn, "Sidharth", "Siddharth").is_none());
    }

    // ---------------------------------------------------------------------
    // Undo.
    // ---------------------------------------------------------------------

    /// Undo removes the rule and the candidate in one go, including a
    /// candidate a later sighting created again after the promotion; left
    /// behind, it would re-promote on the very next sighting.
    #[test]
    fn undo_zeroes_the_candidate_and_removes_the_rule_together() {
        let conn = db();
        let removed = RefCell::new(false);

        // Promote, then let a later session re-create the candidate.
        let seen = RefCell::new(Vec::new());
        record_batch(&conn, &[pair("Sidharth", "Siddharth")], "s1", NOW, recording_commit(&seen)).unwrap();
        record_batch(&conn, &[pair("Sidharth", "Siddharth")], "s2", NOW, recording_commit(&seen)).unwrap();
        observe_pair(&conn, &pair("Sidharth", "Siddharth"), "s3", NOW).unwrap();
        assert_eq!(row(&conn, "Sidharth", "Siddharth").unwrap().0, 1);

        let ok = undo_in_db(&conn, "Sidharth", "Siddharth", || {
            *removed.borrow_mut() = true;
            Ok(())
        })
        .unwrap();

        assert!(ok);
        assert!(*removed.borrow(), "the rule was removed");
        assert!(
            row(&conn, "Sidharth", "Siddharth").is_none(),
            "the candidate must be gone, not merely below the threshold"
        );
    }

    /// Both or neither: if the rule cannot be removed, the candidate stays.
    #[test]
    fn undo_that_cannot_remove_the_rule_leaves_the_candidate_alone() {
        let conn = db();
        observe_pair(&conn, &pair("Sidharth", "Siddharth"), "s1", NOW).unwrap();

        let ok = undo_in_db(&conn, "Sidharth", "Siddharth", || {
            Err(anyhow::anyhow!("settings file is read-only"))
        })
        .unwrap();

        assert!(!ok);
        assert_eq!(
            row(&conn, "Sidharth", "Siddharth").unwrap().0,
            1,
            "a half-applied undo is worse than none"
        );
    }

    #[test]
    fn undo_of_something_that_was_never_learned_is_harmless() {
        let conn = db();
        let ok = undo_in_db(&conn, "nothing", "here", || Ok(())).unwrap();
        assert!(ok);
        assert_eq!(rows(&conn), 0);
    }

    // ---------------------------------------------------------------------
    // Removed corrected words leave the file, as the sweep's do.
    // ---------------------------------------------------------------------

    /// A real history file, opened the way the app opens it (WAL, secure
    /// delete), in a fresh temp folder.
    fn file_db(name: &str) -> (std::path::PathBuf, Connection) {
        let dir = std::env::temp_dir().join(format!(
            "bs-candidates-test-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let path = dir.join("history.db");
        let conn = crate::history::open_at(&path).expect("open a history file");
        (path, conn)
    }

    /// Whether `needle`'s bytes are anywhere in the database file or its WAL.
    fn on_disk(db: &std::path::Path, needle: &str) -> bool {
        let wal = db.with_file_name(format!("{}-wal", db.file_name().unwrap().to_string_lossy()));
        [db.to_path_buf(), wal].iter().any(|p| {
            std::fs::read(p)
                .map(|bytes| bytes.windows(needle.len()).any(|w| w == needle.as_bytes()))
                .unwrap_or(false)
        })
    }

    /// A promotion moves the pair into the user's dictionary and deletes the
    /// candidate row; the row's words must not stay behind in the WAL.
    #[test]
    fn a_promoted_candidate_leaves_no_copy_in_the_file_or_its_wal() {
        let (path, conn) = file_db("promote");
        let seen = RefCell::new(Vec::new());
        record_batch(&conn, &[pair("zanzibarqat", "wobblefjord")], "s1", NOW, recording_commit(&seen))
            .unwrap();
        assert!(on_disk(&path, "zanzibarqat"), "control: the candidate is on disk");

        let promoted = record_batch(
            &conn,
            &[pair("zanzibarqat", "wobblefjord")],
            "s2",
            NOW,
            recording_commit(&seen),
        )
        .unwrap();
        assert_eq!(promoted.len(), 1, "control: the pair was promoted");
        for word in ["zanzibarqat", "wobblefjord"] {
            assert!(!on_disk(&path, word), "the promoted candidate's {word} is still in the file");
        }
        drop(conn);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    /// Undo is the user rejecting the rule: the candidate's words go from
    /// the file too, not only from the table.
    #[test]
    fn an_undone_candidate_leaves_no_copy_in_the_file_or_its_wal() {
        let (path, conn) = file_db("undo");
        observe_pair(&conn, &pair("quixotlamb", "jubilvesk"), "s1", NOW).unwrap();
        assert!(on_disk(&path, "quixotlamb"), "control: the candidate is on disk");

        assert!(undo_in_db(&conn, "quixotlamb", "jubilvesk", || Ok(())).unwrap());
        assert!(row(&conn, "quixotlamb", "jubilvesk").is_none());
        for word in ["quixotlamb", "jubilvesk"] {
            assert!(!on_disk(&path, word), "the undone candidate's {word} is still in the file");
        }
        drop(conn);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    // ---------------------------------------------------------------------
    // Housekeeping.
    // ---------------------------------------------------------------------

    #[test]
    fn the_sweep_drops_candidates_that_fell_out_of_the_window() {
        let conn = db();
        observe_pair(&conn, &pair("stale", "steel"), "s1", NOW).unwrap();
        observe_pair(&conn, &pair("fresh", "flesh"), "s1", NOW).unwrap();
        conn.execute(
            "UPDATE learn_candidates SET last_seen = ?1 WHERE from_word = 'stale'",
            params![NOW - CANDIDATE_WINDOW_SECS - 1],
        )
        .unwrap();

        assert_eq!(sweep_stale(&conn, NOW).unwrap(), 1);
        assert!(row(&conn, "stale", "steel").is_none());
        assert!(row(&conn, "fresh", "flesh").is_some());
    }

    // ---------------------------------------------------------------------
    // The settings half. These call the real mutations; only the disk write
    // that `apply_settings` wraps them in is out of reach of a unit test.
    // ---------------------------------------------------------------------

    #[test]
    fn a_promotion_writes_an_auto_rule_with_the_observed_casing() {
        let mut s = Settings::default();
        apply_promotions(&mut s, &[pair("Sidharth", "Siddharth")]);
        assert_eq!(s.replacements.len(), 1);
        assert_eq!(s.replacements[0].from, "Sidharth");
        assert_eq!(s.replacements[0].to, "Siddharth");
        assert!(s.replacements[0].auto, "provenance is what makes Undo safe");
    }

    /// The app's guess never overwrites the user's own spelling.
    #[test]
    fn a_promotion_does_not_disturb_a_rule_the_user_already_has() {
        let mut s = Settings::default();
        s.replacements = vec![Replacement {
            from: "Sidharth".into(),
            to: "Siddhaarth".into(),
            auto: false,
        }];
        apply_promotions(&mut s, &[pair("Sidharth", "Siddharth")]);
        assert_eq!(s.replacements.len(), 1);
        assert_eq!(s.replacements[0].to, "Siddhaarth");
        assert!(!s.replacements[0].auto);
    }

    /// Undo takes back only what the app added, even when the user's own
    /// rule says the same thing.
    #[test]
    fn undo_never_deletes_a_rule_the_user_wrote_themselves() {
        let mut s = Settings::default();
        s.replacements = vec![
            Replacement { from: "Sidharth".into(), to: "Siddharth".into(), auto: false },
            Replacement { from: "Kavita".into(), to: "Kavitha".into(), auto: true },
        ];

        drop_learned_rule(&mut s, "Sidharth", "Siddharth");
        assert_eq!(s.replacements.len(), 2, "a manual rule is not ours to delete");

        // Casing is not identity here either — the rule is matched the same
        // way the candidate row is.
        drop_learned_rule(&mut s, "kavita", "KAVITHA");
        assert_eq!(s.replacements.len(), 1);
        assert!(!s.replacements[0].auto);
    }

    // ---------------------------------------------------------------------
    // Canonical equivalence. The field monitor NFC-normalizes what it reads,
    // so a learned pair arrives spelled apart (`क` + nukta) while a rule the
    // user typed, or a transcript, may hold the precomposed `क़`. Every seam
    // that compares the two has to see one word. See `crate::canonical`.
    // ---------------------------------------------------------------------

    /// `क़लम` (pen) as the monitor produces it, and as it may be typed.
    const PEN_APART: &str = "\u{0915}\u{093C}लम";
    const PEN_TOGETHER: &str = "\u{0958}लम";

    /// A rule the user typed with the precomposed letter must suppress
    /// learning the same word all over again. Without folding, the evidence
    /// accumulates forever and re-promotes a rule that already exists.
    #[test]
    fn a_rule_the_user_already_has_suppresses_the_pair_in_either_spelling() {
        let rules = vec![Replacement {
            from: PEN_TOGETHER.into(),
            to: "pen".into(),
            auto: false,
        }];
        let kept = unknown_pairs(vec![pair(PEN_APART, "pen")], &rules);
        assert!(kept.is_empty(), "the user already corrects this word");

        // The reverse pairing, and a genuinely different word still passes.
        let rules = vec![Replacement {
            from: PEN_APART.into(),
            to: "pen".into(),
            auto: true,
        }];
        assert!(unknown_pairs(vec![pair(PEN_TOGETHER, "pen")], &rules).is_empty());
        assert_eq!(
            unknown_pairs(vec![pair("Sidharth", "Siddharth")], &rules).len(),
            1
        );
    }

    /// The English half of the same gate, unchanged.
    #[test]
    fn unknown_pairs_still_filters_ascii_case_insensitively() {
        let rules = vec![Replacement {
            from: "SIDHARTH".into(),
            to: "Siddharth".into(),
            auto: true,
        }];
        assert!(unknown_pairs(vec![pair("sidharth", "Siddharth")], &rules).is_empty());
        assert_eq!(unknown_pairs(vec![pair("Kavita", "Kavitha")], &rules).len(), 1);
    }

    /// The table's identity: the same word in two spellings is one candidate,
    /// not two that each sit below the threshold forever.
    #[test]
    fn the_two_spellings_of_a_word_are_one_candidate() {
        let conn = db();
        observe_pair(&conn, &pair(PEN_APART, "pen"), "s1", NOW).unwrap();
        observe_pair(&conn, &pair(PEN_TOGETHER, "pen"), "s2", NOW).unwrap();
        assert_eq!(rows(&conn), 1, "one word, one row");

        // And it is promotable when looked up under either spelling.
        assert_eq!(
            promotable(&conn, &[pair(PEN_TOGETHER, "pen")], NOW).unwrap().len(),
            1
        );
        assert_eq!(
            promotable(&conn, &[pair(PEN_APART, "pen")], NOW).unwrap().len(),
            1
        );
    }

    /// Undo's lookup reaches the row whichever spelling the UI hands back.
    #[test]
    fn undo_finds_the_candidate_under_either_spelling() {
        let conn = db();
        observe_pair(&conn, &pair(PEN_APART, "pen"), "s1", NOW).unwrap();
        assert_eq!(delete_candidate(&conn, PEN_TOGETHER, "pen").unwrap(), 1);
        assert_eq!(rows(&conn), 0);
    }

    /// A promotion must not fire twice because the batch named the word two
    /// ways, and Undo must reach a rule stored in the other spelling.
    #[test]
    fn promotion_and_undo_agree_across_spellings() {
        let mut s = Settings::default();
        apply_promotions(&mut s, &[pair(PEN_APART, "pen")]);
        assert_eq!(s.replacements.len(), 1);
        // The same word arriving precomposed is not a second rule.
        apply_promotions(&mut s, &[pair(PEN_TOGETHER, "pen")]);
        assert_eq!(s.replacements.len(), 1, "one word, one rule");

        drop_learned_rule(&mut s, PEN_TOGETHER, "pen");
        assert!(s.replacements.is_empty(), "undo missed the other spelling");
    }
}
