//! Pause/resume media playback around a dictation via GSMTC (`audio.pause_media`).
//!
//! Never synthesizes `VK_MEDIA_PLAY_PAUSE`: that key toggles playback, so
//! sending it to an already-paused player would *start* it. GSMTC instead
//! lets this module see and set each session's actual playback state, and
//! hold on to exactly the sessions it paused so resume never wakes up a
//! player the user had paused themselves before dictating.
//!
//! Every session that reports Playing is paused, not only the one Windows
//! treats as current: a video in a browser tab and a music app can both be
//! audible, and either one talking over the dictation is the problem this
//! solves. Resume works from the session objects the pause collected rather
//! than looking sessions up again, so it reaches exactly those and cannot
//! mistake a different session from the same app for one of them.
//!
//! The WinRT calls block (`IAsyncOperation::join`), so none of them may
//! happen on the controller thread. [`pause_playing`] spawns one thread that
//! runs the whole pause-wait-resume lifecycle: it pauses every playing
//! session, parks until the [`PauseGuard`] the caller holds is dropped, then
//! resumes exactly what it paused. Failures are debug-logged only: a media
//! player that doesn't pause is a worse dictation experience, not a broken
//! one, and the feature ships off by default.
//!
//! Pausing is never abandoned on a timeout; resume waits at most
//! [`WINRT_CAP`]. An abandoned pause would leave a thread pausing sessions
//! nobody will resume, the state [`PauseGuard`] exists to rule out. An
//! abandoned resume loses nothing: the sessions are already known (the
//! `Vec<Session>` [`try_pause_playing`] returned), and its thread still
//! calls `TryPlayAsync` on each of them. So resume runs on a second,
//! cap-bounded thread, not the one that did the pausing.

// `media` holds two unrelated things that both concern audio the app did
// not record: the playback this module pauses, and the files
// `probe` reads and `decode` converts before an import uploads them. They
// share no code with the pause/resume half — the submodules are here because
// `media::probe` and `media::decode` are where a reader looks for them, not
// because they need anything above.
pub mod decode;
pub mod probe;

use crossbeam_channel::{bounded, Receiver, Sender};
use std::time::Duration;
use windows::Media::Control::GlobalSystemMediaTransportControlsSession as Session;
use windows::Media::Control::GlobalSystemMediaTransportControlsSessionPlaybackStatus as PlaybackStatus;
use windows::Media::Control::GlobalSystemMediaTransportControlsSessionManager as SessionManager;

/// The `Playing` member of `GlobalSystemMediaTransportControlsSessionPlaybackStatus`,
/// as the raw value [`is_playing`] compares against.
const PLAYING: i32 = PlaybackStatus::Playing.0;

/// How long [`resume`] waits for the paused sessions to confirm they were
/// told to play. It bounds the waiting only: the play requests go out on
/// their own threads and carry on after the wait gives up.
///
/// Measured on the development machine with a muted `MediaPlayer` in another
/// process: 20 pause/play round trips took 0.9-26 ms, median 1.2 ms. A
/// browser tab has not been timed. Two seconds is about 75 times the slowest
/// round trip seen, and costs nothing when it is reached, because no
/// user-facing thread waits on it.
///
/// It bounds the whole batch, and one player that hangs cannot hold up the
/// others: each session gets its own thread, so every play request is sent
/// whatever state its neighbours are in.
const WINRT_CAP: Duration = Duration::from_secs(2);

/// Run `f` on its own thread and wait up to `cap` for it to finish. `None`
/// on timeout: the inner thread, if stuck on a misbehaving WinRT
/// call, is abandoned rather than joined — Rust has no way to cancel a
/// blocking foreign call, and a leaked thread is harmless next to leaving
/// the caller (already a fire-and-forget thread of its own) hung on it
/// forever.
fn with_cap<T: Send + 'static>(cap: Duration, f: impl FnOnce() -> T + Send + 'static) -> Option<T> {
    let (tx, rx) = bounded(1);
    std::thread::spawn(move || {
        let _ = tx.send(f());
    });
    rx.recv_timeout(cap).ok()
}

/// Whether a session's `PlaybackStatus` means it is playing right now — the
/// one and only reason to pause it. A free function so the verified enum
/// value is pinned by a test without a live GSMTC session manager.
fn is_playing(status: i32) -> bool {
    status == PLAYING
}

/// Keeps a pause alive. Dropping it resumes exactly the sessions that were
/// paused for it and nothing else.
///
/// There is no explicit resume call: "paused, and nobody left who will resume
/// it" is not a state this feature has any use for, and tying the resume to a
/// guard's lifetime is what makes it unrepresentable. It also avoids a race
/// a pause-reply message would have: a reply that lands after the recording
/// has stopped can be dropped by the controller's tail-flush drain, leaving
/// the user's music paused with no code path left to resume it.
pub struct PauseGuard {
    /// The pause thread parks on this channel's receiver; dropping the sender
    /// is the signal. Nothing is ever sent on it.
    _resume_on_drop: Sender<()>,
}

/// Pause every currently-playing GSMTC session, and keep them paused until
/// the returned guard is dropped.
pub fn pause_playing() -> PauseGuard {
    let (tx, rx) = bounded::<()>(0);
    std::thread::spawn(move || {
        // Not `with_cap`: giving up here would abandon a thread that keeps
        // pausing sessions this function has already stopped waiting for,
        // and nothing else holds a reference to resume them — see the
        // module doc. This call blocks for as long as the slowest session's
        // `TryPauseAsync` takes; it always finishes (or errors), and either
        // way this thread always reaches `wait_for_resume` next.
        let paused = match try_pause_playing() {
            Ok(sessions) => sessions,
            Err(e) => {
                tracing::debug!("GSMTC pause failed: {e}");
                return;
            }
        };
        if paused.is_empty() {
            return;
        }
        wait_for_resume(&rx);
        resume(paused);
    });
    PauseGuard {
        _resume_on_drop: tx,
    }
}

