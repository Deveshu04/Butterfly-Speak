//! The log file and how long it is kept.
//!
//! One file per day (UTC), `butterfly-speak.log.<date>`, under
//! [`crate::logs_dir`]. The lines carry timings, counts and error kinds,
//! never dictation text.
//!
//! A server's own words do reach them in a few places: `sarvam::ws` logs a
//! realtime error frame's message and a close frame's reason, and
//! `sarvam::key` logs the close frame of a key check. The app sends those
//! sockets audio and Dictionary hints, never transcript text, so these lines
//! hold none; a change that sends text on them must look at them again. Chat
//! replies do carry text, and stay out: `format::backend::HttpFailure` keeps
//! an error body out, and `format::backend::parse_reply` keeps a reply of the
//! wrong shape out, as the Sarvam translate and batch paths do.
//!
//! A serde error from anything a server sent is never logged with `{e}`:
//! serde_json's message quotes a string value of the wrong type, which can
//! be the transcript. `sarvam::codec::parse_server` (the realtime frames),
//! `sarvam::translate`, `sarvam::batch`, `sarvam::batch_job` (the batch job
//! API's replies), `asr::custom` (the custom transcription reply) and
//! `format::backend::parse_reply` log its category and position only.
//!
//! With the environment variable `BS_DEBUG_HOTKEYS` set, the keyboard hook
//! traces every key press in every app: modifiers by name, any other key
//! unnamed, and the bindings that fire by kind or index; while a shortcut is
//! being recorded in Settings, the keys recorded. A warning at start says it
//! is on. It is for developers, and off unless someone sets it.
//!
//! Logs written by Speak 0.1.0 (the same file names, so pruned below too)
//! can hold dictation text that a server repeated in an error message or a
//! misshapen reply, or that a misshapen realtime frame carried.
//!
//! Logs are still the user's, so they do not pile up: at start, a file older
//! than [`MAX_AGE`] is deleted, and while the app runs the appender keeps at
//! most [`FILES_KEPT`] files.

use std::path::Path;
use std::time::{Duration, SystemTime};

/// The file name before the date. Unchanged from the first release, so the
/// files older builds wrote are pruned too.
pub(crate) const PREFIX: &str = "butterfly-speak.log";

/// How many daily files the appender keeps while the app runs.
pub(crate) const FILES_KEPT: usize = 7;

/// A log file this old is deleted the next time the app starts.
pub(crate) const MAX_AGE: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// The daily appender, holding at most [`FILES_KEPT`] files.
pub(crate) fn appender(
    dir: &Path,
) -> Result<tracing_appender::rolling::RollingFileAppender, tracing_appender::rolling::InitError> {
    tracing_appender::rolling::RollingFileAppender::builder()
        .rotation(tracing_appender::rolling::Rotation::DAILY)
        .filename_prefix(PREFIX)
        .max_log_files(FILES_KEPT)
        .build(dir)
}

/// Delete this app's log files last written before `now - MAX_AGE`. Only
/// plain files whose name starts with [`PREFIX`] are touched; a folder or a
/// link is left alone. Returns how many were deleted.
pub(crate) fn prune_expired(dir: &Path, now: SystemTime) -> usize {
    let Some(cutoff) = now.checked_sub(MAX_AGE) else {
        return 0;
    };
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    let mut removed = 0;
    for entry in entries.flatten() {
        if !entry.file_name().to_string_lossy().starts_with(PREFIX) {
            continue;
        }
        // `DirEntry::metadata` does not follow a link, so a link is neither
        // a file here nor ever deleted.
        let Ok(meta) = entry.metadata() else { continue };
        if !meta.is_file() {
            continue;
        }
        let old = meta.modified().map(|m| m < cutoff).unwrap_or(false);
        if old && std::fs::remove_file(entry.path()).is_ok() {
            removed += 1;
        }
    }
    removed
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "bs-logs-test-{name}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_aged(dir: &Path, name: &str, age: Duration) {
        let path = dir.join(name);
        std::fs::write(&path, b"line\n").unwrap();
        let f = std::fs::File::options().write(true).open(&path).unwrap();
        f.set_modified(SystemTime::now() - age).unwrap();
    }

    const DAY: Duration = Duration::from_secs(24 * 60 * 60);

    /// A file past the week goes at start, whatever else is in the folder;
    /// a recent one, and anything that is not this app's log, stays.
    #[test]
    fn a_log_older_than_a_week_is_deleted_at_start() {
        let dir = temp_dir("age");
        write_aged(&dir, "butterfly-speak.log.2026-01-01", 30 * DAY);
        write_aged(&dir, "butterfly-speak.log.2026-01-02", 8 * DAY);
        write_aged(&dir, "butterfly-speak.log.2026-01-09", 2 * DAY);
        write_aged(&dir, "notes.txt", 30 * DAY);
        std::fs::create_dir(dir.join("butterfly-speak.log.folder")).unwrap();

        assert_eq!(prune_expired(&dir, SystemTime::now()), 2);
        let mut left: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        left.sort();
        assert_eq!(
            left,
            [
                "butterfly-speak.log.2026-01-09",
                "butterfly-speak.log.folder",
                "notes.txt"
            ]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// While the app runs, the appender itself keeps no more than
    /// [`FILES_KEPT`] daily files, today's included.
    #[test]
    fn the_appender_keeps_seven_daily_files() {
        let dir = temp_dir("count");
        for day in 1..=10 {
            write_aged(&dir, &format!("butterfly-speak.log.2026-01-{day:02}"), Duration::ZERO);
            // Creation time orders the files for the appender's own prune.
            std::thread::sleep(Duration::from_millis(15));
        }
        let appender = appender(&dir).expect("build the appender");
        drop(appender);
        let logs = std::fs::read_dir(&dir)
            .unwrap()
            .filter(|e| {
                e.as_ref()
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with(PREFIX)
            })
            .count();
        assert_eq!(logs, FILES_KEPT);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
