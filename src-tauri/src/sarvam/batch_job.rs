//! Sarvam's asynchronous Batch STT **job** API — the only path that can
//! transcribe a file longer than the 30 s the synchronous endpoint accepts
//! (`sarvam::batch::MAX_FALLBACK_DURATION_MS`), and therefore the engine
//! behind file import.
//!
//! Five calls, in order, against
//! `https://api.sarvam.ai/speech-to-text/job/v1`:
//!
//! | Step | Call |
//! |---|---|
//! | [`JobClient::create_job`] | `POST /` → a `job_id` |
//! | [`JobClient::upload`]     | `POST /upload-files` → a presigned URL, then a `PUT` of the bytes to it |
//! | [`JobClient::start`]      | `POST /{job_id}/start` |
//! | [`JobClient::poll`]       | `GET /{job_id}/status` |
//! | [`JobClient::download`]   | `POST /download-files` → a presigned URL, then a `GET` of the payload |
//!
//! # Verified against the live service
//!
//! Every shape below was confirmed by running real jobs against the real
//! service, because the published documentation does not describe the one
//! thing a parser needs: the schema *inside* a downloaded output file. The
//! download endpoint's reference page documents only the wrapper that hands
//! out presigned URLs. `testdata/batch_job_output.json` is a real job's
//! payload, field for field, with the text replaced by placeholders.
//!
//! What the live jobs showed that the docs do not:
//!
//! - **`timestamps` is three parallel arrays, not a list of segments.**
//!   `{"words": [...], "start_time_seconds": [...], "end_time_seconds": [...]}`.
//!   [`parse_output`] zips them; see its doc for what it does when they
//!   disagree in length, which is the failure mode this representation
//!   invites and an array-of-objects would not.
//! - **`words` does not hold words.** An 82 s file came back as five entries
//!   of 100–260 characters each. Sarvam's docs already say Batch timestamps
//!   are chunk-level, not word-level; the field name argues otherwise and the
//!   docs are the ones telling the truth.
//! - **Units are float seconds** from the start of the file, and chunks abut
//!   exactly (`end[i] == start[i+1]`).
//! - **`timestamps` is `null`, not `{}` or `[]`, when `with_timestamps` is
//!   false.**
//! - **`transcript` is the chunk texts joined by a single space**, so the
//!   full text never has to be reassembled from the segments.
//! - **Create answers `202`, not `200`.** Anything checking `== 200` breaks
//!   on the very first call.
//! - **`job_id` is not a bare UUID.** It is `YYYYMMDD_<uuid>` — a
//!   date-prefixed string, which is why it is percent-encoded into the path
//!   rather than pasted in.
//! - **The presigned upload PUT needs `x-ms-blob-type: BlockBlob`.** The
//!   storage backend reported itself as `Azure_V1` and Azure's Put Blob
//!   refuses without it.
//! - **`language_probability` is populated only when the server auto-detected
//!   the language** (`language_code: "unknown"` → `0.998`); it is `null`
//!   whenever a code was supplied.
//!
//! # Odia, settled
//!
//! `od-IN` and `or-IN` were each sent as `job_parameters.language_code` on
//! their own tiny job. `od-IN` was accepted (`202`) and the job completed
//! with an Odia transcript. `or-IN` was rejected at create with **HTTP 400
//! and an empty response body** — Sarvam's silent-400 signature, which
//! `format::backend` also handles. So the Batch job API is on the REST side
//! of the app's known split, and [`super::batch::to_rest_language_code`] is
//! exactly the translation this module needs.
//!
//! [`JobClient::create_job`] applies that translation itself rather than
//! trusting callers to. Leaving it to the call site made correct Odia a
//! convention, and the cost of one caller forgetting is a 400 with an empty
//! body — a failure that reads as a network problem rather than a spelling
//! one.
//!
//! # 429 is not fatal here
//!
//! Sarvam's 429 is an ordinary rate limit that clears in under a minute
//! (measured at roughly 100 requests/minute against the chat endpoint), so
//! waiting a little gets the request through. Treating it as fatal would turn
//! a two-second pause into a failed import, so every request here retries a
//! 429 with jittered backoff.
//!
//! # Logging
//!
//! Status codes and counts. Never a response body — the 2xx body on the
//! download leg **is** the user's transcript. Never a path. And never a
//! presigned URL: those carry a SAS token, which makes the URL itself a
//! bearer credential for the blob, so it is redacted even in the failure
//! branches. [`tests::no_log_field_can_carry_a_presigned_url_or_a_transcript`]
//! pins that.

use super::net_error::{classify_io_error, NetFailure};
use serde::Deserialize;
use std::path::Path;
use std::time::Duration;

pub const JOB_BASE_URL: &str = "https://api.sarvam.ai/speech-to-text/job/v1";

/// Matches [`super::batch::BATCH_MODEL`] and [`super::REALTIME_MODEL`]'s own
/// version. `saaras:v4` exists and adds Global English on top of v3's
/// 22-Indic set, but switching the import path alone would mean a file and a
/// dictation of the same audio could disagree — a difference the user would
/// read as a bug in one of them.
pub const JOB_MODEL: &str = "saaras:v3";

/// Sarvam accepts up to 20 files per job (stated consistently on every batch
/// page). The app submits one file per job regardless: a job is the unit that
/// fails, and batching four imports into one job means one bad file taking the
/// other three down with it. Unused in production **on purpose**: it records a
/// bound this codebase declines to spend. `import`'s
/// `every_file_gets_its_own_job_rather_than_sarvams_batch_of_twenty` reads it
/// and fails if it ever stops being greater than one.
#[allow(dead_code)]
pub const MAX_FILES_PER_JOB: usize = 20;

// ---------------------------------------------------------------- polling --

/// First gap between status polls. Short, because a live 3 s file went from
/// `start` to `Completed` inside the first 4 s poll — waiting longer would
/// add latency to the common case for nothing.
pub const POLL_INITIAL: Duration = Duration::from_secs(2);

/// Ceiling on the gap. A live 82 s file took 46 s; at a 10 s cap that costs
/// at most 10 s of staleness on a job of any length, which is noise next to
/// the job itself.
pub const POLL_MAX: Duration = Duration::from_secs(10);

/// Status polls in a row that may fail on the network or with a 5xx before
/// the wait gives up. One failed poll says nothing about a job that is
/// running, and already billed; at [`POLL_MAX`] apart, five in a row is most
/// of a minute of a status route that will not answer.
pub const POLL_FAILURES_TOLERATED: u32 = 5;

/// Fixed cost of a job irrespective of length: queueing, model warm-up, the
/// two presigned-URL round trips. A live 3 s file finished in ~4 s and an
/// 82 s file in ~46 s, so 2 minutes is generous rather than tight.
pub const DEADLINE_BASE: Duration = Duration::from_secs(120);

/// Wall-clock seconds allowed per second of audio. Sarvam transcribed 82 s
/// of audio in 46 s (0.56x) and 3 s in under 4 s; 2x is roughly a four-fold
/// margin on the measured rate.
pub const DEADLINE_PER_AUDIO_SECOND: f64 = 2.0;

/// Nothing waits longer than this, whatever the arithmetic says. Sarvam
/// publishes no turnaround SLA and its own status-endpoint doc warns a job
/// can exceed the SDK's 10-minute default under load, so this is not "the
/// job must be done by now" — it is "the app stops watching". The `job_id`
/// outlives the wait, so a caller that wants to resume later can.
pub const DEADLINE_CEILING: Duration = Duration::from_secs(30 * 60);

/// How long to keep polling, and how fast.
///
/// A value rather than three constants read inside the loop, so a test can
/// exercise the deadline in milliseconds without the loop knowing it is
/// being tested and without `wait_for_completion` growing a "for tests"
/// parameter that production also has to pass.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PollSchedule {
    pub first: Duration,
    pub max: Duration,
    pub deadline: Duration,
}

impl PollSchedule {
    /// The real schedule for a file of known length.
    ///
    /// An unknown length gets the ceiling, not the base. The base would be a
    /// two-minute deadline applied to a file that could be an hour long, and
    /// abandoning a job that is running fine — one the user has already been
    /// billed for — is a worse failure than waiting too long for one that is
    /// stuck. `media::probe` reports an unknown duration for exactly the
    /// containers that tend to be long recordings (a header-less VBR MP3, a
    /// WebM a recorder never went back to patch).
    pub fn for_audio(duration_s: Option<f64>) -> PollSchedule {
        let deadline = match duration_s {
            Some(s) if s.is_finite() && s > 0.0 => {
                let scaled = DEADLINE_BASE.as_secs_f64() + DEADLINE_PER_AUDIO_SECOND * s;
                let capped = scaled.min(DEADLINE_CEILING.as_secs_f64());
                Duration::from_secs_f64(capped)
            }
            _ => DEADLINE_CEILING,
        };
        PollSchedule {
            first: POLL_INITIAL,
            max: POLL_MAX,
            deadline,
        }
    }

    /// Doubling, capped. Saturating rather than `*2`, so a caller that hands
    /// in an absurd `first` cannot overflow the multiply.
    fn next_delay(&self, prev: Duration) -> Duration {
        prev.saturating_mul(2).min(self.max)
    }
}

// ------------------------------------------------------- request budgets --
//
// These are PER-ATTEMPT `RequestBuilder::timeout` budgets, not a client-wide
// `read_timeout`. The distinction is the whole point, and it is not the one
// reqwest's naming suggests.
//
// `ClientBuilder::read_timeout` sounds like a per-read idle timer. On the
// **response body** it is one — `reqwest-0.12.28/src/async_impl/body.rs`
// wraps the body in a `ReadTimeoutBody` that resets on every frame. On the
// request itself it is not: `execute_request` arms exactly one
// `Sleep(read_timeout)` when the request is constructed
// (`async_impl/client.rs:2637-2642`) and `PendingRequest::poll` fails the
// request the moment it fires, before the response head has arrived
// (`client.rs:3053-3059`). So for a request whose body IS the upload, it is a
// deadline on "finish sending and get a reply", with no credit for progress.
//
// So a 60 s `read_timeout` would be a hard ceiling on upload size rather than
// a stall detector: Azure answers 201 only after the last byte, so a ~57 MB
// file (a 60-minute 128 kbps MP3) on a 5 Mbit/s uplink takes ~95 s and would
// fail as `NetFailure::Timeout` — "Sarvam didn't answer in time", on a
// connection that is working perfectly. Anything past ~75 MB would fail even
// at 10 Mbit/s, putting a two-hour import (about 230 MB once converted to
// 16 kHz mono WAV) out of reach of any residential uplink.

/// Budget for the small JSON legs: create, upload-files, start, status and
/// download-files. All are a few hundred bytes each way, so 30 s means the
/// request is stalled rather than merely slow — the slowest seen live was
/// 1.4 s.
pub const REQUEST_TIMEOUT_JSON: Duration = Duration::from_secs(30);

/// Budget for the presigned GET of the finished transcript. Longer than the
/// JSON legs because the payload is a real (if small) document and the far
/// side is blob storage rather than Sarvam's own API.
pub const REQUEST_TIMEOUT_DOWNLOAD: Duration = Duration::from_secs(60);

/// Fixed part of the upload budget: connection setup, TLS, and Azure's commit
/// of the finished blob — none of which scale with the file.
pub const UPLOAD_TIMEOUT_BASE: Duration = Duration::from_secs(60);

/// The throughput floor the upload budget is computed against, in bytes per
/// second (128 KiB/s, i.e. ~1 Mbit/s).
///
/// A **floor rate is the only stall definition reqwest can express on the send
/// side**: there is no per-read timer for an outgoing body (see above), so
/// "this connection has gone quiet" has to be approximated by "this upload is
/// slower than any working connection would be". 1 Mbit/s is comfortably below
/// any uplink that could have downloaded this app in the first place, so a
/// transfer under it is wedged rather than slow — and it is high enough that
/// the budget stays finite: a two-hour import's ~230 MB gets ~30 minutes, not
/// forever.
///
/// # What the whole-transfer shape costs, recorded rather than fixed
///
/// Because the budget is a deadline on the whole PUT and not a progress
/// detector, a connection that dies at byte 0 is indistinguishable from one
/// that is merely slow until the deadline expires: a 400 MB file buys ~54
/// minutes of silence before the user is told anything went wrong. Noticing
/// sooner needs a body that reports progress (a stream whose last-byte-sent
/// instant can be checked), which is the same change as streaming the file
/// in [`JobClient::upload`] (see the known limit there) — not a different
/// number here. Deliberately left alone: the cancel button reaches this leg
/// immediately, so the cost is a stale progress row, not a stuck app.
pub const UPLOAD_FLOOR_BYTES_PER_SEC: u64 = 128 * 1024;

