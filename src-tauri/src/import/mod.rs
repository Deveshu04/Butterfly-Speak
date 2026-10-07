//! Importing recordings: a queue of files, each turned into an `upload` note
//! by one Sarvam batch job.
//!
//! # Where the files come from
//!
//! Both ways in are handled in Rust. The file dialog is opened by
//! `import_pick_files`, and a drop onto the main window arrives as a
//! `WindowEvent::DragDrop` in `lib.rs`. Either way the paths go straight into
//! [`ImportQueue::enqueue`] and the webview never sees them; the page only
//! receives [`ImportProgressPayload`] snapshots, which carry each file's own
//! name and nothing of its directory.
//!
//! # What a run does
//!
//! Start claims a run and works through the queued rows one at a time, in the
//! order they were added. For each row it probes the file (is it audio, how
//! long is it, is it within the ceilings), converts it to 16 kHz mono WAV,
//! uploads that, runs a batch job, downloads the transcript and saves the
//! note. Every file is converted, whatever its format, because 16 kHz mono
//! WAV is the one input the batch job API transcribes reliably; see
//! `media::decode`.
//!
//! Each file gets a job of its own even though Sarvam accepts several per
//! job. A job succeeds or fails as a whole, so sharing one would let a single
//! unreadable file take the other rows down with it.
//!
//! # Runs, and how they are stopped
//!
//! The queue keeps a run number. Start, Cancel and Clear each move it on, and
//! a run only writes while the number is still its own: every row update,
//! the note save and the end-of-run bookkeeping check it under the queue
//! lock. A run that has been superseded therefore stops quietly at its next
//! check and leaves everything it would have touched to whoever owns the
//! queue now.
//!
//! Stopping the network work is the other half. The row being transcribed
//! has a request id registered in a [`CancelRegistry`]; Cancel and Clear fire
//! that registration, which ends an upload, a status poll or a download at
//! once instead of when it next returns.
//!
//! # Logging
//!
//! Counts, states and outcomes only. A path, a file name or any part of a
//! transcript is personal, so none of them is ever a log field; the file name
//! appears only in the progress payload the Import page renders.
//! `tests::the_module_logs_no_path_or_filename_field` reads this file back to
//! hold that line.

pub mod cancel;

use crate::events::{ImportItemPayload, ImportProgressPayload};
use crate::media::probe;
use crate::notes;
use crate::sarvam::batch_job::{self, JobError, Segment};
use cancel::{CancelRegistry, CancelToken, Registration};
use std::collections::HashSet;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// How much of a recording the transcript has to reach, as the last
/// segment's end over the probed duration, before the note goes without an
/// [`incomplete_banner`]. Reaching it exactly counts as complete.
///
/// Measured with one batch job (16 kHz mono WAV, English and Hindi speech,
/// six files):
/// - A quiet ending of up to about 3 s is absorbed: the last segment's end
///   is stretched to the end of the file, so coverage reads 1.0. That held
///   for a 9 s memo with 3.2 s of silence after the words.
/// - A longer ending of silence or music is not: the last segment ends
///   within 0.1 s of the last word. 20 s of speech followed by 20 s of
///   silence, or of music, read 0.49; 16 s of speech and 30 s of music, 0.35.
///
/// So on a complete recording the figure is the share taken up by
/// speech, and the banner cannot tell a long quiet ending from a job that
/// stopped early; the ratio decides how long an ending may be before the
/// user is told to check. Two thirds keeps the usual endings clear (a
/// fumbled stop is absorbed, an outro or a round of applause is a small
/// share of anything long) and still flags a transcript that loses the last
/// third of a recording. A complete recording that is more than a third
/// silence or music at its end gets the banner, and the banner's own wording
/// ("may be incomplete", with both positions) lets the user see why.
pub const MIN_COVERAGE_RATIO: f64 = 2.0 / 3.0;

/// Where a row is in its import. [`Self::as_str`] is the wire form the Import
/// page keys its labels on, so the eight strings are a contract with
/// `src/lib/events.ts`.
///
/// `Probing` and `Converting` happen on this machine; `Uploading` and
/// `Transcribing` are announced from inside [`Steps::transcribe`], because
/// only it knows when the WAV is written and when the job has started.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ItemState {
    Queued,
    Probing,
    Converting,
    Uploading,
    Transcribing,
    Done,
    Failed,
    Cancelled,
}

impl ItemState {
    pub fn as_str(self) -> &'static str {
        match self {
            ItemState::Queued => "queued",
            ItemState::Probing => "probing",
            ItemState::Converting => "converting",
            ItemState::Uploading => "uploading",
            ItemState::Transcribing => "transcribing",
            ItemState::Done => "done",
            ItemState::Failed => "failed",
            ItemState::Cancelled => "cancelled",
        }
    }

    /// True once nothing more will happen to the row in this run: it became a
    /// note, it failed, or it was cancelled. The progress bar counts exactly
    /// these.
    pub fn is_settled(self) -> bool {
        matches!(
            self,
            ItemState::Done | ItemState::Failed | ItemState::Cancelled
        )
    }
}

/// One file in the queue.
struct Row {
    id: u64,
    /// The file's own name, shown on the page and used for the note title.
    name: String,
    path: PathBuf,
    state: ItemState,
    error: Option<String>,
    detail: Option<&'static str>,
    note_id: Option<i64>,
}

impl Row {
    fn payload(&self) -> ImportItemPayload {
        ImportItemPayload {
            id: self.id,
            name: self.name.clone(),
            state: self.state.as_str(),
            error: self.error.clone(),
            detail: self.detail,
            note_id: self.note_id,
        }
    }
}

/// The name a row shows when its path has no final component to show.
const UNNAMED_FILE: &str = "Unnamed file";

/// Shown on a row whose container states no playing time. `media::probe`
/// returns `None` rather than `0.0` for exactly this case, and `check_limits`
/// lets it pass — so the honest thing is to import it and say so, not to
/// refuse it on the strength of a ceiling that could not be applied.
const LENGTH_UNKNOWN: &str = "Length unknown";

/// Everything the queue lock guards.
#[derive(Default)]
struct Ledger {
    rows: Vec<Row>,
    /// The id the next added row gets. Never reset, so ids stay unique for
    /// the life of the process, clears included.
    next_row_id: u64,
    running: bool,
    /// The current run's number. Start, Cancel and Clear each advance it; a
    /// run whose number no longer matches has been superseded.
    run: u64,
    /// The request id of the row that may be on the network, tagged with the
    /// run that opened it.
    on_wire: Option<(u64, String)>,
}

impl Ledger {
    fn progress(&self) -> ImportProgressPayload {
        let count = |state: ItemState| self.rows.iter().filter(|r| r.state == state).count() as u32;
        let done = count(ItemState::Done);
        let failed = count(ItemState::Failed);
        let cancelled = count(ItemState::Cancelled);
        let total = self.rows.len() as u32;
        let settled = done + failed + cancelled;
        // Settled over total, rounded half up in integers.
        let percent = if total == 0 {
            0
        } else {
            (settled * 200 + total) / (total * 2)
        };
        ImportProgressPayload {
            running: self.running,
            total,
            done,
            failed,
            cancelled,
            percent,
            items: self.rows.iter().map(Row::payload).collect(),
        }
    }

    fn row_mut(&mut self, id: u64) -> Option<&mut Row> {
        self.rows.iter_mut().find(|r| r.id == id)
    }
}

/// Where a progress snapshot goes. A trait so a run can be tested without
/// an `AppHandle`, and so the emitted sequence — not merely the final state —
/// is what the tests assert.
pub trait ProgressSink: Send + Sync + 'static {
    fn emit(&self, payload: ImportProgressPayload);
}

/// The `AppHandle` implementation. Lives here rather than in `commands.rs` so
/// the event name is used in exactly one place.
pub struct AppSink(pub tauri::AppHandle);

impl ProgressSink for AppSink {
    fn emit(&self, payload: ImportProgressPayload) {
        use tauri::Emitter;
        let _ = self.0.emit(crate::events::IMPORT_PROGRESS, payload);
    }
}

/// What a run's next step is, decided under the lock.
enum Next {
    /// Work on this row: its id, its path and its display name.
    Row(u64, PathBuf, String),
    /// Nothing queued is left. The run has already been closed; these are
    /// its final counts.
    Finished(ImportProgressPayload),
    /// Another Start, Cancel or Clear owns the queue now.
    Superseded,
}

/// What a run records on one of its rows as the row moves along.
enum Mark {
    /// The row is at this step, or ended cancelled.
    At(ItemState),
    /// The row ended in failure, with a sentence for the user.
    Failed(String),
    /// The row ended as this note.
    Saved(i64),
}

impl Mark {
    fn write_to(self, row: &mut Row) {
        match self {
            Mark::At(state) => row.state = state,
            Mark::Failed(message) => {
                row.state = ItemState::Failed;
                row.error = Some(message);
            }
            Mark::Saved(note_id) => {
                row.state = ItemState::Done;
                row.note_id = Some(note_id);
            }
        }
    }
}

/// The queue's state, the sink it publishes to, and the cancel registry for
/// the row on the network. Shared between the `ImportQueue` handle and the
/// run task.
struct Core {
    ledger: Mutex<Ledger>,
    sink: Box<dyn ProgressSink>,
    requests: CancelRegistry,
}

impl Core {
    /// The lock, recovered if a panic poisoned it. Every change made under it
    /// is a whole step, so a poisoned ledger is still a consistent one.
    fn ledger(&self) -> std::sync::MutexGuard<'_, Ledger> {
        self.ledger.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Send the current snapshot. Built under the lock, emitted after it is
    /// released, so a sink that calls back into the queue cannot deadlock.
    fn publish(&self) {
        let payload = self.ledger().progress();
        self.sink.emit(payload);
    }

    fn is_superseded(&self, run: u64) -> bool {
        self.ledger().run != run
    }

    /// Record `mark` on row `id`, then publish, but only while `run` owns the
    /// queue and the row is still there. Returns whether it was recorded.
    fn mark(&self, run: u64, id: u64, mark: Mark) -> bool {
        let recorded = {
            let mut ledger = self.ledger();
            let current = ledger.run == run;
            match ledger.row_mut(id) {
                Some(row) if current => {
                    mark.write_to(row);
                    true
                }
                _ => false,
            }
        };
        if recorded {
            self.publish();
        }
        recorded
    }

    /// Pick the first queued row this run has not handled yet. When there is
    /// none, close the run in the same critical section, so a file added a
    /// moment later either joins this run or waits for the next Start.
    fn next(&self, run: u64, handled: &HashSet<u64>) -> Next {
        let mut ledger = self.ledger();
        if ledger.run != run {
            return Next::Superseded;
        }
        let waiting = ledger
            .rows
            .iter()
            .filter(|row| !handled.contains(&row.id))
            .find(|row| matches!(row.state, ItemState::Queued));
        if let Some(row) = waiting {
            return Next::Row(row.id, row.path.clone(), row.name.clone());
        }
        ledger.running = false;
        ledger.on_wire = None;
        Next::Finished(ledger.progress())
    }

    /// Move row `id` to `converting`, record `detail`, and record a fresh
    /// request id as this run's work on the wire. `None` if the run has been
    /// superseded.
    fn open_request(&self, run: u64, id: u64, detail: Option<&'static str>) -> Option<String> {
        let request_id = uuid::Uuid::new_v4().to_string();
        {
            let mut ledger = self.ledger();
            if ledger.run != run {
                return None;
            }
            let row = ledger.row_mut(id)?;
            row.state = ItemState::Converting;
            if detail.is_some() {
                row.detail = detail;
            }
            ledger.on_wire = Some((run, request_id.clone()));
        }
        self.publish();
        Some(request_id)
    }

