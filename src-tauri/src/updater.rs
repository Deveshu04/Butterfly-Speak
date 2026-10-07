//! Finding and installing new versions of the app.
//!
//! This module owns the background schedule that asks the update feed whether
//! a newer build exists, the check the user starts from Help & about, and the
//! install that runs only when the user presses Install. No window holds an
//! `updater:*` permission: pages reach this code only through the three
//! commands at the end of this file and follow [`events::UPDATE_STATE`].
//!
//! Rules that are easy to break by accident:
//!
//! - `settings.updates.auto_check` is read each time an automatic check falls
//!   due, so flipping it needs no restart. It gates the request itself, not
//!   just the notice: with it off, the app sends nothing to the update host.
//!   A check the user starts ignores it.
//! - One check at a time. A check asked for while another is out, or while an
//!   install is running, returns at once and changes nothing; whoever asked
//!   sees the running check's result through the event.
//! - A check writes its verdict only if nothing else wrote the state while its
//!   request was out (see [`StateCell`]). Otherwise the verdict is dropped, so
//!   a late answer never covers an install error the user is reading.
//! - An automatic check that fails puts back what was showing and logs a
//!   warning. Only a check the user started puts an error on screen.
//! - Install is refused while a dictation or an import is running, because
//!   the installer ends this process ([`install_blocker`]).
//! - Log lines name the kind of check, the versions and the outcome. They
//!   never carry the manifest or any response body.

use crate::commands::Backend;
use crate::events;
use serde::Serialize;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tauri::{AppHandle, Emitter, Manager, State};
use tauri_plugin_updater::{Update, UpdaterExt};

/// Wait between launch and the first automatic check. A login launch shares
/// the disk and network with every other startup program, so the request
/// stays clear of the hook, the microphone and the window coming up, yet
/// still answers inside the first minute for someone who opened the app to
/// look for an update.
pub const LAUNCH_CHECK_DELAY: Duration = Duration::from_secs(30);
/// Time between automatic checks while the app stays running: three requests
/// a day per install, and a release reaches a machine left on within a
/// working day.
pub const CHECK_CADENCE: Duration = Duration::from_secs(8 * 60 * 60);
/// How often the scheduler compares the wall clock with the last automatic
/// check. Polling the clock, rather than sleeping for the whole cadence, lets
/// a machine that slept through a due check make one check soon after it
/// wakes.
const SCHEDULE_TICK: Duration = Duration::from_secs(10 * 60);
/// A relaunch inside this window skips the startup check.
pub const STARTUP_SKIP_WINDOW_SECS: u64 = 60 * 60;
/// Per request. The manifest is a few hundred bytes; a server that has not
/// answered in this long is not going to.
pub const CHECK_TIMEOUT: Duration = Duration::from_secs(20);
/// Release notes are plain text on the About page; anything longer is a
/// changelog, and the page is not one.
pub const NOTES_MAX_CHARS: usize = 4000;
/// Sidecar beside `settings.json`: unix seconds of the last check — successful
/// or not, because the point of the stamp is to rate-limit the feed, and a
/// relaunch loop against an unreachable server is exactly the case it exists
/// for.
const LAST_CHECK_SIDECAR: &str = "last-update-check";
/// How long the second [`install_blocker`] ask will wait for a dictation that
/// began during the download. Long enough for an ordinary utterance and its
/// transcription, short enough that a hands-free session left running does
/// not look like a hang.
pub const INSTALL_WAIT: Duration = Duration::from_secs(30);
/// The poll inside that wait. There is no notification for "the controller
/// went Idle"; 500 ms is imperceptible next to the install that follows.
const INSTALL_POLL: Duration = Duration::from_millis(500);

/// Shown when Install is pressed while the import queue is draining. A batch
/// upload runs for minutes and has no resume, so the sentence names the two
/// ways out rather than asking the user to guess.
pub const IMPORT_BUSY: &str = "Finish or cancel the import first, then install the update.";
/// Shown when Install is pressed during a dictation or a transform.
pub const DICTATION_BUSY: &str =
    "Wait for the current dictation or transform to finish, then install the update.";