/// The four per-attempt budgets, as a value.
///
/// A value rather than four constants read at the call sites, for the reason
/// [`PollSchedule`] gives for the same choice: a test has to be able to
/// exercise a timeout in milliseconds without the client knowing it is being
/// tested, and without every production caller having to pass budgets it has
/// no opinion about.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Budgets {
    pub json: Duration,
    pub download: Duration,
    pub upload_base: Duration,
    pub upload_floor_bytes_per_sec: u64,
}

impl Default for Budgets {
    fn default() -> Self {
        Budgets {
            json: REQUEST_TIMEOUT_JSON,
            download: REQUEST_TIMEOUT_DOWNLOAD,
            upload_base: UPLOAD_TIMEOUT_BASE,
            upload_floor_bytes_per_sec: UPLOAD_FLOOR_BYTES_PER_SEC,
        }
    }
}

impl Budgets {
    /// The per-attempt budget for PUTting `byte_count` bytes.
    ///
    /// Saturating throughout, so an absurd size cannot wrap into a short
    /// timeout; a zero floor rate would divide by zero, so it falls back to
    /// the base rather than panicking on a value only a test can supply.
    ///
    /// **Per attempt**, and [`JobClient::send`] retries a 429 up to
    /// [`RetryPolicy::max_attempts`] times, so the worst case is this budget
    /// multiplied by five: a repeatedly-throttled two-hour import could sit
    /// here for over two hours. Recorded rather than capped — a whole-run deadline is the
    /// honest fix and belongs to the import queue above this module, which is
    /// where "try the whole thing again" already lives, and a throttled upload
    /// is still making progress towards a job the user asked for.
    pub fn upload(&self, byte_count: u64) -> Duration {
        if self.upload_floor_bytes_per_sec == 0 {
            return self.upload_base;
        }
        let scaled = byte_count / self.upload_floor_bytes_per_sec;
        self.upload_base
            .saturating_add(Duration::from_secs(scaled))
    }
}

// ------------------------------------------------------------ 429 retries --

/// Sarvam's throttle clears; this is how long the app is willing to wait for
/// it to.
///
/// Five *attempts* means four *waits* — the fifth attempt's failure returns
/// rather than sleeping — so the budget is `2 + 6 + 18 + 45 = 71 s`, plus up
/// to 4 s of jitter. That clears the ~1 minute window a request-per-minute
/// limit resets on, and stays short enough that a genuinely exhausted quota
/// still surfaces as an error while the user is watching.
/// [`tests::the_retry_budget_outlasts_a_one_minute_throttle_window`] sums the
/// four waits that are actually taken rather than all five delays, so the
/// number above cannot drift from the behaviour.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RetryPolicy {
    pub max_attempts: u32,
    pub base: Duration,
    pub factor: u32,
    pub max: Duration,
    pub jitter: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        RetryPolicy {
            max_attempts: 5,
            base: Duration::from_secs(2),
            factor: 3,
            max: Duration::from_secs(45),
            jitter: Duration::from_secs(1),
        }
    }
}

impl RetryPolicy {
    /// Delay before retry number `attempt` (1-based), given a jitter
    /// fraction in `0.0..1.0`.
    ///
    /// `factor.pow(attempt - 1)` — the exponent is `attempt - 1`, so the
    /// first retry waits `base`, not `base * factor`.
    ///
    /// Jitter is added, never multiplied in: two clients throttled at the
    /// same instant must not come back at the same instant, and a *fraction*
    /// of the delay would keep them correlated at short delays where the
    /// collision matters most.
    pub fn delay(&self, attempt: u32, jitter_fraction: f64) -> Duration {
        let step = self
            .factor
            .checked_pow(attempt.saturating_sub(1))
            .unwrap_or(u32::MAX);
        let base = self.base.saturating_mul(step).min(self.max);
        let j = self.jitter.mul_f64(jitter_fraction.clamp(0.0, 1.0));
        base + j
    }
}

/// A fraction in `0.0..1.0` to spread retries with. Deliberately not a
/// cryptographic source and deliberately not a new dependency: the only
/// requirement is that two processes throttled at the same moment do not
/// wake together, and the low bits of the clock satisfy it.
fn jitter_fraction() -> f64 {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    f64::from(nanos % 1_000) / 1_000.0
}

// ------------------------------------------------------------------ types --

/// What to ask the job for. Field names match Sarvam's `job_parameters`
/// exactly; see [`CreateRequest`] for the wire form.
#[derive(Clone, Debug, PartialEq)]
pub struct JobCfg {
    /// Already in REST vocabulary — run it through
    /// [`super::batch::to_rest_language_code`] first. `"unknown"` means
    /// auto-detect, and is the only value that makes the server populate
    /// `language_probability`.
    pub language_code: String,
    pub mode: String,
    pub model: String,
    pub with_timestamps: bool,
}

impl Default for JobCfg {
    fn default() -> Self {
        JobCfg {
            language_code: "unknown".into(),
            mode: "transcribe".into(),
            model: JOB_MODEL.into(),
            // On for imports, always. A dictation is pasted the moment it
            // lands and has no use for timing, but an imported recording
            // becomes a document the user scrolls through, and the segment
            // boundaries are the only structure a wall of transcript has.
            with_timestamps: true,
        }
    }
}

/// The server's handle for a job. Opaque on purpose: it is a
/// `YYYYMMDD_<uuid>` string today and nothing outside this module should
/// depend on that.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JobId(String);

impl JobId {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Where a job is. `Completed` carries the output filenames because they are
/// only knowable from the status response — the download endpoint takes them
/// as input, and the observed `"0.json"` naming is an undocumented
/// convention this module refuses to hardcode.
#[derive(Clone, Debug, PartialEq)]
pub enum JobStatus {
    Queued,
    Running,
    Completed { outputs: Vec<String> },
    Failed { reason: String },
}

/// One chunk of transcript with the span of audio it came from. Seconds from
/// the start of the file, as the server reports them.
#[derive(Clone, Debug, PartialEq)]
pub struct Segment {
    pub start_s: f64,
    pub end_s: f64,
    pub text: String,
}

/// Everything that can go wrong, in the shape
/// [`NetFailure::user_message`] established: a sentence about what happened,
/// never a status code or a library's error text.
#[derive(Clone, Debug, PartialEq)]
pub enum JobError {
    /// Never reached Sarvam. Carries the existing errno→prose taxonomy.
    Network(NetFailure),
    /// 401/403 — the key is wrong, missing or revoked.
    Unauthorized,
    /// 429 that survived every retry in [`RetryPolicy`].
    RateLimited,
    /// A 4xx the request itself caused. `or-IN` lands here, as an empty-bodied
    /// 400.
    Rejected { status: u16 },
    /// A 5xx.
    ServiceError { status: u16 },
    /// The job ran and failed. `reason` is the server's own `error_message`,
    /// which is a diagnostic string and never transcript content.
    JobFailed { reason: String },
    /// [`PollSchedule::deadline`] elapsed with the job still running.
    TimedOut,
    /// The caller asked to stop waiting — the user cancelled the import.
    ///
    /// Distinct from [`JobError::TimedOut`] on purpose: a deadline means the
    /// job may be sick, a cancellation means nothing is wrong at all. Telling
    /// a user their import "timed out" because they pressed Cancel is the
    /// small lie that makes an error list untrustworthy. The job itself is not
    /// stopped server-side — Sarvam has no cancel endpoint — so this is
    /// honestly "Butterfly Speak stopped waiting", not "the job was killed".
    Cancelled,
    /// A 2xx whose body was not the shape this module was written against.
    Malformed,
}

impl JobError {
    pub fn user_message(&self) -> String {
        match self {
            JobError::Network(f) => f.user_message().to_string(),
            JobError::Unauthorized => {
                "Sarvam rejected the API key — check it in Settings".to_string()
            }
            JobError::RateLimited => {
                "Sarvam is rate-limiting this account — wait a minute and try the import again"
                    .to_string()
            }
            JobError::Rejected { .. } => "Sarvam wouldn't accept this recording".to_string(),
            JobError::ServiceError { .. } => {
                "Sarvam had a problem transcribing this file — try again in a moment".to_string()
            }
            JobError::JobFailed { .. } => {
                "Sarvam couldn't transcribe this recording".to_string()
            }
            JobError::TimedOut => {
                "This import is taking longer than expected — Sarvam may still finish it, but \
                 Butterfly Speak has stopped waiting"
                    .to_string()
            }
            JobError::Cancelled => "Import cancelled".to_string(),
            JobError::Malformed => {
                "Sarvam's reply wasn't in a form Butterfly Speak understands".to_string()
            }
        }
    }

    /// Whether the whole import is worth attempting again as-is. A rejected
    /// request and a failed job will fail identically the second time.
    ///
    /// Read by this module's own tests and by nothing in production yet: the
    /// import queue surfaces a failed item's sentence and leaves the decision
    /// to the user, who is the one who knows whether the wifi came back.
    /// Removal trigger: a Retry affordance on the import page, which is what
    /// this is the input to.
    #[allow(dead_code)]
    pub fn is_retryable(&self) -> bool {
        match self {
            JobError::Network(f) => f.is_retryable(),
            JobError::RateLimited | JobError::ServiceError { .. } | JobError::TimedOut => true,
            // A cancellation is not a failure to retry — the user decides
            // whether to start the import again, and doing it for them is
            // exactly what they asked not to happen.
            JobError::Cancelled
            | JobError::Unauthorized
            | JobError::Rejected { .. }
            | JobError::JobFailed { .. }
            | JobError::Malformed => false,
        }
    }
}

// ------------------------------------------------------------- wire types --

#[derive(serde::Serialize)]
struct CreateRequest<'a> {
    job_parameters: JobParameters<'a>,
}

#[derive(serde::Serialize)]
struct JobParameters<'a> {
    language_code: &'a str,
    model: &'a str,
    mode: &'a str,
    with_timestamps: bool,
}

#[derive(Deserialize)]
struct CreateResponse {
    job_id: String,
}

#[derive(serde::Serialize)]
struct FilesRequest<'a> {
    job_id: &'a str,
    files: &'a [String],
}

/// Both `upload-files` and `download-files` answer with the same envelope
/// under a different key, so one type covers both legs.
#[derive(Deserialize)]
struct PresignedResponse {
    #[serde(default)]
    upload_urls: std::collections::HashMap<String, PresignedEntry>,
    #[serde(default)]
    download_urls: std::collections::HashMap<String, PresignedEntry>,
}

#[derive(Deserialize)]
struct PresignedEntry {
    file_url: String,
}

#[derive(Deserialize)]
struct StatusResponse {
    #[serde(default)]
    job_state: String,
    #[serde(default)]
    error_message: Option<String>,
    #[serde(default)]
    job_details: Vec<JobDetail>,
}

#[derive(Deserialize)]
struct JobDetail {
    #[serde(default)]
    outputs: Vec<FileRef>,
    #[serde(default)]
    state: Option<String>,
    #[serde(default)]
    error_message: Option<String>,
}

#[derive(Deserialize)]
struct FileRef {
    #[serde(default)]
    file_name: String,
}

/// The downloaded payload. Shape verified live and pinned by
/// `testdata/batch_job_output.json`.
#[derive(Deserialize)]
struct JobOutput {
    /// Deliberately **not** `#[serde(default)]`.
    ///
    /// Every other field here tolerates absence, because Sarvam adds fields
    /// over time and an unknown or missing one must never fail an import.
    /// This one is the opposite case: it is the payload. With a default, a
    /// 200 carrying `{}` — or an error envelope, or a body from some future
    /// endpoint that shares nothing with this one — would deserialize
    /// happily into an empty string and be saved as a *successful* import of
    /// a note with no words in it. Requiring it means that body is
    /// `Malformed`, which is what it is.
    transcript: String,
    /// `null` — not `{}` — when the job ran with `with_timestamps: false`.
    #[serde(default)]
    timestamps: Option<Timestamps>,
}

/// Three parallel arrays. See [`parse_output`] for why that matters.
#[derive(Deserialize)]
struct Timestamps {
    /// Chunks, despite the name. See this module's doc.
    #[serde(default)]
    words: Vec<String>,
    #[serde(default)]
    start_time_seconds: Vec<f64>,
    #[serde(default)]
    end_time_seconds: Vec<f64>,
}

// ----------------------------------------------------------------- client --

/// Percent-encode one path segment.
///
/// `job_id` comes back from the server as `YYYYMMDD_<uuid>` — safe today, but
/// pasting a server-supplied string into a URL path is how a future format
/// change becomes a request against a path nobody intended.
/// [`percent_encoding::NON_ALPHANUMERIC`] is not usable here (it would escape
/// the `_` and `-` that the real id contains), so the set is exactly the
/// characters that change what a path *means*.
fn encode_segment(s: &str) -> String {
    const PATH_UNSAFE: &percent_encoding::AsciiSet = &percent_encoding::CONTROLS
        .add(b'/')
        .add(b'\\')
        .add(b'?')
        .add(b'#')
        .add(b'%')
        .add(b' ');
    percent_encoding::utf8_percent_encode(s, PATH_UNSAFE).to_string()
}

