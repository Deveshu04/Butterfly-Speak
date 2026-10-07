//! Dictation history: SQLite (rusqlite, bundled) + FTS5 full-text search over
//! past transcriptions, with a background retention sweep. Everything goes
//! through [`Recorder`], a cheap `Clone` handle to a dedicated thread that
//! owns the `rusqlite::Connection` — `Connection` is `!Sync`, so it can't
//! live behind a shared `Arc<Mutex<_>>` the way `Settings` does; instead this
//! mirrors `audio::spawn`/`sarvam::spawn`: one thread owns the resource, and
//! everyone else talks to it over a channel. `Recorder`'s calls are blocking
//! round-trips (a `crossbeam_channel::bounded(1)` reply per request) — fine
//! here because SQLite on a local WAL file answers in microseconds, and every
//! caller (a Tauri command) already runs off the UI thread.
//!
//! NEVER log transcript text — `Cmd::Record`'s error path below logs only the
//! error, never `entry.text`/`entry.raw_text`.
//!
//! ## Integration
//! The write side is `Controller::record_history`. Its two main callers in
//! `Controller::handle`:
//!
//! - `ControlMsg::FinalResult`, once that arm has assembled the final `text`
//!   (post style, post smart-space) and still owns `raw` — the same values it
//!   hands `events::TRANSCRIPT_FINAL`, so a row here and the
//!   `stats.svelte.ts` feed can't disagree about a dictation. Every row
//!   written that way is [`Outcome::Done`] — with `error_code =
//!   "cloud-limit"` when the relay's weekly or 30-minute limit ended the
//!   dictation first (`FinalResult::cut_short`): the words were pasted, but
//!   they stop where the limit did.
//! - `ControlMsg::CloudTruncated`: a cloud dictation whose socket died
//!   mid-utterance and whose batch fallback couldn't re-transcribe the whole
//!   thing. The text is real (same rules → polish → guardrail pipeline) but
//!   known-incomplete, so it is *never* pasted — History is the only place it
//!   exists, which is exactly why this arm files it. [`Outcome::Failed`] with
//!   `error_code = "connection-lost"`, and deliberately no
//!   `events::TRANSCRIPT_FINAL`: nothing reached the user's document, so
//!   nothing should reach the stats feed either.
//!
//! Two more `Failed` rows come from the route seam: a route that declined to
//! produce text (`route-unavailable`) and a finalize timeout that was still
//! holding a route's transcript (`route-timeout`).
//!
//! Nothing is filed for a `CloudError`/`FinalizeTimeout` with no transcript
//! at all, for an empty transcript, or for a recording the speech gate
//! turned away: there is no text to keep. The `error_code` vocabulary is the
//! `ERR_*` constants in `controller`.
//!
//! ## Schema
//! `transcriptions(id, created_at, outcome, error_code, text, raw_text, words,
//! duration_ms, app, route, provider, model)` plus an
//! external-content FTS5 shadow (its tokenizer, and why it takes the Indic
//! combining marks as letters, are in `store.rs`), kept in sync by triggers.
//! `PRAGMA user_version` records the layout; `journal_mode = WAL`. DB file:
//! `settings::config_dir().join("history.db")`. See `store.rs` for every
//! statement.
//!
//! One other table shares this file and this thread:
//! `learn_candidates(from_word, to_word, count, last_seen, session_id)`, the
//! auto-learn frequency guard's evidence store. It is not history's — see
//! `learn::candidates` for its schema, its statements, and why it lives here
//! (one `!Sync` connection, one thread, one transaction discipline). All
//! history owns of it is creating the table in `store::migrate` and the
//! [`Recorder::with_connection`] door it arrives through.
//!
//! ## Retention
//! `Settings.history` carries `enabled` (default `true`) and `keep_days`
//! (default `0` = forever — Butterfly Speak's privacy default is local-only
//! data kept until the user says otherwise). The DB thread sweeps once as
//! soon as it opens the connection (before reading its first message, see
//! [`run`]), again every [`SWEEP_INTERVAL`], and again on every
//! [`RetentionCfg`] change that actually differs from the thread's current
//! one. `keep_days == 0` is the sweep's own no-op (`store::sweep_expired`);
//! every other `keep_days` value is purged regardless of `enabled`.
//! `enabled = false` only gates writes (`Cmd::Record` is a no-op while it's
//! off); it is deliberately NOT also a sweep gate, because that would make
//! "Keep dictation history" look like a purge control it isn't, while
//! genuinely orphaning rows past `keep_days` forever the moment a user turns
//! it off. See the `SetRetention` and `sweep_tick` arms of [`run`].
//!
//! The same sweep removes auto-learn's corrected words (`learn_candidates`)
//! once they fall out of their fixed window
//! (`learn::candidates::CANDIDATE_WINDOW_SECS`), whatever `keep_days` and the
//! learning switch say.
//!
//! ## The only home of dictation text on disk
//! This table is the one place the app keeps what a user dictated. Home's
//! "Today" list reads it (`history_list`), Home's Edit writes back to it
//! ([`Recorder::update_text`]), and the webview's `stats.svelte.ts` keeps
//! counters only, never text. So the History page's delete and clear, and
//! the retention sweep, reach every stored copy. They reach the bytes too:
//! secure delete, FTS5's in-place `secure-delete` and a WAL checkpoint after
//! each removal leave no free-page, WAL or index-segment copy behind (see
//! `store.rs`'s module doc). While history is off, Home
//! shows the session's dictations from memory and nothing is written.