/// Why installing right now would destroy work, or `None` if it would not.
///
/// The hand-off ends this process — the plugin calls `std::process::exit(0)`
/// after `ShellExecuteW` — so anything still in flight in it dies without a
/// history row and without an error. Settings, the history DB (WAL) and the
/// sidecars are all crash-safe; the *job* is not, and that is what this
/// refuses for.
///
/// Import wins when both are set, and the reason is the user's next move
/// rather than any ranking of the two jobs: a dictation ends on its own in
/// seconds, so its sentence would be stale by the time it was read, while an
/// import needs a decision (wait it out or cancel it) that only its own
/// sentence asks for.
pub fn install_blocker(dictating: bool, importing: bool) -> Option<&'static str> {
    if importing {
        Some(IMPORT_BUSY)
    } else if dictating {
        Some(DICTATION_BUSY)
    } else {
        None
    }
}

/// Whether the hand-off would cut short work the user started with a chord:
/// a dictation in any state but Idle, or a transform, which has copied the
/// user's selection and not yet pasted over it. `crash_recovery` asks the
/// same question before it relaunches.
pub(crate) fn dictating(dictation_busy: &AtomicBool, transform_busy: &AtomicBool) -> bool {
    dictation_busy.load(Ordering::SeqCst) || transform_busy.load(Ordering::SeqCst)
}

/// The live flags behind [`install_blocker`], read together so the two call
/// sites cannot drift apart.
fn install_blocked(app: &AppHandle) -> Option<&'static str> {
    let backend = app.state::<Backend>();
    install_blocker(
        dictating(&backend.dictation_busy, &backend.transform_busy),
        backend.import.snapshot().running,
    )
}

/// What the frontend renders. One event per transition on
/// [`events::UPDATE_STATE`]; `update_status` returns the same shape for a page
/// that mounts mid-flight.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum UpdateState {
    #[default]
    Idle,
    Checking,
    UpToDate,
    Available { version: String, notes: Option<String> },
    Downloading { version: String, percent: Option<u8> },
    /// The installer has been handed the bytes; the process is about to exit.
    Installing { version: String },
    /// Only ever set by an action the user took. A background check that
    /// fails leaves the previous state in place and logs.
    Error { message: String },
}

/// Who asked for a check. Decides whether the switch applies, what a failure
/// shows and whether an offer raises a system notification.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CheckOrigin {
    /// The one automatic check shortly after launch.
    Launch,
    /// An automatic check on the running cadence.
    Cadence,
    /// The user pressed Check for updates.
    User,
}

impl CheckOrigin {
    fn is_automatic(self) -> bool {
        !matches!(self, CheckOrigin::User)
    }

    fn label(self) -> &'static str {
        match self {
            CheckOrigin::Launch => "automatic, at launch",
            CheckOrigin::Cadence => "automatic, on the cadence",
            CheckOrigin::User => "user",
        }
    }
}

/// The state, plus a monotonic count of the writes to it.
///
/// Both under one lock so that "write this only if nothing has written since
/// I last did" is a single atomic step rather than a load followed by a store
/// with a gap in between. [`Updates::set`] hands back the count its write
/// produced — a *mark* — and [`Updates::set_if_current`] takes one back; see
/// [`check_for_update`] for the race that pair exists for.
#[derive(Default)]
struct StateCell {
    state: UpdateState,
    writes: u64,
}

impl StateCell {
    /// Replace the state; returns the mark identifying this write.
    fn write(&mut self, next: UpdateState) -> u64 {
        self.state = next;
        self.writes += 1;
        self.writes
    }

    /// Whether `mark` is still the most recent write — nothing has touched
    /// the state since the writer that produced it.
    fn is_current(&self, mark: u64) -> bool {
        self.writes == mark
    }
}

/// Managed state. `found` is the update the card is showing, so Install
/// installs exactly that and never re-checks.
#[derive(Default)]
pub struct Updates {
    state: Mutex<StateCell>,
    found: Mutex<Option<Update>>,
    checking: AtomicBool,
    installing: AtomicBool,
    /// Versions already announced by a system notification this process life.
    notified: Mutex<Option<String>>,
}