/// Block until the [`PauseGuard`] is dropped. Split out so the mechanism —
/// the *drop* is the signal, nothing is ever sent — is testable without a
/// live media session.
fn wait_for_resume(rx: &Receiver<()>) {
    // `recv` fails the moment the last sender is gone, which is the drop.
    let _ = rx.recv();
}

fn try_pause_playing() -> windows::core::Result<Vec<Session>> {
    let manager = SessionManager::RequestAsync()?.join()?;
    let sessions = manager.GetSessions()?;

    let mut paused = Vec::new();
    for session in &sessions {
        let Ok(status) = session.GetPlaybackInfo().and_then(|i| i.PlaybackStatus()) else {
            continue;
        };
        if !is_playing(status.0) {
            continue;
        }
        match session.TryPauseAsync().and_then(|op| op.join()) {
            Ok(true) => paused.push(session),
            Ok(false) => {} // the player refused the pause request
            Err(e) => tracing::debug!("GSMTC pause failed for one session: {e}"),
        }
    }
    Ok(paused)
}

/// Tell every session in `paused` to play again, each from a thread of its
/// own, and wait up to [`WINRT_CAP`] for them all to answer.
///
/// Called from the pause thread once the guard is gone, so nothing the user
/// is waiting on blocks here. Giving up on the wait loses nothing: the
/// threads keep going and every request is still delivered.
fn resume(paused: Vec<Session>) {
    let total = paused.len();
    let answered = with_cap(WINRT_CAP, move || {
        let calls: Vec<_> = paused
            .into_iter()
            .map(|session| {
                std::thread::spawn(move || session.TryPlayAsync().and_then(|op| op.join()))
            })
            .collect();
        let (mut resumed, mut declined, mut failed) = (0usize, 0usize, 0usize);
        for call in calls {
            match call.join() {
                Ok(Ok(true)) => resumed += 1,
                Ok(Ok(false)) => declined += 1,
                Ok(Err(e)) => {
                    failed += 1;
                    tracing::debug!("GSMTC play request failed for one session: {e}");
                }
                Err(_) => {
                    failed += 1;
                    tracing::debug!("a GSMTC play thread panicked");
                }
            }
        }
        (resumed, declined, failed)
    });
    match answered {
        Some((resumed, declined, failed)) => {
            tracing::debug!(resumed, declined, failed, total, "GSMTC resume answered")
        }
        None => tracing::debug!(total, "GSMTC resume still waiting at the cap; left running"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The verified value: GSMTC reports 4 for Playing. Nothing else may be
    /// touched — pausing an already-paused session would be harmless on its
    /// own, but *recording* it as one this module paused is not, since it
    /// would then be played on resume.
    #[test]
    fn only_the_playing_status_selects_a_session_for_pause() {
        assert!(is_playing(PLAYING));
        // Closed, Opened, Changing, Stopped, Paused.
        for status in [0, 1, 2, 3, 5] {
            assert!(!is_playing(status), "status {status} must not be paused");
        }
    }

    /// The guard's whole contract: the pause thread is released by the guard
    /// being *dropped*, not by any message. Nothing sends on this channel, so
    /// a thread that waited for a value instead would park forever and the
    /// user's music would never come back.
    #[test]
    fn dropping_the_guard_releases_the_pause_thread() {
        let (tx, rx) = bounded::<()>(0);
        let parked = std::thread::spawn(move || wait_for_resume(&rx));
        drop(tx);
        parked
            .join()
            .expect("the pause thread must wake when the guard is dropped");
    }

    #[test]
    fn with_cap_returns_the_value_when_the_work_finishes_in_time() {
        assert_eq!(with_cap(Duration::from_millis(500), || 42), Some(42));
    }

    /// The whole point of `WINRT_CAP`: a stuck call must not hang the caller
    /// forever. The inner thread here is deliberately abandoned mid-sleep —
    /// that's the documented tradeoff, not a leak this test needs to clean
    /// up.
    #[test]
    fn with_cap_gives_up_after_the_cap_even_if_the_work_never_returns() {
        let result = with_cap(Duration::from_millis(20), || {
            std::thread::sleep(Duration::from_secs(3600));
        });
        assert_eq!(result, None);
    }

    /// Why `pause_playing` calls `try_pause_playing` directly instead of
    /// through `with_cap` (unlike `resume`): `with_cap` can give up on work
    /// that finishes moments later anyway, silently dropping its result. For
    /// `resume` that's harmless — the abandoned thread still fires
    /// `TryPlayAsync` on every session, nothing was tracking the result. For
    /// `pause`, the result *is* the list of sessions to resume; losing it
    /// here is exactly the "paused, and nobody left to resume it" bug this
    /// test pins against regressing.
    #[test]
    fn with_cap_can_drop_a_result_that_finishes_just_after_the_cap() {
        let (done_tx, done_rx) = bounded::<u32>(1);
        let result = with_cap(Duration::from_millis(20), move || {
            std::thread::sleep(Duration::from_millis(80));
            let _ = done_tx.send(7);
            7
        });
        assert_eq!(result, None, "with_cap gave up before the work finished");
        // The work itself still completed and produced a real result — the
        // equivalent of a paused-sessions Vec that `with_cap` would have
        // thrown away instead of handing to `resume`.
        assert_eq!(
            done_rx.recv_timeout(Duration::from_millis(500)),
            Ok(7),
            "the abandoned thread must still produce its result, even though with_cap discarded it"
        );
    }
}