    /// A cancellable handle for `request_id`, or `None` when `run` no longer
    /// owns the queue.
    ///
    /// A Cancel or Clear that lands in this method's window can only do one
    /// of two things: bump the run number before the ownership test below,
    /// which the test sees, or reach the registry after the handle exists,
    /// which fires it. Either way the row cannot get onto the network with
    /// a token nobody can fire.
    fn register_if_current(&self, run: u64, request_id: &str) -> Option<Registration<'_>> {
        let handle = self.requests.register(request_id);
        (!self.is_superseded(run)).then_some(handle)
    }

    /// Forget `request_id` as the work on the wire, but only while it is
    /// still this run's.
    fn close_request(&self, run: u64, request_id: &str) {
        let mut ledger = self.ledger();
        let ours = matches!(&ledger.on_wire, Some((r, id)) if *r == run && id == request_id);
        if ours {
            ledger.on_wire = None;
        }
    }

    /// End whatever run owns the queue. Every unsettled row becomes
    /// `cancelled`, or every row goes when `empty` is set, and the network
    /// work in flight is fired. Returns how many registrations fired.
    fn supersede(&self, empty: bool) -> usize {
        let on_wire = {
            let mut ledger = self.ledger();
            ledger.run += 1;
            ledger.running = false;
            if empty {
                ledger.rows.clear();
            } else {
                for row in ledger.rows.iter_mut().filter(|r| !r.state.is_settled()) {
                    row.state = ItemState::Cancelled;
                }
            }
            ledger.on_wire.take()
        };
        let aborted = on_wire.map_or(0, |(_, request_id)| self.requests.cancel(&request_id));
        self.publish();
        aborted
    }
}

// ---------------------------------------------------------------------------
// The work, behind a seam.
// ---------------------------------------------------------------------------

/// One finished transcription.
pub struct Transcript {
    pub segments: Vec<Segment>,
    pub text: String,
}

/// Why a step stopped. `Cancelled` is kept apart from `Failed` all the way
/// through: a cancelled item is not a failed one, and telling the user their
/// import "failed" because they cancelled it is the small lie that makes an
/// error list untrustworthy.
#[derive(Debug)]
pub enum StepError {
    Cancelled,
    /// Already a finished sentence for the user.
    Failed(String),
}

type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// The work a run does for one row, kept behind a trait so the queue can be
/// driven by a stub in tests. [`LiveSteps`] is the implementation that reads
/// real files and talks to Sarvam.
pub trait Steps: Send + Sync + 'static {
    /// Read the container headers and apply the client-side ceilings. Blocking:
    /// the run calls it on a blocking thread.
    fn probe(&self, path: &Path) -> Result<probe::Probe, String>;

    /// Convert, upload, start, wait and download. `on_state` is how the item
    /// moves from `converting` to `uploading` to `transcribing` — both
    /// transitions are only knowable in here, once the WAV is written and once
    /// the bytes are up and the job has been started.
    ///
    /// The run leaves the item in [`ItemState::Converting`] before calling
    /// this, so an implementation that converts nothing should announce
    /// `Uploading` immediately.
    fn transcribe<'a>(
        &'a self,
        path: &'a Path,
        duration_s: Option<f64>,
        cancel: &'a CancelToken,
        on_state: &'a (dyn Fn(ItemState) + Send + Sync),
    ) -> BoxFuture<'a, Result<Transcript, StepError>>;

    /// Persist the note. Blocking, for the same reason as `probe`.
    fn save_note(&self, note: notes::NewNote) -> Result<i64, String>;

    /// Undo a [`Self::save_note`] whose run was cancelled while the INSERT was
    /// in flight (see `import_one`). Blocking, like the save. It cannot fail in
    /// a way the user could act on (the run is already cancelled and the row
    /// was written milliseconds ago), so it reports nothing and logs instead.
    fn discard_note(&self, id: i64);
}

// ---------------------------------------------------------------------------
// The queue.
// ---------------------------------------------------------------------------

/// The import queue. Cheap to call from any thread; a run, once started,
/// lives on its own task and holds the shared state itself.
pub struct ImportQueue {
    core: Arc<Core>,
}

/// Shown when Start is pressed while a run is already going. Not reachable
/// from the shipped UI (the button is disabled), which is exactly why it has to
/// be a sentence rather than an `unreachable!`.
const ALREADY_RUNNING: &str = "An import is already running — wait for it to finish, or cancel it.";
const NOTHING_QUEUED: &str = "There's nothing to import — add a recording first.";

impl ImportQueue {
    pub fn new(sink: Box<dyn ProgressSink>) -> Self {
        ImportQueue {
            core: Arc::new(Core {
                ledger: Mutex::new(Ledger::default()),
                sink,
                requests: CancelRegistry::default(),
            }),
        }
    }

    /// Add one row per path, in order. A file whose extension is not accepted
    /// is still added, already failed with a sentence that names the
    /// extension, so it cannot vanish without a word. Returns how many files
    /// were accepted. Never starts a run.
    pub fn enqueue(&self, paths: Vec<PathBuf>) -> usize {
        let added = paths.len();
        let mut accepted = 0;
        {
            let mut ledger = self.core.ledger();
            for path in paths {
                let id = ledger.next_row_id;
                ledger.next_row_id += 1;
                let name = path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .filter(|n| !n.is_empty())
                    .unwrap_or_else(|| UNNAMED_FILE.to_string());
                let (state, error) = if probe::is_accepted_extension(&path) {
                    accepted += 1;
                    (ItemState::Queued, None)
                } else {
                    let ext = path
                        .extension()
                        .map(|e| e.to_string_lossy().to_lowercase())
                        .unwrap_or_default();
                    let refusal = probe::ProbeError::UnsupportedExtension { ext };
                    (ItemState::Failed, Some(refusal.user_message()))
                };
                ledger.rows.push(Row {
                    id,
                    name,
                    path,
                    state,
                    error,
                    detail: None,
                    note_id: None,
                });
            }
        }
        tracing::info!(
            added,
            accepted,
            refused = added - accepted,
            "import rows added"
        );
        self.core.publish();
        accepted
    }

    pub fn snapshot(&self) -> ImportProgressPayload {
        self.core.ledger().progress()
    }

    /// Claim a run and spawn it. The task is detached: it keeps going when
    /// the Import page unmounts, and the page picks the progress back up from
    /// [`Self::snapshot`].
    pub fn start(&self, steps: Arc<dyn Steps>) -> Result<(), String> {
        let run = self.claim_run()?;
        tauri::async_runtime::spawn(run_queue(Arc::clone(&self.core), steps, run));
        Ok(())
    }

    /// Mark the queue running under a fresh run number, without spawning.
    fn claim_run(&self) -> Result<u64, String> {
        let mut ledger = self.core.ledger();
        let has_work = ledger.rows.iter().any(|row| matches!(row.state, ItemState::Queued));
        let refusal = match (ledger.running, has_work) {
            (true, _) => Some(ALREADY_RUNNING),
            (false, false) => Some(NOTHING_QUEUED),
            (false, true) => None,
        };
        if let Some(sentence) = refusal {
            return Err(sentence.to_string());
        }
        ledger.running = true;
        ledger.run += 1;
        let run = ledger.run;
        drop(ledger);
        tracing::info!("import run started");
        self.core.publish();
        Ok(run)
    }

    /// Stop the current run: rows not yet settled become `cancelled` and the
    /// network work in flight is aborted. Returns how many in-flight
    /// operations were aborted, `0` when nothing was running.
    pub fn cancel(&self) -> usize {
        let aborted = self.core.supersede(false);
        tracing::info!(aborted, "import run cancelled");
        aborted
    }

    /// Stop the current run as [`Self::cancel`] does and remove every row. A
    /// new run can start straight away.
    pub fn clear(&self) {
        let aborted = self.core.supersede(true);
        tracing::info!(aborted, "every import row removed");
    }
}

/// Shown on a row when the probe's thread dies instead of answering.
const MSG_PROBE_CRASHED: &str =
    "Butterfly Speak couldn't check that file — please try importing it again.";

/// Shown on a row when the save's thread dies after the transcript arrived.
const MSG_SAVE_CRASHED: &str =
    "The transcript came back, but saving it as a note failed — please try importing it again.";

/// One run: the queued rows, one at a time, in the order they were added,
/// until none is left or the run is superseded.
async fn run_queue(core: Arc<Core>, steps: Arc<dyn Steps>, run: u64) {
    let mut handled = HashSet::new();
    loop {
        match core.next(run, &handled) {
            Next::Superseded => return,
            Next::Finished(progress) => {
                tracing::info!(
                    total = progress.total,
                    done = progress.done,
                    failed = progress.failed,
                    cancelled = progress.cancelled,
                    "import run reached the end of the queue"
                );
                // A fresh snapshot rather than the one taken at the close: if
                // a newer Start slipped in since, the page must see that one.
                core.publish();
                return;
            }
            Next::Row(id, path, name) => {
                handled.insert(id);
                import_one(&core, &steps, run, id, path, &name).await;
            }
        }
    }
}

/// Probe, transcribe and save one row. Returns early, writing nothing, as
/// soon as the run turns out to be superseded.
async fn import_one(
    core: &Arc<Core>,
    steps: &Arc<dyn Steps>,
    run: u64,
    id: u64,
    path: PathBuf,
    name: &str,
) {
    if !core.mark(run, id, Mark::At(ItemState::Probing)) {
        return;
    }
    let probed = {
        let steps = Arc::clone(steps);
        let path = path.clone();
        tokio::task::spawn_blocking(move || steps.probe(&path)).await
    };
    if core.is_superseded(run) {
        return;
    }
    let duration_s = match probed {
        Ok(Ok(p)) => p.duration_s,
        Ok(Err(message)) => {
            tracing::info!("an import was refused by the probe");
            core.mark(run, id, Mark::Failed(message));
            return;
        }
        Err(e) => {
            tracing::error!(panicked = e.is_panic(), "probing an import did not return");
            core.mark(run, id, Mark::Failed(MSG_PROBE_CRASHED.to_string()));
            return;
        }
    };

    let detail = duration_s.is_none().then_some(LENGTH_UNKNOWN);
    let Some(request_id) = core.open_request(run, id, detail) else {
        return;
    };
    let Some(registration) = core.register_if_current(run, &request_id) else {
        return;
    };
    let on_state = {
        let core = Arc::clone(core);
        move |state: ItemState| {
            core.mark(run, id, Mark::At(state));
        }
    };
    let outcome = steps
        .transcribe(&path, duration_s, registration.token(), &on_state)
        .await;
    drop(registration);
    core.close_request(run, &request_id);
    if core.is_superseded(run) {
        return;
    }

    let transcript = match outcome {
        Ok(t) => t,
        Err(StepError::Cancelled) => {
            core.mark(run, id, Mark::At(ItemState::Cancelled));
            return;
        }
        Err(StepError::Failed(message)) => {
            tracing::info!("an import failed while transcribing");
            core.mark(run, id, Mark::Failed(message));
            return;
        }
    };
    if transcript_is_empty(&transcript) {
        tracing::info!("an import transcribed to nothing and was not saved");
        core.mark(run, id, Mark::Failed(MSG_NOTHING_TRANSCRIBED.to_string()));
        return;
    }

    let note = build_note(name, duration_s, &transcript);
    if core.is_superseded(run) {
        return;
    }
    let saved = {
        let steps = Arc::clone(steps);
        tokio::task::spawn_blocking(move || steps.save_note(note)).await
    };
    match saved {
        Ok(Ok(note_id)) => {
            if !core.mark(run, id, Mark::Saved(note_id)) {
                // Cancelled or cleared while the INSERT ran: the note is
                // committed but nothing in the queue points at it any more.
                tracing::info!("an import was cancelled during its save; rolling the note back");
                let steps = Arc::clone(steps);
                let rollback = tokio::task::spawn_blocking(move || steps.discard_note(note_id));
                if let Err(e) = rollback.await {
                    tracing::error!(
                        panicked = e.is_panic(),
                        "rolling back a cancelled import's note did not return"
                    );
                }
            }
        }
        Ok(Err(message)) => {
            tracing::warn!("an import's note could not be saved");
            core.mark(run, id, Mark::Failed(message));
        }
        Err(e) => {
            tracing::error!(panicked = e.is_panic(), "the import save thread failed");
            core.mark(run, id, Mark::Failed(MSG_SAVE_CRASHED.to_string()));
        }
    }
}

// ---------------------------------------------------------------------------
// Turning a transcript into a note. Pure, so every rule below is testable
// without a queue, a file or a database.
// ---------------------------------------------------------------------------

/// One timed segment as it is stored in a note's `transcript_json`.
///
/// The field names are fixed: `notes::transcript_text` reads exactly
/// `text`, `start` and `end`, and notes already on users' disks hold this
/// shape. Renaming a field would make those notes unsearchable.
#[derive(serde::Serialize)]
struct StoredSegment<'a> {
    text: &'a str,
    start: f64,
    end: f64,
}

