//! Stopping an import job wherever it waits on the network.
//!
//! The import queue sends one file at a time to Sarvam's batch API, and that
//! job can spend minutes in an upload, in a status poll that sleeps between
//! requests, or in a download. Cancel and Clear have to stop it there, not
//! when the request in hand comes back. So the queue gives each
//! file a fresh request id, files the job here under that id before anything
//! goes out, and hands the job the [`CancelToken`] it gets back; every wait on
//! the import path races that token.
//!
//! The press can land before the job is filed, while it runs, or after it is
//! over, and the registry is built so that the timing never matters:
//! - An id names one job. [`CancelRegistry::cancel`] takes the job out and
//!   fires its token, and an id with nothing filed under it stops nothing.
//!   The queue's run number covers the moment before a job is filed.
//! - A [`Registration`] keeps its job filed for as long as it lives and lets
//!   go when dropped, so a return, a `?`, a panic or a dropped future can
//!   never leave a finished job behind.
//! - A guard only lets go of the job it joined. After a cancel has taken that
//!   job out, work filed under the same id is a new job with a new token, and
//!   the old guard leaves it alone.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};
use tokio::sync::watch;

/// A one-way "stop what you are doing" flag, cloneable and awaitable.
///
/// `tokio::sync::watch` rather than a bare `AtomicBool`: a polled flag can only
/// be noticed at the next checkpoint, and the longest wait on the import path
/// (`batch_job`'s status poll, up to ten seconds between requests) would hold
/// the user's Cancel button hostage for exactly that gap. A watch channel gives
/// both readings — [`Self::is_cancelled`] for a checkpoint and
/// [`Self::cancelled`] for a `select!` arm that unblocks the moment the flag
/// flips.
#[derive(Clone, Debug)]
pub struct CancelToken {
    /// Held as an `Arc` so every clone shares one flag *and* so the sender
    /// outlives every receiver — `changed()` can then never fail, which is
    /// what lets [`Self::cancelled`] be a plain loop.
    tx: Arc<watch::Sender<bool>>,
}

impl Default for CancelToken {
    fn default() -> Self {
        Self::new()
    }
}

impl CancelToken {
    pub fn new() -> Self {
        let (tx, _rx) = watch::channel(false);
        CancelToken { tx: Arc::new(tx) }
    }

    /// Fire the token. Firing it again changes nothing: the flag only ever
    /// goes from live to fired.
    ///
    /// `send_replace`, never `send`: `watch::Sender::send` **fails when there
    /// are no receivers**, and the common case here is exactly that — a token
    /// is handed out before anything awaits it, so a cancel arriving between
    /// filing and the first `cancelled()` would have been silently dropped.
    /// `send_replace` updates the value regardless and wakes whoever
    /// subscribes later. Pinned by
    /// [`tests::token::a_token_fired_before_anyone_waits_still_wakes_the_waiter`].
    pub fn cancel(&self) {
        self.tx.send_replace(true);
    }

    pub fn is_cancelled(&self) -> bool {
        *self.tx.borrow()
    }

    /// Resolves once the token has fired, immediately if it already has.
    ///
    /// Written as "check the current value, then wait for a change" rather
    /// than `changed().await` alone: a `watch` receiver's `changed()` only
    /// reports values sent *after* it subscribed, so a token cancelled before
    /// this is first awaited would otherwise never resolve — the exact case
    /// that happens when a cancel lands between an item's checkpoint and its
    /// network call.
    pub async fn cancelled(&self) {
        let mut rx = self.tx.subscribe();
        if *rx.borrow_and_update() {
            return;
        }
        while rx.changed().await.is_ok() {
            if *rx.borrow() {
                return;
            }
        }
        // Unreachable while `self` is alive, since `self` owns the sender.
        // Parking rather than returning matters: returning would read as
        // "cancelled" to every `select!` arm that awaits this.
        std::future::pending().await
    }

    /// Whether `other` is a clone of this token rather than a separate one.
    fn same_flag(&self, other: &CancelToken) -> bool {
        Arc::ptr_eq(&self.tx, &other.tx)
    }
}

