//! Per-stage latency capture.
//!
//! Latency is judged at p90/p99, not p50, and measured from *end of speech*:
//! `sarvam::ws`'s `drain_start` is the controller's own end-of-speech instant
//! (`Controller::finish_recording` stamps it *before* the up-to-250ms
//! `collect_tail` wait, and carries it in on `CloudCmd::Finish`), not the
//! instant the dispatcher task happens to receive that command.
//!
//! `sarvam::ws` records one `StageTimings` per dictation and logs it as a
//! single structured line carrying [`TARGET`] in its `timing_target` field
//! (durations and counts only — never transcript text, per
//! `sarvam::codec::parse_server`'s logging policy) so an aggregator can
//! filter on that field instead of regex-scraping message text or relying
//! on the tracing metadata target, which stays at its default (the module
//! path) specifically so a crate-scoped `RUST_LOG` still captures this line.
//! Every dictation the user finished speaking through (`Finish` sent, so
//! `drain_start` was set) logs exactly one such line, at `info`: a sample
//! (`skipped = false`, full `StageTimings`, plus `errored` — true when the
//! drain ended via a mid-session failure rather than cleanly, so a
//! salvaged-but-tainted sample reads differently from a healthy one) or a
//! skip (`skipped = true`, a short `reason`, no duration fields — logged
//! from the three points in `sarvam::ws` where a drain that was already
//! underway ends without producing a sample: a pre-`Finish` failure that a
//! `Finish` later catches up to, a `Cancel`/shutdown after `Finish` — most
//! notably `ControlMsg::FinalizeTimeout`'s watchdog aborting a session
//! that's taking too long — or a new dictation superseding one still
//! draining). `percentiles` turns a run of the `skipped = false`
//! samples into the p50/p90/p99 shape above; [`TimingWindow`] feeds it the
//! last [`SUMMARY_WINDOW`] of them every [`SUMMARY_EVERY`] dictations, so
//! real use reports its own latency shape without any offline tooling.

use std::time::Duration;

/// Discriminator for the one-line-per-dictation timing log emitted by
/// `sarvam::ws::log_timing_sample`/`log_timing_skipped`, carried as that
/// line's `timing_target` field. An aggregator should filter on that field
/// rather than regex-scraping the shared rolling log by message text — the
/// latter breaks silently the moment a field is reordered.
///
/// The same target rides on the rolling summary line, which
/// `sarvam::ws::log_timing_sample` emits every [`SUMMARY_EVERY`] dictations
/// (`summary = true`, the p50/p90/p99 of the last [`SUMMARY_WINDOW`]
/// samples), so filtering on `timing_target` alone selects three line
/// shapes, not one. An aggregator separates them by two fields: skip lines
/// carry `skipped = true`, summary lines carry `summary = true`, and sample
/// lines carry neither.
///
/// Deliberately a plain field, not the tracing event's own metadata target
/// (i.e. never passed as `tracing::info!(target: TARGET, ...)`): that would
/// replace the event's metadata target — otherwise the default module path,
/// same as every other log line in `sarvam::ws` — with this bare string,
/// which falls outside any crate-scoped `RUST_LOG` directive (`EnvFilter`
/// matches metadata targets by prefix). A directive such as
/// `butterfly_speak_lib=info`, plausible for someone collecting exactly
/// this data, would then silently admit zero timing samples.
pub const TARGET: &str = "dictation_timing";

