use std::time::Instant;

/// How much a dictation was fixed up between the raw transcript and the
/// final text, for the Insights "fixes" card.
#[derive(Debug, Clone, Copy, Default)]
pub struct FixCounts {
    /// Word-level edits made by the cleanup pipeline + AI polish.
    pub words_corrected: u32,
    /// Dictionary correction rules + snippet expansions that fired.
    pub dict_fixes: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    PushToTalk,
    HandsFree,
}

#[derive(Debug)]
pub enum DictationState {
    Idle,
    Recording { mode: Mode, started: Instant },
    Finalizing { req_id: u64 },
    Injecting,
}

/// Every message the controller thread consumes. Hotkeys, audio, ASR results
/// and injection completion all funnel through one channel so the state
/// machine has a single, ordered view of the world.
pub enum ControlMsg {
    /// A dictation chord went down, carrying which one it was — the main
    /// push-to-talk binding, the translate chord or the voice-agent chord.
    /// The intent is stamped on the session here, at the start, so what the
    /// user asked for is decided before a word is spoken rather than guessed
    /// from the transcript afterwards.
    ChordDown(crate::routes::ChordKind),
    /// The held dictation chord was released. Intentionally intent-free:
    /// whichever chord started the recording, the release means the same
    /// thing, and the hook only ever holds one at a time.
    ChordUp,
    Escape,
    /// 16 kHz mono samples; only flows while the audio gate is open.
    Audio(Vec<f32>),
    /// RMS level of the most recent chunk, for the overlay waveform + meter.
    Level(f32),
    /// Marker sent by the audio pump after it flushed the tail on gate close.
    AudioTail,
    /// The audio stream died (device unplugged / no device).
    AudioFailed,
    FinalResult {
        req_id: u64,
        text: String,
        /// The verbatim transcript, before rule cleanup or AI formatting —
        /// Undo AI Edit restores this when the user rejects a polish.
        raw: String,
        fixes: FixCounts,
        /// Set when the formatter's output was rejected by the guardrail (or
        /// the model call failed outright) and `text` fell back to the
        /// rule-cleaned pipeline output. A silent fallback would let a dead
        /// model go unnoticed, so a rejection is always reported.
        notice: Option<String>,
        /// The service ended this dictation before the user did — the
        /// relay's weekly or 30-minute limit — and `text` is everything
        /// transcribed up to that point. Still pasted (it is whole as far as
        /// it goes, and `notice` says why it stops); the History row carries
        /// `cloud-limit` so it reads as cut short rather than as a dictation
        /// the user ended. `false` on every other path.
        cut_short: bool,
    },
    /// A cloud session ended while the user was still recording, after some
    /// words had already been transcribed, and nothing can transcribe what
    /// they say next: the relay ended it on purpose (`4029` or `4030`), or
    /// the socket died with no batch rescue left (`sarvam::ws::rescue_left`).
    /// The controller stops the recording right there, the way a released key
    /// would, so the session's result arrives now — pasted with the limit's
    /// notice, or filed as a truncation — instead of after the user has
    /// talked on into a closed socket. `session` guards against a stale end
    /// the way a mid-recording `CloudError` does.
    CloudEnded {
        session: u64,
    },
    /// A network transcription attempt failed — a Sarvam realtime session, or
    /// a request to the user's own endpoint (`asr::custom`). `req_id == 0`
    /// means the failure happened mid-recording, before any finalize request
    /// existed (real request ids start at 1); in that case `session`
    /// identifies which cloud session died so a stale error can't cancel the
    /// recording that replaced it.
    ///
    /// The custom-endpoint path always sends a real `req_id` and `session:
    /// 0`: the whole utterance is already recorded before a single byte goes
    /// out, so it can never fail mid-recording, and it has no session counter
    /// to be raced by — the `Recording` arm's `session` guard is unreachable
    /// from it, and the `Finalizing` arm matches on `req_id` alone.
    CloudError {
        req_id: u64,
        session: u64,
        message: String,
    },
    /// A Sarvam session that produced a transcript known to be *incomplete*:
    /// the socket died mid-utterance, and the batch fallback could not
    /// re-transcribe the whole thing from the tee (see
    /// `sarvam::ws::DrainOutcome::Truncated`). The user kept speaking past
    /// the point this text stops.
    ///
    /// Deliberately not a `FinalResult` with a `notice`: the fragment must
    /// not be pasted at all. Pasting the first half of someone's sentence and
    /// flashing a note about it still leaves them to notice, diagnose and
    /// delete it — the silent-truncation class this project treats as its
    /// worst failure. And deliberately not a plain `CloudError` either, which
    /// would throw the words away: they went through the same rules → polish
    /// → guardrail pipeline as any other dictation, so they are worth filing
    /// in History where the user can go and get them.
    ///
    /// `text`/`raw` are exactly what `FinalResult` would have carried; the
    /// controller files them and reports `message` (the drain's own
    /// connection-lost error) instead of injecting anything.
    CloudTruncated {
        req_id: u64,
        text: String,
        /// The verbatim transcript, before rule cleanup or AI formatting —
        /// recorded alongside `text` exactly as `FinalResult`'s is.
        raw: String,
        message: String,
    },
    /// A deferred route (`routes::RouteOutcome::Deferred`) finished.
    ///
    /// Carries `req_id` for the same reason `FinalResult` does: the job runs
    /// on the async runtime, outside the controller's ordering, so its answer
    /// can arrive after the dictation that asked for it was cancelled,
    /// superseded by a new recording, or given up on by the finalize
    /// watchdog. The controller applies it only while it is still holding
    /// that dictation's transcript (`route_result_is_current`); otherwise it
    /// is dropped, because pasting into whatever the user is doing now would
    /// be worse than losing it.
    ///
    /// `text: None` means paste nothing, exactly as in `RouteOutcome`.
    RouteResult {
        req_id: u64,
        text: Option<String>,
        notice: Option<String>,
        /// `routes::RouteDone::pasted` — the route already put `text` into the
        /// document (the selection lane's verify-then-replace, which has to
        /// paste inside the verification window it opened). The controller
        /// files the row and stops rather than injecting it a second time.
        pasted: bool,
    },
    /// Watchdog: finalization took too long (network hang, dropped reply).
    /// Covers a deferred route too — it waits inside the same `Finalizing`
    /// state, under the same clock, rather than starting a second one.
    FinalizeTimeout { req_id: u64 },
    /// The injection thread finished. `injected` is false when the paste
    /// never landed: the clipboard failed, or an Undo's readback could not
    /// confirm that the selection really was the app's own text. The
    /// controller only arms Undo when this is true — a record for a paste
    /// that never happened points at the user's own pre-existing characters,
    /// and undoing it would delete them.
    InjectionDone { injected: bool },
    /// A transform shortcut fired (index into settings.transforms).
    TransformChord(usize),
    /// An app shortcut fired (hotkeys::SHORTCUT_* index).
    AppShortcut(usize),
    /// Deferred overlay hide (e.g. after an error flash).
    HideOverlay,
    /// The OS reported waking from suspend (`WM_POWERBROADCAST` /
    /// `PBT_APMRESUMEAUTOMATIC`, watched by `system_events::spawn`). A cloud
    /// session's socket does not survive S3 sleep, so any in-flight
    /// dictation on the cloud provider is invalidated rather than left to
    /// wait out timeouts against a connection that is already dead.
    SystemResumed,
    /// Windows locked the session (`WTS_SESSION_LOCK`, watched by
    /// `system_events::spawn`). A live recording is cancelled: nothing is
    /// pasted and nothing is filed, because whatever the microphone heard
    /// after the lock is the room, not a dictation. Sent straight to the
    /// controller at the lock, rather than left to the keyboard hook, which
    /// hears nothing until the session is unlocked.
    SessionLocked,
    /// The tray's Pause item turned dictation off. A live recording is
    /// cancelled the same way as for `SessionLocked`: with the hook paused,
    /// no key could end it.
    Paused,
    /// The user deleted a dictation in History, or cleared History. The
    /// controller keeps the last dictation in memory for Paste and Copy last
    /// transcript; a deleted dictation must not come back through them.
    HistoryRemoved(HistoryRemoval),
}

/// What a History delete or clear took away, for [`ControlMsg::HistoryRemoved`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HistoryRemoval {
    /// History was cleared.
    All,
    /// One dictation was deleted: the row's text and its verbatim transcript.
    Row { text: String, raw: Option<String> },
}