impl Updates {
    fn cell(&self) -> MutexGuard<'_, StateCell> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn current(&self) -> UpdateState {
        self.cell().state.clone()
    }

    /// Replace the state and tell the frontend; returns the mark identifying
    /// this write, for a caller that will later want to know whether it still
    /// owns what it wrote.
    fn set(&self, app: &AppHandle, next: UpdateState) -> u64 {
        let mark = self.cell().write(next.clone());
        self.emit(app, next);
        mark
    }

    /// The same, but only while `mark` is still the most recent write.
    /// `false` means somebody else has written since and the caller's news is
    /// stale — nothing is written and nothing is emitted.
    fn set_if_current(&self, app: &AppHandle, mark: u64, next: UpdateState) -> bool {
        {
            let mut cell = self.cell();
            if !cell.is_current(mark) {
                return false;
            }
            cell.write(next.clone());
        }
        self.emit(app, next);
        true
    }

    fn emit(&self, app: &AppHandle, next: UpdateState) {
        if let Err(e) = app.emit(events::UPDATE_STATE, next) {
            tracing::warn!("update state emit failed: {e}");
        }
    }
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Whether the startup check should run, given the sidecar stamp. Pure, so
/// the window rule is pinned by tests rather than by reading the file.
pub fn startup_check_due(last: Option<u64>, now: u64) -> bool {
    match last {
        None => true,
        Some(last) if last > now => true,
        Some(last) => now - last >= STARTUP_SKIP_WINDOW_SECS,
    }
}

fn read_last_check() -> Option<u64> {
    std::fs::read_to_string(crate::settings::config_dir().join(LAST_CHECK_SIDECAR))
        .ok()
        .and_then(|s| s.trim().parse().ok())
}

fn stamp_last_check() {
    let dir = crate::settings::config_dir();
    if let Err(e) = std::fs::create_dir_all(&dir)
        .and_then(|_| std::fs::write(dir.join(LAST_CHECK_SIDECAR), now_secs().to_string()))
    {
        tracing::warn!("couldn't stamp last update check: {e}");
    }
}

pub fn percent(downloaded: u64, total: Option<u64>) -> Option<u8> {
    let total = total.filter(|t| *t > 0)?;
    Some((downloaded.saturating_mul(100) / total).min(100) as u8)
}

pub fn tidy_notes(body: Option<&str>) -> Option<String> {
    let t = body?.trim();
    if t.is_empty() {
        return None;
    }
    Some(t.chars().take(NOTES_MAX_CHARS).collect())
}

/// Whether automatic checks are switched on in `settings`.
pub fn automatic_checks_on(settings: &crate::settings::Settings) -> bool {
    settings.updates.auto_check
}

/// Whether a check from `origin` may send its request, given the switch.
fn may_run(origin: CheckOrigin, switch_on: bool) -> bool {
    !origin.is_automatic() || switch_on
}

/// Whether the cadence has come round again since the automatic check at
/// `last` (unix seconds). A clock that went backwards counts as due, so a bad
/// clock errs towards checking rather than never checking again.
fn cadence_due(last: u64, now: u64) -> bool {
    now < last || now - last >= CHECK_CADENCE.as_secs()
}

/// What a failed request leaves on screen. The user who pressed the button
/// gets a sentence; an automatic check puts back `before`, the state that
/// was showing when it started.
fn state_after_failure(
    origin: CheckOrigin,
    before: UpdateState,
    error: &tauri_plugin_updater::Error,
) -> UpdateState {
    if origin.is_automatic() {
        before
    } else {
        UpdateState::Error { message: prose(error) }
    }
}

/// Whether a found update also gets a system notification: only for an
/// automatic check whose result reached the screen. A user who pressed Check
/// is already looking at the card.
fn announces(origin: CheckOrigin, landed: bool) -> bool {
    origin.is_automatic() && landed
}