mod entry;
/// Earlier layouts of this file, for the upgrade tests.
#[cfg(test)]
mod legacy;
mod store;

pub use entry::{Entry, NewEntry, Outcome};
/// For the notes tables, which share this file: see `store`'s module doc.
pub(crate) use store::scrub_removed_text;
/// Other modules' tests open a real history file the way the app does.
#[cfg(test)]
pub(crate) use store::open_at;

use crossbeam_channel::{Receiver, Sender};
use rusqlite::Connection;
use std::path::PathBuf;
use std::time::Duration;

/// How often the DB thread re-checks retention even if settings never
/// change. This is the slack on "nothing older than `keep_days`" while the
/// app runs: a row can outlive its window by up to this long (and, while the
/// app is closed, until the next start sweeps). The delete is indexed on
/// `created_at` and cheap, so hourly costs nothing.
const SWEEP_INTERVAL: Duration = Duration::from_secs(60 * 60);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RetentionCfg {
    pub enabled: bool,
    pub keep_days: u32,
}

enum Cmd {
    Record(NewEntry),
    List {
        page: u32,
        page_size: u32,
        reply: Sender<Vec<Entry>>,
    },
    Search {
        query: String,
        limit: u32,
        reply: Sender<Vec<Entry>>,
    },
    Delete {
        id: i64,
        reply: Sender<Option<(String, Option<String>)>>,
    },
    UpdateText {
        id: i64,
        text: String,
        reply: Sender<bool>,
    },
    Clear {
        reply: Sender<u32>,
    },
    SetRetention(RetentionCfg),
    /// Run something else's SQL on this thread. See
    /// [`Recorder::with_connection`].
    Task(Box<dyn FnOnce(&Connection) + Send>),
}

/// Handle to the history DB thread. Cheap to clone (it's just a channel
/// sender) — hand a clone to anything that needs to record or query.
#[derive(Clone)]
pub struct Recorder {
    tx: Sender<Cmd>,
}

impl Recorder {
    /// Fire-and-forget: never blocks the caller on disk I/O. Silently
    /// dropped if the DB thread has gone away (e.g. it failed to open the
    /// file) — history is a convenience feature, not load-bearing for
    /// dictation itself, so a lost write here must never surface as a user-
    /// facing error.
    ///
    /// Called from `Controller::record_history` — see the module doc comment.
    pub fn record(&self, entry: NewEntry) {
        let _ = self.tx.send(Cmd::Record(entry));
    }

    pub fn list(&self, page: u32, page_size: u32) -> Vec<Entry> {
        self.roundtrip(|reply| Cmd::List { page, page_size, reply })
            .unwrap_or_default()
    }

    pub fn search(&self, query: String, limit: u32) -> Vec<Entry> {
        self.roundtrip(|reply| Cmd::Search { query, limit, reply })
            .unwrap_or_default()
    }

    /// [`Self::remove`] as a yes or no, for the tests.
    #[cfg(test)]
    pub fn delete(&self, id: i64) -> bool {
        self.remove(id).is_some()
    }