/// A position in a recording, written the way a media player's time display
/// writes it, so the user can find the spot the banner names: `m:ss` under
/// an hour, `h:mm:ss` from an hour on. Minutes are not padded below an hour;
/// seconds, and minutes after an hour, always have two digits.
///
/// Fractions of a second are dropped, not rounded, because a player's
/// elapsed-time counter only ticks over once the whole second has played.
/// A negative or non-finite input reads as `0:00`.
fn clock(seconds: f64) -> String {
    let whole = if seconds.is_finite() && seconds > 0.0 {
        seconds.floor() as u64
    } else {
        0
    };
    let (h, m, s) = (whole / 3600, whole / 60 % 60, whole % 60);
    if h == 0 {
        format!("{m}:{s:02}")
    } else {
        format!("{h}:{m:02}:{s:02}")
    }
}

/// The line a note gets when its transcript stops well short of the recording.
///
/// `None` whenever there is nothing to compare: no probed duration (a
/// header-less VBR MP3 states none), or no timestamps at all. Claiming a
/// transcript is incomplete on the strength of a number that was never measured
/// would be worse than saying nothing.
pub fn incomplete_banner(duration_s: Option<f64>, segments: &[Segment]) -> Option<String> {
    let duration = duration_s.filter(|d| d.is_finite() && *d > 0.0)?;
    let last = segments.last()?.end_s;
    if !last.is_finite() || last >= duration * MIN_COVERAGE_RATIO {
        return None;
    }
    Some(format!(
        "{BANNER_OPENER} — it ends at {} of a {} recording.]",
        clock(last),
        clock(duration)
    ))
}

/// How every [`incomplete_banner`] starts. A constant so the banner and the
/// stripper below cannot drift into disagreeing about what one looks like.
const BANNER_OPENER: &str = "[Transcript may be incomplete";

/// `text` without a leading [`incomplete_banner`] line.
///
/// The banner is a note about the *recording*, not part of what was said, and
/// [`build_note`] puts it on the first line — so it is the first thing a model
/// asked to title the note reads, and a note whose transcript came up short
/// gets titled after the warning rather than after its content.
///
/// Only ever applied to a copy on its way to a model. The stored note keeps
/// its banner: the user has to be told the transcript may be short, and that
/// is the line that tells them.
///
/// A banner-only note (an empty transcript) strips to `""`, which the title
/// call already refuses with "there's nothing in this note" — the right answer,
/// where titling from the warning text is not.
pub fn without_incomplete_banner(text: &str) -> &str {
    let trimmed = text.trim_start();
    if !trimmed.starts_with(BANNER_OPENER) {
        return text;
    }
    match trimmed.split_once('\n') {
        // A closing `]` on that first line is what makes it the whole banner
        // and not a note that merely opens with the same words.
        Some((first, rest)) if first.trim_end().ends_with(']') => rest.trim_start(),
        None if trimmed.trim_end().ends_with(']') => "",
        _ => text,
    }
}

/// The note's title: the file name without its last extension, trimmed.
/// A name with nothing left after that gets a generic title rather than an
/// empty one.
pub fn title_from_file_name(file_name: &str) -> String {
    let stem = Path::new(file_name)
        .file_stem()
        .map(|s| s.to_string_lossy().trim().to_string())
        .unwrap_or_default();
    if stem.is_empty() {
        "Imported recording".to_string()
    } else {
        stem
    }
}

/// Whether a finished transcription produced anything a note could hold.
///
/// Sarvam answers a recording it could not read exactly the way it answers
/// silence: a job that creates, uploads, starts and completes normally, with
/// one output and no text. Measured on a 7 MB `.mp4` whose audio track
/// probed fine (`isomp4`, 44100 Hz) — `segments=0 text_chars=0`.
///
/// Without this check such a run reports **done**: [`build_note`] turns the
/// empty text into an empty `content` (`incomplete_banner` cannot fire either,
/// having no last segment to measure), so the queue says IMPORTED and Notes
/// grows a note titled after the file with nothing in it. To the user that
/// reads as "the app lost my transcript" rather than "there was never a
/// transcript", which is the more alarming of the two and the wrong one.
///
/// Deliberately conservative: *any* readable text anywhere keeps the import,
/// because a short transcript is still a transcript and
/// [`incomplete_banner`] is what warns about those.
pub fn transcript_is_empty(t: &Transcript) -> bool {
    t.text.trim().is_empty() && t.segments.iter().all(|s| s.text.trim().is_empty())
}

/// What an import that transcribed nothing tells the user. A finished
/// sentence, like every other [`StepError::Failed`] message, and deliberately
/// short of blaming the file: the two causes this cannot tell apart are a
/// recording with no speech in it and a container Sarvam would not read.
pub const MSG_NOTHING_TRANSCRIBED: &str =
    "Sarvam returned no text for this recording — it may have no speech in it, or be in a format Sarvam couldn't read.";

/// The note an import becomes: type `upload`, titled after the file, with the
/// transcript as its content and its segments as `transcript_json`.
///
/// When [`incomplete_banner`] fires, the banner is the first line and the
/// transcript follows it. A transcript with no timed segments stores no
/// segment JSON and keeps its text. Takes the file's name, never its path.
pub fn build_note(file_name: &str, duration_s: Option<f64>, t: &Transcript) -> notes::NewNote {
    let stored: Vec<StoredSegment<'_>> = t
        .segments
        .iter()
        .map(|s| StoredSegment {
            text: &s.text,
            start: s.start_s,
            end: s.end_s,
        })
        .collect();
    // A transcript that fails to serialize must not fail the import; the note
    // is still worth having without its timing.
    let transcript_json = (!stored.is_empty())
        .then(|| serde_json::to_string(&stored).ok())
        .flatten();

    let content = match incomplete_banner(duration_s, &t.segments) {
        Some(banner) if t.text.is_empty() => banner,
        Some(banner) => format!("{banner}\n\n{}", t.text),
        None => t.text.clone(),
    };

    notes::NewNote {
        folder_id: None,
        kind: Some("imported".to_string()),
        title: Some(title_from_file_name(file_name)),
        content: Some(content),
        transcript_json,
        // The name only. A note is shown, exported and mirrored to disk, and
        // the folder a recording came from is not the note's business.
        imported_file: Some(file_name.to_string()),
        audio_seconds: duration_s,
        // "Now", which is what the columns default to, and on purpose: the
        // note list is ordered by last change, so a fresh import lands at the
        // top where the user is looking for it. A file's modified time is
        // often when it was downloaded or synced from a phone rather than
        // when it was recorded, and a note stamped with it would be filed
        // among older notes.
        created_at: None,
        updated_at: None,
    }
}

// ---------------------------------------------------------------------------
// The live implementation.
// ---------------------------------------------------------------------------

/// The real [`Steps`]: symphonia for the probe and the conversion, the Sarvam
/// batch job API for the transcript, and the history database for the note.
///
/// Built once per run from a snapshot of settings (key, language, mode,
/// ceilings, mirror folder), so a settings change made during a run applies
/// to the next one rather than to half of this one.
pub struct LiveSteps {
    client: batch_job::JobClient,
    cfg: batch_job::JobCfg,
    limits: probe::ImportLimits,
    notes: crate::history::Recorder,
    /// Where the markdown mirror writes, or `None` when it must not write —
    /// `settings::NotesSettings::mirror_root`, snapshotted at run start like
    /// every other field here.
    ///
    /// Snapshotted rather than read per save for this struct's own reason (a
    /// settings change mid-run is not seen) *and* for a lock-ordering one: the
    /// settings guard must never be held across a `with_connection`, and
    /// `save_note` runs inside one.
    mirror_root: Option<PathBuf>,
}

const NOTES_UNAVAILABLE: &str =
    "The transcript came back, but the local database didn't open this session, so it \
     couldn't be saved as a note.";

/// Give up on a connection that never opens. Short, because a host that has
/// not answered a TCP handshake in this long is not slow, it is unreachable.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

/// The client carries **only** the connect timeout. Every per-request budget
/// lives on the `RequestBuilder` inside `JobClient`, sized to the leg — see
/// that module's request-budget constants.
///
/// This deliberately does not set `read_timeout`. An earlier revision did, on
/// the strength of the name, and it silently capped uploads: on the request
/// side that setting is a single deadline armed when the request is built and
/// checked before the response head arrives, not the per-read idle timer it
/// sounds like, so a 60 s value refused any file that took longer than 60 s to
/// push — about 57 MB on a 5 Mbit/s uplink. A whole-request budget is still
/// what reqwest can express here; the fix is to size it against the bytes
/// rather than to pick one number for every leg.
fn http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .build()
        // `expect`, not a fallback to `Client::new()`: the fallback would
        // silently drop the connect timeout and hand back a client that looks
        // fine and hangs differently. This fails only if the TLS backend
        // cannot be initialised, which no amount of retrying fixes.
        .expect("reqwest client (TLS backend failed to initialise)")
}

impl LiveSteps {
    /// `language_code` is the *realtime* code from settings; it is translated
    /// here, once. `sarvam::batch::to_rest_language_code` is the existing
    /// mapping and the Batch job API is on the REST side of that split —
    /// sending `or-IN` instead of `od-IN` earns an empty-bodied HTTP 400.
    pub fn new(
        api_key: String,
        language_code: &str,
        mode: String,
        limits: probe::ImportLimits,
        notes: crate::history::Recorder,
        mirror_root: Option<PathBuf>,
    ) -> Self {
        LiveSteps {
            client: batch_job::JobClient::new(http_client(), batch_job::JOB_BASE_URL, api_key),
            cfg: batch_job::JobCfg {
                // Passed through as the app's own realtime vocabulary
                // (`or-IN`, `auto`). `create_job` translates it to the REST
                // vocabulary itself — deliberately, so correct Odia is not a
                // convention every call site has to remember — and doing it
                // again here would only make this the second place that has
                // to stay right.
                language_code: language_code.to_string(),
                mode,
                ..batch_job::JobCfg::default()
            },
            limits,
            notes,
            mirror_root,
        }
    }
}

/// Race one leg of the job against the cancel token.
///
/// `biased`, so an already-cancelled token wins before the request is even
/// built. `wait_for_completion` is deliberately **not** wrapped this way: it
/// takes the cancellation future itself, so it can return a clean
/// [`JobError::Cancelled`] and log that it stopped waiting on a job whose id it
/// still knows. Dropping that future from out here would abandon the same work
/// with nothing to say about it.
async fn guard<T>(
    cancel: &CancelToken,
    work: impl Future<Output = Result<T, JobError>>,
) -> Result<T, StepError> {
    // Checked before the future is even polled: a request built and
    // immediately dropped still opens a connection, and on the upload leg it
    // would start reading the converted WAV, about 230 MB for a two-hour
    // recording.
    if cancel.is_cancelled() {
        return Err(StepError::Cancelled);
    }
    tokio::select! {
        biased;
        () = cancel.cancelled() => Err(StepError::Cancelled),
        result = work => result.map_err(from_job_error),
    }
}

fn from_job_error(e: JobError) -> StepError {
    match e {
        JobError::Cancelled => StepError::Cancelled,
        other => StepError::Failed(other.user_message()),
    }
}

/// A scratch WAV that deletes itself.
///
/// The name is minted per instance rather than per item, so two runs — or
/// two *installs* sharing the OS temp directory — cannot land on it at once.
/// A run handles one file at a time today, and nothing here relies on
/// that: the cost of a UUID is nothing against a concurrent import silently
/// uploading the other one's audio.
///
/// The extension matters and is not decoration. `sarvam::batch_job`'s
/// `upload_file_name` derives the name on the wire from the path it is given,
/// so `.wav` here is what makes Sarvam's storage layer receive `audio.wav`.
///
/// `Drop` rather than an explicit cleanup call, as with
/// [`cancel::Registration`]: a guard cannot be forgotten on an early return,
/// a `?`, or a future that is dropped mid-`await`. A two-hour recording
/// converts to about 230 MB of the user's own speech, which is not something
/// to leave in `%TEMP%` because a branch was missed.
struct ScratchWav(PathBuf);