/// One job filed under a request id: the token that stops it, and how many
/// live guards hold it.
struct Job {
    token: CancelToken,
    holders: usize,
}

/// The import jobs on the network, by request id.
#[derive(Default)]
pub struct CancelRegistry {
    jobs: Mutex<HashMap<String, Job>>,
}

impl CancelRegistry {
    /// File work under `request_id` until the returned guard drops.
    ///
    /// When a job is already filed under the id, the work joins it and gets
    /// the same token, so one cancel stops both; otherwise a new job starts
    /// with a token of its own.
    pub fn register(&self, request_id: &str) -> Registration<'_> {
        let mut jobs = self.lock();
        let job = jobs.entry(request_id.to_string()).or_insert_with(|| Job {
            token: CancelToken::new(),
            holders: 0,
        });
        job.holders += 1;
        Registration {
            registry: self,
            request_id: request_id.to_string(),
            token: job.token.clone(),
        }
    }

    /// Stop the job filed under `request_id`. The count is how many guards
    /// were holding it, so 0 means there was nothing to stop.
    pub fn cancel(&self, request_id: &str) -> usize {
        let Some(job) = self.lock().remove(request_id) else {
            return 0;
        };
        job.token.cancel();
        job.holders
    }

    /// One guard on the job that holds `token` is gone. The job leaves when
    /// its last guard does; a job filed after a cancel has another token and
    /// is not this guard's to touch.
    fn release(&self, request_id: &str, token: &CancelToken) {
        let mut jobs = self.lock();
        let Some(job) = jobs.get_mut(request_id) else {
            return;
        };
        if !job.token.same_flag(token) {
            return;
        }
        job.holders -= 1;
        if job.holders == 0 {
            jobs.remove(request_id);
        }
    }

    /// Every change under this lock is one whole step, so a lock poisoned by
    /// a panic elsewhere still guards a consistent map; it is taken back
    /// rather than turned into a second panic inside a cancel or a drop.
    fn lock(&self) -> MutexGuard<'_, HashMap<String, Job>> {
        self.jobs.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// How many ids have a job filed under them.
    #[cfg(test)]
    fn filed(&self) -> usize {
        self.lock().len()
    }
}

/// A piece of work's hold on its job. Dropping it lets go.
pub struct Registration<'a> {
    registry: &'a CancelRegistry,
    request_id: String,
    token: CancelToken,
}

impl Registration<'_> {
    /// The token a cancel of this work's request id fires.
    pub fn token(&self) -> &CancelToken {
        &self.token
    }
}