    /// Delete one row and hand back what it held (`text`, `raw_text`), so
    /// the caller can forget any other copy of those words. `None` when no
    /// row had that id or the thread is gone.
    pub fn remove(&self, id: i64) -> Option<(String, Option<String>)> {
        self.roundtrip(|reply| Cmd::Delete { id, reply }).flatten()
    }

    /// Home's Edit: store the user's correction of a dictation in the one
    /// place its text lives. `Some(false)` when the row is gone, `None` when
    /// the thread is (the database failed to open), so Home can say which.
    pub fn update_text(&self, id: i64, text: String) -> Option<bool> {
        self.roundtrip(|reply| Cmd::UpdateText { id, text, reply })
    }

    pub fn clear(&self) -> u32 {
        self.roundtrip(|reply| Cmd::Clear { reply }).unwrap_or(0)
    }

    /// Push a retention change to the DB thread (called from `set_settings`
    /// on every save, unconditionally — the thread itself decides whether
    /// anything actually changed).
    pub fn set_retention(&self, cfg: RetentionCfg) {
        let _ = self.tx.send(Cmd::SetRetention(cfg));
    }

    /// Run `f` on the DB thread, with the connection, and wait for its
    /// answer. `None` means the thread is gone (the DB failed to open), which
    /// callers must treat as "this feature is off this session", never as an
    /// error to surface.
    ///
    /// This is the escape hatch for modules that own their own table in this
    /// database rather than in one of their own — currently only
    /// `learn::candidates`, whose rows have to be written in the same
    /// transaction discipline (and on the same `!Sync` `Connection`) as
    /// everything else here. It exists so that `history` does not have to
    /// grow a command variant per feature and learn what a correction is.
    ///
    /// `f` runs ON the DB thread: it must never call back into a `Recorder`
    /// method, which would wait for a reply the thread cannot send while it
    /// is inside `f`.
    ///
    /// LOCK ORDERING — the caller's obligation, and the one that is easy to
    /// break by accident: **no caller may hold the settings lock (`RwLock<Settings>`),
    /// read or write, across this call.** `f` may take that lock on the DB
    /// thread (`learn::candidates::apply_settings` does), so a caller holding
    /// it while blocking here would be waiting for a thread that is waiting
    /// for the caller. A read guard is just as fatal as a write guard: it
    /// blocks the writer inside `f`. Clone the `Settings` you need and drop
    /// the guard before calling.
    pub fn with_connection<T: Send + 'static>(
        &self,
        f: impl FnOnce(&Connection) -> T + Send + 'static,
    ) -> Option<T> {
        let (reply, rx) = crossbeam_channel::bounded(1);
        self.tx
            .send(Cmd::Task(Box::new(move |conn| {
                let _ = reply.send(f(conn));
            })))
            .ok()?;
        rx.recv().ok()
    }

    fn roundtrip<T: Send + 'static>(&self, make: impl FnOnce(Sender<T>) -> Cmd) -> Option<T> {
        let (reply, rx) = crossbeam_channel::bounded(1);
        self.tx.send(make(reply)).ok()?;
        rx.recv().ok()
    }
}

/// Spawn the DB thread and return a handle to it. `db_path` is opened (and
/// migrated) on the thread itself, not here — a slow first-open (cold disk
/// cache) must not stall the caller (`start_backend`, on the main thread).
pub fn spawn(db_path: PathBuf, initial: RetentionCfg) -> Recorder {
    let (tx, rx) = crossbeam_channel::unbounded();
    std::thread::Builder::new()
        .name("history-db".into())
        .spawn(move || run(db_path, initial, rx))
        .expect("spawn history-db thread");
    Recorder { tx }
}