impl ScratchWav {
    fn new() -> ScratchWav {
        ScratchWav(
            std::env::temp_dir().join(format!("bs-import-{}.wav", uuid::Uuid::new_v4())),
        )
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for ScratchWav {
    fn drop(&mut self) {
        // A conversion that failed has already removed it; a missing file is
        // the expected case, not an error worth a line in the log.
        if let Err(e) = std::fs::remove_file(&self.0) {
            if e.kind() != std::io::ErrorKind::NotFound {
                tracing::warn!(kind = ?e.kind(), "a converted import could not be cleaned up");
            }
        }
    }
}

/// How long a converted import must sit untouched before the startup sweep
/// takes it. This app is single-instance, so at its start none of its own
/// runs is live; the wait is for a second install (a development build, say)
/// sharing `%TEMP%` and importing right now.
const SCRATCH_STALE_AFTER: Duration = Duration::from_secs(24 * 60 * 60);

/// Remove converted imports that a killed run left behind in `dir` (the OS
/// temp folder), and return how many went. A run killed mid-import (the
/// uninstaller and the installer both kill the app) never reaches
/// [`ScratchWav`]'s `Drop`. Only files with the exact name a `ScratchWav`
/// mints (`bs-import-<uuid>.wav`) and older than [`SCRATCH_STALE_AFTER`] are
/// touched; folders never are. Called once at startup, off the main thread.
pub fn sweep_stale_scratch(dir: &Path, now: std::time::SystemTime) -> usize {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    let mut removed = 0;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let is_scratch = name
            .strip_prefix("bs-import-")
            .and_then(|rest| rest.strip_suffix(".wav"))
            .is_some_and(|id| id.len() == 36 && uuid::Uuid::parse_str(id).is_ok());
        if !is_scratch {
            continue;
        }
        // `DirEntry::metadata` does not follow a link, so a link or folder
        // under this name is left alone.
        let Ok(meta) = entry.metadata() else { continue };
        if !meta.is_file() {
            continue;
        }
        let stale = meta
            .modified()
            .ok()
            .and_then(|at| now.duration_since(at).ok())
            .is_some_and(|age| age > SCRATCH_STALE_AFTER);
        if stale && std::fs::remove_file(entry.path()).is_ok() {
            removed += 1;
        }
    }
    removed
}

/// What the conversion says when the thread running it dies. It cannot happen
/// — `media::decode` returns its errors as values — so this is the sentence
/// for a bug, which still has to be a sentence.
const MSG_CONVERSION_PANICKED: &str =
    "Butterfly Speak couldn't prepare that recording for upload — please try the import again.";

/// Decode `src` to 16 kHz mono WAV at `dst`, off the async runtime.
///
/// `spawn_blocking` for the same reason `batch_job::upload` uses it: this
/// runtime also carries `ws::run_session`'s single dispatcher, and decoding a
/// two-hour recording is minutes of uninterrupted CPU. The token is checked
/// before the task is even spawned and then once per packet inside it, so a
/// cancel does not have to wait out the whole file.
///
/// Deliberately **not** wrapped in [`guard`], unlike every network leg. A
/// blocking task cannot be aborted by dropping its `JoinHandle` — it runs on
/// regardless — so racing it against the token would return while the decode
/// carried on writing to a scratch file the caller's guard had already
/// deleted, and leave the orphan behind. Waiting for the task to notice the
/// token itself is the only thing that actually ends the work.
async fn convert_for_upload(
    src: &Path,
    dst: &Path,
    cancel: &CancelToken,
) -> Result<crate::media::decode::Converted, StepError> {
    use crate::media::decode::{self, DecodeError};

    if cancel.is_cancelled() {
        return Err(StepError::Cancelled);
    }
    let owned_src = src.to_path_buf();
    let owned_dst = dst.to_path_buf();
    let token = cancel.clone();
    let done = tokio::task::spawn_blocking(move || {
        decode::to_wav_16k_mono(&owned_src, &owned_dst, &|| token.is_cancelled())
    })
    .await;
    match done {
        Ok(Ok(converted)) => Ok(converted),
        // A cancel is not a failure, here or anywhere else on this path.
        Ok(Err(DecodeError::Cancelled)) => Err(StepError::Cancelled),
        Ok(Err(e)) => Err(StepError::Failed(e.user_message())),
        Err(e) => {
            tracing::error!(panicked = e.is_panic(), "the audio conversion thread failed");
            Err(StepError::Failed(MSG_CONVERSION_PANICKED.to_string()))
        }
    }
}

impl Steps for LiveSteps {
    fn probe(&self, path: &Path) -> Result<probe::Probe, String> {
        let size = std::fs::metadata(path)
            .map(|m| m.len())
            .map_err(|e| {
                tracing::warn!(kind = ?e.kind(), "could not stat the file to import");
                probe::ProbeError::Unreadable.user_message()
            })?;
        let probed = probe::probe(path).map_err(|e| e.user_message())?;
        probe::check_limits(&probed, size, &self.limits).map_err(|e| e.user_message())?;
        Ok(probed)
    }