/// The name a file is uploaded under: `audio.<ext>`, never the user's own.
///
/// See [`JobClient::upload`] for why. The extension is kept because Sarvam
/// sniffs the container itself but the storage layer still writes a file, and
/// a lowercase ASCII-alphanumeric extension is the one part of a user's
/// filename that carries no information about the recording. Anything else —
/// a missing extension, or one with a character that has no business in a
/// blob name — degrades to a bare `audio`, which live jobs confirmed is fine:
/// the server auto-detects the format from the bytes.
///
/// # What the import path actually hands this
///
/// Always a `.wav`, and therefore always `audio.wav`. `import::LiveSteps`
/// decodes every recording to 16 kHz mono PCM WAV before uploading it —
/// Sarvam's batch endpoint reliably transcribes nothing else (see
/// [`crate::media::decode`]) — and the scratch file it writes is named with a
/// `.wav` extension so that this function agrees. The `<ext>` generality
/// below is therefore no longer exercised by the shipped caller; it stays
/// because this is a client for Sarvam's API and not for one call site, and
/// because the degraded cases are what keep a future caller from sending a
/// name that fails the exact-match lookup in [`JobClient::upload`].
///
/// `pub(crate)` so the import path can pin that agreement from its own side,
/// in `import`'s `a_scratch_wav_is_uniquely_named_and_ends_in_wav`.
pub(crate) fn upload_file_name(path: &Path) -> String {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .filter(|e| !e.is_empty() && e.len() <= 8 && e.chars().all(|c| c.is_ascii_alphanumeric()));
    match ext {
        Some(e) => format!("audio.{e}"),
        None => "audio".to_string(),
    }
}

pub struct JobClient {
    http: reqwest::Client,
    /// A field rather than [`JOB_BASE_URL`] for the reason
    /// `sarvam::batch::transcribe` gives for the same choice: a loopback stub
    /// server is the only way to test a five-call lifecycle without spending
    /// real jobs.
    base_url: String,
    api_key: String,
    retry: RetryPolicy,
    budgets: Budgets,
}

impl JobClient {
    pub fn new(http: reqwest::Client, base_url: impl Into<String>, api_key: impl Into<String>) -> Self {
        JobClient {
            http,
            base_url: base_url.into(),
            api_key: api_key.into(),
            retry: RetryPolicy::default(),
            budgets: Budgets::default(),
        }
    }

    /// Used by the timeout tests, which cannot afford the real budgets'
    /// minute-scale waits. Production always takes [`Budgets::default`].
    #[allow(dead_code)]
    pub fn with_budgets(mut self, budgets: Budgets) -> Self {
        self.budgets = budgets;
        self
    }

    /// Used by this module's 429 tests, which cannot afford the real policy's
    /// ~71 s of backoff. Production always takes [`RetryPolicy::default`];
    /// remove the allow if a caller ever needs to tune it.
    #[allow(dead_code)]
    pub fn with_retry_policy(mut self, retry: RetryPolicy) -> Self {
        self.retry = retry;
        self
    }

    /// Send `build()`'s request, retrying a 429 with jittered backoff.
    ///
    /// `build` is a closure rather than a `RequestBuilder`, because a
    /// `RequestBuilder` carrying a body cannot be cloned and a retry needs a
    /// fresh one. Only 429 retries here — a 5xx does not, because every
    /// caller of this is a step in a lifecycle whose later steps would have
    /// to be re-driven anyway, and the import queue above is where "try the
    /// whole thing again" belongs.
    /// `timeout` is applied **per attempt**, on the `RequestBuilder` rather
    /// than on the client, so each leg gets a budget sized to what it actually
    /// transfers — and so a 429 retry starts its budget afresh instead of
    /// inheriting the elapsed time of the attempt that was throttled. See the
    /// request-budget constants above for why this cannot be a client-wide
    /// `read_timeout`.
    async fn send(
        &self,
        label: &'static str,
        timeout: Duration,
        build: impl Fn() -> reqwest::RequestBuilder,
    ) -> Result<(reqwest::StatusCode, String), JobError> {
        let mut attempt = 1u32;
        loop {
            let resp = build().timeout(timeout).send().await.map_err(|e| {
                let f = classify_reqwest_error(&e);
                tracing::warn!(step = label, failure = ?f, "batch job request never reached Sarvam");
                JobError::Network(f)
            })?;
            let status = resp.status();

            if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
                if attempt >= self.retry.max_attempts {
                    tracing::warn!(step = label, attempts = attempt, "batch job gave up on HTTP 429");
                    return Err(JobError::RateLimited);
                }
                let wait = self.retry.delay(attempt, jitter_fraction());
                // Not fatal — see the module doc.
                tracing::info!(
                    step = label,
                    attempt,
                    wait_ms = wait.as_millis(),
                    "Sarvam throttled the batch job; backing off"
                );
                tokio::time::sleep(wait).await;
                attempt += 1;
                continue;
            }

            // Read the body before judging the status: an empty-bodied 400 is
            // Sarvam's own signature (this is how `or-IN` is rejected), and
            // the body is needed on success regardless.
            let body = match resp.text().await {
                Ok(b) => b,
                Err(e) => {
                    let f = classify_reqwest_error(&e);
                    tracing::warn!(step = label, failure = ?f, "batch job response body read failed");
                    return Err(JobError::Network(f));
                }
            };

            if !status.is_success() {
                // Status only. Never the body — see the module doc.
                tracing::warn!(
                    step = label,
                    status = status.as_u16(),
                    body_bytes = body.len(),
                    "batch job request was refused"
                );
                return Err(status_to_error(status));
            }
            return Ok((status, body));
        }
    }

    /// `POST /` with the job parameters. Answers **202**, not 200.
    ///
    /// The language code is translated here rather than trusted from the
    /// caller. Live jobs showed that this endpoint rejects `or-IN` with an
    /// empty-bodied 400 and accepts `od-IN`, and the app's own realtime
    /// vocabulary uses `or-IN` — so leaving the translation to every call site
    /// makes correct Odia a convention that one forgetful caller breaks, with a
    /// failure that looks like a network problem rather than a spelling one.
    /// [`super::batch::to_rest_language_code`] is idempotent, so a caller that
    /// has already translated loses nothing by this.
    pub async fn create_job(&self, cfg: &JobCfg) -> Result<JobId, JobError> {
        let language_code = super::batch::to_rest_language_code(&cfg.language_code);
        let body = CreateRequest {
            job_parameters: JobParameters {
                language_code,
                model: &cfg.model,
                mode: &cfg.mode,
                with_timestamps: cfg.with_timestamps,
            },
        };
        let url = self.base_url.clone();
        let (status, raw) = self
            .send("create", self.budgets.json, || {
                self.http
                    .post(&url)
                    .header(super::AUTH_HEADER, &self.api_key)
                    .json(&body)
            })
            .await?;
        let parsed: CreateResponse = parse_json(&raw, "create")?;
        tracing::info!(status = status.as_u16(), "batch job created");
        Ok(JobId(parsed.job_id))
    }

    /// `POST /upload-files` for a presigned URL, then `PUT` the bytes to it.
    ///
    /// # The name on the wire is not the user's
    ///
    /// Sarvam keys the presigned URL by the filename it was given, so the
    /// same string has to appear in the request and the lookup — but nothing
    /// requires that string to be the user's own. It is
    /// [`upload_file_name`]'s generic `audio.<ext>`, for two reasons.
    ///
    /// Privacy first: the name becomes the blob's name in Sarvam's storage,
    /// and a filename is content. "Therapy 2026-09-01.m4a" says most of what
    /// the recording says, and this app's own tripwire is that a transcript
    /// never leaves it — a title that summarizes one should not either.
    ///
    /// Reliability second: only plain ASCII names have been seen to round-trip.
    /// A name with spaces, Devanagari or an emoji may come back
    /// normalized, percent-encoded or transliterated in `upload_urls`, and
    /// the exact-match lookup below would then miss and fail a perfectly
    /// healthy file as `Malformed`. An English-first app whose users record
    /// in 22 Indian languages will meet non-ASCII filenames constantly.
    pub async fn upload(&self, job: &JobId, path: &Path) -> Result<(), JobError> {
        let file_name = upload_file_name(path);
        let files = [file_name.clone()];
        let req = FilesRequest {
            job_id: job.as_str(),
            files: &files,
        };
        let url = format!("{}/upload-files", self.base_url);
        let (_, raw) = self
            .send("upload-files", self.budgets.json, || {
                self.http
                    .post(&url)
                    .header(super::AUTH_HEADER, &self.api_key)
                    .json(&req)
            })
            .await?;
        let parsed: PresignedResponse = parse_json(&raw, "upload-files")?;
        let entry = parsed.upload_urls.get(&file_name).ok_or_else(|| {
            // The filename is the user's, so it is counted, not named.
            tracing::warn!(
                offered = parsed.upload_urls.len(),
                "upload-files returned no presigned URL for the file that was asked for"
            );
            JobError::Malformed
        })?;

        // A read of ~230 MB (a two-hour import, converted) is far too
        // long to hold a runtime worker, and this runtime also carries
        // `ws::run_session`'s single dispatcher task.
        //
        // Known limit: the file is read whole, so the upload holds its full
        // size in RAM — ~230 MB for a two-hour import — and the upload budget
        // has no progress signal to work from. Streaming it would fix both,
        // but `send`'s retry closure is `Fn` and re-builds the body on every
        // attempt, so the stream would have to be re-openable per attempt.
        let owned = path.to_path_buf();
        let bytes = tokio::task::spawn_blocking(move || std::fs::read(owned))
            .await
            .map_err(|_| JobError::Malformed)?
            .map_err(|e| {
                tracing::warn!(kind = ?e.kind(), "could not read the file to upload");
                JobError::Network(classify_io_error(&e))
            })?;
        let byte_count = bytes.len();
        // `Bytes`, not the `Vec`, because `send`'s retry closure must be `Fn`
        // and therefore clones its body on *every* attempt including the
        // first. A `Vec` there would put a second copy of a file that can run
        // to hundreds of megabytes on the heap to pay for a retry that usually never happens;
        // cloning `Bytes` is a refcount bump.
        let bytes = bytes::Bytes::from(bytes);

        let target = entry.file_url.clone();
        let (status, _) = self
            .send("upload-put", self.budgets.upload(byte_count as u64), || {
                // `x-ms-blob-type` is what makes Azure's Put Blob accept a
                // presigned PUT at all (verified live; without it the PUT
                // fails). It is an unrecognized, ignored header on the other
                // three `storage_container_type` backends Sarvam documents,
                // so it is sent unconditionally rather than switched on a
                // field whose value the app would then have to keep up with.
                self.http
                    .put(&target)
                    .header("x-ms-blob-type", "BlockBlob")
                    .header("content-type", "application/octet-stream")
                    .body(bytes.clone())
            })
            .await?;
        tracing::info!(
            status = status.as_u16(),
            byte_count,
            "batch job audio uploaded"
        );
        Ok(())
    }

    /// `POST /{job_id}/start`. An empty JSON body; the docs' optional
    /// `ptu_id` query parameter is for provisioned throughput the app does
    /// not have.
    pub async fn start(&self, job: &JobId) -> Result<(), JobError> {
        let url = format!("{}/{}/start", self.base_url, encode_segment(job.as_str()));
        let (status, _) = self
            .send("start", self.budgets.json, || {
                self.http
                    .post(&url)
                    .header(super::AUTH_HEADER, &self.api_key)
                    .json(&serde_json::json!({}))
            })
            .await?;
        tracing::info!(status = status.as_u16(), "batch job started");
        Ok(())
    }

    /// `GET /{job_id}/status`.
    ///
    /// The documented job states are `Accepted`, `Pending`, `Running`,
    /// `Completed` and `Failed`, and the overview page also mentions a
    /// partially-completed outcome. Matching is case-insensitive and
    /// **anything unrecognized counts as still running** rather than as a
    /// failure: a state this module has not heard of is far more likely to be
    /// a new intermediate one than a new terminal one, and treating it as
    /// terminal would abandon a job that was about to succeed. The deadline
    /// is what stops an unknown state looping forever.
    pub async fn poll(&self, job: &JobId) -> Result<JobStatus, JobError> {
        let url = format!("{}/{}/status", self.base_url, encode_segment(job.as_str()));
        let (_, raw) = self
            .send("status", self.budgets.json, || {
                self.http
                    .get(&url)
                    .header(super::AUTH_HEADER, &self.api_key)
            })
            .await?;
        let parsed: StatusResponse = parse_json(&raw, "status")?;
        Ok(interpret_status(&parsed))
    }

    /// Poll until the job settles, backing off and honouring the deadline —
    /// or until `cancel` resolves. A poll that fails on the network or with a
    /// 5xx is asked again after the next back-off, up to
    /// [`POLL_FAILURES_TOLERATED`] in a row.
    ///
    /// Returns the output filenames to hand to [`Self::download`]. The first
    /// poll happens after `schedule.first`, not immediately: `start` has just
    /// returned and no live job has finished in under a second, so an
    /// immediate poll is a guaranteed-wasted request against an
    /// endpoint with an undocumented rate limit.
    ///
    /// # The cancellation parameter
    ///
    /// A deadline alone is not enough. Between two polls this loop sleeps for
    /// up to [`POLL_MAX`], so a user pressing Cancel would go on watching a
    /// spinner for as much as ten seconds while a request they abandoned
    /// finished quietly. Worse, on a long file the loop can legitimately sit
    /// here for the full [`DEADLINE_CEILING`] — half an hour of a queue that
    /// cannot be stopped.
    ///
    /// It is a bare `Future` rather than a cancellation-token type so this
    /// module keeps its dependencies pointing one way: `import` knows about
    /// `sarvam`, and `sarvam` does not need to know about `import`. A caller
    /// with nothing to cancel passes [`std::future::pending()`].
    ///
    /// The **sleep** is what the cancellation races, not the in-flight status
    /// request. Dropping a poll mid-flight would save at most one short round
    /// trip and would lose the state it was about to report; letting it land
    /// and then checking costs nothing a user can perceive.
    pub async fn wait_for_completion(
        &self,
        job: &JobId,
        schedule: &PollSchedule,
        cancel: impl std::future::Future<Output = ()>,
    ) -> Result<Vec<String>, JobError> {
        let started = tokio::time::Instant::now();
        let mut delay = schedule.first;
        let mut polls = 0u32;
        let mut failed_polls = 0u32;
        // Pinned once, outside the loop: a fresh cancellation future per
        // iteration would restart whatever wait it represents, and one that
        // had already fired would be forgotten between polls.
        tokio::pin!(cancel);
        loop {
            if started.elapsed() >= schedule.deadline {
                tracing::warn!(
                    polls,
                    waited_s = started.elapsed().as_secs(),
                    "batch job deadline elapsed; no longer waiting"
                );
                return Err(JobError::TimedOut);
            }
            tokio::select! {
                biased;
                // First, so an already-cancelled caller never spends another
                // poll — and never sleeps out the rest of the current gap.
                () = &mut cancel => {
                    tracing::info!(
                        polls,
                        waited_s = started.elapsed().as_secs(),
                        "batch job wait cancelled; the job itself is left running"
                    );
                    return Err(JobError::Cancelled);
                }
                () = tokio::time::sleep(delay) => {}
            }
            polls += 1;
            let status = match self.poll(job).await {
                Ok(status) => {
                    failed_polls = 0;
                    status
                }
                // A status request that met a network blip or a 5xx says
                // nothing about the job, which is still running and already
                // billed. Back off and ask again, unless the route has kept
                // failing long enough to count as down.
                Err(e @ (JobError::Network(_) | JobError::ServiceError { .. })) => {
                    failed_polls += 1;
                    if failed_polls >= POLL_FAILURES_TOLERATED {
                        tracing::warn!(polls, failed_polls, "batch job status kept failing; no longer waiting");
                        return Err(e);
                    }
                    tracing::info!(polls, failed_polls, "batch job status request failed; asking again");
                    delay = schedule.next_delay(delay);
                    continue;
                }
                Err(e) => return Err(e),
            };
            match status {
                JobStatus::Completed { outputs } => {
                    tracing::info!(polls, outputs = outputs.len(), "batch job completed");
                    return Ok(outputs);
                }
                JobStatus::Failed { reason } => {
                    tracing::warn!(polls, "batch job failed");
                    return Err(JobError::JobFailed { reason });
                }
                JobStatus::Queued | JobStatus::Running => {
                    delay = schedule.next_delay(delay);
                }
            }
        }
    }

    /// `POST /download-files` for a presigned URL, then `GET` the payload.
    ///
    /// `outputs` comes from [`JobStatus::Completed`] rather than being
    /// derived: live jobs return `"0.json"`, but that naming is nowhere in
    /// the documentation and a client that assumes it is a client that breaks
    /// the day a job returns two files.
    ///
    /// **Only `outputs[0]` is fetched.** Every name in `outputs` is asked for
    /// in the `download-files` request, so every presigned URL comes back,
    /// but just the first payload is downloaded and parsed. That is exact
    /// today and not a shortcut: the app submits one file per job
    /// ([`MAX_FILES_PER_JOB`] documents why), so a job has exactly one
    /// output. If the queue above ever batches files into a single job, this
    /// is the function that has to grow a loop — and the caller would also
    /// need to say which segments belong to which file, which is a different
    /// return type, not just another iteration.
    ///
    /// The presigned GET carries **no** `api-subscription-key`. The SAS token
    /// in the URL is the credential; sending the API key to a storage host
    /// would leak it outside Sarvam's own API surface.
    pub async fn download(
        &self,
        job: &JobId,
        outputs: &[String],
    ) -> Result<(Vec<Segment>, String), JobError> {
        let first = outputs.first().ok_or(JobError::Malformed)?;
        let req = FilesRequest {
            job_id: job.as_str(),
            files: outputs,
        };
        let url = format!("{}/download-files", self.base_url);
        let (_, raw) = self
            .send("download-files", self.budgets.json, || {
                self.http
                    .post(&url)
                    .header(super::AUTH_HEADER, &self.api_key)
                    .json(&req)
            })
            .await?;
        let parsed: PresignedResponse = parse_json(&raw, "download-files")?;
        let entry = parsed.download_urls.get(first).ok_or_else(|| {
            tracing::warn!(
                offered = parsed.download_urls.len(),
                "download-files returned no presigned URL for the requested output"
            );
            JobError::Malformed
        })?;

        let target = entry.file_url.clone();
        let (_, payload) = self
            .send("download-get", self.budgets.download, || {
                self.http.get(&target)
            })
            .await?;
        parse_output(&payload)
    }
}