impl Drop for Registration<'_> {
    fn drop(&mut self) {
        self.registry.release(&self.request_id, &self.token);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// Request ids in the shape the import queue mints them (`Uuid::new_v4`).
    const FILE_A: &str = "3f2c9a4e-6b1d-4e8a-9c27-5d0b8f1e6a93";
    const FILE_B: &str = "b81e04d7-2c5f-4a39-8e6b-71f0c9d2a4e5";

    // -- The token on its own ------------------------------------------------

    mod token {
        use super::*;

        #[test]
        fn clones_share_one_flag_and_firing_twice_is_harmless() {
            let token = CancelToken::new();
            let clone = token.clone();
            assert!(!clone.is_cancelled());
            token.cancel();
            token.cancel();
            assert!(clone.is_cancelled());
            assert!(token.same_flag(&clone));
            assert!(!token.same_flag(&CancelToken::new()));
        }

        /// The cancel lands between a checkpoint and the network call, so
        /// nothing is subscribed yet when the flag flips.
        #[tokio::test]
        async fn a_token_fired_before_anyone_waits_still_wakes_the_waiter() {
            let token = CancelToken::new();
            token.cancel();
            tokio::time::timeout(Duration::from_secs(5), token.cancelled())
                .await
                .expect("a fired token must resolve at once");
        }

        #[tokio::test]
        async fn a_waiter_wakes_when_the_token_fires() {
            let token = CancelToken::new();
            let waiter = token.clone();
            let task = tokio::spawn(async move { waiter.cancelled().await });
            token.cancel();
            tokio::time::timeout(Duration::from_secs(5), task)
                .await
                .expect("the waiter must wake when the flag flips")
                .expect("the waiter task must not panic");
        }

        /// Every `select!` arm on the import path awaits this; a live token
        /// that resolved would read as a cancel the moment it was polled.
        #[tokio::test]
        async fn a_live_token_keeps_its_waiter_waiting() {
            let token = CancelToken::new();
            let waited = tokio::time::timeout(Duration::from_millis(50), token.cancelled()).await;
            assert!(waited.is_err(), "a live token must keep waiting");
        }
    }

    // -- When the press lands in one job's life ------------------------------

    mod press_timing {
        use super::*;

        /// Nothing is filed yet: the press stops nothing and leaves nothing,
        /// and the job filed afterwards starts live.
        #[test]
        fn a_press_before_the_job_is_filed_stops_nothing() {
            let registry = CancelRegistry::default();
            assert_eq!(registry.cancel(FILE_A), 0);
            assert_eq!(registry.filed(), 0);

            let job = registry.register(FILE_A);
            assert!(!job.token().is_cancelled());
        }

        #[test]
        fn a_press_while_the_job_runs_stops_it() {
            let registry = CancelRegistry::default();
            let job = registry.register(FILE_A);
            assert_eq!(registry.cancel(FILE_A), 1);
            assert!(job.token().is_cancelled());
            assert_eq!(registry.filed(), 0, "a stopped job is no longer filed");
        }

        #[test]
        fn a_press_after_the_job_let_go_finds_nothing() {
            let registry = CancelRegistry::default();
            let job = registry.register(FILE_A);
            assert_eq!(registry.filed(), 1);
            drop(job);
            assert_eq!(registry.filed(), 0);
            assert_eq!(registry.cancel(FILE_A), 0);
        }

        #[test]
        fn a_second_press_on_the_same_job_finds_nothing() {
            let registry = CancelRegistry::default();
            let _job = registry.register(FILE_A);
            assert_eq!(registry.cancel(FILE_A), 1);
            assert_eq!(registry.cancel(FILE_A), 0);
        }

        /// The guard of a stopped job can still be alive when new work is
        /// filed under the same id. Dropping it must not unfile the new job.
        #[test]
        fn a_guard_outliving_its_press_leaves_the_next_job_alone() {
            let registry = CancelRegistry::default();
            let stopped = registry.register(FILE_A);
            assert_eq!(registry.cancel(FILE_A), 1);

            let next = registry.register(FILE_A);
            assert!(!next.token().is_cancelled(), "the new job starts live");
            drop(stopped);

            assert_eq!(registry.filed(), 1);
            assert_eq!(registry.cancel(FILE_A), 1, "the new job must still be reachable");
            assert!(next.token().is_cancelled());
        }
    }

    // -- Which work a press reaches ------------------------------------------

    mod reach {
        use super::*;

        #[test]
        fn a_press_stops_only_the_file_it_names() {
            let registry = CancelRegistry::default();
            let a = registry.register(FILE_A);
            let b = registry.register(FILE_B);
            assert_eq!(registry.cancel(FILE_A), 1);
            assert!(a.token().is_cancelled());
            assert!(!b.token().is_cancelled());
            assert_eq!(registry.filed(), 1);
        }

        /// Two pieces of work filed under one id are one job: they share a
        /// token, and the count says how many were holding it.
        #[test]
        fn work_filed_under_one_id_shares_the_job() {
            let registry = CancelRegistry::default();
            let upload = registry.register(FILE_A);
            let poll = registry.register(FILE_A);
            assert!(upload.token().same_flag(poll.token()));
            assert_eq!(registry.cancel(FILE_A), 2);
            assert!(upload.token().is_cancelled());
            assert!(poll.token().is_cancelled());
        }

        #[test]
        fn a_job_stays_filed_until_its_last_guard_lets_go() {
            let registry = CancelRegistry::default();
            let upload = registry.register(FILE_A);
            drop(registry.register(FILE_A));
            assert_eq!(registry.filed(), 1, "one guard still holds the job");
            assert_eq!(registry.cancel(FILE_A), 1);
            assert!(upload.token().is_cancelled());
            drop(upload);
            assert_eq!(registry.filed(), 0);
        }
    }
}