    fn transcribe<'a>(
        &'a self,
        path: &'a Path,
        duration_s: Option<f64>,
        cancel: &'a CancelToken,
        on_state: &'a (dyn Fn(ItemState) + Send + Sync),
    ) -> BoxFuture<'a, Result<Transcript, StepError>> {
        Box::pin(async move {
            // The user's own file is never what goes on the wire — see the
            // module doc. The guard deletes the WAV on every exit from this
            // block: success, failure, and the future being dropped mid-flight
            // by a cancel.
            let scratch = ScratchWav::new();
            let converted = convert_for_upload(path, scratch.path(), cancel).await?;
            on_state(ItemState::Uploading);

            // The decoded length beats the container's, which may be absent
            // (a VBR MP3 with no Xing header) or wrong. `or`, not a
            // replacement: the probe's figure is what the note and the
            // coverage banner already use, and only the poll schedule is
            // decided here.
            let duration_s = duration_s.or(Some(converted.duration_s));

            let job = guard(cancel, self.client.create_job(&self.cfg)).await?;
            guard(cancel, self.client.upload(&job, scratch.path())).await?;
            guard(cancel, self.client.start(&job)).await?;
            // The bytes are up and Sarvam has the job; everything from here is
            // waiting on the model.
            on_state(ItemState::Transcribing);
            let schedule = batch_job::PollSchedule::for_audio(duration_s);
            let outputs = self
                .client
                .wait_for_completion(&job, &schedule, cancel.cancelled())
                .await
                .map_err(from_job_error)?;
            let (segments, text) = guard(cancel, self.client.download(&job, &outputs)).await?;
            Ok(Transcript { segments, text })
        })
    }

    fn save_note(&self, note: notes::NewNote) -> Result<i64, String> {
        // Through `commit_then_mirror`, like every other note writer: an
        // import that skipped it left the note with no `.md` until the user
        // happened to edit it later, so the mirror was quietly missing exactly
        // the notes the user never opened.
        let root = self.mirror_root.clone();
        self.notes
            .with_connection(move |conn| {
                notes::mirror::commit_then_mirror(
                    conn,
                    root.as_deref(),
                    |c| notes::create_note(c, &note).map_err(|e| e.to_string()),
                    |_, id| notes::mirror::Job::Write(vec![*id]),
                )
            })
            .unwrap_or_else(|| Err(NOTES_UNAVAILABLE.to_string()))
    }

    fn discard_note(&self, id: i64) {
        // Safe to delete by primary key and only by primary key: this row was
        // INSERTed by the save a moment ago, so the id cannot name anything the
        // user wrote. Counts and outcomes only — never the id's note.
        //
        // The file goes with the row. `save_note` mirrors now, so a rollback
        // that only deleted the row would leave a cancelled import's `.md`
        // behind for good — nothing else ever sweeps an id whose row is gone.
        let root = self.mirror_root.clone();
        let rolled_back = self.notes.with_connection(move |conn| {
            notes::mirror::commit_then_mirror(
                conn,
                root.as_deref(),
                |c| notes::delete_note(c, id),
                |_, _| notes::mirror::Job::Remove { ids: vec![id], dir: None },
            )
        });
        if !matches!(rolled_back, Some(Ok(true))) {
            tracing::warn!("a cancelled import's note could not be rolled back");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    // ------------------------------------------------------------- stubs --

    #[derive(Default)]
    struct Recorded {
        payloads: Mutex<Vec<ImportProgressPayload>>,
        /// Fires one `cancel` from *inside* the publish that first announces
        /// this state. `Weak`, because this sink lives inside the `Core` it
        /// points back at.
        ///
        /// A hook rather than a second task: the drain runs from a publish to
        /// its next await with no yield point, so an interruption planted here
        /// lands at a known instruction instead of wherever the scheduler
        /// happens to put it.
        cancel_at: Mutex<Option<(&'static str, std::sync::Weak<Core>)>>,
    }

    impl ProgressSink for Arc<Recorded> {
        fn emit(&self, payload: ImportProgressPayload) {
            // Taken before the payload is recorded, and only ever once: the
            // cancel below publishes too, and this must not recurse.
            let interrupt = {
                let mut slot = self.cancel_at.lock().expect("recorded sink");
                match slot.as_ref() {
                    Some((state, shared))
                        if payload.items.iter().any(|i| i.state == *state) =>
                    {
                        let shared = shared.upgrade();
                        *slot = None;
                        shared
                    }
                    _ => None,
                }
            };
            self.payloads.lock().expect("recorded sink").push(payload);
            if let Some(shared) = interrupt {
                ImportQueue { core: shared }.cancel();
            }
        }
    }

    impl Recorded {
        /// Cancel the queue the first time an item is published in `state`.
        fn cancel_when_an_item_reaches(&self, state: &'static str, queue: &ImportQueue) {
            *self.cancel_at.lock().expect("recorded sink") =
                Some((state, Arc::downgrade(&queue.core)));
        }

        /// Every state one item passed through, in order, de-duplicated —
        /// which is the sequence the UI actually renders.
        fn states_of(&self, item_id: u64) -> Vec<String> {
            let mut seen: Vec<String> = Vec::new();
            for payload in self.payloads.lock().expect("recorded sink").iter() {
                if let Some(item) = payload.items.iter().find(|i| i.id == item_id) {
                    if seen.last().map(String::as_str) != Some(item.state) {
                        seen.push(item.state.to_string());
                    }
                }
            }
            seen
        }

        fn last(&self) -> ImportProgressPayload {
            self.payloads
                .lock()
                .expect("recorded sink")
                .last()
                .cloned()
                .expect("at least one payload must have been emitted")
        }
    }

    /// What one stubbed file should do.
    #[derive(Clone)]
    enum Behaviour {
        Ok(&'static str),
        ProbeFails,
        /// Fails the conversion, i.e. before `Uploading` is ever announced —
        /// the shape an Opus `.webm` or a damaged file takes. Nothing may
        /// reach the network, and no note may be written.
        ConversionFails,
        TranscribeFails,
        /// Completes normally and returns nothing at all — the shape Sarvam
        /// answers with for some real `.mp4` files (`segments=0
        /// text_chars=0`). Not an error: the provider says it succeeded.
        TranscribesNothing,
        /// Park until released, so a test can cancel mid-flight.
        Blocks(Arc<tokio::sync::Notify>),
        /// Park until released and then **succeed**, ignoring the token
        /// entirely — a provider that finished just as the cancel landed.
        ///
        /// [`Behaviour::Blocks`] cannot produce that case: its own select is
        /// biased on the token, so a cancelled run always gets `Cancelled`
        /// back and the late-result path is never taken. This is the only way
        /// to hand the drain a real success after its run was superseded,
        /// which is what the run-number check exists for.
        BlocksIgnoringCancel(Arc<tokio::sync::Notify>),
    }

    struct StubSteps {
        behaviour: std::collections::HashMap<String, Behaviour>,
        duration_s: Option<f64>,
        /// Every filename `transcribe` was called with, in order — how
        /// seriality is proven.
        transcribed: Mutex<Vec<String>>,
        /// How many transcriptions are in flight right now. Never more than
        /// one, ever.
        in_flight: Arc<AtomicUsize>,
        peak_in_flight: Arc<AtomicUsize>,
        saved: Mutex<Vec<notes::NewNote>>,
        next_note_id: AtomicUsize,
        /// Note ids the run rolled back after a cancel during the save.
        discarded: Mutex<Vec<i64>>,
        /// Cancels the queue from *inside* `save_note`, i.e. during the round
        /// trip the check before the save cannot cover.
        cancel_on_save: Mutex<Option<std::sync::Weak<Core>>>,
    }

    impl StubSteps {
        fn new(files: &[(&str, Behaviour)]) -> Arc<StubSteps> {
            StubSteps::with_duration(files, Some(100.0))
        }

        fn with_duration(files: &[(&str, Behaviour)], duration_s: Option<f64>) -> Arc<StubSteps> {
            Arc::new(StubSteps {
                behaviour: files
                    .iter()
                    .map(|(name, b)| ((*name).to_string(), b.clone()))
                    .collect(),
                duration_s,
                transcribed: Mutex::new(Vec::new()),
                in_flight: Arc::new(AtomicUsize::new(0)),
                peak_in_flight: Arc::new(AtomicUsize::new(0)),
                saved: Mutex::new(Vec::new()),
                next_note_id: AtomicUsize::new(1),
                discarded: Mutex::new(Vec::new()),
                cancel_on_save: Mutex::new(None),
            })
        }

        fn behaviour_for(&self, path: &Path) -> Behaviour {
            let name = path
                .file_name()
                .expect("stub paths always have a file name")
                .to_string_lossy()
                .into_owned();
            self.behaviour
                .get(&name)
                .cloned()
                .unwrap_or(Behaviour::Ok("stub transcript"))
        }
    }

    impl Steps for StubSteps {
        fn probe(&self, path: &Path) -> Result<probe::Probe, String> {
            match self.behaviour_for(path) {
                Behaviour::ProbeFails => Err("that file isn't audio".to_string()),
                _ => Ok(probe::Probe {
                    duration_s: self.duration_s,
                    container: "wave",
                    sample_rate: Some(16_000),
                }),
            }
        }

        fn transcribe<'a>(
            &'a self,
            path: &'a Path,
            _duration_s: Option<f64>,
            cancel: &'a CancelToken,
            on_state: &'a (dyn Fn(ItemState) + Send + Sync),
        ) -> BoxFuture<'a, Result<Transcript, StepError>> {
            let name = path.file_name().expect("named").to_string_lossy().into_owned();
            self.transcribed.lock().expect("stub log").push(name);
            let live = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
            self.peak_in_flight.fetch_max(live, Ordering::SeqCst);
            let behaviour = self.behaviour_for(path);
            let in_flight = Arc::clone(&self.in_flight);
            Box::pin(async move {
                // The drain leaves the item in `Converting`; a stub that
                // converts nothing still has to walk the same states the live
                // implementation does, or the sequences these tests assert
                // would be measuring the stub rather than the drain.
                if matches!(behaviour, Behaviour::ConversionFails) {
                    in_flight.fetch_sub(1, Ordering::SeqCst);
                    return Err(StepError::Failed(
                        crate::media::decode::DecodeError::UnsupportedCodec { codec: "Opus" }
                            .user_message(),
                    ));
                }
                on_state(ItemState::Uploading);
                on_state(ItemState::Transcribing);
                // A real await point before the work, so `Behaviour::Ok` is
                // not a future that completes on its first poll. Without it
                // the drain never yields between items, and the peak-in-flight
                // assertion in `the_drain_is_serial_and_keeps_the_queued_order`
                // could not have observed an overlap even if the drain had
                // been written to run everything at once.
                tokio::task::yield_now().await;
                let result = match behaviour {
                    Behaviour::ConversionFails => unreachable!("handled above"),
                    Behaviour::TranscribeFails => {
                        Err(StepError::Failed("Sarvam couldn't transcribe it".to_string()))
                    }
                    Behaviour::Blocks(gate) => {
                        tokio::select! {
                            biased;
                            () = cancel.cancelled() => Err(StepError::Cancelled),
                            () = gate.notified() => Ok(Transcript {
                                segments: vec![Segment { start_s: 0.0, end_s: 90.0, text: "released".into() }],
                                text: "released".to_string(),
                            }),
                        }
                    }
                    // Deliberately never looks at `cancel`.
                    Behaviour::BlocksIgnoringCancel(gate) => {
                        gate.notified().await;
                        Ok(Transcript {
                            segments: vec![Segment {
                                start_s: 0.0,
                                end_s: 90.0,
                                text: "late".into(),
                            }],
                            text: "late".to_string(),
                        })
                    }
                    Behaviour::TranscribesNothing => Ok(Transcript {
                        segments: Vec::new(),
                        text: String::new(),
                    }),
                    Behaviour::Ok(text) => Ok(Transcript {
                        segments: vec![Segment {
                            start_s: 0.0,
                            end_s: 90.0,
                            text: text.to_string(),
                        }],
                        text: text.to_string(),
                    }),
                    Behaviour::ProbeFails => unreachable!("the probe already refused this file"),
                };
                in_flight.fetch_sub(1, Ordering::SeqCst);
                result
            })
        }

        fn save_note(&self, note: notes::NewNote) -> Result<i64, String> {
            // Fired before the id is minted, so the cancel really does land
            // inside the round trip the drain is awaiting.
            let interrupt = self.cancel_on_save.lock().expect("stub saves").take();
            if let Some(shared) = interrupt.and_then(|w| w.upgrade()) {
                ImportQueue { core: shared }.cancel();
            }
            let id = self.next_note_id.fetch_add(1, Ordering::SeqCst) as i64;
            self.saved.lock().expect("stub saves").push(note);
            Ok(id)
        }

        fn discard_note(&self, id: i64) {
            self.discarded.lock().expect("stub saves").push(id);
        }
    }

    fn queue_with(sink: Arc<Recorded>) -> ImportQueue {
        ImportQueue::new(Box::new(sink))
    }

    fn paths(names: &[&str]) -> Vec<PathBuf> {
        names
            .iter()
            .map(|n| PathBuf::from(format!("C:/nowhere/{n}")))
            .collect()
    }

    /// Run to completion on the current runtime rather than through
    /// `start`, which would spawn onto Tauri's global runtime and leave the
    /// test with nothing to await.
    async fn run_now(queue: &ImportQueue, steps: Arc<dyn Steps>) {
        let run = queue.claim_run().expect("the queue must be startable");
        run_queue(Arc::clone(&queue.core), steps, run).await;
    }

    // ------------------------------------------------------------- queue --

    #[tokio::test]
    async fn a_file_walks_the_whole_state_machine_and_becomes_a_note() {
        let sink = Arc::new(Recorded::default());
        let queue = queue_with(Arc::clone(&sink));
        let steps = StubSteps::new(&[("talk.wav", Behaviour::Ok("hello there"))]);
        assert_eq!(queue.enqueue(paths(&["talk.wav"])), 1);

        run_now(&queue, Arc::clone(&steps) as Arc<dyn Steps>).await;

        assert_eq!(
            sink.states_of(0),
            ["queued", "probing", "converting", "uploading", "transcribing", "done"]
        );
        let saved = steps.saved.lock().expect("stub saves");
        assert_eq!(saved.len(), 1);
        assert_eq!(saved[0].kind.as_deref(), Some("imported"));
        assert_eq!(saved[0].title.as_deref(), Some("talk"));
        assert_eq!(saved[0].content.as_deref(), Some("hello there"));
        assert_eq!(saved[0].imported_file.as_deref(), Some("talk.wav"));
        assert_eq!(saved[0].audio_seconds, Some(100.0));
    }

    /// Concurrency exactly one, in order: one file on the wire at a time,
    /// never all of them at once.
    #[tokio::test]
    async fn the_drain_is_serial_and_keeps_the_queued_order() {
        let sink = Arc::new(Recorded::default());
        let queue = queue_with(Arc::clone(&sink));
        let steps = StubSteps::new(&[]);
        queue.enqueue(paths(&["a.wav", "b.mp3", "c.flac"]));

        run_now(&queue, Arc::clone(&steps) as Arc<dyn Steps>).await;

        assert_eq!(
            *steps.transcribed.lock().expect("stub log"),
            ["a.wav", "b.mp3", "c.flac"]
        );
        assert_eq!(
            steps.peak_in_flight.load(Ordering::SeqCst),
            1,
            "two transcriptions must never be on the wire at once"
        );
        assert_eq!(sink.last().done, 3);
        assert_eq!(sink.last().percent, 100);
    }

    /// One file per job, always — and never the 20 Sarvam would accept.
    ///
    /// The constant exists to be *not* used: a job is the unit that fails, so
    /// batching four imports into one means one unreadable file taking three
    /// good ones down with it, which is the opposite of what a per-item queue
    /// is for. Three files therefore produce three separate transcribe calls,
    /// each with exactly one path.
    #[tokio::test]
    async fn every_file_gets_its_own_job_rather_than_sarvams_batch_of_twenty() {
        assert!(
            batch_job::MAX_FILES_PER_JOB > 1,
            "if Sarvam ever caps a job at one file this test is measuring nothing"
        );
        let sink = Arc::new(Recorded::default());
        let queue = queue_with(Arc::clone(&sink));
        let steps = StubSteps::new(&[]);
        queue.enqueue(paths(&["a.wav", "b.wav", "c.wav"]));

        run_now(&queue, Arc::clone(&steps) as Arc<dyn Steps>).await;

        let calls = steps.transcribed.lock().expect("stub log");
        assert_eq!(
            calls.len(),
            3,
            "three files must mean three jobs, not one job of three"
        );
        assert_eq!(sink.last().done, 3);
    }

    /// One bad file must not take the rest of the queue down — the whole
    /// reason a job carries one file (see the module doc).
    #[tokio::test]
    async fn one_failure_does_not_stop_the_queue() {
        let sink = Arc::new(Recorded::default());
        let queue = queue_with(Arc::clone(&sink));
        let steps = StubSteps::new(&[
            ("bad.wav", Behaviour::ProbeFails),
            ("worse.wav", Behaviour::TranscribeFails),
        ]);
        queue.enqueue(paths(&["bad.wav", "good.wav", "worse.wav"]));

        run_now(&queue, Arc::clone(&steps) as Arc<dyn Steps>).await;

        let last = sink.last();
        assert_eq!(last.done, 1);
        assert_eq!(last.failed, 2);
        assert_eq!(last.percent, 100, "failed items settle, so the bar completes");
        assert_eq!(last.items[0].state, "failed");
        assert_eq!(last.items[0].error.as_deref(), Some("that file isn't audio"));
        assert_eq!(last.items[1].state, "done");
        assert_eq!(last.items[2].state, "failed");
        assert!(!last.running);
    }

    /// A probe failure must never reach the network. Sarvam invents a
    /// transcript for a non-audio upload and bills for it (measured live), so
    /// "probe first, always" is the whole safety property.
    #[tokio::test]
    async fn a_file_the_probe_refuses_is_never_uploaded() {
        let sink = Arc::new(Recorded::default());
        let queue = queue_with(Arc::clone(&sink));
        let steps = StubSteps::new(&[("notes.wav", Behaviour::ProbeFails)]);
        queue.enqueue(paths(&["notes.wav"]));

        run_now(&queue, Arc::clone(&steps) as Arc<dyn Steps>).await;

        assert!(
            steps.transcribed.lock().expect("stub log").is_empty(),
            "nothing may be uploaded after the probe refuses it"
        );
        assert_eq!(sink.states_of(0), ["queued", "probing", "failed"]);
    }

    /// Sarvam can complete a job for a real `.mp4` and return no text. That
    /// is a failure, not an IMPORTED row and a titled, empty note.
    #[tokio::test]
    async fn a_transcription_that_returns_nothing_fails_and_writes_no_note() {
        let sink = Arc::new(Recorded::default());
        let queue = queue_with(Arc::clone(&sink));
        let steps = StubSteps::new(&[("talk.mp4", Behaviour::TranscribesNothing)]);
        queue.enqueue(paths(&["talk.mp4"]));

        run_now(&queue, Arc::clone(&steps) as Arc<dyn Steps>).await;

        assert!(
            steps.saved.lock().expect("stub saves").is_empty(),
            "an import with no transcript must not write a note"
        );
        let last = sink.last();
        assert_eq!(last.failed, 1);
        assert_eq!(last.done, 0);
        assert_eq!(last.items[0].state, "failed");
        assert_eq!(last.items[0].error.as_deref(), Some(MSG_NOTHING_TRANSCRIBED));
    }

    /// The rule is about readable text, not about the shape it arrives in: a
    /// provider that returns segments carrying only whitespace has still
    /// transcribed nothing.
    #[test]
    fn only_readable_text_keeps_an_import() {
        let blank = Transcript { segments: Vec::new(), text: String::new() };
        assert!(transcript_is_empty(&blank));

        let whitespace = Transcript {
            segments: vec![Segment { start_s: 0.0, end_s: 1.0, text: "  ".into() }],
            text: "\n".to_string(),
        };
        assert!(transcript_is_empty(&whitespace));

        let real = Transcript {
            segments: vec![Segment { start_s: 0.0, end_s: 1.0, text: "hello".into() }],
            text: "hello".to_string(),
        };
        assert!(!transcript_is_empty(&real));

        // One word is a transcript. `incomplete_banner` warns about short
        // ones; this predicate must not refuse them.
        let tiny = Transcript { segments: Vec::new(), text: "ok".to_string() };
        assert!(!transcript_is_empty(&tiny));
    }

    /// `probe` answers `None` for a container that states no
    /// playing time (a header-less VBR MP3, a WebM a recorder never patched),
    /// and `check_limits` lets it through. The import still runs — but the row
    /// says which file the app could not measure, because the length ceiling did
    /// not apply to it and its job waits on the 30-minute ceiling rather than
    /// a deadline scaled to its length.
    #[tokio::test]
    async fn a_file_whose_length_could_not_be_read_says_so_without_failing() {
        let sink = Arc::new(Recorded::default());
        let queue = queue_with(Arc::clone(&sink));
        let steps = StubSteps::with_duration(&[("live.webm", Behaviour::Ok("recorded"))], None);
        queue.enqueue(paths(&["live.webm"]));

        run_now(&queue, Arc::clone(&steps) as Arc<dyn Steps>).await;

        let last = sink.last();
        assert_eq!(
            last.items[0].state, "done",
            "an unknown length is not a failure"
        );
        assert_eq!(last.items[0].detail, Some(LENGTH_UNKNOWN));
        assert!(last.items[0].error.is_none());
        // With no duration to compare against, nothing may be claimed about
        // the transcript's coverage either.
        let saved = steps.saved.lock().expect("stub saves");
        assert_eq!(saved[0].content.as_deref(), Some("recorded"));
        assert_eq!(saved[0].audio_seconds, None);
    }

    /// A measurable file must NOT carry the remark — otherwise it says nothing.
    #[tokio::test]
    async fn a_file_with_a_known_length_carries_no_remark() {
        let sink = Arc::new(Recorded::default());
        let queue = queue_with(Arc::clone(&sink));
        let steps = StubSteps::new(&[("talk.wav", Behaviour::Ok("hello"))]);
        queue.enqueue(paths(&["talk.wav"]));

        run_now(&queue, Arc::clone(&steps) as Arc<dyn Steps>).await;

        assert_eq!(sink.last().items[0].detail, None);
    }

    /// A file outside the accepted list is added as a failed row that names
    /// its extension, so the user sees why it was not imported instead of
    /// wondering where it went.
    #[test]
    fn an_unsupported_extension_is_refused_by_name_not_silently_dropped() {
        let sink = Arc::new(Recorded::default());
        let queue = queue_with(Arc::clone(&sink));
        assert_eq!(queue.enqueue(paths(&["clip.mov", "talk.wav"])), 1);

        let last = sink.last();
        assert_eq!(last.total, 2);
        assert_eq!(last.failed, 1);
        assert_eq!(last.items[0].state, "failed");
        assert!(
            last.items[0]
                .error
                .as_deref()
                .expect("a refusal must say why")
                .contains(".mov"),
            "the message has to name the extension it refused"
        );
        assert_eq!(last.items[1].state, "queued");
    }

    #[test]
    fn adding_files_never_starts_the_run_by_itself() {
        let sink = Arc::new(Recorded::default());
        let queue = queue_with(Arc::clone(&sink));
        queue.enqueue(paths(&["talk.wav"]));
        assert!(!sink.last().running);
    }

    #[test]
    fn starting_an_empty_queue_is_refused_with_a_sentence() {
        let queue = queue_with(Arc::new(Recorded::default()));
        assert_eq!(queue.claim_run(), Err(NOTHING_QUEUED.to_string()));
    }

    #[test]
    fn a_second_start_while_running_is_refused() {
        let queue = queue_with(Arc::new(Recorded::default()));
        queue.enqueue(paths(&["a.wav", "b.wav"]));
        queue.claim_run().expect("the first start must be accepted");
        assert_eq!(queue.claim_run(), Err(ALREADY_RUNNING.to_string()));
    }

    // ------------------------------------------------------------ cancel --

    #[tokio::test]
    async fn cancelling_mid_flight_aborts_the_network_work_and_settles_the_queue() {
        let sink = Arc::new(Recorded::default());
        let queue = queue_with(Arc::clone(&sink));
        let gate = Arc::new(tokio::sync::Notify::new());
        let steps = StubSteps::new(&[("first.wav", Behaviour::Blocks(Arc::clone(&gate)))]);
        queue.enqueue(paths(&["first.wav", "second.wav"]));

        let run = queue.claim_run().expect("startable");
        let shared = Arc::clone(&queue.core);
        let drain_task = tokio::spawn(run_queue(
            shared,
            Arc::clone(&steps) as Arc<dyn Steps>,
            run,
        ));

        // Let the drain reach the parked transcription.
        while steps.transcribed.lock().expect("stub log").is_empty() {
            tokio::task::yield_now().await;
        }

        assert_eq!(queue.cancel(), 1, "the in-flight operation must be aborted");
        drain_task.await.expect("the drain must not panic");

        let last = sink.last();
        assert!(!last.running);
        assert_eq!(last.items[0].state, "cancelled");
        assert_eq!(
            last.items[1].state, "cancelled",
            "an item that never started still settles, so the UI unlocks at once"
        );
        assert_eq!(
            *steps.transcribed.lock().expect("stub log"),
            ["first.wav"],
            "the second file must never be started"
        );
    }

    /// Every write names the run it belongs to, and `mark` drops one from a
    /// run that has since been cancelled: a transcript that finishes after
    /// the user pressed Cancel leaves its row as the cancel left it.
    #[tokio::test]
    async fn a_result_arriving_after_a_cancel_writes_nothing() {
        let sink = Arc::new(Recorded::default());
        let queue = queue_with(Arc::clone(&sink));
        let gate = Arc::new(tokio::sync::Notify::new());
        // Ignores the token, so it hands back a genuine success *after* the
        // run was cancelled. `Behaviour::Blocks` would answer `Cancelled` and
        // never exercise the guard this test is named for.
        let steps = StubSteps::new(&[(
            "slow.wav",
            Behaviour::BlocksIgnoringCancel(Arc::clone(&gate)),
        )]);
        queue.enqueue(paths(&["slow.wav"]));

        let run = queue.claim_run().expect("startable");
        let drain_task = tokio::spawn(run_queue(
            Arc::clone(&queue.core),
            Arc::clone(&steps) as Arc<dyn Steps>,
            run,
        ));
        while steps.transcribed.lock().expect("stub log").is_empty() {
            tokio::task::yield_now().await;
        }

        // Cancel, then let the stub succeed anyway — the late-result race.
        queue.cancel();
        gate.notify_one();
        drain_task.await.expect("the drain must not panic");

        assert_eq!(
            sink.last().items[0].state,
            "cancelled",
            "a successful transcript arriving after the cancel must not \
             flip the item to done"
        );
        assert!(
            steps.saved.lock().expect("stub saves").is_empty(),
            "a cancelled run must not save a note from a transcript that came back afterwards"
        );
    }

    /// `clear` has to fire the registry token, not just move the run on.
    ///
    /// Bumping the run id stops the loop and hides whatever comes back, but leaves
    /// the request on the wire — an in-flight PUT of a half-gigabyte file goes
    /// on uploading a recording whose queue row no longer exists, while
    /// `running = false` lets a fresh run start beside it.
    #[tokio::test]
    async fn clearing_the_queue_aborts_the_work_that_is_still_on_the_wire() {
        let sink = Arc::new(Recorded::default());
        let queue = queue_with(Arc::clone(&sink));
        let gate = Arc::new(tokio::sync::Notify::new());
        let steps = StubSteps::new(&[("big.wav", Behaviour::Blocks(Arc::clone(&gate)))]);
        queue.enqueue(paths(&["big.wav"]));

        let run = queue.claim_run().expect("startable");
        let drain_task = tokio::spawn(run_queue(
            Arc::clone(&queue.core),
            Arc::clone(&steps) as Arc<dyn Steps>,
            run,
        ));
        while steps.transcribed.lock().expect("stub log").is_empty() {
            tokio::task::yield_now().await;
        }

        // No `notify_one`: if `clear` did not fire the token, nothing else
        // ever releases the stub and this await hangs rather than passing.
        queue.clear();
        tokio::time::timeout(std::time::Duration::from_secs(5), drain_task)
            .await
            .expect("clear must abort the in-flight leg, not leave it running")
            .expect("the drain must not panic");

        assert_eq!(sink.last().total, 0);
        assert!(!sink.last().running);
    }

    /// The gap between recording the request id of a row and registering it,
    /// taken one step at a time. A Cancel or a Clear landing in it finds
    /// nothing to fire, so the run itself has to notice before anything goes
    /// out: `register_if_current` must refuse, and its registration must not
    /// linger where a later cancel would have to find it.
    #[test]
    fn a_cancel_or_clear_between_recording_and_registering_the_request_keeps_it_off_the_network() {
        for clear in [false, true] {
            let queue = queue_with(Arc::new(Recorded::default()));
            queue.enqueue(paths(&["talk.wav"]));
            let run = queue.claim_run().expect("startable");
            let request_id = queue
                .core
                .open_request(run, 0, None)
                .expect("the run still owns the queue");

            if clear {
                queue.clear();
            } else {
                assert_eq!(queue.cancel(), 0, "nothing is registered yet, so nothing fires");
            }

            assert!(
                queue.core.register_if_current(run, &request_id).is_none(),
                "a superseded run must not get a token to transcribe with (clear: {clear})"
            );
            assert_eq!(
                queue.core.requests.cancel(&request_id),
                0,
                "the refused registration must be gone (clear: {clear})"
            );
            assert!(queue.core.ledger().on_wire.is_none());
            assert!(!queue.snapshot().running);
        }
    }

    /// The same window, end to end. The stub parks on its cancel token and
    /// nothing else ever releases it, so a drain that carried on past the
    /// cancel hangs here rather than failing an assertion — which is the honest
    /// shape of the defect: a job on the wire that nothing can stop.
    #[tokio::test]
    async fn a_cancel_landing_before_the_token_is_registered_stops_the_drain() {
        let sink = Arc::new(Recorded::default());
        let queue = queue_with(Arc::clone(&sink));
        // Fires while the item is being marked `converting`: after its
        // request id is recorded and before anything is registered.
        sink.cancel_when_an_item_reaches("converting", &queue);
        let gate = Arc::new(tokio::sync::Notify::new());
        let steps = StubSteps::new(&[("talk.wav", Behaviour::Blocks(Arc::clone(&gate)))]);
        queue.enqueue(paths(&["talk.wav"]));

        let run = queue.claim_run().expect("startable");
        let drain_task = tokio::spawn(run_queue(
            Arc::clone(&queue.core),
            Arc::clone(&steps) as Arc<dyn Steps>,
            run,
        ));
        tokio::time::timeout(Duration::from_secs(5), drain_task)
            .await
            .expect("a cancel landing before the registration must still stop the run")
            .expect("the drain must not panic");

        assert!(
            steps.transcribed.lock().expect("stub log").is_empty(),
            "nothing may go on the wire once the cancel has landed"
        );
        assert_eq!(
            sink.states_of(0),
            ["queued", "probing", "converting", "cancelled"]
        );
        assert!(!sink.last().running);
    }

    /// A conversion that cannot happen must end the item there: no upload, no
    /// note, and the decoder's own sentence rather than a generic one.
    ///
    /// This is the Opus path, which is permanent — symphonia has no Opus
    /// decoder and there is no pure-Rust one to adopt, so `.opus` files and
    /// browser-recorded `.webm` will always land here. The failure has to be
    /// worth reading, because it is the only thing the user gets.
    #[tokio::test]
    async fn a_file_that_cannot_be_converted_is_never_uploaded_and_writes_no_note() {
        let sink = Arc::new(Recorded::default());
        let queue = queue_with(Arc::clone(&sink));
        let steps = StubSteps::new(&[("voice.opus", Behaviour::ConversionFails)]);
        queue.enqueue(paths(&["voice.opus"]));

        run_now(&queue, Arc::clone(&steps) as Arc<dyn Steps>).await;

        assert_eq!(
            sink.states_of(0),
            ["queued", "probing", "converting", "failed"],
            "a failed conversion must never reach `uploading`"
        );
        assert!(
            steps.saved.lock().expect("stub saves").is_empty(),
            "a file that could not be converted must not become a note"
        );
        let last = sink.last();
        assert_eq!(last.failed, 1);
        assert_eq!(last.percent, 100);
        let message = last.items[0]
            .error
            .clone()
            .expect("a failed item must carry a sentence");
        assert!(message.contains("Opus"), "{message}");
        assert!(message.contains("WAV"), "{message}");
    }

    /// A cancel landing *during* the note INSERT. The row is already
    /// committed by the time the run can see the cancel, and the run-number
    /// check then refuses to record it — so the note is rolled
    /// back, rather than the queue saying "Cancelled" while an upload note sits
    /// in Notes that nothing in the UI points at.
    #[tokio::test]
    async fn a_note_committed_while_the_cancel_was_landing_is_rolled_back() {
        let sink = Arc::new(Recorded::default());
        let queue = queue_with(Arc::clone(&sink));
        let steps = StubSteps::new(&[("talk.wav", Behaviour::Ok("hello"))]);
        *steps.cancel_on_save.lock().expect("stub saves") = Some(Arc::downgrade(&queue.core));
        queue.enqueue(paths(&["talk.wav"]));

        run_now(&queue, Arc::clone(&steps) as Arc<dyn Steps>).await;

        assert_eq!(
            steps.saved.lock().expect("stub saves").len(),
            1,
            "the INSERT is already committed when the cancel becomes visible"
        );
        assert_eq!(
            *steps.discarded.lock().expect("stub saves"),
            [1],
            "the note a cancelled run wrote must be taken back out"
        );
        let last = sink.last();
        assert_eq!(last.items[0].state, "cancelled");
        assert!(last.items[0].note_id.is_none());
    }

    /// `cancel` is fired by a button the user can press at any time, including
    /// when nothing is running.
    #[test]
    fn cancelling_an_idle_queue_is_a_no_op() {
        let sink = Arc::new(Recorded::default());
        let queue = queue_with(Arc::clone(&sink));
        queue.enqueue(paths(&["a.wav"]));
        assert_eq!(queue.cancel(), 0);
        assert_eq!(sink.last().items[0].state, "cancelled");
    }

    #[tokio::test]
    async fn a_superseded_drain_cannot_unlock_a_newer_run() {
        let sink = Arc::new(Recorded::default());
        let queue = queue_with(Arc::clone(&sink));
        let gate = Arc::new(tokio::sync::Notify::new());
        let steps = StubSteps::new(&[("slow.wav", Behaviour::Blocks(Arc::clone(&gate)))]);
        queue.enqueue(paths(&["slow.wav"]));

        let stale_run = queue.claim_run().expect("startable");
        let stale = tokio::spawn(run_queue(
            Arc::clone(&queue.core),
            Arc::clone(&steps) as Arc<dyn Steps>,
            stale_run,
        ));
        while steps.transcribed.lock().expect("stub log").is_empty() {
            tokio::task::yield_now().await;
        }

        // A cancel plus a fresh enqueue and start: the new run owns the queue.
        queue.cancel();
        queue.enqueue(paths(&["new.wav"]));
        let fresh_run = queue.claim_run().expect("startable again");
        gate.notify_one();
        stale.await.expect("the stale drain must not panic");

        assert!(
            sink.last().running,
            "the stale drain's exit must not clear the new run's running flag"
        );
        assert_ne!(stale_run, fresh_run);
    }

    #[test]
    fn clearing_the_queue_stops_the_run_it_emptied() {
        let sink = Arc::new(Recorded::default());
        let queue = queue_with(Arc::clone(&sink));
        queue.enqueue(paths(&["a.wav"]));
        let run = queue.claim_run().expect("startable");
        queue.clear();
        assert!(queue.core.is_superseded(run));
        assert_eq!(sink.last().total, 0);
        assert!(!sink.last().running);
    }

    // -------------------------------------------------------- note shape --

    fn transcript(text: &str, spans: &[(f64, f64)]) -> Transcript {
        Transcript {
            segments: spans
                .iter()
                .map(|(a, b)| Segment {
                    start_s: *a,
                    end_s: *b,
                    text: text.to_string(),
                })
                .collect(),
            text: text.to_string(),
        }
    }

    #[test]
    fn the_title_is_the_file_name_without_its_extension() {
        assert_eq!(title_from_file_name("Team sync 2026-09-02.m4a"), "Team sync 2026-09-02");
        assert_eq!(title_from_file_name("notes.tar.gz"), "notes.tar");
        assert_eq!(title_from_file_name("no-extension"), "no-extension");
        assert_eq!(title_from_file_name(".wav"), ".wav");
        assert_eq!(title_from_file_name(""), "Imported recording");
    }

    /// The invariant that makes the deliberately tolerant
    /// `notes::transcript_text` and this module's stored shape one decision
    /// rather than two guesses.
    #[test]
    fn the_stored_shape_is_the_one_the_notes_index_reads() {
        let note = build_note("talk.wav", Some(100.0), &transcript("hello", &[(0.0, 90.0)]));
        let json = note.transcript_json.as_deref().expect("segments serialize");
        assert_eq!(json, r#"[{"text":"hello","start":0.0,"end":90.0}]"#);
        assert_eq!(
            notes::transcript_text(Some(json)).as_deref(),
            Some("hello"),
            "the notes FTS index must be able to read what import writes"
        );
    }

    #[test]
    fn a_transcript_with_no_timestamps_stores_no_segment_json() {
        let note = build_note(
            "talk.wav",
            Some(100.0),
            &Transcript {
                segments: Vec::new(),
                text: "text only".to_string(),
            },
        );
        assert!(note.transcript_json.is_none());
        assert_eq!(note.content.as_deref(), Some("text only"));
    }

    // ----------------------------------------------------------- banner --

    #[test]
    fn a_transcript_stopping_just_short_of_the_ratio_says_so() {
        let duration = 600.0;
        let end = duration * MIN_COVERAGE_RATIO - 1.0;
        let banner = incomplete_banner(Some(duration), &transcript("x", &[(0.0, end)]).segments)
            .expect("coverage below the ratio must be flagged");
        assert!(banner.starts_with(BANNER_OPENER), "{banner}");
        assert!(banner.contains("incomplete"), "{banner}");
        assert!(
            banner.contains(&format!("it ends at {} of a 10:00 recording", clock(end))),
            "{banner}"
        );
        assert!(banner.ends_with(".]"), "{banner}");
    }

    /// Exactly on the ratio counts as complete; the smallest step under it
    /// does not.
    #[test]
    fn coverage_exactly_at_the_ratio_gets_no_banner() {
        let duration = 600.0;
        let at = duration * MIN_COVERAGE_RATIO;
        assert!(incomplete_banner(Some(duration), &transcript("x", &[(0.0, at)]).segments).is_none());
        let under = at - 1e-6;
        assert!(incomplete_banner(Some(duration), &transcript("x", &[(0.0, under)]).segments).is_some());
    }

    #[test]
    fn full_coverage_gets_no_banner() {
        assert!(incomplete_banner(Some(100.0), &transcript("x", &[(0.0, 98.0)]).segments).is_none());
    }

    /// The two "nothing to compare" cases. Claiming incompleteness on the
    /// strength of a number that was never measured would be a guess dressed
    /// as a finding.
    #[test]
    fn nothing_is_claimed_when_there_is_nothing_to_compare() {
        assert!(
            incomplete_banner(None, &transcript("x", &[(0.0, 1.0)]).segments).is_none(),
            "a container that states no duration proves nothing"
        );
        assert!(
            incomplete_banner(Some(600.0), &[]).is_none(),
            "with_timestamps off leaves no last timestamp to judge"
        );
        assert!(incomplete_banner(Some(0.0), &transcript("x", &[(0.0, 0.0)]).segments).is_none());
    }

    #[test]
    fn the_banner_is_the_first_line_of_the_note_and_the_transcript_survives_it() {
        let note = build_note("talk.m4a", Some(600.0), &transcript("only the start", &[(0.0, 60.0)]));
        let content = note.content.expect("content");
        assert!(content.starts_with("[Transcript may be incomplete"));
        assert!(content.ends_with("only the start"));
    }

    /// The banner is a note about the recording, and it is the first thing a
    /// title model reads — so the copy handed to one has it taken off, while
    /// the stored note (the assertion above) keeps it.
    #[test]
    fn the_banner_comes_off_the_copy_a_title_model_sees() {
        let note = build_note("talk.m4a", Some(600.0), &transcript("only the start", &[(0.0, 60.0)]));
        let content = note.content.expect("content");
        assert_eq!(without_incomplete_banner(&content), "only the start");

        // A banner-only note (an empty transcript) strips to nothing, which is
        // what `title::generate_title` already refuses with a sentence.
        let empty = build_note("talk.m4a", Some(600.0), &Transcript {
            segments: transcript("", &[(0.0, 60.0)]).segments,
            text: String::new(),
        });
        assert_eq!(without_incomplete_banner(&empty.content.expect("content")), "");
    }

    /// Anything that is not one of this module's banners is returned
    /// untouched — including a note that merely opens with the same words and
    /// never closes the bracket.
    #[test]
    fn text_without_a_banner_is_returned_as_it_is() {
        for raw in [
            "just a transcript",
            "",
            "   leading space and no banner",
            "[Transcript may be incomplete and this line never closes\nmore text",
            "some prose\n[Transcript may be incomplete — it ends at 1:00 of a 10:00 recording.]",
        ] {
            assert_eq!(without_incomplete_banner(raw), raw, "for {raw:?}");
        }
    }

    #[test]
    fn the_clock_reads_like_a_player_and_drops_fractions() {
        assert_eq!(clock(0.0), "0:00");
        assert_eq!(clock(0.99), "0:00", "a second is not reached until it has played");
        assert_eq!(clock(59.9), "0:59");
        assert_eq!(clock(125.0), "2:05");
        assert_eq!(clock(3_599.99), "59:59");
        assert_eq!(clock(3_600.0), "1:00:00");
        assert_eq!(clock(3_725.4), "1:02:05");
        assert_eq!(clock(36_000.0), "10:00:00");
        assert_eq!(clock(-3.0), "0:00");
        assert_eq!(clock(f64::NAN), "0:00");
    }

    // ----------------------------------------------------------- mirror --

    /// A unique directory under the OS temp dir, removed on drop — the same
    /// construction `notes::mirror`'s own tests use, because the crate has no
    /// `tempfile` dependency.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(name: &str) -> Self {
            let path = unique_temp(name, "");
            std::fs::create_dir_all(&path).expect("create temp mirror dir");
            TempDir(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn unique_temp(name: &str, suffix: &str) -> PathBuf {
        let mut path = std::env::temp_dir();
        path.push(format!(
            "bs-import-{name}-{}-{}{suffix}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock is after the epoch")
                .as_nanos()
        ));
        path
    }

    /// Every `.md` under the mirror root, as `folder/file` strings, sorted.
    fn mirrored(root: &Path) -> Vec<String> {
        let mut found = Vec::new();
        for dir in std::fs::read_dir(root).expect("mirror root").flatten() {
            if !dir.path().is_dir() {
                continue;
            }
            let folder = dir.file_name().to_string_lossy().into_owned();
            for file in std::fs::read_dir(dir.path()).expect("mirror folder").flatten() {
                found.push(format!("{folder}/{}", file.file_name().to_string_lossy()));
            }
        }
        found.sort();
        found
    }

    /// An import's note is mirrored to its `.md` in the same save, and a
    /// cancelled save's rollback removes the file with the row, so no orphan
    /// is left.
    ///
    /// Run through the real `history` DB thread, because "the file is there by
    /// the time the save returns" is a claim about where the mirror runs.
    #[test]
    fn an_import_mirrors_its_note_and_a_cancelled_one_leaves_nothing_behind() {
        let dir = TempDir::new("mirror");
        let db_path = unique_temp("mirror-db", ".db");
        let recorder = crate::history::spawn(
            db_path.clone(),
            crate::history::RetentionCfg { enabled: true, keep_days: 0 },
        );
        let steps = LiveSteps::new(
            "k".to_string(),
            "en-IN",
            "transcription".to_string(),
            probe::ImportLimits::default(),
            recorder.clone(),
            Some(dir.path().to_path_buf()),
        );

        let id = steps
            .save_note(build_note(
                "Standup.wav",
                Some(100.0),
                &transcript("hello there", &[(0.0, 90.0)]),
            ))
            .expect("the note is saved");

        let file_name = notes::mirror::note_file_name(id, "Standup");
        assert_eq!(
            mirrored(dir.path()),
            vec![format!("{}/{file_name}", notes::mirror::UNFILED_DIR)],
            "an imported note must be mirrored by the save that stored it"
        );
        let text = std::fs::read_to_string(
            notes::mirror::note_dir(dir.path(), None).join(&file_name),
        )
        .expect("read the mirrored note");
        assert!(text.starts_with("---\n"), "{text}");
        assert!(text.ends_with("hello there"), "{text}");

        // The rollback after a cancelled save, now that there is a file to
        // roll back.
        steps.discard_note(id);
        assert!(
            mirrored(dir.path()).is_empty(),
            "a cancelled import must leave no .md behind"
        );

        drop(steps);
        drop(recorder);
        let _ = std::fs::remove_file(&db_path);
    }

    /// The mirror being off is still off for an import: the note is stored and
    /// not one byte is written outside the database.
    #[test]
    fn an_import_with_the_mirror_off_writes_no_file() {
        let dir = TempDir::new("mirror-off");
        let db_path = unique_temp("mirror-off-db", ".db");
        let recorder = crate::history::spawn(
            db_path.clone(),
            crate::history::RetentionCfg { enabled: true, keep_days: 0 },
        );
        let steps = LiveSteps::new(
            "k".to_string(),
            "en-IN",
            "transcription".to_string(),
            probe::ImportLimits::default(),
            recorder.clone(),
            None,
        );

        let id = steps
            .save_note(build_note(
                "Standup.wav",
                Some(100.0),
                &transcript("hello there", &[(0.0, 90.0)]),
            ))
            .expect("the note is saved");
        assert!(recorder
            .with_connection(move |conn| notes::get_note(conn, id).unwrap().is_some())
            .expect("the DB thread answered"));
        assert!(mirrored(dir.path()).is_empty());

        drop(steps);
        drop(recorder);
        let _ = std::fs::remove_file(&db_path);
    }

    /// An import's transcript becomes a note whatever the history settings
    /// say, and Clear all history does not remove it. The privacy page and
    /// the README say so; change them with this.
    #[test]
    fn an_imports_note_is_kept_with_history_off_and_after_clear_all() {
        let db_path = unique_temp("history-off-db", ".db");
        let recorder = crate::history::spawn(
            db_path.clone(),
            crate::history::RetentionCfg { enabled: false, keep_days: 1 },
        );
        let steps = LiveSteps::new(
            "k".to_string(),
            "en-IN",
            "transcription".to_string(),
            probe::ImportLimits::default(),
            recorder.clone(),
            None,
        );

        let id = steps
            .save_note(build_note(
                "Standup.wav",
                Some(100.0),
                &transcript("hello there", &[(0.0, 90.0)]),
            ))
            .expect("the note is saved with history off");
        recorder.clear();
        assert!(recorder
            .with_connection(move |conn| notes::get_note(conn, id).unwrap().is_some())
            .expect("the DB thread answered"));

        drop(steps);
        drop(recorder);
        let _ = std::fs::remove_file(&db_path);
    }

    // ---------------------------------------------------------- privacy --

    /// Whether a `tracing!` body names `banned` as a field.
    ///
    /// Three spellings, not one. `field = expr` is the obvious one, but
    /// `info!(path, …)` is valid shorthand that records the local variable
    /// under its own name, and `?path` / `%path` are the Debug and Display
    /// sigils — all three put the value in the log, and only the first
    /// contains an `=`. (The level prefix is spelled without its crate path
    /// on purpose: the scan below looks for that prefix, and would otherwise
    /// read this very sentence as a log call and fail on it.) So the match is
    /// on the field *position*: immediately after the opening paren or a
    /// comma, optionally behind a sigil, and ending on a word boundary so
    /// `path_free_count` is not a false positive.
    fn binds_field(body: &str, banned: &str) -> bool {
        let bytes = body.as_bytes();
        for (i, _) in body.match_indices(banned) {
            // Must start a token: the character before is `(`, `,`, `?` or `%`
            // (the sigils themselves being preceded by `(` or `,`).
            let mut j = i;
            while j > 0 && matches!(bytes[j - 1], b'?' | b'%') {
                j -= 1;
            }
            let before = body[..j].trim_end();
            let opens = before.ends_with('(') || before.ends_with(',');
            if !opens {
                continue;
            }
            // Must end one: `path` and `path = x` are hits, `pathological` is
            // not.
            let after = body[i + banned.len()..].trim_start();
            let ends = after.is_empty()
                || after.starts_with('=')
                || after.starts_with(',')
                || after.starts_with(')');
            if ends {
                return true;
            }
        }
        false
    }

    // ------------------------------------------------- the scratch WAV --

    /// Two properties in one place because they fail the same way. A name
    /// that can collide means one concurrent import uploading another's
    /// audio; an extension that is not `.wav` means Sarvam's storage layer
    /// receives `audio.opus` (or `audio`) for a file that is a WAV, because
    /// `batch_job::upload_file_name` reads the path it is handed.
    #[test]
    fn a_scratch_wav_is_uniquely_named_and_ends_in_wav() {
        let a = ScratchWav::new();
        let b = ScratchWav::new();
        assert_ne!(a.path(), b.path(), "two scratch files must never collide");
        for s in [&a, &b] {
            assert_eq!(
                s.path().extension().and_then(|e| e.to_str()),
                Some("wav"),
                "the name on the wire is derived from this extension"
            );
            assert_eq!(s.path().parent(), Some(std::env::temp_dir().as_path()));
        }
        assert_eq!(
            batch_job::upload_file_name(a.path()),
            "audio.wav",
            "whatever the user picked, Sarvam must receive a WAV by name too"
        );
    }

    /// The whole point of the guard: the user's speech does not stay in
    /// `%TEMP%` because a branch was missed.
    #[test]
    fn a_scratch_wav_deletes_itself_when_it_goes_out_of_scope() {
        let kept = {
            let scratch = ScratchWav::new();
            std::fs::write(scratch.path(), b"RIFF....WAVE").expect("write the scratch file");
            assert!(scratch.path().exists());
            scratch.path().to_path_buf()
        };
        assert!(
            !kept.exists(),
            "a converted import must not outlive the run that made it"
        );
    }

    /// A run the app was killed in (the uninstaller and the installer both
    /// kill it) never reaches `Drop`, so its converted audio would sit in
    /// `%TEMP%` for good. The startup sweep takes those, and only those: a
    /// scratch file's exact name, untouched for longer than a run lasts.
    #[test]
    fn the_startup_sweep_removes_only_stale_scratch_wavs() {
        let dir = TempDir::new("stale-scratch");
        let now = std::time::SystemTime::now();
        let old = now - std::time::Duration::from_secs(3 * 24 * 60 * 60);
        let make = |name: &str, at: std::time::SystemTime| {
            let p = dir.path().join(name);
            let f = std::fs::File::create(&p).expect("create a fixture file");
            f.set_modified(at).expect("date the fixture file");
            p
        };
        let stale = make("bs-import-0f8c2d1e-3b4a-4c5d-8e9f-0a1b2c3d4e5f.wav", old);
        let kept = [
            // A run in progress (another install sharing %TEMP%).
            make("bs-import-1f8c2d1e-3b4a-4c5d-8e9f-0a1b2c3d4e5f.wav", now),
            // Names ScratchWav never mints.
            make("bs-import-notes-1234.wav", old),
            make("bs-import-2f8c2d1e-3b4a-4c5d-8e9f-0a1b2c3d4e5f.txt", old),
            make("recording.wav", old),
        ];
        let folder = dir.path().join("bs-import-3f8c2d1e-3b4a-4c5d-8e9f-0a1b2c3d4e5f.wav");
        std::fs::create_dir_all(&folder).expect("create a fixture folder");

        assert_eq!(sweep_stale_scratch(dir.path(), now), 1);
        assert!(!stale.exists(), "a stale converted import survived the sweep");
        for p in &kept {
            assert!(p.exists(), "the sweep removed {}", p.display());
        }
        assert!(folder.exists(), "the sweep removed a folder");
    }

    /// A ticked "Delete app data" removes the same files from %TEMP%
    /// (src-tauri/windows/hooks.nsh). Rename the scratch file and this fails
    /// until the hook follows it.
    #[test]
    fn the_uninstall_hook_removes_the_scratch_wavs() {
        const HOOK: &str = include_str!("../../windows/hooks.nsh");
        assert!(HOOK.contains("!define BS_TEMP_BASE \"$TEMP\""));
        assert!(HOOK.contains("\\bs-import-*.wav\""));
        let scratch = ScratchWav::new();
        let name = scratch.path().file_name().unwrap().to_str().unwrap().to_string();
        assert!(name.starts_with("bs-import-") && name.ends_with(".wav"), "{name}");
        assert_eq!(scratch.path().parent(), Some(std::env::temp_dir().as_path()));
    }

    /// ...and a scratch file that was never written is not an error on the
    /// way out, since every failed conversion deletes its own output first.
    #[test]
    fn a_scratch_wav_that_was_never_written_drops_quietly() {
        let scratch = ScratchWav::new();
        let path = scratch.path().to_path_buf();
        assert!(!path.exists());
        drop(scratch);
        assert!(!path.exists());
    }

    /// The tripwire's own detector, tested — otherwise "no log names a path"
    /// rests on a matcher nobody checked. The three shorthand spellings are
    /// the ones the first version of this missed.
    #[test]
    fn the_log_tripwire_catches_every_way_to_name_a_field() {
        for hit in [
            "info!(path = %p, \"x\")",
            "info!(path, \"x\")",
            "info!(?path, \"x\")",
            "info!(%path, \"x\")",
            "warn!(kind = ?e, path, \"x\")",
            "warn!(count = 1, ?path)",
        ] {
            assert!(binds_field(hit, "path"), "missed a field in {hit:?}");
        }
        for miss in [
            "info!(\"could not open the file to import\")",
            "info!(path_free_count = 3, \"x\")",
            "info!(count = 1, \"the path is never logged\")",
            "info!(dropped = paths.len(), \"x\")",
        ] {
            assert!(!binds_field(miss, "path"), "false positive on {miss:?}");
        }
    }

    /// The tripwire, read back out of this file: no log field may carry a path,
    /// a filename, or any part of a transcript. Counts and states only.
    ///
    /// It reads the **whole** macro invocation, not the first line of it: every
    /// interesting log call here spans several lines, so a first-line-only
    /// check would pass while `path = …` sat on line two — which is exactly the
    /// shape a careless edit takes.
    #[test]
    fn the_module_logs_no_path_or_filename_field() {
        const BANNED: &[&str] = &[
            "path", "name", "file", "text", "title", "content", "transcript", "segment",
        ];
        let source = include_str!("mod.rs");
        let mut checked = 0;
        for (offset, _) in source.match_indices("tracing::") {
            let rest = &source[offset + "tracing::".len()..];
            // Only real invocations; the string literal in this test's own body
            // is followed by a quote, not a level name.
            if !["info!", "warn!", "error!", "debug!", "trace!"]
                .iter()
                .any(|level| rest.starts_with(level))
            {
                continue;
            }
            let body = match rest.find(");") {
                Some(end) => &rest[..end],
                None => rest,
            };
            checked += 1;
            for banned in BANNED {
                assert!(
                    !binds_field(body, banned),
                    "a log call names the field {banned:?}, which can carry something \
                     personal:\ntracing::{body});"
                );
            }
        }
        assert!(
            checked >= 5,
            "the scan found only {checked} log calls; if the module stopped logging, \
             this test stopped testing anything"
        );
    }
}