#[derive(Clone, Copy, Debug, Default)]
pub struct StageTimings {
    /// Wall-clock time from end of speech to the drain loop exiting. Not
    /// Sarvam's contribution alone: up to `TAIL_FLUSH_TIMEOUT_MS` (250 ms) of
    /// this span is our own `collect_tail` wait, entirely client-side, before
    /// `Finish` is even sent. This field sums time with two different owners
    /// and does not separate them:
    ///   - ours: the `collect_tail` wait, and the connect/handshake wait on
    ///     early release (both described below);
    ///   - Sarvam's: the network/processing time between the finish frame
    ///     reaching the wire and the finals this dispatcher was waiting on
    ///     coming back.
    ///
    /// A report that attributes the whole of `drain_ms` to Sarvam overstates
    /// their share by up to 250 ms on every sample; splitting the two out
    /// would need a second timestamp (when the finish frame actually hits
    /// the wire) that this struct doesn't carry today.
    ///
    /// The origin is the instant `Controller::finish_recording` stamps
    /// *before* it blocks on `collect_tail`, carried in on `CloudCmd::Finish`
    /// and used as-is rather than the dispatcher's own receipt time; the end
    /// is the drain loop exiting: the last final/close/deadline that ends the
    /// wait for more transcript pieces. Sampled there directly, before the
    /// best-effort goodbye (`end`) frame is sent, so a stalled write on a
    /// slow session is never charged here, and before the drained
    /// finals/partial are assembled into `raw`, which runs later and is not
    /// what stops this clock. Includes the connect/handshake wait when the
    /// hotkey was released before `session.begin` arrived, and the
    /// controller's own `collect_tail` wait before `Finish` was even sent —
    /// both are real drain time from the user's perspective.
    pub drain_ms: u64,
    /// Time spent in the cloud polish call (`chat::polish`) plus the
    /// guardrail check immediately after it. A `0` here is always a real
    /// measurement — formatting genuinely didn't run (empty drain, or the
    /// user's cleanup level is `Off`) — never a missing one.
    pub format_ms: u64,
    /// One directly-measured wall-clock span from the same end-of-speech
    /// origin as `drain_ms` to immediately before the log line is emitted —
    /// deliberately *not* `drain_ms + format_ms`, a sum that would silently
    /// drop every millisecond between the two stages. The gap it captures
    /// includes the goodbye write, the cleanup-settings lock read and clone,
    /// `raw`'s clone, the rule-based cleanup pipeline
    /// (`cleanup::run_cloud_pipeline` — `cleanup::snippets` compiles a fresh
    /// `Regex` per replacement and per snippet on every dictation, uncached,
    /// so this grows with the user's rule count), and `words_changed`'s
    /// bounded LCS diff.
    ///
    /// Still excludes everything from the log line onward: the send to the
    /// controller, per-app style application, and text injection into the
    /// focused app all happen after this is recorded and are not measured
    /// by this struct — true at both of `log_timing_sample`'s call sites,
    /// which both log before they send.
    pub total_ms: u64,
    /// Milliseconds from the polish request being sent to its first content
    /// token, when the streaming (Sarvam) path measured it. `None` when
    /// formatting did not run, failed before a token arrived, or went
    /// through the non-streaming custom backend. Logged as `ttft_ms=0` in
    /// those cases — the same "0 means didn't happen" convention
    /// `format_ms` uses.
    pub ttft_ms: Option<u64>,
    /// Chunks polished in the background before the key was released;
    /// `format_ms` then covers only the tail call plus its guard — the
    /// critical path.
    pub segments: u32,
    /// Milliseconds spent at finish waiting for the background chunk worker
    /// to drain its queue, before the tail call could start.
    ///
    /// The one part of the background work that is *not* free: a chunk still
    /// in flight when the key goes up delays the tail, and `format_ms` — the
    /// tail call plus its guard — does not cover it. `0` whenever there was
    /// nothing to wait for (the single-call path, or every chunk already
    /// back), which is a real measurement, not a missing one.
    pub chunk_wait_ms: u64,
}

impl StageTimings {
    /// Builds the reported triple from independently-timed stage `Duration`s
    /// plus one directly-measured `total` span. One constructor means a
    /// change to what `total` means (see its doc comment above) has exactly
    /// one call site to update.
    ///
    /// Rounds each duration to the nearest millisecond rather than
    /// truncating: `.as_millis() as u64` truncates toward zero, a
    /// one-directional downward bias applied at every stage that has no
    /// place in a latency report.
    pub fn new(drain: Duration, format: Duration, total: Duration) -> Self {
        Self {
            drain_ms: round_ms(drain),
            format_ms: round_ms(format),
            total_ms: round_ms(total),
            ttft_ms: None,
            segments: 0,
            chunk_wait_ms: 0,
        }
    }

    /// Attaches the first-token time, rounded like the other stages.
    pub fn with_ttft(mut self, ttft: Option<Duration>) -> Self {
        self.ttft_ms = ttft.map(round_ms);
        self
    }

    /// Attaches the wait for the background chunk worker to drain, rounded
    /// like the other stages. `Duration::ZERO` when there was no wait.
    pub fn with_chunk_wait(mut self, wait: Duration) -> Self {
        self.chunk_wait_ms = round_ms(wait);
        self
    }