fn run(db_path: PathBuf, initial: RetentionCfg, rx: Receiver<Cmd>) {
    let conn = match store::open_at(&db_path) {
        Ok(c) => c,
        Err(e) => {
            tracing::error!("history db failed to open ({e:#}); history is disabled this session");
            return;
        }
    };

    let mut retention = initial;
    // Sweep once, right now, using the real disk-loaded settings this thread was
    // spawned with — don't wait for SWEEP_INTERVAL's first tick or for a settings
    // change that may never come. Without this, a user who sets a `keep_days` window
    // once and never revisits Settings again would have nothing purged in any session
    // shorter than SWEEP_INTERVAL (1h).
    sweep(&conn, retention);
    let sweep_tick = crossbeam_channel::tick(SWEEP_INTERVAL);

    loop {
        crossbeam_channel::select! {
            recv(rx) -> msg => {
                let Ok(cmd) = msg else {
                    break; // every Sender dropped: the app is shutting down
                };
                match cmd {
                    Cmd::Record(entry) => {
                        // Master switch acts at write time, not by deleting.
                        if !retention.enabled {
                            continue;
                        }
                        if let Err(e) = store::insert(&conn, &entry) {
                            tracing::warn!("history insert failed: {e}");
                        }
                    }
                    Cmd::List { page, page_size, reply } => {
                        let rows = store::list(&conn, page, page_size)
                            .unwrap_or_else(|e| { tracing::warn!("history list failed: {e}"); Vec::new() });
                        let _ = reply.send(rows);
                    }
                    Cmd::Search { query, limit, reply } => {
                        let rows = store::search(&conn, &query, limit)
                            .unwrap_or_else(|e| { tracing::warn!("history search failed: {e}"); Vec::new() });
                        let _ = reply.send(rows);
                    }
                    Cmd::Delete { id, reply } => {
                        let removed = store::delete_returning(&conn, id)
                            .unwrap_or_else(|e| { tracing::warn!("history delete failed: {e}"); None });
                        let _ = reply.send(removed);
                    }
                    Cmd::UpdateText { id, text, reply } => {
                        // Not gated on `retention.enabled`: an edit changes a
                        // row that already exists, it never files a new one.
                        let ok = store::update_text(&conn, id, &text)
                            .unwrap_or_else(|e| { tracing::warn!("history update failed: {e}"); false });
                        let _ = reply.send(ok);
                    }
                    Cmd::Clear { reply } => {
                        let n = store::clear(&conn)
                            .unwrap_or_else(|e| { tracing::warn!("history clear failed: {e}"); 0 });
                        let _ = reply.send(n);
                    }
                    Cmd::SetRetention(cfg) => {
                        // No first-sync sweep is needed here: `initial` above
                        // is already the real, disk-loaded `Settings.history`
                        // (settings::load() runs synchronously in start_backend,
                        // before this thread is even spawned), and the
                        // unconditional sweep before this loop runs once against
                        // it before any message is read. This arm's job is
                        // narrower: react only to a *change*, so
                        // the sweep never silently goes stale relative to what's on
                        // disk without re-sweeping on every unrelated settings save.
                        let changed = cfg != retention;
                        retention = cfg;
                        if changed {
                            sweep(&conn, retention);
                        }
                    }
                    Cmd::Task(f) => f(&conn),
                }
            }
            recv(sweep_tick) -> _ => {
                // A periodic re-sweep on a fixed interval regardless of
                // settings activity. The *first* sweep isn't
                // gated on this tick, though — see the unconditional sweep right
                // before this loop starts, which covers a session shorter than
                // SWEEP_INTERVAL that never touches Settings again.
                sweep(&conn, retention);
            }
        }
    }
}