/// A sentence for the person who pressed the button.
///
/// Matched on the plugin's error discriminants, never on message text. Its
/// `Error` is `#[non_exhaustive]`, so the catch-all is not optional — and it
/// keeps a new variant in a future plugin release from becoming a compile
/// error here.
fn prose(e: &tauri_plugin_updater::Error) -> String {
    use tauri_plugin_updater::Error as E;
    match e {
        E::Reqwest(_) => {
            "Couldn't reach the update server. Check your connection and try again.".into()
        }
        // Not a connection problem, despite the name. `Network` has exactly
        // one raise site — `download`, on a response that arrived carrying a
        // non-success status (plugin `updater.rs:713-717`) — so it is the
        // shape of the most likely first-release mistake: an installer
        // uploaded under a different name than the manifest's `url`, or a
        // manifest pointing at a release that was never published. The status
        // is carried through because it is the one thing that tells whoever
        // cut the release which of those it was.
        E::Network(s) => format!("The update server couldn't provide the download ({s})."),
        E::Minisign(_) | E::SignatureUtf8(_) | E::Base64(_) => {
            "The download didn't match its signature, so it was not installed.".into()
        }
        // `ReleaseNotFound` is "no usable release JSON"; the two `Target*`
        // variants are "the manifest has no build for this platform". Both
        // read the same way to the person who pressed the button: there is
        // nothing here for them yet.
        E::ReleaseNotFound | E::TargetNotFound(_) | E::TargetsNotFound(_) => {
            "No update was found for this build.".into()
        }
        E::Io(_) => "Couldn't write the update to disk.".into(),
        other => format!("Couldn't update: {other}"),
    }
}

/// Asks the feed once and puts the answer on the card.
///
/// Returns without a request, and without touching the state, when the
/// switch keeps an automatic check from running, when another check is out,
/// or when an install is running. Otherwise it writes `Checking`, waits for
/// the feed, writes the verdict only if the `Checking` mark is still current,
/// stamps the sidecar and clears the in-flight flag.
pub async fn check_for_update(app: &AppHandle, origin: CheckOrigin) {
    let switch_on = {
        let backend = app.state::<Backend>();
        let settings = backend.settings.read().unwrap_or_else(|e| e.into_inner());
        automatic_checks_on(&settings)
    };
    if !may_run(origin, switch_on) {
        tracing::debug!(origin = origin.label(), "update check not sent: automatic checks are off");
        return;
    }

    let updates = app.state::<Updates>();
    if updates.installing.load(Ordering::SeqCst) {
        tracing::debug!(origin = origin.label(), "update check not sent: an install is running");
        return;
    }
    if updates.checking.swap(true, Ordering::SeqCst) {
        tracing::debug!(origin = origin.label(), "update check not sent: one is already out");
        return;
    }

    let before = updates.current();
    let mark = updates.set(app, UpdateState::Checking);
    let current = app.package_info().version.to_string();

    let mut offered: Option<String> = None;
    let verdict = match ask_feed(app).await {
        Ok(Some(update)) => {
            let version = update.version.clone();
            let notes = tidy_notes(update.body.as_deref());
            tracing::info!(origin = origin.label(), %current, offered = %version, "update check: a newer version is available");
            // Stored before the card write, and whether or not that write
            // lands: Install always takes the newest offer known.
            *updates.found.lock().unwrap_or_else(|e| e.into_inner()) = Some(update);
            offered = Some(version.clone());
            UpdateState::Available { version, notes }
        }
        Ok(None) => {
            tracing::info!(origin = origin.label(), %current, "update check: up to date");
            *updates.found.lock().unwrap_or_else(|e| e.into_inner()) = None;
            UpdateState::UpToDate
        }
        Err(e) => {
            if origin.is_automatic() {
                tracing::warn!(origin = origin.label(), %current, "update check failed: {e}");
            } else {
                tracing::info!(origin = origin.label(), %current, "update check failed: {e}");
            }
            state_after_failure(origin, before, &e)
        }
    };

    let landed = updates.set_if_current(app, mark, verdict);
    if !landed {
        tracing::info!(origin = origin.label(), "update check result dropped: the state changed while the request was out");
    }
    if let Some(version) = offered {
        if announces(origin, landed) {
            announce(app, &version);
        }
    }

    stamp_last_check();
    updates.checking.store(false, Ordering::SeqCst);
}

/// One request to the feed, bounded by [`CHECK_TIMEOUT`].
async fn ask_feed(app: &AppHandle) -> Result<Option<Update>, tauri_plugin_updater::Error> {
    app.updater_builder().timeout(CHECK_TIMEOUT).build()?.check().await
}