/// Map a non-2xx status onto the taxonomy. 429 never arrives here — it is
/// consumed by the retry loop and only becomes [`JobError::RateLimited`]
/// after the attempts run out.
fn status_to_error(status: reqwest::StatusCode) -> JobError {
    let code = status.as_u16();
    match code {
        401 | 403 => JobError::Unauthorized,
        429 => JobError::RateLimited,
        400..=499 => JobError::Rejected { status: code },
        500..=599 => JobError::ServiceError { status: code },
        // A 1xx or 3xx that reqwest surfaced instead of following.
        _ => JobError::Rejected { status: code },
    }
}

/// Read the job state and, when it is terminal, what came with it.
///
/// Split out from [`JobClient::poll`] so every state transition can be
/// exercised against a literal response body without a server.
fn interpret_status(s: &StatusResponse) -> JobStatus {
    match s.job_state.to_ascii_lowercase().as_str() {
        "completed" => {
            let outputs: Vec<String> = s
                .job_details
                .iter()
                .flat_map(|d| d.outputs.iter())
                .map(|f| f.file_name.clone())
                .filter(|n| !n.is_empty())
                .collect();
            if outputs.is_empty() {
                // "Completed with nothing to download" is a failure wearing a
                // success label; treating it as success would hand the caller
                // an empty `outputs` and make `download` fail with the far
                // less informative `Malformed`.
                return JobStatus::Failed {
                    reason: first_error_message(s)
                        .unwrap_or_else(|| "the job completed with no output files".to_string()),
                };
            }
            JobStatus::Completed { outputs }
        }
        "failed" => JobStatus::Failed {
            reason: first_error_message(s).unwrap_or_else(|| "the job failed".to_string()),
        },
        "accepted" | "pending" => JobStatus::Queued,
        // "running", and anything not recognized — see `poll`'s doc.
        _ => JobStatus::Running,
    }
}

/// The most specific non-empty diagnostic in a status response: the per-file
/// message first, then the job-level one. Both are server-authored
/// diagnostics; neither is transcript content.
fn first_error_message(s: &StatusResponse) -> Option<String> {
    s.job_details
        .iter()
        .find_map(|d| {
            d.error_message
                .as_ref()
                .filter(|m| !m.trim().is_empty())
                .cloned()
                .or_else(|| {
                    d.state
                        .as_ref()
                        .filter(|st| !st.eq_ignore_ascii_case("Success") && !st.trim().is_empty())
                        .cloned()
                })
        })
        .or_else(|| {
            s.error_message
                .as_ref()
                .filter(|m| !m.trim().is_empty())
                .cloned()
        })
}

/// Parse a downloaded output file into segments plus the full transcript.
///
/// # Why the zip is defensive
///
/// The payload represents segments as three parallel arrays, so "the arrays
/// disagree in length" is a state the wire format can express and an
/// array-of-objects could not. Every live response had all three equal, but a
/// parser that indexes `start[i]` off `words.len()` is one server-side change
/// away from a panic on the user's file. The zip therefore runs to the
/// **shortest** array and drops the remainder.
///
/// An entry is skipped when its text is blank or its start is not a finite
/// number; an end that is not finite, or that precedes its start, collapses
/// to the start. A missing timestamp degrades the export; it never fails the
/// import.
///
/// The full text is `transcript` verbatim, not the segments rejoined: live
/// jobs confirm `transcript` is exactly the chunks joined by a single
/// space, so rejoining would be reconstructing a string the server already
/// sent — and would silently lose any chunk this function dropped.
pub fn parse_output(raw: &str) -> Result<(Vec<Segment>, String), JobError> {
    let out: JobOutput = parse_json(raw, "download-get")?;
    let full_text = out.transcript;

    let Some(ts) = out.timestamps else {
        // `with_timestamps: false`, or a job that produced none. Text with no
        // structure is a complete, usable import.
        return Ok((Vec::new(), full_text));
    };

    let n = ts
        .words
        .len()
        .min(ts.start_time_seconds.len())
        .min(ts.end_time_seconds.len());
    if n != ts.words.len() {
        tracing::warn!(
            texts = ts.words.len(),
            starts = ts.start_time_seconds.len(),
            ends = ts.end_time_seconds.len(),
            "timestamp arrays disagreed in length; using the shortest"
        );
    }

    let mut segments = Vec::with_capacity(n);
    for i in 0..n {
        let text = ts.words[i].trim();
        let start = ts.start_time_seconds[i];
        if text.is_empty() || !start.is_finite() {
            continue;
        }
        let end = ts.end_time_seconds[i];
        let end = if end.is_finite() && end >= start { end } else { start };
        segments.push(Segment {
            start_s: start,
            end_s: end,
            text: text.to_string(),
        });
    }
    tracing::info!(
        segments = segments.len(),
        text_chars = full_text.chars().count(),
        "batch job output parsed"
    );
    Ok((segments, full_text))
}

/// Deserialize, logging only what cannot carry content on failure.
///
/// Never `{e}`: `serde_json::Error`'s `Display` is a bare line/column for a
/// *syntax* error, but a type mismatch embeds the offending value — and on
/// the download leg that value is the user's transcript. Same trap
/// `sarvam::batch::transcribe` and `sarvam::translate` already pin.
fn parse_json<T: serde::de::DeserializeOwned>(raw: &str, step: &'static str) -> Result<T, JobError> {
    serde_json::from_str(raw).map_err(|e| {
        tracing::warn!(
            step,
            category = ?e.classify(),
            line = e.line(),
            column = e.column(),
            "batch job reply was not the documented shape"
        );
        JobError::Malformed
    })
}