fn sweep(conn: &Connection, retention: RetentionCfg) {
    // `enabled` is deliberately NOT checked here: it's a write-time master
    // switch (see `Cmd::Record` above), not a purge gate. `keep_days == 0`
    // ("forever") is the sweep's own no-op condition, enforced inside
    // `store::sweep_expired`. If this returned early on `!enabled`, a user
    // who set a retention window, let it run once, and then turned "Keep
    // dictation history" off would have every row from that point on kept
    // forever instead of purged — the opposite of what turning history off
    // implies, and no compensating write-side delete exists to cover it.
    match store::sweep_expired(conn, retention.keep_days) {
        Ok(0) => {}
        Ok(n) => tracing::info!("history retention swept {n} expired transcription(s)"),
        Err(e) => tracing::warn!("history retention sweep failed: {e}"),
    }
    // Corrected words (`learn_candidates`) share this file and this sweep.
    // Their window is fixed and does not depend on `keep_days`, the learning
    // switch or a later correction arriving: without this they left only
    // when a new correction came in with learning on.
    crate::learn::candidates::sweep_stale_now(conn);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration as StdDuration;

    fn spawn_test(path: PathBuf, initial: RetentionCfg) -> Recorder {
        spawn(path, initial)
    }

    fn temp_db_path(name: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "bs-history-test-{name}-{}-{}.db",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        p
    }

    #[test]
    fn record_then_list_round_trips_through_the_thread() {
        let path = temp_db_path("record-list");
        let rec = spawn_test(path, RetentionCfg { enabled: true, keep_days: 0 });
        rec.record(NewEntry { text: "hello from the thread".into(), ..Default::default() });
        // No synchronous ack for `record`; poll briefly rather than assume
        // the write landed before the very next message is processed.
        let mut rows = Vec::new();
        for _ in 0..50 {
            rows = rec.list(0, 10);
            if !rows.is_empty() {
                break;
            }
            std::thread::sleep(StdDuration::from_millis(20));
        }
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].text, "hello from the thread");
    }

    #[test]
    fn record_is_a_noop_when_history_disabled() {
        let path = temp_db_path("disabled");
        let rec = spawn_test(path, RetentionCfg { enabled: false, keep_days: 0 });
        rec.record(NewEntry { text: "should not be stored".into(), ..Default::default() });
        // Round-trip a List, which is synchronous — by the time it answers,
        // the Record sent earlier (same channel, in order) has already been
        // handled one way or the other.
        let rows = rec.list(0, 10);
        assert!(rows.is_empty());
    }

    #[test]
    fn delete_and_clear_round_trip() {
        let path = temp_db_path("delete-clear");
        let rec = spawn_test(path, RetentionCfg { enabled: true, keep_days: 0 });
        rec.record(NewEntry { text: "a".into(), ..Default::default() });
        rec.record(NewEntry { text: "b".into(), ..Default::default() });
        let mut rows = Vec::new();
        for _ in 0..50 {
            rows = rec.list(0, 10);
            if rows.len() == 2 {
                break;
            }
            std::thread::sleep(StdDuration::from_millis(20));
        }
        assert_eq!(rows.len(), 2);
        assert!(rec.delete(rows[0].id));
        assert_eq!(rec.list(0, 10).len(), 1);
        assert_eq!(rec.clear(), 1);
        assert!(rec.list(0, 10).is_empty());
    }

    /// Home's Edit reaches the row through the thread, and a later List sees
    /// the corrected text.
    #[test]
    fn update_text_round_trips_through_the_thread() {
        let path = temp_db_path("update-text");
        let rec = spawn_test(path, RetentionCfg { enabled: true, keep_days: 0 });
        rec.record(NewEntry { text: "draft one".into(), ..Default::default() });
        let rows = rec.list(0, 10);
        assert_eq!(rows.len(), 1);
        assert_eq!(rec.update_text(rows[0].id, "final draft".into()), Some(true));
        assert_eq!(rec.list(0, 10)[0].text, "final draft");
        assert_eq!(rec.update_text(rows[0].id + 1, "no such row".into()), Some(false));
    }

    /// A database that failed to open is not the same as a row that is gone:
    /// Home tells the user one or the other, so the handle must too.
    #[test]
    fn update_text_says_so_when_the_database_is_not_running() {
        // A directory cannot be opened as a database file.
        let dir = temp_db_path("not-a-file");
        std::fs::create_dir_all(&dir).unwrap();
        let rec = spawn_test(dir.clone(), RetentionCfg { enabled: true, keep_days: 0 });
        assert_eq!(rec.update_text(1, "anything".into()), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The door `learn::candidates` comes through: a closure runs on the DB
    /// thread with the migrated connection, and its answer comes back. The
    /// `learn_candidates` table has to be there, since the same migration
    /// that creates `transcriptions` creates it.
    #[test]
    fn with_connection_runs_on_the_db_thread_and_sees_the_migrated_schema() {
        let path = temp_db_path("with-connection");
        let rec = spawn_test(path, RetentionCfg { enabled: true, keep_days: 0 });

        let thread_name = rec
            .with_connection(|conn| {
                let count: i64 = conn
                    .query_row("SELECT COUNT(*) FROM learn_candidates", [], |r| r.get(0))
                    .expect("learn_candidates exists after migration");
                assert_eq!(count, 0);
                std::thread::current().name().map(str::to_string)
            })
            .expect("the DB thread answered");
        assert_eq!(thread_name.as_deref(), Some("history-db"));
    }

    /// Back-dates a row directly on disk (bypassing the thread, which has no
    /// way to insert with a chosen `created_at`) so a fresh `spawn` opens a
    /// DB that already has something to purge.
    fn seed_expired_row(path: &PathBuf, text: &str, days_old: u32) {
        let conn = super::store::open_at(path).expect("open db for seeding");
        super::store::insert(&conn, &NewEntry { text: text.into(), ..Default::default() })
            .expect("seed insert");
        conn.execute(
            &format!("UPDATE transcriptions SET created_at = datetime('now', '-{days_old} days')"),
            [],
        )
        .expect("backdate seeded row");
    }

    /// With only the periodic tick and the `SetRetention` arm, retention would purge
    /// nothing in a session shorter than `SWEEP_INTERVAL` (1h) that never touched
    /// Settings. `run` sweeps once, synchronously, before it ever reads a message —
    /// prove that happens with no `SetRetention` sent and no tick elapsed, by relying
    /// on `list`'s round trip: since the startup sweep runs before the thread's loop
    /// starts accepting messages, `list` can only answer after the sweep has already
    /// completed.
    #[test]
    fn startup_sweep_purges_expired_rows_before_any_message_is_handled() {
        let path = temp_db_path("startup-sweep");
        seed_expired_row(&path, "thirty days old", 30);

        let rec = spawn_test(path, RetentionCfg { enabled: true, keep_days: 7 });
        let rows = rec.list(0, 10);
        assert!(
            rows.is_empty(),
            "the startup sweep should have purged the expired row before List was ever handled"
        );
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

    /// Corrected words (`learn_candidates`) are kept for at most the
    /// candidate window, whatever the learning switch says and whether or
    /// not the user ever corrects another word: the history thread sweeps
    /// them at start and every hour with the dictations, and the swept words
    /// leave the file too. Learning is not running in this test at all.
    #[test]
    fn the_sweep_drops_stale_corrected_words_whatever_the_learning_switch_says() {
        use crate::learn::candidates::CANDIDATE_WINDOW_SECS;
        let path = temp_db_path("stale-candidates");
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        {
            let conn = super::store::open_at(&path).expect("open db for seeding");
            let add = |from: &str, to: &str, last_seen: i64| {
                conn.execute(
                    "INSERT INTO learn_candidates (from_word, to_word, count, last_seen, session_id)
                     VALUES (?1, ?2, 1, ?3, 's1')",
                    rusqlite::params![from, to, last_seen],
                )
                .expect("seed a candidate");
            };
            add("zanzibarqat", "wobblefjord", now - CANDIDATE_WINDOW_SECS - 60);
            add("freshfrom", "freshto", now);
        }
        assert!(on_disk(&path, "zanzibarqat"), "control: the stale words are on disk");

        let rec = spawn_test(path.clone(), RetentionCfg { enabled: false, keep_days: 0 });
        let left = rec
            .with_connection(|conn| {
                let mut stmt = conn.prepare("SELECT from_word FROM learn_candidates").unwrap();
                stmt.query_map([], |r| r.get::<_, String>(0))
                    .unwrap()
                    .collect::<Result<Vec<_>, _>>()
                    .unwrap()
            })
            .expect("the DB thread answered");
        assert_eq!(left, vec!["freshfrom".to_string()], "the stale candidate survived the sweep");
        for word in ["zanzibarqat", "wobblefjord"] {
            assert!(!on_disk(&path, word), "the swept word {word} is still in the file");
        }
    }

    /// `sweep()` must not return early on `!retention.enabled`: treating the write-time
    /// master switch as a purge gate too would mean a user who turned "Keep dictation
    /// history" off kept every row past `keep_days` forever from that point on. The
    /// sweep must still run (and `keep_days` must still be honoured) while `enabled` is
    /// false.
    #[test]
    fn startup_sweep_purges_expired_rows_even_when_history_recording_is_disabled() {
        let path = temp_db_path("sweep-while-disabled");
        seed_expired_row(&path, "should still be purged", 30);

        let rec = spawn_test(path, RetentionCfg { enabled: false, keep_days: 7 });
        let rows = rec.list(0, 10);
        assert!(
            rows.is_empty(),
            "the sweep must purge rows past keep_days even while `enabled` is false"
        );
    }
}