    /// Attaches the number of background chunks this dictation polished
    /// before the key was released. Zero is the single-call path.
    pub fn with_segments(mut self, n: u32) -> Self {
        self.segments = n;
        self
    }
}

/// Rounds to the nearest millisecond, ties away from zero, instead of the
/// truncate-toward-zero of `Duration::as_millis`. See `StageTimings::new`.
fn round_ms(d: Duration) -> u64 {
    ((d.as_micros() + 500) / 1000) as u64
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Percentiles {
    pub p50: u64,
    pub p90: u64,
    pub p99: u64,
    pub n: usize,
}

/// Nearest-rank percentiles. Sorts `samples` in place (the caller's copy is
/// consumed as scratch space — pass a clone if the original order matters).
///
/// Used by [`TimingWindow`] for the rolling summary the dispatcher logs.
pub fn percentiles(samples: &mut [u64]) -> Percentiles {
    if samples.is_empty() {
        return Percentiles::default();
    }
    samples.sort_unstable();
    let pick = |q: f64| {
        let rank = (q * samples.len() as f64).ceil().max(1.0) as usize;
        samples[rank.min(samples.len()) - 1]
    };
    Percentiles {
        p50: pick(0.50),
        p90: pick(0.90),
        p99: pick(0.99),
        n: samples.len(),
    }
}

/// Emit a summary line every this many dictations.
pub const SUMMARY_EVERY: usize = 10;
/// Summaries cover at most this many most-recent dictations.
pub const SUMMARY_WINDOW: usize = 50;

/// The percentiles of the last [`SUMMARY_WINDOW`] dictations.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TimingSummary {
    pub n: usize,
    pub drain: Percentiles,
    pub format: Percentiles,
    pub total: Percentiles,
}

/// A rolling window of dictation timings that yields a [`TimingSummary`]
/// every [`SUMMARY_EVERY`] pushes. Owned by the cloud dispatcher, one per
/// app lifetime. Every sample counts, errored ones included: a dictation
/// that failed after 5 s still took the user 5 s.
#[derive(Default)]
pub struct TimingWindow {
    drain: std::collections::VecDeque<u64>,
    format: std::collections::VecDeque<u64>,
    total: std::collections::VecDeque<u64>,
    pushed: usize,
}