/// A system notification, only when the window is not there to show the
/// card, and at most once per version while the app runs, so an update left
/// uninstalled never becomes a recurring nag.
fn announce(app: &AppHandle, version: &str) {
    use tauri_plugin_notification::NotificationExt;
    let updates = app.state::<Updates>();
    let mut notified = updates.notified.lock().unwrap_or_else(|e| e.into_inner());
    if notified.as_deref() == Some(version) {
        return;
    }
    let window_up = app
        .get_webview_window("main")
        .and_then(|w| w.is_visible().ok())
        .unwrap_or(false);
    if window_up {
        return;
    }
    *notified = Some(version.to_string());
    if let Err(e) = app
        .notification()
        .builder()
        .title(format!("Butterfly Speak {version} is available"))
        .body("Open Butterfly Speak from the tray to install it.")
        .show()
    {
        tracing::warn!("update notification failed: {e}");
    }
}

/// Download with progress, verify, hand off. Runs to completion on the async
/// runtime; the caller returns immediately and the card follows the events.
/// On success the plugin exits the process — the `Installing` state is the
/// last thing this process says.
fn spawn_install(app: AppHandle) {
    tauri::async_runtime::spawn(async move {
        let updates = app.state::<Updates>();
        // The "already installing" latch is taken before `found` is read, not
        // after: a check that lands mid-download clears `found` when it comes
        // back up-to-date, and with the other order a second click would then
        // answer "nothing to install" while the first download was still
        // running. The in-flight install is unaffected either way — it holds
        // its own clone of the `Update`.
        if updates.installing.swap(true, Ordering::SeqCst) {
            return; // already on it — the card is already showing progress
        }
        let found = updates.found.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let Some(update) = found else {
            updates.installing.store(false, Ordering::SeqCst);
            updates.set(
                &app,
                UpdateState::Error {
                    message: "No update is ready to install. Check for updates first.".into(),
                },
            );
            return;
        };
        // Asked before a single byte moves, so a refusal is instant and the
        // sentence is something the user can act on straight away rather than
        // after a download they then find out was wasted.
        if let Some(message) = install_blocked(&app) {
            tracing::info!("update install refused: {message}");
            updates.set(&app, UpdateState::Error { message: message.into() });
            updates.installing.store(false, Ordering::SeqCst);
            return;
        }
        let version = update.version.clone();
        updates.set(&app, UpdateState::Downloading { version: version.clone(), percent: None });

        let progress_app = app.clone();
        let progress_version = version.clone();
        let mut downloaded: u64 = 0;
        let mut shown: Option<u8> = None;
        let bytes = update
            .download(
                move |chunk, total| {
                    downloaded += chunk as u64;
                    let pct = percent(downloaded, total);
                    if pct != shown {
                        shown = pct;
                        progress_app.state::<Updates>().set(
                            &progress_app,
                            UpdateState::Downloading { version: progress_version.clone(), percent: pct },
                        );
                    }
                },
                || {},
            )
            .await;

        // `download` is also what checks the minisign signature, at the end of
        // the transfer — so a tampered payload fails here and never reaches
        // `install`.
        let bytes = match bytes {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!(%version, "update download failed: {e}");
                updates.set(&app, UpdateState::Error { message: prose(&e) });
                updates.installing.store(false, Ordering::SeqCst);
                return;
            }
        };
        // Asked a second time, because the download took as long as it took
        // and the user is not obliged to sit still through it: a dictation
        // begun at 90% would otherwise be killed by the hand-off below. This
        // ask waits rather than refusing outright — the bytes are downloaded
        // and verified, so the cheap outcome is to let a few seconds of speech
        // finish and then install, and only a job still running after
        // `INSTALL_WAIT` sends the user back to the button.
        let mut waited = Duration::ZERO;
        let blocked = loop {
            match install_blocked(&app) {
                None => break None,
                Some(message) if waited >= INSTALL_WAIT => break Some(message),
                Some(_) => {
                    tokio::time::sleep(INSTALL_POLL).await;
                    waited += INSTALL_POLL;
                }
            }
        };
        if let Some(message) = blocked {
            tracing::info!(%version, "verified update held back: {message}");
            updates.set(&app, UpdateState::Error { message: message.into() });
            updates.installing.store(false, Ordering::SeqCst);
            return;
        }
        tracing::info!(%version, bytes = bytes.len(), "update downloaded and signature-verified; handing off to the installer");
        updates.set(&app, UpdateState::Installing { version: version.clone() });
        if let Err(e) = update.install(bytes) {
            tracing::warn!(%version, "update install failed: {e}");
            updates.set(&app, UpdateState::Error { message: prose(&e) });
            updates.installing.store(false, Ordering::SeqCst);
        }
    });
}