/// Bridge a `reqwest::Error` onto the existing errno→prose taxonomy.
///
/// `reqwest::Error` has no `ErrorKind`, so the discriminants worth matching
/// live one or two layers down the source chain as an `io::Error` that
/// [`classify_io_error`] already knows how to read (Winsock DNS codes, TLS
/// via the `rustls::Error` payload, refused/reset/timed-out). `is_timeout` is
/// checked first because a reqwest request-level timeout has no io::Error
/// underneath it at all — it is reqwest's own deadline firing.
fn classify_reqwest_error(e: &reqwest::Error) -> NetFailure {
    if e.is_timeout() {
        return NetFailure::Timeout;
    }
    let mut cur: Option<&(dyn std::error::Error + 'static)> = Some(e);
    while let Some(err) = cur {
        if let Some(io) = err.downcast_ref::<std::io::Error>() {
            return classify_io_error(io);
        }
        cur = err.source();
    }
    NetFailure::Other
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    /// The live payload's shape, with the text replaced by placeholders. This
    /// file is the parser's specification — see its own `_comment`.
    const LIVE_SHAPE: &str = include_str!("testdata/batch_job_output.json");

    // ------------------------------------------------------------ parser --

    /// The parser must handle the real payload, three parallel arrays and
    /// all.
    #[test]
    fn the_live_payload_shape_parses_into_segments_and_full_text() {
        let (segments, text) = parse_output(LIVE_SHAPE).expect("the live shape must parse");
        assert_eq!(segments.len(), 5);
        assert_eq!(segments[0].start_s, 0.13);
        assert_eq!(segments[0].end_s, 20.39);
        assert_eq!(segments[0].text, "PLACEHOLDER CHUNK ONE.");
        assert_eq!(segments[4].start_s, 73.09);
        assert_eq!(segments[4].end_s, 80.48);
        assert!(text.starts_with("PLACEHOLDER CHUNK ONE."));
        assert!(text.ends_with("PLACEHOLDER CHUNK FIVE."));
    }

    /// Chunks abut exactly in every live response. Pinned because a parser
    /// that quietly reordered or de-duplicated them would still pass the test
    /// above.
    #[test]
    fn the_live_segments_are_contiguous_and_in_order() {
        let (segments, _) = parse_output(LIVE_SHAPE).expect("the live shape must parse");
        for pair in segments.windows(2) {
            assert_eq!(
                pair[0].end_s, pair[1].start_s,
                "chunks abut in every observed response"
            );
        }
    }

    /// `transcript` is authoritative for the full text, not the segments
    /// rejoined — live responses have them equal, and reconstructing
    /// would lose any chunk the zip dropped.
    #[test]
    fn the_full_text_comes_from_transcript_not_from_rejoining_the_chunks() {
        let raw = r#"{"transcript":"THE SERVER SENT THIS",
            "timestamps":{"words":["a","b"],
            "start_time_seconds":[0.0,1.0],"end_time_seconds":[1.0,2.0]}}"#;
        let (segments, text) = parse_output(raw).expect("must parse");
        assert_eq!(text, "THE SERVER SENT THIS");
        assert_eq!(segments.len(), 2);
    }

    /// `with_timestamps: false` sends `null`, not `{}` — verified live. Text
    /// with no segments is still a complete import.
    #[test]
    fn a_null_timestamps_field_yields_text_with_no_segments() {
        let raw = r#"{"request_id":"x","transcript":"PLACEHOLDER","timestamps":null,
            "diarized_transcript":null,"language_code":"en-IN"}"#;
        let (segments, text) = parse_output(raw).expect("must parse");
        assert!(segments.is_empty());
        assert_eq!(text, "PLACEHOLDER");
    }

    /// A single-chunk response — what every short file returns.
    #[test]
    fn a_single_chunk_response_parses() {
        let raw = r#"{"request_id":"x","transcript":"PLACEHOLDER",
            "timestamps":{"words":["PLACEHOLDER"],"start_time_seconds":[0.0],
            "end_time_seconds":[2.93]},"language_code":"od-IN"}"#;
        let (segments, _) = parse_output(raw).expect("must parse");
        assert_eq!(segments.len(), 1);
        assert_eq!(segments[0].end_s, 2.93);
    }

    /// The failure mode three parallel arrays invite and an array of objects
    /// could not. Indexing off `words.len()` here would panic on the user's
    /// file.
    #[test]
    fn arrays_of_different_lengths_truncate_to_the_shortest_instead_of_panicking() {
        let raw = r#"{"transcript":"PLACEHOLDER",
            "timestamps":{"words":["a","b","c"],"start_time_seconds":[0.0,1.0],
            "end_time_seconds":[1.0]}}"#;
        let (segments, _) = parse_output(raw).expect("must parse");
        assert_eq!(segments.len(), 1);
        assert_eq!(segments[0].text, "a");
    }

    /// A missing timestamp degrades the export; it never fails the import.
    #[test]
    fn blank_and_unusable_entries_are_skipped_not_fatal() {
        let raw = r#"{"transcript":"PLACEHOLDER",
            "timestamps":{"words":["   ","real","also real"],
            "start_time_seconds":[0.0,1.0,2.0],
            "end_time_seconds":[1.0,0.5,3.0]}}"#;
        let (segments, _) = parse_output(raw).expect("must parse");
        assert_eq!(segments.len(), 2);
        // The blank one is gone...
        assert_eq!(segments[0].text, "real");
        // ...and an end before its start collapses to the start rather than
        // producing a negative-length span.
        assert_eq!(segments[0].start_s, 1.0);
        assert_eq!(segments[0].end_s, 1.0);
        assert_eq!(segments[1].end_s, 3.0);
    }

    #[test]
    fn a_reply_that_is_not_json_is_malformed_not_a_panic() {
        assert_eq!(parse_output("not json at all"), Err(JobError::Malformed));
    }

    /// A body with no `transcript` at all must be `Malformed`, not a
    /// successful import of nothing.
    ///
    /// With `#[serde(default)]` on that field — which is right for every
    /// other field, since Sarvam adds them over time — an empty object or an
    /// error envelope would deserialize happily into an empty string and be
    /// saved as a note with no words in it, reported as success.
    #[test]
    fn a_payload_with_no_transcript_field_is_malformed_not_an_empty_success() {
        assert_eq!(parse_output("{}"), Err(JobError::Malformed));
        assert_eq!(
            parse_output(r#"{"error":"something went wrong","code":"INTERNAL"}"#),
            Err(JobError::Malformed)
        );
    }

    /// ...but a transcript that is genuinely empty is a real answer: silence
    /// transcribes to nothing, and that is not a malformed reply.
    #[test]
    fn an_explicitly_empty_transcript_is_a_valid_if_empty_result() {
        let (segments, text) = parse_output(r#"{"transcript":"","timestamps":null}"#)
            .expect("an empty transcript is an answer, not a broken reply");
        assert!(segments.is_empty());
        assert_eq!(text, "");
    }

    /// Sarvam adds fields over time (`audio_hash` and `audio_mime` are in the
    /// live payload and mean nothing to this parser). An unknown field must
    /// never fail an import.
    #[test]
    fn unknown_fields_are_ignored() {
        let raw = r#"{"transcript":"PLACEHOLDER","timestamps":null,
            "a_field_from_next_year":{"nested":[1,2,3]}}"#;
        let (_, text) = parse_output(raw).expect("must tolerate new fields");
        assert_eq!(text, "PLACEHOLDER");
    }

    // ------------------------------------------------------------ status --

    fn status_body(job_state: &str, outputs: &[&str], err: &str) -> StatusResponse {
        let files: Vec<String> = outputs.iter().map(|s| format!(r#"{{"file_name":"{s}"}}"#)).collect();
        let raw = format!(
            r#"{{"job_state":"{job_state}","error_message":"{err}",
               "job_details":[{{"outputs":[{}],"state":"Success","error_message":""}}]}}"#,
            files.join(",")
        );
        serde_json::from_str(&raw).expect("test fixture is valid JSON")
    }

    #[test]
    fn the_documented_job_states_map_to_the_right_status() {
        assert_eq!(interpret_status(&status_body("Accepted", &[], "")), JobStatus::Queued);
        assert_eq!(interpret_status(&status_body("Pending", &[], "")), JobStatus::Queued);
        assert_eq!(interpret_status(&status_body("Running", &[], "")), JobStatus::Running);
        assert_eq!(
            interpret_status(&status_body("Completed", &["0.json"], "")),
            JobStatus::Completed {
                outputs: vec!["0.json".to_string()]
            }
        );
    }

    /// Matching is case-insensitive: the state is a server-supplied string,
    /// and a casing change is not a reason to abandon a job.
    #[test]
    fn job_state_matching_ignores_case() {
        assert!(interpret_status(&status_body("COMPLETED", &["0.json"], "")).is_completed());
        assert_eq!(interpret_status(&status_body("running", &[], "")), JobStatus::Running);
    }

    /// A state this module has not heard of is treated as still running, not
    /// as a failure — see `poll`'s doc. The deadline stops the loop.
    #[test]
    fn an_unrecognized_job_state_keeps_waiting_rather_than_failing_the_import() {
        assert_eq!(
            interpret_status(&status_body("PartiallyCompleted", &[], "")),
            JobStatus::Running
        );
    }

    #[test]
    fn a_failed_job_carries_the_servers_own_reason() {
        let raw = r#"{"job_state":"Failed","error_message":"job level",
            "job_details":[{"outputs":[],"state":"API Error","error_message":"file level"}]}"#;
        let s: StatusResponse = serde_json::from_str(raw).unwrap();
        // The per-file message is more specific than the job-level one.
        assert_eq!(
            interpret_status(&s),
            JobStatus::Failed {
                reason: "file level".to_string()
            }
        );
    }

    /// "Completed with nothing to download" is a failure wearing a success
    /// label. Reporting it as completed would hand `download` an empty list
    /// and turn a real diagnosis into `Malformed`.
    #[test]
    fn completed_with_no_outputs_is_a_failure_not_a_success() {
        assert!(matches!(
            interpret_status(&status_body("Completed", &[], "")),
            JobStatus::Failed { .. }
        ));
    }

    impl JobStatus {
        fn is_completed(&self) -> bool {
            matches!(self, JobStatus::Completed { .. })
        }
    }

    // ------------------------------------------------- schedule / backoff --

    #[test]
    fn the_poll_delay_doubles_and_then_holds_at_the_cap() {
        let s = PollSchedule::for_audio(Some(10.0));
        assert_eq!(s.first, Duration::from_secs(2));
        let mut d = s.first;
        let mut seen = vec![d];
        for _ in 0..5 {
            d = s.next_delay(d);
            seen.push(d);
        }
        assert_eq!(
            seen,
            vec![
                Duration::from_secs(2),
                Duration::from_secs(4),
                Duration::from_secs(8),
                Duration::from_secs(10),
                Duration::from_secs(10),
                Duration::from_secs(10),
            ]
        );
    }

    #[test]
    fn the_deadline_is_two_minutes_plus_twice_the_audio() {
        // 60 s of audio -> 120 + 120 = 240 s.
        assert_eq!(
            PollSchedule::for_audio(Some(60.0)).deadline,
            Duration::from_secs(240)
        );
        // 82 s, the length of the live job the constants were sized on (it
        // took 46 s).
        assert_eq!(
            PollSchedule::for_audio(Some(82.0)).deadline,
            Duration::from_secs(284)
        );
    }

    #[test]
    fn the_deadline_never_exceeds_the_ceiling() {
        // 2 hours of audio would want 2 min + 4 hours.
        assert_eq!(
            PollSchedule::for_audio(Some(7_200.0)).deadline,
            DEADLINE_CEILING
        );
    }

    /// Documented decision: an unmeasurable file gets the ceiling, because
    /// abandoning a healthy job the user has already paid for is worse than
    /// waiting too long for a stuck one.
    #[test]
    fn an_unknown_audio_length_gets_the_ceiling_not_the_base() {
        assert_eq!(PollSchedule::for_audio(None).deadline, DEADLINE_CEILING);
        assert_eq!(PollSchedule::for_audio(Some(f64::NAN)).deadline, DEADLINE_CEILING);
        assert_eq!(PollSchedule::for_audio(Some(0.0)).deadline, DEADLINE_CEILING);
    }

    /// The exponent is `attempt - 1`, so the first retry waits `base`.
    #[test]
    fn the_first_retry_waits_the_base_delay_not_the_base_times_the_factor() {
        let p = RetryPolicy::default();
        assert_eq!(p.delay(1, 0.0), Duration::from_secs(2));
        assert_eq!(p.delay(2, 0.0), Duration::from_secs(6));
        assert_eq!(p.delay(3, 0.0), Duration::from_secs(18));
        // 54 s would exceed the 45 s cap.
        assert_eq!(p.delay(4, 0.0), Duration::from_secs(45));
        assert_eq!(p.delay(99, 0.0), Duration::from_secs(45));
    }

    /// Jitter is added, not scaled: two clients throttled together must not
    /// come back together, and a fraction of the delay would leave them
    /// correlated exactly where the collision matters most.
    #[test]
    fn jitter_is_added_on_top_and_is_bounded() {
        let p = RetryPolicy::default();
        assert_eq!(p.delay(1, 1.0), Duration::from_secs(3));
        assert_eq!(p.delay(1, 0.5), Duration::from_millis(2_500));
        // Even at the cap, jitter still applies.
        assert_eq!(p.delay(9, 1.0), Duration::from_secs(46));
        // Out-of-range fractions are clamped rather than trusted.
        assert_eq!(p.delay(1, -5.0), Duration::from_secs(2));
        assert_eq!(p.delay(1, 5.0), Duration::from_secs(3));
    }

    #[test]
    fn the_jitter_source_stays_inside_its_range() {
        for _ in 0..64 {
            let f = jitter_fraction();
            assert!((0.0..1.0).contains(&f), "jitter fraction out of range: {f}");
        }
    }

    /// The whole five-attempt budget has to outlast Sarvam's throttle window,
    /// which resets on the order of a minute.
    #[test]
    fn the_retry_budget_outlasts_a_one_minute_throttle_window() {
        let p = RetryPolicy::default();
        // `1..max_attempts`, not `1..=`: five attempts take four waits,
        // because the last attempt's failure returns instead of sleeping.
        let total: Duration = (1..p.max_attempts).map(|a| p.delay(a, 0.0)).sum();
        assert_eq!(
            total,
            Duration::from_secs(71),
            "2 + 6 + 18 + 45; keep RetryPolicy's doc comment in step with this"
        );
        assert!(
            total >= Duration::from_secs(60),
            "the retry budget must outlast the throttle window, got {total:?}"
        );
    }

    // ---------------------------------------------------- error taxonomy --

    #[test]
    fn statuses_map_onto_the_taxonomy() {
        use reqwest::StatusCode;
        assert_eq!(status_to_error(StatusCode::UNAUTHORIZED), JobError::Unauthorized);
        assert_eq!(status_to_error(StatusCode::FORBIDDEN), JobError::Unauthorized);
        // The `or-IN` rejection.
        assert_eq!(
            status_to_error(StatusCode::BAD_REQUEST),
            JobError::Rejected { status: 400 }
        );
        assert_eq!(
            status_to_error(StatusCode::INTERNAL_SERVER_ERROR),
            JobError::ServiceError { status: 500 }
        );
    }

    /// A Sarvam 429 must not be terminal for the import: it is a rate limit
    /// that clears, not a quota that never does.
    #[test]
    fn rate_limiting_is_retryable_not_fatal() {
        assert!(JobError::RateLimited.is_retryable());
        assert!(!JobError::Unauthorized.is_retryable());
        assert!(!JobError::Rejected { status: 400 }.is_retryable());
        assert!(!JobError::JobFailed { reason: "x".into() }.is_retryable());
    }

    #[test]
    fn every_error_says_something_different() {
        let all = [
            JobError::Network(NetFailure::NameNotResolved),
            JobError::Unauthorized,
            JobError::RateLimited,
            JobError::Rejected { status: 400 },
            JobError::ServiceError { status: 503 },
            JobError::JobFailed { reason: "x".into() },
            JobError::TimedOut,
            JobError::Malformed,
        ];
        let msgs: std::collections::HashSet<String> =
            all.iter().map(|e| e.user_message()).collect();
        assert_eq!(msgs.len(), all.len());
    }

    /// A user-facing message must never leak the machinery. A raw status
    /// code in the sentence is the most likely accidental leak, since the
    /// variants carry one.
    #[test]
    fn no_user_message_carries_a_status_code() {
        for e in [
            JobError::Rejected { status: 400 },
            JobError::ServiceError { status: 503 },
        ] {
            let m = e.user_message();
            assert!(!m.contains("400") && !m.contains("503"), "{m}");
        }
    }

    #[test]
    fn a_job_id_is_percent_encoded_into_the_path() {
        // The real shape survives untouched...
        assert_eq!(
            encode_segment("20260906_8e58808e-7a60-41e6-ae9f-7b053bfd52e5"),
            "20260906_8e58808e-7a60-41e6-ae9f-7b053bfd52e5"
        );
        // ...while anything that would change what the path means does not.
        assert_eq!(encode_segment("../../admin"), "..%2F..%2Fadmin");
        assert_eq!(encode_segment("a b"), "a%20b");
        assert_eq!(encode_segment("a?b#c"), "a%3Fb%23c");
    }

    /// The tripwire, mechanically. A presigned URL carries a SAS token, which
    /// makes it a bearer credential for the blob; a transcript is the user's
    /// words. Neither may appear in anything this module logs, so the fields
    /// it does log are checked for both.
    #[test]
    fn no_log_field_can_carry_a_presigned_url_or_a_transcript() {
        let secret_url = "https://x.blob.core.windows.net/c/f.wav?sig=SECRETSAS";
        let transcript = "meet me at the clinic at four";

        // The download leg's parse-failure branch is the sharpest case: a 2xx
        // body that is a bare JSON string makes serde's Display the whole
        // transcript.
        let body = serde_json::to_string(transcript).expect("a JSON string");
        let e = match serde_json::from_str::<JobOutput>(&body) {
            Ok(_) => panic!("a bare string must not parse as the documented shape"),
            Err(e) => e,
        };
        assert!(
            e.to_string().contains(transcript),
            "if this stops holding the hazard changed shape: {e}"
        );
        let logged = format!("{:?} {} {}", e.classify(), e.line(), e.column());
        assert!(!logged.contains(transcript), "{logged}");

        // And the presigned-URL branches log counts, never the entry.
        let raw = format!(r#"{{"upload_urls":{{"a.wav":{{"file_url":"{secret_url}"}}}}}}"#);
        let parsed: PresignedResponse = serde_json::from_str(&raw).expect("must parse");
        let logged = format!("offered = {}", parsed.upload_urls.len());
        assert!(!logged.contains("SECRETSAS"), "{logged}");
        assert!(!logged.contains("blob.core.windows.net"), "{logged}");
    }

    /// The one field a `JobFailed` does carry out of the server's response is
    /// `error_message`, a server-authored diagnostic. Pinned so nobody
    /// "improves" it into carrying the per-file transcript instead.
    #[test]
    fn a_failure_reason_comes_from_error_message_never_from_a_transcript_field() {
        let raw = r#"{"job_state":"Failed","error_message":"",
            "job_details":[{"outputs":[],"state":"API Error",
            "error_message":"unsupported codec","transcript":"SECRET WORDS"}]}"#;
        let s: StatusResponse = serde_json::from_str(raw).unwrap();
        match interpret_status(&s) {
            JobStatus::Failed { reason } => {
                assert_eq!(reason, "unsupported codec");
                assert!(!reason.contains("SECRET"));
            }
            other => panic!("expected a failure, got {other:?}"),
        }
    }

    #[test]
    fn the_default_job_cfg_asks_for_timestamps_on_the_pinned_model() {
        let c = JobCfg::default();
        assert!(c.with_timestamps, "imports always want segment structure");
        assert_eq!(c.model, "saaras:v3");
        assert_eq!(c.mode, "transcribe");
        assert_eq!(c.language_code, "unknown");
    }

    /// The Odia verdict, pinned where a future reader will trip over it: the
    /// Batch job API rejected `or-IN` with an empty-bodied 400 and accepted
    /// `od-IN`, so it is on the REST side of the app's split and the existing
    /// translation is the right one.
    ///
    /// This checks the *mapping* only, which says nothing about the client. The
    /// test that guards the endpoint is
    /// [`odia_reaches_the_wire_as_od_in_even_when_the_caller_says_or_in`]; this
    /// one states the mapping's direction separately from the call that uses
    /// it.
    #[test]
    fn the_odia_mapping_points_at_the_code_the_live_probe_showed_batch_accepts() {
        assert_eq!(super::super::batch::to_rest_language_code("or-IN"), "od-IN");
        assert_eq!(super::super::batch::to_rest_language_code("auto"), "unknown");
        // Idempotent, which is what lets `create_job` apply it unconditionally
        // without punishing a caller that already did.
        assert_eq!(super::super::batch::to_rest_language_code("od-IN"), "od-IN");
    }

    // ------------------------------------------------- stub-server tests --

    /// One canned reply.
    struct Reply {
        status: u16,
        body: String,
        /// How long to sit on the request before answering — the whole body
        /// has already been read by then, so this is the far side taking its
        /// time to reply, exactly like Azure committing a large blob.
        delay: Duration,
    }

    impl Reply {
        fn ok(body: impl Into<String>) -> Reply {
            Reply {
                status: 200,
                body: body.into(),
                delay: Duration::ZERO,
            }
        }
        fn code(status: u16) -> Reply {
            Reply {
                status,
                body: String::new(),
                delay: Duration::ZERO,
            }
        }
        fn after(mut self, delay: Duration) -> Reply {
            self.delay = delay;
            self
        }
    }

    /// What the stub saw and what it will say next.
    struct Stub {
        addr: std::net::SocketAddr,
        /// Request lines ("POST /upload-files HTTP/1.1"), in order.
        seen: Arc<std::sync::Mutex<Vec<String>>>,
        /// Request bodies, in the same order — what actually went on the
        /// wire, which is the only place a claim about what the client sends
        /// can honestly be checked.
        bodies: Arc<std::sync::Mutex<Vec<String>>>,
        served: Arc<AtomicUsize>,
    }

    /// A loopback HTTP stub that answers `replies` in order, one connection
    /// per reply. Same shape as `sarvam::batch`'s and `format::backend`'s stub
    /// servers, extended to a sequence because a job lifecycle is five calls
    /// and the ORDER is half of what needs testing.
    ///
    /// Every reply closes its connection, so reqwest opens a fresh one per
    /// request and the stub never has to implement keep-alive.
    fn stub(replies: Vec<Reply>) -> Stub {
        stub_with(|_| replies)
    }

    /// The same, but the replies are built from the stub's own address.
    ///
    /// The presigned-URL legs need a reply that points back at the stub, and
    /// the address is only knowable after the bind. Discovering a free port by
    /// binding, dropping and re-binding races every other test in the suite
    /// for that port — the OS is free to hand it to someone else in between,
    /// and does. Binding once and handing the address to the caller is the
    /// only version of this that cannot flake.
    fn stub_with(make: impl FnOnce(std::net::SocketAddr) -> Vec<Reply>) -> Stub {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        let addr = listener.local_addr().expect("local addr");
        let replies = make(addr);
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let bodies = Arc::new(std::sync::Mutex::new(Vec::new()));
        let served = Arc::new(AtomicUsize::new(0));
        let seen_w = Arc::clone(&seen);
        let bodies_w = Arc::clone(&bodies);
        let served_w = Arc::clone(&served);

        // A plain OS thread, not a tokio task: the reads below are blocking,
        // and a blocking read on a runtime worker would deadlock the very
        // client this is serving when the test runtime is single-threaded.
        std::thread::spawn(move || {
            use std::io::{Read, Write};
            for reply in replies {
                let Ok((mut socket, _)) = listener.accept() else {
                    return;
                };
                // Read headers, then exactly as many body bytes as
                // Content-Length promises. Reading "until it stops" would
                // race the client's own write of a large PUT body.
                let mut buf = Vec::new();
                let mut chunk = [0u8; 8192];
                let head_end = loop {
                    match socket.read(&mut chunk) {
                        Ok(0) => break buf.len(),
                        Ok(n) => {
                            buf.extend_from_slice(&chunk[..n]);
                            if let Some(i) = find_headers_end(&buf) {
                                break i;
                            }
                        }
                        Err(_) => break buf.len(),
                    }
                };
                let head = String::from_utf8_lossy(&buf[..head_end.min(buf.len())]).to_string();
                let want: usize = head
                    .lines()
                    .find_map(|l| {
                        let (k, v) = l.split_once(':')?;
                        k.eq_ignore_ascii_case("content-length")
                            .then(|| v.trim().parse().ok())?
                    })
                    .unwrap_or(0);
                while buf.len() < head_end + want {
                    match socket.read(&mut chunk) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => buf.extend_from_slice(&chunk[..n]),
                    }
                }
                let line = head.lines().next().unwrap_or("").to_string();
                seen_w.lock().expect("stub log").push(line);
                let body = String::from_utf8_lossy(&buf[head_end.min(buf.len())..]).to_string();
                bodies_w.lock().expect("stub log").push(body);

                // After the request body is fully read, so this models a slow
                // *answer* rather than a slow upload — which is what Azure
                // does on a large PUT and what the budget has to survive.
                if !reply.delay.is_zero() {
                    std::thread::sleep(reply.delay);
                }

                let response = format!(
                    "HTTP/1.1 {} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    reply.status,
                    reply.body.len(),
                    reply.body
                );
                let _ = socket.write_all(response.as_bytes());
                let _ = socket.flush();
                served_w.fetch_add(1, Ordering::SeqCst);
            }
        });

        Stub {
            addr,
            seen,
            bodies,
            served,
        }
    }

    fn find_headers_end(buf: &[u8]) -> Option<usize> {
        buf.windows(4).position(|w| w == b"\r\n\r\n").map(|i| i + 4)
    }

    impl Stub {
        fn base(&self) -> String {
            format!("http://{}", self.addr)
        }
        fn requests(&self) -> Vec<String> {
            self.seen.lock().expect("stub log").clone()
        }
        fn bodies(&self) -> Vec<String> {
            self.bodies.lock().expect("stub log").clone()
        }
    }

    fn client(base: &str) -> JobClient {
        JobClient::new(reqwest::Client::new(), base, "test-key")
    }

    /// Millisecond-scaled budgets with the same *shape* as the real ones: a
    /// small fixed JSON budget, and an upload budget whose fixed part is
    /// smaller still but which grows with the bytes. A fixture of a few dozen
    /// bytes against a 1 byte/s floor buys tens of seconds of headroom, which
    /// is what makes the "big upload survives a slow answer" assertion mean
    /// the same thing here as it does at 128 KiB/s in production.
    fn ms_budgets() -> Budgets {
        Budgets {
            json: Duration::from_millis(150),
            download: Duration::from_millis(150),
            upload_base: Duration::from_millis(150),
            upload_floor_bytes_per_sec: 1,
        }
    }

    /// A throwaway audio file with a *chosen* basename.
    ///
    /// The name matters: `upload` keys the presigned URL by the file's own
    /// basename, so a canned stub reply and the fixture have to agree on it.
    /// Uniqueness therefore lives in a per-test parent directory rather than
    /// in the filename, and the whole directory goes on drop so a failing
    /// assertion leaves nothing behind.
    struct TempAudio(std::path::PathBuf);

    impl TempAudio {
        fn new(name: &str) -> TempAudio {
            let dir = std::env::temp_dir().join(format!(
                "bs-job-{}-{:?}",
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .expect("system clock is after the epoch")
                    .as_nanos(),
                std::thread::current().id()
            ));
            std::fs::create_dir_all(&dir).expect("create the fixture directory");
            let path = dir.join(name);
            std::fs::write(&path, b"RIFFxxxxWAVEfmt ").expect("write the fixture");
            TempAudio(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempAudio {
        fn drop(&mut self) {
            if let Some(dir) = self.0.parent() {
                let _ = std::fs::remove_dir_all(dir);
            }
        }
    }

    /// The full five-call lifecycle against one stub, in order, ending in a
    /// parsed transcript. The presigned URLs the stub hands back point at
    /// itself — which is the point: it proves the client *follows* the URL
    /// the server returned instead of assuming a storage host.
    #[tokio::test]
    async fn the_whole_job_lifecycle_runs_in_order_and_yields_segments() {
        // The upload leg keys the presigned URL by the name the CLIENT sends,
        // which is the generic `audio.<ext>` rather than the file's own
        // basename — hence the canned reply below is keyed "audio.wav" while
        // the fixture on disk is called anything at all. It gets its own
        // directory so parallel tests cannot collide.
        let audio = TempAudio::new("clip.wav");

        let s = stub_with(|addr| {
            let self_url = format!("http://{addr}/blob");
            vec![
                // create -> 202, the real success code
                Reply {
                    status: 202,
                    body: r#"{"job_id":"20260906_abc-def","job_state":"Accepted"}"#.into(),
                    delay: Duration::ZERO,
                },
                // upload-files -> a presigned URL pointing back at the stub
                Reply::ok(format!(
                    r#"{{"job_id":"20260906_abc-def","upload_urls":{{"audio.wav":{{"file_url":"{self_url}"}}}}}}"#
                )),
                // the PUT of the bytes
                Reply {
                    status: 201,
                    body: String::new(),
                    delay: Duration::ZERO,
                },
                // start
                Reply::ok(r#"{"job_state":"Pending"}"#),
                // status: still running, then completed
                Reply::ok(r#"{"job_state":"Running","job_details":[]}"#),
                Reply::ok(
                    r#"{"job_state":"Completed","job_details":[{"outputs":[{"file_name":"0.json"}],"state":"Success"}]}"#,
                ),
                // download-files -> presigned URL, then the payload
                Reply::ok(format!(
                    r#"{{"download_urls":{{"0.json":{{"file_url":"{self_url}"}}}}}}"#
                )),
                Reply::ok(LIVE_SHAPE),
            ]
        });

        let c = client(&s.base());
        let job = c.create_job(&JobCfg::default()).await.expect("create");
        assert_eq!(job.as_str(), "20260906_abc-def");
        c.upload(&job, audio.path()).await.expect("upload");
        c.start(&job).await.expect("start");

        let schedule = PollSchedule {
            first: Duration::from_millis(5),
            max: Duration::from_millis(10),
            deadline: Duration::from_secs(5),
        };
        let outputs = c
            .wait_for_completion(&job, &schedule, std::future::pending())
            .await
            .expect("wait");
        assert_eq!(outputs, vec!["0.json".to_string()]);

        let (segments, text) = c.download(&job, &outputs).await.expect("download");
        assert_eq!(segments.len(), 5);
        assert!(text.starts_with("PLACEHOLDER CHUNK ONE."));

        let reqs = s.requests();
        assert_eq!(reqs.len(), 8, "eight calls: {reqs:?}");
        assert!(reqs[0].starts_with("POST / "), "{}", reqs[0]);
        assert!(reqs[1].contains("/upload-files"), "{}", reqs[1]);
        assert!(reqs[2].starts_with("PUT /blob"), "{}", reqs[2]);
        assert!(reqs[3].contains("/20260906_abc-def/start"), "{}", reqs[3]);
        assert!(reqs[4].contains("/20260906_abc-def/status"), "{}", reqs[4]);
        assert!(reqs[5].contains("/status"), "{}", reqs[5]);
        assert!(reqs[6].contains("/download-files"), "{}", reqs[6]);
        assert!(reqs[7].starts_with("GET /blob"), "{}", reqs[7]);
    }

    /// End to end: a 429 must be a pause, not a failed import. Anything that
    /// put 429 in a fatal set would fail here.
    #[tokio::test]
    async fn a_429_is_retried_and_the_call_still_succeeds() {
        let s = stub(vec![
            Reply::code(429),
            Reply::code(429),
            Reply {
                status: 202,
                body: r#"{"job_id":"20260906_after-throttle"}"#.into(),
                delay: Duration::ZERO,
            },
        ]);
        let c = client(&s.base()).with_retry_policy(RetryPolicy {
            max_attempts: 5,
            base: Duration::from_millis(5),
            factor: 2,
            max: Duration::from_millis(20),
            jitter: Duration::from_millis(1),
        });
        let job = c.create_job(&JobCfg::default()).await.expect("must survive two 429s");
        assert_eq!(job.as_str(), "20260906_after-throttle");
        assert_eq!(s.served.load(Ordering::SeqCst), 3);
    }

    /// ...but a throttle that never clears must still end, rather than
    /// retrying forever.
    #[tokio::test]
    async fn a_429_that_never_clears_gives_up_after_the_budget() {
        let s = stub(vec![
            Reply::code(429),
            Reply::code(429),
            Reply::code(429),
        ]);
        let c = client(&s.base()).with_retry_policy(RetryPolicy {
            max_attempts: 3,
            base: Duration::from_millis(2),
            factor: 2,
            max: Duration::from_millis(5),
            jitter: Duration::from_millis(1),
        });
        assert_eq!(
            c.create_job(&JobCfg::default()).await,
            Err(JobError::RateLimited)
        );
        assert_eq!(s.served.load(Ordering::SeqCst), 3, "exactly max_attempts calls");
    }

    /// Sarvam's empty-bodied 400 must not be mistaken for a parse failure.
    ///
    /// Not provoked with `or-IN`: the client translates that code, so it
    /// never reaches the wire and never *causes* a 400. A nonsense code
    /// stands in.
    #[tokio::test]
    async fn an_empty_bodied_400_is_a_rejection_not_a_malformed_reply() {
        let s = stub(vec![Reply::code(400)]);
        let c = client(&s.base());
        let cfg = JobCfg {
            language_code: "zz-ZZ".into(),
            ..JobCfg::default()
        };
        assert_eq!(
            c.create_job(&cfg).await,
            Err(JobError::Rejected { status: 400 })
        );
    }

    /// Odia, asserted where it matters: on the wire.
    ///
    /// This endpoint answers `or-IN` with an empty-bodied 400 and accepts
    /// `od-IN`. The mapping test above exercises `to_rest_language_code` in
    /// isolation, so only this one proves `create_job` applies it rather than
    /// sending `cfg.language_code` as given.
    #[tokio::test]
    async fn odia_reaches_the_wire_as_od_in_even_when_the_caller_says_or_in() {
        let s = stub(vec![Reply {
            status: 202,
            body: r#"{"job_id":"20260906_odia"}"#.into(),
            delay: Duration::ZERO,
        }]);
        let cfg = JobCfg {
            language_code: "or-IN".into(),
            ..JobCfg::default()
        };
        client(&s.base()).create_job(&cfg).await.expect("create");

        let body = s.bodies().first().cloned().unwrap_or_default();
        assert!(
            body.contains(r#""language_code":"od-IN""#),
            "the realtime spelling must be translated before it is sent: {body}"
        );
        assert!(
            !body.contains("or-IN"),
            "or-IN must never reach this endpoint — it answers an empty 400: {body}"
        );
    }

    /// The auto-detect sentinel travels the same road: the app's `"auto"`
    /// is this endpoint's `"unknown"`, verified live.
    #[tokio::test]
    async fn the_auto_sentinel_reaches_the_wire_as_unknown() {
        let s = stub(vec![Reply {
            status: 202,
            body: r#"{"job_id":"20260906_auto"}"#.into(),
            delay: Duration::ZERO,
        }]);
        let cfg = JobCfg {
            language_code: "auto".into(),
            ..JobCfg::default()
        };
        client(&s.base()).create_job(&cfg).await.expect("create");
        let body = s.bodies().first().cloned().unwrap_or_default();
        assert!(body.contains(r#""language_code":"unknown""#), "{body}");
    }

    /// The user's filename is not Sarvam's business.
    ///
    /// The name becomes the blob's name in Sarvam's storage, and a filename
    /// is content — "Therapy 2026-09-01.m4a" says most of what the recording
    /// says. It is also the exact-match key for the presigned URL, so a name
    /// the server normalizes would fail a healthy file as `Malformed`.
    #[tokio::test]
    async fn the_users_filename_never_reaches_sarvam() {
        let s = stub(vec![Reply::ok(
            r#"{"upload_urls":{"audio.m4a":{"file_url":"http://127.0.0.1:9/blob"}}}"#,
        )]);
        let audio = TempAudio::new("Therapy session 2026-09-01.m4a");
        // The PUT itself goes to a dead port, so this errors — the assertion
        // is about what the FIRST request carried, which has already happened.
        let _ = client(&s.base())
            .upload(&JobId("20260906_x".into()), audio.path())
            .await;

        let body = s.bodies().first().cloned().unwrap_or_default();
        assert!(
            body.contains(r#""files":["audio.m4a"]"#),
            "the wire name must be generic: {body}"
        );
        assert!(
            !body.to_lowercase().contains("therapy"),
            "a filename is content and must not leave the machine: {body}"
        );
    }

    #[test]
    fn the_upload_name_keeps_only_a_safe_extension() {
        assert_eq!(upload_file_name(Path::new("C:/x/Therapy.m4a")), "audio.m4a");
        // Case-folded, so the key matches whatever the server echoes.
        assert_eq!(upload_file_name(Path::new("C:/x/CLIP.WAV")), "audio.wav");
        // No extension, a non-ASCII one, or an absurd one degrades to a bare
        // name — Sarvam sniffs the container from the bytes anyway.
        assert_eq!(upload_file_name(Path::new("C:/x/recording")), "audio");
        assert_eq!(upload_file_name(Path::new("C:/x/गाना.गान")), "audio");
        assert_eq!(upload_file_name(Path::new("C:/x/a.verylongextension")), "audio");
    }

    // ------------------------------------------------- per-leg budgets --

    /// The upload's budget must scale with the bytes, so a slow *answer* to a
    /// large PUT survives — while the same delay on a small JSON leg, which
    /// gets no such credit, does not. Both halves are asserted in one test
    /// because the claim is the *difference* between them: a single
    /// client-wide timeout cannot produce it.
    ///
    /// The fixture is 44 bytes against a 1 byte/s floor, so its upload budget
    /// is ~44 s while the JSON budget is 150 ms. The stub sits on both for
    /// 400 ms.
    #[tokio::test]
    async fn the_upload_budget_grows_with_the_file_while_the_json_legs_stay_short() {
        let audio = TempAudio::new("clip.wav");
        let byte_count = std::fs::metadata(audio.path()).expect("stat fixture").len();
        assert!(
            byte_count > 0,
            "the fixture must have bytes for the budget to scale with"
        );
        let slow = Duration::from_millis(400);

        // The PUT: answered late, and must still succeed.
        let put_stub = stub_with(|addr| {
            let self_url = format!("http://{addr}/blob");
            vec![
                Reply::ok(format!(
                    r#"{{"upload_urls":{{"audio.wav":{{"file_url":"{self_url}"}}}}}}"#
                )),
                Reply::code(201).after(slow),
            ]
        });
        let put_client = client(&put_stub.base()).with_budgets(ms_budgets());
        let started = std::time::Instant::now();
        put_client
            .upload(&JobId("20260906_slow_put".into()), audio.path())
            .await
            .expect("a large upload must survive an answer slower than the JSON budget");
        assert!(
            started.elapsed() >= slow,
            "the test has to have actually waited out the delay"
        );

        // The status GET: the same delay, no size credit, must time out.
        let get_stub = stub(vec![
            Reply::ok(r#"{"job_state":"Running","job_details":[]}"#).after(slow)
        ]);
        let get_client = client(&get_stub.base()).with_budgets(ms_budgets());
        let out = get_client.poll(&JobId("20260906_slow_get".into())).await;
        assert_eq!(
            out,
            Err(JobError::Network(NetFailure::Timeout)),
            "a JSON leg gets no size credit, so the same delay must end it"
        );
    }

    /// The arithmetic, at the real constants.
    #[test]
    fn the_default_upload_budget_clears_a_real_recording_on_a_slow_uplink() {
        let b = Budgets::default();
        // A 60-minute 128 kbps MP3.
        let sixty_min_mp3 = 57 * 1024 * 1024;
        assert!(
            b.upload(sixty_min_mp3) >= Duration::from_secs(500),
            "a ~57 MB upload needs minutes of budget, not the 60 s a flat \
             read_timeout gave it"
        );
        // And the ceiling `media::probe` actually allows must be reachable.
        let ceiling = crate::media::probe::MAX_IMPORT_BYTES;
        assert!(b.upload(ceiling) >= Duration::from_secs(60 * 60));
        // Small files still get a short, sane budget.
        assert_eq!(b.upload(0), UPLOAD_TIMEOUT_BASE);
    }

    /// A zero floor rate is only reachable from a test, and must not panic.
    #[test]
    fn a_zero_floor_rate_falls_back_to_the_base_budget() {
        let b = Budgets {
            upload_floor_bytes_per_sec: 0,
            ..Budgets::default()
        };
        assert_eq!(b.upload(1_000_000), UPLOAD_TIMEOUT_BASE);
    }

    #[tokio::test]
    async fn a_401_is_reported_as_a_key_problem() {
        let s = stub(vec![Reply::code(401)]);
        assert_eq!(
            client(&s.base()).create_job(&JobCfg::default()).await,
            Err(JobError::Unauthorized)
        );
    }

    /// A job that never leaves `Running` must stop at the deadline rather
    /// than polling until the user closes the app.
    #[tokio::test]
    async fn a_job_that_never_finishes_stops_at_the_deadline() {
        let replies: Vec<Reply> = (0..40)
            .map(|_| Reply::ok(r#"{"job_state":"Running","job_details":[]}"#))
            .collect();
        let s = stub(replies);
        let c = client(&s.base());
        let schedule = PollSchedule {
            first: Duration::from_millis(5),
            max: Duration::from_millis(10),
            deadline: Duration::from_millis(120),
        };
        let started = std::time::Instant::now();
        let out = c
            .wait_for_completion(
                &JobId("20260906_stuck".into()),
                &schedule,
                std::future::pending(),
            )
            .await;
        assert_eq!(out, Err(JobError::TimedOut));
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the deadline must actually bound the wait, took {:?}",
            started.elapsed()
        );
    }

    /// A deadline is not a cancellation. Without this the user's Cancel could
    /// not be felt until the current poll gap ran out — up to `POLL_MAX` on a
    /// short file, and up to `DEADLINE_CEILING` on a long one.
    #[tokio::test]
    async fn a_cancelled_wait_stops_between_polls_instead_of_running_the_deadline_out() {
        let replies: Vec<Reply> = (0..40)
            .map(|_| Reply::ok(r#"{"job_state":"Running","job_details":[]}"#))
            .collect();
        let s = stub(replies);
        let c = client(&s.base());
        let schedule = PollSchedule {
            first: Duration::from_millis(20),
            max: Duration::from_millis(20),
            // Deliberately far longer than the test can afford to wait: if the
            // cancellation did nothing, this test would hang rather than fail
            // quietly on a coincidence.
            deadline: Duration::from_secs(600),
        };
        let stop = tokio::sync::Notify::new();
        let job = JobId("20260906_slow".into());
        let started = std::time::Instant::now();
        let (out, ()) = tokio::join!(
            c.wait_for_completion(&job, &schedule, stop.notified()),
            async {
                tokio::time::sleep(Duration::from_millis(60)).await;
                stop.notify_waiters();
            }
        );
        assert_eq!(out, Err(JobError::Cancelled));
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the cancellation must end the wait, took {:?}",
            started.elapsed()
        );
    }

    /// `biased` in the select: a caller who cancelled before the wait began
    /// must not spend a single poll. Otherwise cancelling an item that has
    /// just been started still costs a request against an endpoint with an
    /// undocumented rate limit.
    #[tokio::test]
    async fn an_already_cancelled_wait_never_polls_at_all() {
        let s = stub(vec![Reply::ok(
            r#"{"job_state":"Completed","job_details":[{"outputs":[{"file_name":"0.json"}]}]}"#,
        )]);
        let c = client(&s.base());
        let schedule = PollSchedule {
            first: Duration::from_millis(5),
            max: Duration::from_millis(10),
            deadline: Duration::from_secs(30),
        };
        let out = c
            .wait_for_completion(
                &JobId("20260906_done".into()),
                &schedule,
                std::future::ready(()),
            )
            .await;
        assert_eq!(out, Err(JobError::Cancelled));
        assert_eq!(
            s.served.load(Ordering::SeqCst),
            0,
            "an already-cancelled wait must not send a status request"
        );
    }

    /// One status poll that meets a 5xx or a network blip says nothing about
    /// the job, which is still running and already billed. The wait goes on.
    #[tokio::test]
    async fn a_poll_that_fails_now_and_then_does_not_end_the_wait() {
        let s = stub(vec![
            Reply::ok(r#"{"job_state":"Running","job_details":[]}"#),
            Reply::code(503),
            Reply::code(502),
            Reply::ok(r#"{"job_state":"Running","job_details":[]}"#),
            Reply::code(500),
            Reply::ok(
                r#"{"job_state":"Completed","job_details":[{"outputs":[{"file_name":"0.json"}]}]}"#,
            ),
        ]);
        let c = client(&s.base());
        let schedule = PollSchedule {
            first: Duration::from_millis(5),
            max: Duration::from_millis(10),
            deadline: Duration::from_secs(30),
        };
        let out = c
            .wait_for_completion(&JobId("20260906_blip".into()), &schedule, std::future::pending())
            .await;
        assert_eq!(out, Ok(vec!["0.json".to_string()]));
    }

    /// A status route that keeps failing is not a blip: the wait ends with
    /// the failure rather than running the whole deadline out.
    #[tokio::test]
    async fn polls_that_keep_failing_end_the_wait() {
        let s = stub((0..20).map(|_| Reply::code(503)).collect());
        let c = client(&s.base());
        let schedule = PollSchedule {
            first: Duration::from_millis(5),
            max: Duration::from_millis(10),
            deadline: Duration::from_secs(30),
        };
        let out = c
            .wait_for_completion(&JobId("20260906_down".into()), &schedule, std::future::pending())
            .await;
        assert_eq!(out, Err(JobError::ServiceError { status: 503 }));
        assert!(s.served.load(Ordering::SeqCst) < 20, "the wait must stop on its own");
    }

    /// A server-side job failure must surface as `JobFailed` with the
    /// server's own reason, not as a timeout.
    #[tokio::test]
    async fn a_failed_job_ends_the_wait_immediately() {
        let s = stub(vec![Reply::ok(
            r#"{"job_state":"Failed","error_message":"","job_details":[{"outputs":[],"state":"API Error","error_message":"unsupported audio"}]}"#,
        )]);
        let c = client(&s.base());
        let schedule = PollSchedule {
            first: Duration::from_millis(5),
            max: Duration::from_millis(10),
            deadline: Duration::from_secs(30),
        };
        assert_eq!(
            c.wait_for_completion(
                &JobId("20260906_bad".into()),
                &schedule,
                std::future::pending()
            )
            .await,
            Err(JobError::JobFailed {
                reason: "unsupported audio".into()
            })
        );
    }

    /// The presigned PUT is the one call in the lifecycle that must NOT carry
    /// the API key (the SAS token is the credential, and the storage host is
    /// not Sarvam's API) and MUST carry `x-ms-blob-type`, without which Azure
    /// refuses the write. Both are asserted off the bytes actually sent.
    #[tokio::test]
    async fn the_presigned_put_sends_the_blob_type_header_and_not_the_api_key() {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        let captured = Arc::new(std::sync::Mutex::new(String::new()));
        let cap_w = Arc::clone(&captured);

        std::thread::spawn(move || {
            // 1: upload-files. 2: the PUT whose headers this test is about.
            for i in 0..2 {
                let Ok((mut socket, _)) = listener.accept() else {
                    return;
                };
                let mut buf = Vec::new();
                let mut chunk = [0u8; 8192];
                loop {
                    match socket.read(&mut chunk) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            buf.extend_from_slice(&chunk[..n]);
                            if find_headers_end(&buf).is_some() {
                                break;
                            }
                        }
                    }
                }
                let head = String::from_utf8_lossy(&buf).to_string();
                let body = if i == 0 {
                    cap_w.lock().unwrap().clear();
                    format!(
                        r#"{{"upload_urls":{{"audio.wav":{{"file_url":"http://{addr}/blob"}}}}}}"#
                    )
                } else {
                    *cap_w.lock().unwrap() = head;
                    String::new()
                };
                let resp = format!(
                    "HTTP/1.1 200 X\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = socket.write_all(resp.as_bytes());
            }
        });

        let audio = TempAudio::new("clip.wav");
        let c = client(&format!("http://{addr}"));
        c.upload(&JobId("20260906_put".into()), audio.path())
            .await
            .expect("upload");

        let head = captured.lock().unwrap().clone();
        assert!(
            head.to_ascii_lowercase().contains("x-ms-blob-type: blockblob"),
            "Azure's Put Blob refuses without it: {head}"
        );
        assert!(
            !head.to_ascii_lowercase().contains(super::super::AUTH_HEADER),
            "the API key must never reach a storage host: {head}"
        );
    }

    /// A file that vanished between the probe and the upload must be an
    /// error, not a panic — the picker hands over a path, and a path is a
    /// promise about the past.
    #[tokio::test]
    async fn a_file_that_disappeared_before_upload_is_an_error_not_a_panic() {
        // The presigned URL is handed out under the name the client actually
        // asks for — the generic one — so the upload gets all the way to the
        // read before it fails. That is the failure this test is about, rather
        // than the lookup miss the test below covers.
        let s = stub(vec![Reply::ok(
            r#"{"upload_urls":{"audio.wav":{"file_url":"http://127.0.0.1:9/blob"}}}"#,
        )]);
        let c = client(&s.base());
        let missing = std::env::temp_dir().join("gone.wav");
        let _ = std::fs::remove_file(&missing);
        match c.upload(&JobId("20260906_x".into()), &missing).await {
            Err(JobError::Network(_)) => {}
            other => panic!("expected the read to fail as a classified error, got {other:?}"),
        }
    }

    /// An `upload_urls` map that does not contain the file that was asked for
    /// is malformed, not a silent no-op that would leave `start` transcribing
    /// nothing.
    #[tokio::test]
    async fn an_upload_reply_missing_our_file_is_malformed() {
        let s = stub(vec![Reply::ok(
            r#"{"upload_urls":{"someone-elses.wav":{"file_url":"http://127.0.0.1:9/x"}}}"#,
        )]);
        let audio = TempAudio::new("clip.wav");
        let out = client(&s.base())
            .upload(&JobId("20260906_x".into()), audio.path())
            .await;
        assert_eq!(out, Err(JobError::Malformed));
    }

    /// Nothing listening at all: the failure must land in the errno→prose
    /// taxonomy rather than as an opaque error.
    #[tokio::test]
    async fn an_unreachable_endpoint_is_classified_as_a_network_failure() {
        // Port 9 (discard) on loopback refuses rather than hanging.
        let c = client("http://127.0.0.1:9");
        match c.create_job(&JobCfg::default()).await {
            Err(JobError::Network(f)) => {
                assert!(f.is_retryable(), "a refused connect is worth retrying");
            }
            other => panic!("expected a classified network failure, got {other:?}"),
        }
    }
}