impl TimingWindow {
    pub fn push(&mut self, t: &StageTimings) -> Option<TimingSummary> {
        fn push_capped(q: &mut std::collections::VecDeque<u64>, v: u64) {
            if q.len() == SUMMARY_WINDOW {
                q.pop_front();
            }
            q.push_back(v);
        }
        push_capped(&mut self.drain, t.drain_ms);
        push_capped(&mut self.format, t.format_ms);
        push_capped(&mut self.total, t.total_ms);
        self.pushed += 1;
        if self.pushed % SUMMARY_EVERY != 0 {
            return None;
        }
        let pct = |q: &std::collections::VecDeque<u64>| {
            let mut v: Vec<u64> = q.iter().copied().collect();
            percentiles(&mut v)
        };
        Some(TimingSummary {
            n: self.drain.len(),
            drain: pct(&self.drain),
            format: pct(&self.format),
            total: pct(&self.total),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(drain: u64, format: u64, total: u64) -> StageTimings {
        StageTimings {
            drain_ms: drain,
            format_ms: format,
            total_ms: total,
            ttft_ms: None,
            segments: 0,
            chunk_wait_ms: 0,
        }
    }

    #[test]
    fn percentiles_use_nearest_rank() {
        let mut s: Vec<u64> = (1..=100).collect();
        let p = percentiles(&mut s);
        assert_eq!(p.n, 100);
        assert_eq!(p.p50, 50);
        assert_eq!(p.p90, 90);
        assert_eq!(p.p99, 99);
    }

    #[test]
    fn a_single_sample_is_every_percentile() {
        let p = percentiles(&mut [420]);
        assert_eq!((p.p50, p.p90, p.p99, p.n), (420, 420, 420, 1));
    }

    #[test]
    fn no_samples_is_all_zero() {
        let p = percentiles(&mut []);
        assert_eq!((p.p50, p.p90, p.p99, p.n), (0, 0, 0, 0));
    }

    #[test]
    fn unsorted_input_is_handled() {
        let p = percentiles(&mut [900, 100, 500]);
        assert_eq!(p.p50, 500);
    }

    /// Truncation is a one-directional downward bias. 12.9 ms must report
    /// as 13, not 12.
    #[test]
    fn round_ms_does_not_truncate_toward_zero() {
        assert_eq!(round_ms(Duration::from_micros(12_900)), 13);
    }

    #[test]
    fn round_ms_rounds_half_up_at_the_midpoint() {
        assert_eq!(round_ms(Duration::from_micros(499)), 0);
        assert_eq!(round_ms(Duration::from_micros(500)), 1);
        assert_eq!(round_ms(Duration::from_micros(1_499)), 1);
        assert_eq!(round_ms(Duration::from_micros(1_500)), 2);
    }

    #[test]
    fn round_ms_of_zero_is_zero() {
        assert_eq!(round_ms(Duration::ZERO), 0);
    }

    /// `total_ms` must reflect the directly-measured span, not silently fall
    /// back to re-deriving the sum of the other two fields.
    #[test]
    fn stage_timings_total_is_not_the_sum_of_the_parts() {
        let t = StageTimings::new(
            Duration::from_millis(100),
            Duration::from_millis(50),
            Duration::from_millis(200), // a real 50ms gap a sum would hide
        );
        assert_eq!(t.drain_ms, 100);
        assert_eq!(t.format_ms, 50);
        assert_eq!(t.total_ms, 200);
        assert_ne!(t.total_ms, t.drain_ms + t.format_ms);
    }

    #[test]
    fn segments_default_to_zero_and_are_settable() {
        let t = StageTimings::new(
            Duration::from_millis(1),
            Duration::from_millis(2),
            Duration::from_millis(3),
        );
        assert_eq!(t.segments, 0);
        assert_eq!(t.with_segments(4).segments, 4);
    }

    /// The tail cannot start until the last background chunk comes back, so
    /// the wait for the in-flight chunk is a real part of the critical path
    /// that `format_ms` (the tail call plus its guard) does not cover.
    #[test]
    fn the_chunk_wait_defaults_to_zero_and_is_rounded_like_the_other_stages() {
        let t = StageTimings::new(
            Duration::from_millis(1),
            Duration::from_millis(2),
            Duration::from_millis(3),
        );
        assert_eq!(t.chunk_wait_ms, 0);
        assert_eq!(t.with_chunk_wait(Duration::from_micros(12_900)).chunk_wait_ms, 13);
    }

    #[test]
    fn ttft_is_optional_and_rounded_like_the_other_stages() {
        let t = StageTimings::new(
            Duration::from_millis(100),
            Duration::from_millis(200),
            Duration::from_millis(350),
        );
        assert_eq!(t.ttft_ms, None);
        let t = t.with_ttft(Some(Duration::from_micros(180_500)));
        assert_eq!(t.ttft_ms, Some(181));
    }

    /// A summary every ten dictations, over at most the last fifty — the
    /// p50/p90/p99 shape, from real use, no tooling.
    #[test]
    fn a_summary_is_emitted_every_tenth_sample_over_the_window() {
        let mut w = TimingWindow::default();
        for i in 1..=9 {
            assert!(w.push(&sample(i, i, i)).is_none(), "sample {i}");
        }
        let s = w.push(&sample(10, 10, 10)).expect("tenth sample summarises");
        assert_eq!(s.n, 10);
        assert_eq!((s.drain.p50, s.drain.p90, s.drain.p99), (5, 9, 10));
        assert_eq!(s.total.p50, 5);
        for i in 11..=19 {
            assert!(w.push(&sample(i, i, i)).is_none());
        }
        assert_eq!(w.push(&sample(20, 20, 20)).unwrap().n, 20);
    }

    #[test]
    fn the_window_keeps_only_the_last_fifty_samples() {
        let mut w = TimingWindow::default();
        let mut last = None;
        for i in 1..=60 {
            if let Some(s) = w.push(&sample(i, i, i)) {
                last = Some(s);
            }
        }
        let s = last.unwrap();
        assert_eq!(s.n, SUMMARY_WINDOW);
        // Samples 1..=10 fell out; the window is 11..=60.
        assert_eq!(s.drain.p50, 35);
        assert_eq!(s.drain.p99, 60);
    }
}