/// Manages [`Updates`] and starts the background schedule: one check
/// [`LAUNCH_CHECK_DELAY`] after launch (skipped inside the relaunch window),
/// then one each time [`CHECK_CADENCE`] comes round. Call it after `Backend`
/// is managed, because every automatic check reads the switch from there.
pub fn start(app: &AppHandle) {
    app.manage(Updates::default());
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(LAUNCH_CHECK_DELAY).await;
        if startup_check_due(read_last_check(), now_secs()) {
            check_for_update(&app, CheckOrigin::Launch).await;
        } else {
            tracing::info!(
                "launch update check skipped: the last check was less than {} min ago",
                STARTUP_SKIP_WINDOW_SECS / 60
            );
        }

        // Counted from here whether or not the launch check ran, so a skipped
        // one does not pull the next check forward.
        let mut last_due = now_secs();
        let mut ticks = tokio::time::interval(SCHEDULE_TICK);
        // After a sleep, one tick fires and the rest are dropped rather than
        // replayed, so waking can cost at most one check.
        ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        ticks.tick().await; // the first tick is immediate
        loop {
            ticks.tick().await;
            let now = now_secs();
            if cadence_due(last_due, now) {
                // Moved on even when the switch keeps the check from running,
                // so turning it back on waits for the next due time.
                last_due = now;
                check_for_update(&app, CheckOrigin::Cadence).await;
            }
        }
    });
}

#[tauri::command]
pub fn update_status(updates: State<'_, Updates>) -> UpdateState {
    updates.current()
}

/// Manual check. Never gated by `updates.auto_check`.
#[tauri::command]
pub async fn update_check(app: AppHandle) -> Result<UpdateState, String> {
    check_for_update(&app, CheckOrigin::User).await;
    Ok(app.state::<Updates>().current())
}

#[tauri::command]
pub fn update_install(app: AppHandle) -> Result<(), String> {
    spawn_install(app);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_first_launch_with_no_stamp_is_due() {
        assert!(startup_check_due(None, 1_800_000_000));
    }

    #[test]
    fn a_relaunch_inside_the_window_is_not_due() {
        let now = 1_800_000_000;
        assert!(!startup_check_due(Some(now - STARTUP_SKIP_WINDOW_SECS + 1), now));
    }

    #[test]
    fn a_relaunch_at_the_window_edge_is_due() {
        let now = 1_800_000_000;
        assert!(startup_check_due(Some(now - STARTUP_SKIP_WINDOW_SECS), now));
    }

    /// The clock went backwards (a stamp from the future). Checking is the
    /// safe side of a bad clock; silently never checking is not.
    #[test]
    fn a_stamp_from_the_future_is_due() {
        assert!(startup_check_due(Some(1_900_000_000), 1_800_000_000));
    }

    #[test]
    fn percent_is_none_without_a_total_and_never_exceeds_100() {
        assert_eq!(percent(10, None), None);
        assert_eq!(percent(0, Some(0)), None);
        assert_eq!(percent(50, Some(200)), Some(25));
        assert_eq!(percent(999, Some(200)), Some(100));
    }

    /// The frontend switches on `kind` and reads camelCase fields; a rename on
    /// either side is a silent blank card, so the wire shape is pinned here.
    #[test]
    fn state_serializes_with_a_kind_tag_and_camel_case_fields() {
        let v = serde_json::to_value(UpdateState::Available {
            version: "0.2.0".into(),
            notes: Some("Fixes".into()),
        })
        .unwrap();
        assert_eq!(v["kind"], "available");
        assert_eq!(v["version"], "0.2.0");
        assert_eq!(v["notes"], "Fixes");
        let v = serde_json::to_value(UpdateState::UpToDate).unwrap();
        assert_eq!(v["kind"], "upToDate");
        let v = serde_json::to_value(UpdateState::Downloading {
            version: "0.2.0".into(),
            percent: Some(42),
        })
        .unwrap();
        assert_eq!(v["kind"], "downloading");
        assert_eq!(v["percent"], 42);
    }

    /// Release notes are shown as plain text; an empty or whitespace body is
    /// "no notes", and a novel is cut so the About page stays a page.
    #[test]
    fn notes_are_trimmed_capped_and_none_when_blank() {
        assert_eq!(tidy_notes(Some("  \n ")), None);
        assert_eq!(tidy_notes(Some("  Fixes a thing  ")), Some("Fixes a thing".into()));
        let long = "x".repeat(NOTES_MAX_CHARS + 50);
        assert_eq!(tidy_notes(Some(&long)).unwrap().chars().count(), NOTES_MAX_CHARS);
        assert_eq!(tidy_notes(None), None);
    }

    /// The stale-verdict guard, in the shape of the race it exists for.
    ///
    /// A check writes `Checking` and its request hangs for up to
    /// `CHECK_TIMEOUT`. Inside that window the user presses Install, whose
    /// download fails fast and leaves an error on the card — so by the time
    /// the check resumes, `installing` is false again and a flag can no
    /// longer see that anything happened. The write mark can, which is the
    /// whole reason it exists: the check must not speak over an error the
    /// user is looking at.
    #[test]
    fn a_verdict_is_stale_once_anything_else_has_written_the_state() {
        let mut cell = StateCell::default();
        let mine = cell.write(UpdateState::Checking);
        assert!(cell.is_current(mine), "nothing has interrupted this check yet");

        cell.write(UpdateState::Downloading { version: "0.2.0".into(), percent: None });
        cell.write(UpdateState::Error { message: "download failed".into() });
        assert!(
            !cell.is_current(mine),
            "the check would be overwriting the install's error"
        );

        // And a check nobody interrupted still gets to speak.
        let mine = cell.write(UpdateState::Checking);
        assert!(cell.is_current(mine));
    }

    /// Installing exits the process, so the four combinations of "is anything
    /// in flight" are the whole install policy and are pinned here rather
    /// than left to the two call sites in `spawn_install`.
    #[test]
    fn an_install_is_refused_while_work_is_in_flight_and_the_import_speaks_first() {
        assert_eq!(install_blocker(false, false), None, "nothing running: install");
        assert_eq!(install_blocker(true, false), Some(DICTATION_BUSY));
        assert_eq!(install_blocker(false, true), Some(IMPORT_BUSY));
        // Both at once: the import is the one the user has to decide about;
        // the dictation will be over before they finish reading either
        // sentence.
        assert_eq!(install_blocker(true, true), Some(IMPORT_BUSY));
    }

    /// A transform counts as work in flight, the same as a dictation: it has
    /// the user's selection on the clipboard and a paste still to make.
    #[test]
    fn a_running_transform_blocks_an_install_like_a_dictation() {
        let (idle, busy) = (AtomicBool::new(false), AtomicBool::new(true));
        assert!(!dictating(&idle, &idle));
        assert!(dictating(&busy, &idle));
        assert!(dictating(&idle, &busy));
        assert_eq!(
            install_blocker(dictating(&idle, &busy), false),
            Some(DICTATION_BUSY)
        );
    }

    /// Both sentences have to name the thing to do, not just the problem —
    /// the card shows them with no other affordance.
    #[test]
    fn the_refusals_tell_the_user_what_to_do_about_it() {
        for message in [IMPORT_BUSY, DICTATION_BUSY] {
            assert!(message.contains("install the update"), "{message}");
            assert!(message.ends_with('.'), "a sentence, not a label: {message}");
        }
    }

    /// The sentence the user reads has to match the failure they actually
    /// hit. `Network` is raised only for a response that *arrived* with a bad
    /// status — a 404 on a mis-uploaded installer — so telling them to check
    /// their connection would send them after the wrong thing entirely.
    ///
    /// Only the constructible arms: `Error` is `#[non_exhaustive]`, which
    /// blocks an exhaustive `match` from outside the plugin but not a literal.
    #[test]
    fn prose_separates_an_unreachable_server_from_a_missing_download() {
        use tauri_plugin_updater::Error as E;

        let missing = prose(&E::Network("Download request failed with status: 404 Not Found".into()));
        assert!(missing.contains("couldn't provide the download"), "{missing}");
        assert!(missing.contains("404"), "the status is what identifies the mistake: {missing}");
        assert!(
            !missing.contains("connection"),
            "a 404 is not a connectivity problem: {missing}"
        );

        assert!(prose(&E::ReleaseNotFound).contains("No update was found"));
        assert!(prose(&E::TargetNotFound("windows-x86_64".into())).contains("No update was found"));
        assert!(prose(&E::SignatureUtf8("nope".into())).contains("didn't match its signature"));
    }

    /// Automatic checks follow the one settings field and nothing else, so a
    /// user who turns it off gets no requests from any other path.
    #[test]
    fn the_gate_is_exactly_the_auto_check_setting() {
        let mut s = crate::settings::Settings::default();
        assert!(automatic_checks_on(&s));
        s.updates.auto_check = false;
        assert!(!automatic_checks_on(&s));
    }

    #[test]
    fn a_due_automatic_check_runs_only_with_the_switch_on() {
        for origin in [CheckOrigin::Launch, CheckOrigin::Cadence] {
            assert!(!may_run(origin, false), "{origin:?} ran with automatic checks off");
            assert!(may_run(origin, true), "{origin:?} did not run with automatic checks on");
        }
    }

    #[test]
    fn a_check_the_user_starts_runs_with_the_switch_off() {
        assert!(may_run(CheckOrigin::User, false));
        assert!(may_run(CheckOrigin::User, true));
    }

    /// A background failure must not replace what the user was looking at,
    /// including an offer found earlier.
    #[test]
    fn an_automatic_failure_puts_back_what_was_showing() {
        use tauri_plugin_updater::Error as E;
        let offer = UpdateState::Available { version: "0.3.0".into(), notes: None };
        for origin in [CheckOrigin::Launch, CheckOrigin::Cadence] {
            assert_eq!(state_after_failure(origin, offer.clone(), &E::ReleaseNotFound), offer);
            assert_eq!(
                state_after_failure(origin, UpdateState::Idle, &E::Network("503".into())),
                UpdateState::Idle
            );
        }
    }

    #[test]
    fn a_failed_user_check_shows_the_sentence_for_its_error() {
        use tauri_plugin_updater::Error as E;
        let error = E::Network("Download request failed with status: 404 Not Found".into());
        assert_eq!(
            state_after_failure(CheckOrigin::User, UpdateState::Idle, &error),
            UpdateState::Error { message: prose(&error) }
        );
    }

    #[test]
    fn only_an_automatic_offer_that_reached_the_card_is_announced() {
        assert!(announces(CheckOrigin::Launch, true));
        assert!(announces(CheckOrigin::Cadence, true));
        assert!(!announces(CheckOrigin::Cadence, false), "a dropped verdict is not news");
        assert!(!announces(CheckOrigin::User, true), "the user is already looking at the card");
        assert!(!announces(CheckOrigin::User, false));
    }

    #[test]
    fn the_cadence_is_due_once_a_full_interval_has_passed() {
        let last = 1_800_000_000;
        let cadence = CHECK_CADENCE.as_secs();
        assert!(!cadence_due(last, last));
        assert!(!cadence_due(last, last + cadence - 1));
        assert!(cadence_due(last, last + cadence));
    }

    /// Waking after days asleep is one due check, and once the scheduler
    /// moves its mark to now the next one is a full cadence away.
    #[test]
    fn a_long_sleep_makes_one_catch_up_check_not_a_burst() {
        let last = 1_800_000_000;
        let woke = last + 3 * 24 * 60 * 60;
        assert!(cadence_due(last, woke));
        assert!(!cadence_due(woke, woke + SCHEDULE_TICK.as_secs()));
    }

    #[test]
    fn a_clock_that_went_backwards_counts_as_due() {
        assert!(cadence_due(1_900_000_000, 1_800_000_000));
    }
}
