//! The state machine and orchestration hub. Single consumer of ControlMsg;
//! owns the utterance buffer and drives recording (push-to-talk and
//! double-tap hands-free), the overlay pill, finalization and injection.

use crate::asr::offline::AsrMsg;
use crate::events;
use crate::overlay;
use crate::sarvam::{CloudCmd, Endpointing, SessionCfg};
use crate::settings::{Provider, Settings};
use crate::speech_gate;
use crate::state::{ControlMsg, DictationState, Mode};
use crate::tones::{self, Cue};
use crossbeam_channel::{Receiver, Sender};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};
use tauri::Emitter;

const TAIL_FLUSH_TIMEOUT_MS: u64 = 250;
const ERROR_FLASH_MS: u64 = 1800;

/// Push-to-talk safety stop: a chord held this long finalizes exactly as if
/// the user had released it, rather than recording forever. Hands-free is
/// exempt — a long hands-free session is the feature working as intended,
/// not a stuck key.
const MAX_PUSH_DURATION: Duration = Duration::from_millis(300_000);

/// After any stop, a chord-down within this window is treated as key
/// chatter (mechanical switch bounce, or a bounced synthetic event from the
/// hook) rather than a deliberate new press, and is ignored. Armed only by
/// `finish_recording` — a tap-discard (`cancel_recording` from `ChordUp`)
/// deliberately arms `last_tap` instead, for the double-tap-to-hands-free
/// gesture, which needs a fast *second* press to land, not be swallowed.
const POST_STOP_COOLDOWN: Duration = Duration::from_millis(300);

/// The notice for an utterance the gate judged too quiet to be speech.
/// Shared between the speech gate (silence detected before finalization was
/// even attempted) and an empty ASR/cloud result (silence that made it all
/// the way through) — same user-facing meaning either way.
const QUIET_NOTICE: &str = "Didn't catch that";

/// Distinct from `QUIET_NOTICE`: every sample was exactly 0.0, which on
/// Windows means the input device is muted, not that the user spoke softly.
const MUTED_MIC_NOTICE: &str = "Mic seems muted — check Windows Sound settings";

/// `history::NewEntry::error_code` for a `ControlMsg::CloudTruncated` row —
/// the socket died mid-utterance and no whole-utterance fallback existed, so
/// what is stored is a known fragment. Short and machine-stable: History
/// turns each code into a sentence of its own, so a new code here needs its
/// sentence there too. It is the first entry in the `error_code` vocabulary
/// the `history` module doc describes.
const ERR_CONNECTION_LOST: &str = "connection-lost";

/// `history::NewEntry::error_code` for a dictation whose route declined to
/// produce anything (`routes::RouteOutcome::text == None`) — today only the
/// voice agent, which must never let a command reach the document as prose.
/// The transcript is real and worth keeping; what failed is the route, so the
/// row says so instead of pretending the dictation completed. Second entry in
/// the `error_code` vocabulary the `history` module doc leaves open.
const ERR_ROUTE_UNAVAILABLE: &str = "route-unavailable";

/// `history::NewEntry::error_code` for a Cloud dictation the relay ended
/// before the user did — its weekly or 30-minute limit. Unlike the codes
/// above, it goes on a `Done` row: the words up to the limit were
/// pasted. The code is what lets History say the dictation was cut short.
const ERR_CLOUD_LIMIT: &str = "cloud-limit";

/// Last resort for a route that withheld its text without saying why. It
/// should be unreachable — a silent skip is precisely the failure the route
/// seam exists to prevent — so the pill says *something* rather than a
/// dictation ending with nothing typed and nothing shown.
const ROUTE_SKIPPED_NOTICE: &str = "Nothing typed — that route didn't run";

/// `history::NewEntry::error_code` for a dictation whose deferred route was
/// still running when the finalize watchdog expired. Distinct from
/// `ERR_ROUTE_UNAVAILABLE`, which is a route that answered and declined: this
/// one never answered at all, and the row exists so the words the controller
/// was holding are not lost with it.
const ERR_ROUTE_TIMEOUT: &str = "route-timeout";

/// `history::NewEntry::error_code` for a dictation whose deferred route was
/// still running when the machine woke from sleep. The transcript had
/// arrived and is filed; the route's connection did not survive the sleep,
/// so nothing is typed.
const ERR_INTERRUPTED: &str = "interrupted";

/// Shown when `ControlMsg::SystemResumed` tears down an in-flight cloud
/// dictation. The socket was talking to Sarvam before the machine slept;
/// TCP/TLS state does not survive S3 suspend, so it is dead the instant the
/// machine wakes even though nothing has told the dispatcher task yet — waiting
/// out its own timeouts would just make the user stare at a pill that can never
/// resolve.
const RESUME_NOTICE: &str = "Woke from sleep — try dictating again";

/// Floor for "that press was a tap, not a dictation". A hold this short can't
/// contain speech, so it is discarded and arms the double-tap window.
///
/// This is deliberately independent of `min_hold_ms` (default 150 ms): real
/// taps hold each press for roughly 100-250 ms, so keyed off 150 ms many taps
/// would take the finalize path instead — flashing "Didn't catch that" over
/// a fifth of a second of silence and clearing `last_tap` on the way, which
/// puts hands-free out of reach.
const TAP_MAX: Duration = Duration::from_millis(350);

/// Whether a push-to-talk hold was a tap rather than a dictation. `min_hold_ms`
/// can raise the bar (a user who wants to guard against brush-presses) but
/// never lower it past `TAP_MAX`, or double-tap stops being detectable.
fn is_tap(held: Duration, min_hold_ms: u64) -> bool {
    held < Duration::from_millis(min_hold_ms).max(TAP_MAX)
}

/// State needed to make "Undo AI Edit" a *verified* select-and-replace
/// instead of a blind paste. Recorded when a dictation result is injected
/// (the `FinalResult` arm of `Controller::handle`, via `next_last_injection`)
/// but only armed once `InjectionDone` confirms the paste actually landed,
/// and cleared by anything that writes to the document afterwards. Never
/// partially written: writing `raw` into a bare field before an early return
/// can skip the rest would let a filler-only utterance ("um") desync it from
/// the text that was actually injected.
#[derive(Debug, Clone, PartialEq, Eq)]
struct LastInjection {
    /// What was actually typed into the target app. Undo reads the selection
    /// back through the clipboard and refuses to replace anything that isn't
    /// byte-for-byte this string.
    text: String,
    /// The verbatim transcript, before rule cleanup or AI formatting.
    raw: String,
    /// When the injection happened, for the `UNDO_WINDOW` pre-filter.
    ///
    /// Not a hard bound: `Instant` is QueryPerformanceCounter-backed on
    /// Windows and does not reliably advance across S3 sleep, so closing the
    /// lid and reopening it can present an hours-old record as seconds old.
    /// That is tolerable only because the readback is what actually protects
    /// the document — a stale record simply fails to match and nothing
    /// happens.
    at: Instant,
    /// Foreground app captured at chord-down (`crate::foreground::capture`'s
    /// `Target::app`).
    ///
    /// This is the **process** name, not the window: Chrome tab A → tab B,
    /// two Notepad windows, and VS Code file A → file B all "match". It is a
    /// cheap filter for the obvious case, not a guarantee that the caret is
    /// where it was — the readback is what provides that.
    app: Option<String>,
}

/// How long an injection stays eligible for the verified select-and-replace.
///
/// A pre-filter, not the safety mechanism. Ctrl+C is not universally "copy"
/// (a terminal may read it as an interrupt), so the readback probe is worth
/// skipping once a record is old enough that the caret has almost certainly
/// moved on. Nothing about correctness rests on this number: past the window
/// Undo still offers the transcript on the clipboard, and inside it the
/// readback still has to agree before a character is touched.
const UNDO_WINDOW: Duration = Duration::from_secs(30);

/// The notice shown whenever Undo declines to touch the document — both when
/// the pre-filter rejects the attempt outright and when the readback refuses
/// to confirm the selection.
const UNDO_COPIED_NOTICE: &str = "Raw transcript copied — press Ctrl+V";

/// What Undo AI Edit should attempt, from what the controller knows before
/// it probes the document.
#[derive(Debug, Clone, PartialEq, Eq)]
enum UndoDecision {
    /// Nothing recorded, or formatting changed nothing — there is genuinely
    /// nothing to undo.
    Nothing,
    /// Worth probing: select `char_count` characters back from the caret,
    /// check they read back as `expected`, and only then paste `replacement`
    /// over them. Reaching this arm is permission to *look*, not permission
    /// to delete — `injection::replace_last` still has the final say.
    Replace {
        char_count: usize,
        expected: String,
        replacement: String,
    },
    /// Not worth probing (stale, wrong app, or a script where "one
    /// character" is ambiguous) — copy `text` to the clipboard and say so
    /// instead of synthesizing keystrokes into a document we have no
    /// business touching.
    CopyOnly { text: String },
}

/// The verbatim transcript as it should be written *back over* an injection:
/// carrying the trailing space [`smart_space_append`] added, when the
/// injected text has one the transcript lacks.
///
/// Smart spacing runs after the formatting pipeline, on the final assembly in
/// the `FinalResult` arm, so a dictation the formatter left alone still
/// reaches `LastInjection` with `text` exactly one space longer than `raw`.
/// Two consequences, and this covers both:
///
/// * [`undo_decision`]'s "formatting changed nothing" guard has to compare
///   against this rather than `rec.raw`, or it can never fire under stock
///   settings — `smart_space` defaults on and the `formal` style is an
///   identity, so an already-punctuated cloud reply arrives here as an
///   "edit" and every dictation looks worth probing;
/// * an undo that *does* fire must not silently delete the space the app
///   itself appended so the user could keep typing. Undoing the AI edit is
///   not undoing the spacing.
///
/// Only the in-place replace needs this. The `CopyOnly` fallback hands the
/// user text to paste wherever they choose, where a trailing space is the
/// app's guess about a caret it no longer knows the position of.
fn undo_replacement(rec: &LastInjection) -> String {
    if rec.text.ends_with(char::is_whitespace) && !rec.raw.ends_with(char::is_whitespace) {
        smart_space_append(rec.raw.clone())
    } else {
        rec.raw.clone()
    }
}

/// Decide what Undo AI Edit should attempt. A free function — `elapsed`,
/// `current_app` and `is_terminal` are passed in rather than computed here —
/// so eligibility is unit-testable without a live `Instant`, a real
/// foreground window, or a whole `Controller` (which owns an `AppHandle` and
/// five channels, so constructing one in a unit test is not reasonable).
///
/// These conditions are a **pre-filter**, not a proof. They cannot justify
/// deleting `char_count` characters on their own — the user may have typed
/// since, clicked elsewhere in the same document, moved to a second window of
/// the same process (`app` is the process name), or slept the machine
/// through the timer. `injection::replace_last` reads the selection back and
/// compares it before pasting, so each condition here only decides whether
/// the probe is worth its cost:
///
/// * recorded text actually differs from the raw transcript — otherwise
///   there is nothing to undo in the first place. Compared against
///   [`undo_replacement`], not `rec.raw`, so the app's own trailing space
///   doesn't count as an edit;
/// * within `UNDO_WINDOW` — past that the caret has almost certainly moved
///   and the probe would just waste a clipboard round-trip;
/// * same foreground app — a probe synthesizes Ctrl+C, which some apps
///   (terminals) read as an interrupt rather than a copy, so it should not
///   fire into a window the text never went into. An app that can't be
///   determined on either side (`None`) counts as a mismatch, not a free
///   pass;
/// * not a terminal — when the paste *did* go to a terminal, same-app
///   passes and the probe's Ctrl+C could reach whatever is running there as
///   an interrupt, so a target `foreground::is_terminal` says is a console is
///   never probed at all, only the clipboard fallback. Unlike its three
///   neighbours this condition is load-bearing for safety, not economy —
///   `foreground::is_terminal`'s doc has why, and why it is necessarily
///   best-effort (a hardcoded list will be wrong for someone; both failure
///   directions are benign; a classic-conhost window is reported as its
///   *attached client*, so a REPL or CLI tool hosted there shows up under its
///   own name, which no list can enumerate);
/// * ASCII-only injected text — Shift+Left moves by one editing unit, which
///   is not reliably one Rust `char` for Devanagari or other combining-mark
///   scripts across this app's 23 supported languages, so `char_count` would
///   select the wrong span. The readback would catch it, but a probe that is
///   *designed* to miscount is not worth firing.
fn undo_decision(
    recorded: Option<&LastInjection>,
    elapsed: Duration,
    current_app: Option<&str>,
    is_terminal: bool,
) -> UndoDecision {
    let Some(rec) = recorded else {
        return UndoDecision::Nothing;
    };
    let replacement = undo_replacement(rec);
    if rec.raw.is_empty() || replacement == rec.text {
        return UndoDecision::Nothing;
    }
    let app_matches = matches!((rec.app.as_deref(), current_app), (Some(a), Some(b)) if a == b);
    let worth_probing =
        elapsed <= UNDO_WINDOW && app_matches && !is_terminal && rec.text.is_ascii();
    if worth_probing {
        UndoDecision::Replace {
            char_count: rec.text.chars().count(),
            expected: rec.text.clone(),
            replacement,
        }
    } else {
        UndoDecision::CopyOnly {
            text: rec.raw.clone(),
        }
    }
}

/// What the injection currently in flight means for the undo bookkeeping,
/// resolved when `InjectionDone` reports whether the paste actually landed.
/// One field rather than two so "a dictation and an undo are both in flight"
/// is unrepresentable; the state machine already makes it unreachable (both
/// paths run through `DictationState::Injecting`, which every entry point
/// refuses to start from), and this keeps it that way by construction.
#[derive(Debug)]
enum InFlight {
    /// A dictation result. Arms Undo with this record — but only if the
    /// paste landed. `inject_text` can fail outright (arboard error, another
    /// process holding the clipboard open) while `start_injection` merely
    /// logs it, and a record armed for a paste that never happened points at
    /// characters the user typed themselves.
    Arm(LastInjection),
    /// An Undo. If the readback couldn't confirm the selection, the document
    /// is untouched and there is nothing on screen to point the user at — so
    /// fall through to the same clipboard fallback the pre-filter uses.
    Undo { raw: String },
}

/// What an `InjectionDone` means for the undo bookkeeping.
#[derive(Debug, PartialEq)]
enum UndoBookkeeping {
    /// Install this as the live undo target: its paste is on screen, ending
    /// at the caret.
    Arm(LastInjection),
    /// Leave Undo disarmed — nothing undo-relevant was in flight, the paste
    /// never landed, or an undo just consumed the record.
    Disarm,
    /// An undo left the document untouched. Nothing on screen changed, so the
    /// record stays exactly as valid as it was; hand the user `raw` on the
    /// clipboard instead.
    FallBackToClipboard { raw: String },
}

/// Resolve an in-flight injection against whether its paste actually landed.
/// A free function so the two rules that keep Undo from destroying text the
/// app never wrote have regression tests without a `Controller`: arm only on
/// success, and treat an unverified replace as "nothing happened".
fn injection_outcome(in_flight: Option<InFlight>, injected: bool) -> UndoBookkeeping {
    match in_flight {
        Some(InFlight::Arm(rec)) if injected => UndoBookkeeping::Arm(rec),
        // The paste failed, so the document still holds whatever the user put
        // there. Arming `rec` would aim a later Shift+Left at their own
        // characters.
        Some(InFlight::Arm(_)) => UndoBookkeeping::Disarm,
        // The replace landed: the caret now sits after the verbatim
        // transcript, so there is nothing left to undo.
        Some(InFlight::Undo { .. }) if injected => UndoBookkeeping::Disarm,
        Some(InFlight::Undo { raw }) => UndoBookkeeping::FallBackToClipboard { raw },
        // Not an undo-relevant injection (paste-last). `start_injection`
        // already dropped the record, since that paste moved the caret.
        None => UndoBookkeeping::Disarm,
    }
}

/// Which engine will transcribe the dictation that is starting.
///
/// A three-way snapshot rather than bools, because there are three transcribers
/// and two bools would make "both at once" representable. Resolved once, at
/// chord-down (`stt_path`), and carried through the whole dictation: everything
/// downstream — whether a cloud session gets torn down, which watchdog is
/// armed, what History calls the row — has to describe the dictation as it
/// actually ran.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum SttPath {
    /// Sarvam realtime streaming (`sarvam::ws`).
    #[default]
    Cloud,
    /// The on-device model (`asr::offline`).
    Local,
    /// The user's own OpenAI-compatible endpoint (`asr::custom`) — recorded
    /// whole, then posted once, which is the local path's shape.
    Custom,
}

impl SttPath {
    /// What a history row calls the engine that produced it.
    ///
    /// The first two are exactly the strings `settings::Provider` serializes
    /// to, keeping the History page's chip in the same vocabulary as the
    /// settings file; that claim is pinned by a test rather than left to a
    /// comment a `rename_all` change would quietly falsify. `Custom` has no
    /// counterpart there on purpose: the custom endpoint is a *toggle on top
    /// of* a provider (`customEndpoint.useForStt`), not a third value of
    /// `provider`, so there is no enum variant to agree with — and a row that
    /// said "sarvam" for audio Sarvam never received would be a lie in the
    /// one place the user goes to check what happened.
    ///
    /// The lane is a parameter for that same reason. Both cloud providers
    /// take the same realtime path, so `SttPath::Cloud` on its own cannot say
    /// *whose* cloud heard the audio — and a Cloud dictation filed as
    /// "sarvam" would be exactly the lie the paragraph above forbids: that
    /// user has no Sarvam account, and their words went to the relay.
    fn history_label(self, lane: &crate::sarvam::Lane) -> &'static str {
        match (self, lane) {
            (SttPath::Cloud, crate::sarvam::Lane::Byok) => "sarvam",
            (SttPath::Cloud, crate::sarvam::Lane::Cloud { .. }) => "cloud",
            (SttPath::Local, _) => "local",
            (SttPath::Custom, _) => "custom",
        }
    }
}

/// Which transcriber a dictation starting *now* will use.
///
/// The custom endpoint wins over `provider` when it is switched on for STT:
/// the toggle is the more specific, more recently expressed intent, and a
/// user who configured their own transcription server did not mean "unless
/// the provider dropdown says otherwise".
///
/// A free function for the usual reason — `Controller` needs an `AppHandle`
/// and five channels, so the decision has to live outside it to be testable.
fn stt_path(provider: Provider, use_for_stt: bool) -> SttPath {
    if use_for_stt {
        return SttPath::Custom;
    }
    match provider {
        // Both cloud providers take the realtime socket; which host it dials
        // is `lane_for`'s question, not this one's.
        Provider::Sarvam | Provider::Cloud => SttPath::Cloud,
        Provider::Local => SttPath::Local,
    }
}

/// Which host a dictation starting *now* talks to.
///
/// Separate from [`stt_path`] because they answer different questions:
/// `stt_path` picks the transcriber (cloud socket, on-device model, or the
/// user's own endpoint), while this picks *whose* cloud — the user's Sarvam
/// key, or Butterfly Labs' relay. Both are resolved at chord-down and
/// carried with the session.
///
/// `pub(crate)` because a dictation is not the only thing that talks to
/// a host: transform, the voice agent and note actions all make a chat call
/// on whichever lane the install is on, and each resolves it from the same
/// settings snapshot this reads.
pub(crate) fn lane_for(s: &crate::settings::Settings) -> crate::sarvam::Lane {
    use crate::sarvam::Lane;
    match s.provider {
        // The hidden override is hand-edited, so it is checked rather than
        // trusted — see `CloudSettings::relay_base`.
        Provider::Cloud => Lane::Cloud {
            relay: s.cloud.relay_base(),
        },
        Provider::Sarvam | Provider::Local => Lane::Byok,
    }
}

/// The chat half of [`Controller::capable`]: whether a chat-completions call
/// could be made at all on this lane, right now.
///
/// A free function for the usual reason — a `Controller` owns an `AppHandle`
/// and five channels — and `signed_in` is a closure rather than a `bool`
/// because asking costs something: `auth::session::status` reads the
/// credential store on any run that has not yet spent a token, and this is
/// called at every chord-down. A Bring-your-own-key install must not pay for
/// an answer about a lane it is not on.
fn chat_capable(
    lane: &crate::sarvam::Lane,
    sarvam_key: Option<&str>,
    polish_model: &str,
    signed_in: impl FnOnce() -> bool,
) -> bool {
    match lane {
        crate::sarvam::Lane::Byok => {
            crate::endpoint::chat_backend_exists(sarvam_key, polish_model)
        }
        // A Cloud install holds no Sarvam key at all, so asking the question
        // in those terms answers "no" for every chat-gated feature — the
        // agent, transforms, note actions — however signed in the user is.
        // The relay is their chat host, and a sign-in is what it takes.
        crate::sarvam::Lane::Cloud { .. } => {
            crate::endpoint::cloud_chat_backend_exists(signed_in())
        }
    }
}

/// A dictation whose transcript has arrived but whose route has not finished
/// with it yet (`routes::RouteOutcome::Deferred`).
///
/// Everything the `FinalResult` arm had in hand at the moment it handed off,
/// held so the answer can be applied through exactly the same tail
/// (`finish_route`) a synchronous route takes. Nothing here is re-derived
/// when the job answers: the foreground target, the verbatim transcript and
/// the fix counts all describe the dictation as it was, and re-reading any of
/// them later would describe a different moment.
///
/// Cleared by `set_state(Idle)`, which is what makes a cancelled or
/// superseded job's answer unapplicable rather than merely unwanted.
struct PendingRoute {
    /// The dictation this belongs to — `DictationState::Finalizing`'s id.
    req_id: u64,
    dict_target: Option<crate::foreground::Target>,
    target_app: Option<String>,
    raw: String,
    fixes: crate::state::FixCounts,
    /// The pipeline's own output — what `routes::apply` was handed, and what
    /// the route was asked to improve on.
    ///
    /// Held because `apply` *consumes* it: once a job is in flight the only
    /// copy of the cleaned dictation lives inside a future on another thread,
    /// and Escape needs something to paste when it stops waiting for that
    /// future. Cloned for every dictation, including the ones whose route
    /// answers inline and drops this a microsecond later — a few hundred
    /// bytes on a path that also writes a SQLite row and drives the
    /// clipboard, in exchange for the controller never being in the position
    /// of holding a dictation it cannot produce.
    cleaned: String,
    /// The guardrail's notice from `FinalResult`, if the formatter's output
    /// was rejected and `text` fell back to the rule pipeline's.
    guard_notice: Option<String>,
    /// The notice `routes::resolve` produced at chord-down, if a step was
    /// already known to be skipped or unavailable before the user finished
    /// speaking.
    route_notice: Option<crate::routes::Notice>,
    /// `FinalResult::cut_short`: the service ended this dictation, so its
    /// History row says so (`completed_error_code`).
    cut_short: bool,
}

/// Whether a `ControlMsg::CloudEnded` should stop the recording in front of
/// the controller: guarded exactly like a mid-recording `CloudError`
/// (`req_id == 0`) — a live recording, on the cloud path, of the very
/// session the relay ended. Anything else is a stale end from a session
/// that is already over. Free so the table is testable without a live
/// `Controller`.
fn relay_end_is_current(
    state: &DictationState,
    cloud_on: bool,
    session: u64,
    current_session: u64,
) -> bool {
    matches!(state, DictationState::Recording { .. }) && cloud_on && session == current_session
}

/// The History `error_code` for a dictation that was pasted: none, unless
/// the service ended it before the user did (`FinalResult::cut_short`).
fn completed_error_code(cut_short: bool) -> Option<&'static str> {
    cut_short.then_some(ERR_CLOUD_LIMIT)
}

/// Whether a `ControlMsg::RouteResult` belongs to the dictation that is
/// actually waiting for one.
///
/// Mirrors `FinalResult`'s own staleness guard (`Finalizing { req_id }` must
/// match) and adds the half a deferred job needs: the controller must still
/// be holding the `PendingRoute` the job was launched from. A route job runs
/// on the async runtime, outside the controller's message ordering, so its
/// answer can arrive after an Escape, a new recording, or the finalize
/// watchdog — all of which pass through `set_state(Idle)` and drop the
/// pending state. Free, like the other decisions in this file, so the table
/// is testable without a live `Controller`.
fn route_result_is_current(state: &DictationState, pending_req: Option<u64>, req_id: u64) -> bool {
    matches!(state, DictationState::Finalizing { req_id: want } if *want == req_id)
        && pending_req == Some(req_id)
}

/// What a finalize watchdog expiry should do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TimeoutDisposition {
    /// Not for the dictation in flight, or none is. A stale watchdog is a
    /// normal event — every finalize arms one and most are outlived.
    Stale,
    /// End the dictation with the timeout notice. `file_held_transcript` is
    /// set when a route job was still in flight, which means the transcript
    /// itself already arrived and the controller is holding it: those words
    /// went through the whole rules → polish → guardrail pipeline, so they
    /// are filed rather than dropped, the same way `CloudTruncated` files a
    /// fragment it refuses to paste. A plain timeout has nothing in hand and
    /// files nothing, exactly as it always has.
    Expire { file_held_transcript: bool },
}

/// `pending_route_req` is the id of the route the controller is *holding*,
/// not a bare "is one in flight" — the two are only the same thing because
/// `set_state(Idle)` drops the pending route, which is an invariant enforced
/// somewhere else entirely. Taking the id makes this locally sound instead:
/// the transcript is filed when it belongs to the dictation whose watchdog
/// just fired, and a mismatched pair can no longer file one dictation's words
/// against another's expiry.
fn finalize_timeout_disposition(
    state: &DictationState,
    pending_route_req: Option<u64>,
    req_id: u64,
) -> TimeoutDisposition {
    match state {
        DictationState::Finalizing { req_id: want } if *want == req_id => {
            TimeoutDisposition::Expire {
                file_held_transcript: pending_route_req == Some(req_id),
            }
        }
        _ => TimeoutDisposition::Stale,
    }
}

/// What `ControlMsg::Escape` should do, given where the dictation is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EscapeDisposition {
    /// Nothing Escape can interrupt. Includes a plain cloud finalize, where
    /// Escape has always been a no-op: the transcript is already on its way
    /// back and there is nothing to give the user instead of it.
    Ignore,
    /// Throw away a live recording — the historical behaviour.
    CancelRecording,
    /// Stop waiting for a deferred route and finish the dictation with the
    /// text the controller is already holding.
    AbortRoute,
    /// Stop waiting for an on-device transcription. Its watchdog grows with
    /// the utterance and has no cap (`local_finalize_timeout`), so a long
    /// decode is the user's to give up on; the answer that comes back later
    /// is dropped as stale, and nothing is pasted or filed.
    AbandonLocal,
}

/// `pending_route_req` is paired with the finalizing id for the same reason
/// [`finalize_timeout_disposition`]'s is: "a route is pending" and "*this*
/// dictation's route is pending" are only the same claim while an invariant
/// in `set_state` holds, and this decision should not depend on that.
///
/// `local` is whether the dictation is transcribing on the device.
fn escape_disposition(
    state: &DictationState,
    pending_route_req: Option<u64>,
    local: bool,
) -> EscapeDisposition {
    match state {
        DictationState::Recording { .. } => EscapeDisposition::CancelRecording,
        DictationState::Finalizing { req_id } if pending_route_req == Some(*req_id) => {
            EscapeDisposition::AbortRoute
        }
        DictationState::Finalizing { .. } if local => EscapeDisposition::AbandonLocal,
        _ => EscapeDisposition::Ignore,
    }
}

/// What an aborted route wait leaves behind: the text to paste, and the line
/// that explains it.
///
/// Route-dependent, and that is the whole point. A translation that never
/// finished still has the user's words in hand and they are worth pasting
/// untranslated — the module doc of `routes::translate` calls losing them the
/// worse outcome. An agent route is the opposite: its transcript is a
/// *command*, and pasting a command into the document as polished prose is the
/// exact defect `routes` exists to prevent. So anything that is not a
/// translation aborts with nothing typed.
fn route_abort_outcome(
    route: crate::routes::Route,
    cleaned: String,
) -> (Option<String>, crate::routes::Notice) {
    match route {
        crate::routes::Route::Translation => (
            Some(cleaned),
            crate::routes::translate::TRANSLATION_SKIPPED_NOTICE.into(),
        ),
        // `Cleanup` shares the arm, and since the wake word it is reachable:
        // a plain dictation that addressed the agent by name defers like any
        // other agent command, and its `session_route` is still `Cleanup`.
        // Typing nothing is the right answer for exactly the same reason —
        // those words are a command — and it is also the safe answer for a
        // route this function has not been taught about, so a future one has
        // to opt in to pasting rather than inherit it.
        crate::routes::Route::Cleanup | crate::routes::Route::Agent => {
            (None, ROUTE_SKIPPED_NOTICE.into())
        }
    }
}

/// Who wrote the text [`Controller::finish_route`] is about to paste — the one
/// bit auto-learn's field monitor turns on.
///
/// The monitor watches the field a paste landed in and reads a word the user
/// changes there as a correction of what they *dictated*, evidence worth
/// promoting into their personal vocabulary once it is seen twice. That
/// reading is only sound when the words were the user's own. Every route
/// answer — a finished translation, an agent answer, a wake answer — lands
/// through the same `start_injection`, and there a "correction" is the user
/// editing a *model's* prose: nothing about how they say a word, everything
/// about how the model wrote a sentence. Learned as vocabulary it would then
/// rewrite their real dictations. So the paste carries its provenance with it.
///
/// This is the same judgement the selection lane makes by skipping
/// `start_injection` altogether (see the `pasted` branch of `finish_route`),
/// one bit wide, for the pastes that do go through it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TextSource {
    /// The cleanup pipeline: rule cleanup plus formatting over the user's own
    /// words. A plain dictation, and equally a translate chord that never
    /// reached the translator (no key, no target, a source language of
    /// `auto`): cleanup output wearing a notice.
    Dictation,
    /// A model wrote it.
    Model,
}

impl TextSource {
    /// The rule, one `matches!` wide: a route that answers inline
    /// (`RouteOutcome::Ready`) never left the controller thread, and no route
    /// that calls a model can do that — `routes::apply`'s whole contract is
    /// that network work comes back `Deferred`. So `Ready` means the text is
    /// whatever the cleanup pipeline produced, and `Deferred` means a model is
    /// about to write it.
    ///
    /// The one false negative it accepts: a deferred translation that *fails*
    /// pastes the user's dictation back untranslated (`routes::translate`'s
    /// never-lose-the-words rule), and that paste arrives as a `RouteResult`
    /// and so goes unwatched. Missing the occasional learn is the cheap
    /// direction to be wrong in; the other one teaches the dictionary from
    /// model prose.
    fn of(outcome: &crate::routes::RouteOutcome) -> Self {
        match outcome {
            crate::routes::RouteOutcome::Ready { .. } => Self::Dictation,
            crate::routes::RouteOutcome::Deferred(_) => Self::Model,
        }
    }

    /// Whether auto-learn's field monitor may watch a paste from this source.
    fn watch_field(self) -> bool {
        matches!(self, Self::Dictation)
    }
}

/// Whether a system-resume signal (`ControlMsg::SystemResumed`) should tear
/// down the current dictation. A free function — same rationale as
/// `undo_decision`/`injection_outcome` — so the decision is unit-testable
/// without a live `Controller`.
///
/// Only a cloud session actually depends on a socket that suspend can kill
/// out from under it: `state` must be actively waiting on Sarvam
/// (`Recording` — audio is streaming live — or `Finalizing` — the drain/
/// polish call is in flight) *and* `cloud_on`, or there is nothing to
/// invalidate. `Injecting` is excluded even for a cloud dictation: by that
/// point the network round trip is over and a one-shot paste thread is
/// running locally, which sleep cannot have broken.
fn should_cancel_on_resume(state: &DictationState, cloud_on: bool) -> bool {
    cloud_on && matches!(state, DictationState::Recording { .. } | DictationState::Finalizing { .. })
}

/// Whether a session lock or the tray's Pause (`ControlMsg::SessionLocked`,
/// `ControlMsg::Paused`) cancels the dictation in front of the controller:
/// only a live recording, in either mode. Once the recording has stopped the
/// words were spoken before the lock or the pause and are finished as usual.
fn cancels_on_interruption(state: &DictationState) -> bool {
    matches!(state, DictationState::Recording { .. })
}

/// Whether the controller frees the on-device models when a dictation ends:
/// on any provider but Local, which is the one that uses them. A provider
/// switch made while a local dictation ran is completed here.
pub(crate) fn unloads_at_idle(provider: Provider) -> bool {
    provider != Provider::Local
}

/// Whether the controller is holding the transcript of the dictation in
/// front of it while that dictation's route runs. Paired with the finalizing
/// id for the same reason [`finalize_timeout_disposition`] is.
fn holds_route_transcript(state: &DictationState, pending_route_req: Option<u64>) -> bool {
    matches!(state, DictationState::Finalizing { req_id } if pending_route_req == Some(*req_id))
}

/// The text a `Failed` row keeps for a dictation whose route never answered
/// (a finalize timeout, or a machine that slept while the route ran).
///
/// Follows [`route_abort_outcome`]: a translation's held words are the
/// user's own dictation after rules, polish and the guardrail, so that is
/// what is filed. For an agent command or a wake-word dictation the words
/// are a command, and the row keeps the verbatim transcript, as a route that
/// declined (`finish_route`) does.
fn held_row_text<'a>(route: crate::routes::Route, cleaned: &'a str, raw: &'a str) -> &'a str {
    match route {
        crate::routes::Route::Translation => cleaned,
        crate::routes::Route::Cleanup | crate::routes::Route::Agent => raw,
    }
}

/// Whether a History delete or clear takes `last_text` (Paste and Copy last
/// transcript) with it. A free function for the same reason as the ones
/// around it.
///
/// A clear always does. A delete does when the row holds the same words,
/// as its `text` (what was pasted, and what `record_history` filed) or as
/// its verbatim transcript. The one miss: a row edited on Home after a
/// polish changed it, which then matches neither; `last_text` is memory
/// only and goes when the app closes.
fn forgets_last_text(last_text: &str, removed: &crate::state::HistoryRemoval) -> bool {
    use crate::state::HistoryRemoval;
    if last_text.is_empty() {
        return false;
    }
    match removed {
        HistoryRemoval::All => true,
        HistoryRemoval::Row { text, raw } => {
            text == last_text || raw.as_deref() == Some(last_text)
        }
    }
}

/// What a `FinalResult` should record as the new undo target — a *candidate*
/// now, held in `Controller::in_flight` and only installed once the paste is
/// confirmed to have landed.
///
/// A free function so the ordering rule has a regression test that doesn't
/// require constructing a `Controller`: writing `last_raw` from a bare field
/// assignment before the empty-text early return would let a filler-only
/// utterance ("um") overwrite the raw half of the undo state while the text
/// half stayed pointed at the previous dictation. Empty text is never injected,
/// so it must never become — or overwrite — the undo target either: both fields
/// are written together, from one call, or not at all.
fn next_last_injection(
    previous: Option<LastInjection>,
    text: &str,
    raw: String,
    at: Instant,
    app: Option<String>,
) -> Option<LastInjection> {
    if text.is_empty() {
        return previous;
    }
    Some(LastInjection {
        text: text.to_string(),
        raw,
        at,
        app,
    })
}

/// Ends a dictation result with one ASCII space so the user can keep typing
/// straight after it, unless its last character is already whitespace in
/// Unicode's sense (space, tab, newline, no-break space, ideographic space and
/// the rest). Punctuation gets no exception. Empty text becomes a single space.
///
/// Callers apply it only to a dictation's result, only when
/// `settings.dictation.smart_space` is on, and never to a transform or to a
/// snippet's own text.
fn smart_space_append(mut text: String) -> String {
    if !text.chars().next_back().is_some_and(char::is_whitespace) {
        text.push(' ');
    }
    text
}

/// 16 kHz mono, the rate every buffer in this app carries (`audio.rs`
/// resamples to it) — expressed off the gate's own 100 ms window so the two
/// can never drift apart.
const SAMPLES_PER_MS: usize = speech_gate::WINDOW_SAMPLES / 100;

/// How much of an utterance's front the speech gate must ignore when the app
/// played its own start cue into the microphone.
///
/// `begin_recording` opens the audio gate and *then* plays the start cue, and
/// `tones::play` is asynchronous, so the cue lands in the live
/// stream this same recording is buffering — no reordering avoids it. The
/// pump's pre-roll is excluded along with it for two reasons: the controller
/// cannot know how much of it was actually shipped (the ring is capped at
/// `audio::PREROLL_MS` but starts empty), and the ring keeps filling while
/// the gate is closed, so a press soon after a stop carries the *previous*
/// dictation's stop cue.
const CUE_BLEED_MS: usize = crate::audio::PREROLL_MS + tones::AUDIBLE_MS;

/// The gate needs at least this much post-cue audio before judging on that
/// alone; below it, the whole utterance is judged instead.
///
/// The two errors are not symmetric. Trimming too far reads a short-but-real
/// dictation as silence and throws the user's words away; trimming too little
/// leaves the cue in, which costs one unnecessary round trip that ends at the
/// same "Didn't catch that" the gate would have shown. Short recordings get
/// the harmless error. Nothing lands in this band by accident — `TAP_MAX`
/// already discards holds under 350 ms.
const MIN_TRIMMED_MS: usize = 300;

/// What the speech gate is allowed to judge. Only the *evidence* is trimmed:
/// whatever the gate lets through is still transcribed in full.
///
/// See [`CUE_BLEED_MS`]. Without the trim, the cue's own sound lifts every
/// cue-on recording above the gate's silence floor, so a silent hold could
/// never be judged `Silence` and "Didn't catch that" would never show.
fn gate_evidence(audio: &[f32], cues_on: bool) -> &[f32] {
    if !cues_on {
        return audio;
    }
    let skip = (CUE_BLEED_MS * SAMPLES_PER_MS).min(audio.len());
    let trimmed = &audio[skip..];
    if trimmed.len() < MIN_TRIMMED_MS * SAMPLES_PER_MS {
        audio
    } else {
        trimmed
    }
}

/// Finalizing watchdog. The cloud ceiling must exceed the sum of the
/// pipeline's own worst-case limits or a slow but successful transcript
/// gets discarded at the deadline. Every term below is a real budget
/// defined in `sarvam::ws` / `sarvam::batch` / `sarvam::chat` /
/// `routes::selection` / `sarvam::translate`, stacked
/// pessimistically (every one maxed out at once is vanishingly unlikely,
/// but the ceiling has to cover it or this watchdog is the thing that
/// throws the transcript away, not a real failure):
///   ≤2.2 s connect backoff before attempt 1 (`ws::ConnectBackoff`, only
///     after a *previous* utterance's connect failed transiently)
/// + ≤4 s attempt 1 (`ws::CONNECT_TIMEOUT`)
/// + ≤2.2 s connect backoff before attempt 2 (same `ConnectBackoff`, now
///     scheduled by attempt 1's *own* transient failure —
///     `ws::CONNECT_MAX_ATTEMPTS` gives this dictation two real connect
///     attempts, not just a delay before the next one; see its own doc
///     comment)
/// + ≤4 s attempt 2 (`ws::CONNECT_TIMEOUT` again — `CONNECT_MAX_ATTEMPTS`
///     stops here, so there is no third attempt to budget for)
/// + ≤4 s session-begin wait (`ws::FLUSH_WAIT_FLOOR`, unscaled — this is
///     the handshake, not a drain, so utterance duration doesn't apply)
/// + ≤6 s scaled flush-drain ceiling (`ws::FLUSH_WAIT_CEILING`)
/// + ≤1 s goodbye courtesy frame (`ws::GOODBYE_TIMEOUT`)
/// + ≤12 s one batch-fallback attempt (`batch::BATCH_TIMEOUT`, only when the
///     realtime transcript is unusable — nothing at all, or a fragment left
///     by a drain that failed mid-utterance; see
///     `ws::realtime_transcript_stands_alone`)
/// + ≤2 s looking this dictation's credential up again for the polish
///     (`sarvam::CREDENTIAL_WAIT`; on the Cloud lane a sign-in refresh)
/// + ≤18 s polish (`chat::POLISH_TIMEOUT`, three times over). **Three times
///     the deadline, not one**, because a custom OpenAI-compatible host can
///     sit behind this call: the timeout handed to `Backend::complete` is *per
///     request*, and `format::backend` answers a 400/422 from a custom host
///     by sending the request again with fewer optional parameters, up to
///     three requests in all (pinned there by
///     `a_third_refusal_is_returned_as_the_error`). A Sarvam-only
///     install never spends more than the first 6 s of this term; an install
///     whose polish goes to its own server can spend all of it, and the
///     watchdog has to cover that configuration too.
/// + ≤24.87 s **the worst single route.** This term is a *maximum*, not a sum,
///     and that is the whole philosophy of this comment: it prices the worst
///     case of ONE dictation, and one dictation takes exactly one route.
///     Adding the routes together would budget for a dictation that cannot
///     exist. The candidates:
///       - translate: ≤10 s (`translate::TRANSLATE_TIMEOUT` — once:
///         `routes::translate` makes exactly one request and never retries it.
///         A *whole-request* budget, covering connect, send and the reply
///         body, so 10 s really is the worst case)
///       - agent: ≤2.38 s selection resolve (`selection::RESOLVE_BUDGET`,
///         spent before the route runs and only ever on this one — the agent
///         chord is the only thing that captures a selection) + ≤20 s
///         (`chat::AGENT_TIMEOUT`, likewise one request) + ≤2.49 s replace
///         (`selection::REPLACE_BUDGET`)
///     The agent branch is worse on every part, so it sets the term. A
///     wake-word dictation reaches `chat::AGENT_TIMEOUT` too, but never the
///     resolve or the replace, so it sits under this.
/// = 80.27 s. 82 s: the ceiling is the sum rounded up to leave **at least a
///   full second** of margin, which absorbs steps nobody has itemized yet. The
///   pinned test requires the ceiling to clear `sum + 1 s`, so a future term
///   cannot quietly eat the margin.
///
/// Two terms of the agent branch need a note:
///
/// * The resolve term includes `uia::element_for_hwnd`'s bounded bind retry,
///   which is why `selection::UIA_PROBE_BUDGET` is 1.4 s.
/// * **The replace step is part of the finalize.** `routes::agent` awaits
///   `run_replace(..)` *inside* the deferred job, before it answers
///   `RouteDone`, so the controller stays in `Finalizing` under this watchdog
///   for the whole verify-restore-paste sequence. A watchdog firing
///   mid-replace is not a harmless early exit: `selection::replace` burns the
///   session on its first line and then runs to completion with no channel
///   back here, so the user would get a timeout notice, a timeout History
///   row, *and* the paste.
///
/// `selection::REPLACE_BUDGET` prices the clipboard-restore delay at its
/// maximum (`settings::RESTORE_DELAY_MAX_MS`, enforced in `settings::repair`
/// on load and on import), so this ceiling holds for every settings file
/// rather than only the ones the app wrote. It also covers the 25 ms
/// `release_stuck_modifiers` drain.
///
/// The sum is not just prose: `the_cloud_finalize_budget_outlasts_every_step_
/// it_itemizes` re-adds it from the constants themselves. The reason it is
/// worth a test is that overshooting this costs a few seconds of pill on a
/// dictation that was failing anyway, while undershooting it silently
/// discards a dictation that *succeeded* — the two errors are nowhere near
/// symmetric, so the ceiling is set pessimistically and pinned.
const CLOUD_FINALIZE_TIMEOUT: Duration = Duration::from_secs(82);

/// The on-device path's watchdog, for an utterance of `duration_ms`. Same
/// philosophy as `CLOUD_FINALIZE_TIMEOUT` — itemize every step, stack them
/// pessimistically, round up to leave at least a full second of margin —
/// except that decoding scales with the utterance, so the ceiling does too:
///   ≤ the utterance's own length, decoding it
///     ([`LOCAL_DECODE_PERCENT_OF_AUDIO`])
/// + ≤10 s a speech-model load queued ahead of the dictation on the same
///     thread (choosing Local, or another model, loads one)
/// + ≤10 s loading the on-device polish model, which happens on the first
///     polish after launch or after it was unloaded. Each load reads one file
///     from disk, and the largest either reads is under 500 MB (the 491 MB
///     polish model, the 482 MB largest speech model): about ten seconds at
///     the ~50 MB/s of a slow spinning disk
/// + ≤4 s polish (`cleanup::polish`'s `TIMEOUT`, private there and restated
///     here, like the `ws` terms above)
/// + ≤24.87 s the worst single route (`selection::RESOLVE_BUDGET` +
///     `chat::AGENT_TIMEOUT` + `selection::REPLACE_BUDGET`, the same term the
///     other two ceilings carry)
/// = 48.87 s + the utterance. [`LOCAL_FINALIZE_BASE`] is 50 s, leaving 1.13 s
///   for the steps nobody has itemized yet (punctuation, the rule pipeline).
///
/// There is no cap. The audio is already recorded and nothing on this path
/// waits on a network except the route, which has its own timeouts; a cap
/// would drop a long hands-free dictation that is still decoding, with no
/// History row, as a stale result. Escape gives up on one the user no longer
/// wants (`EscapeDisposition::AbandonLocal`).
///
/// `the_local_finalize_budget_outlasts_every_step_it_itemizes` re-adds this
/// from the constants.
fn local_finalize_timeout(duration_ms: u64) -> Duration {
    let decode_ms = duration_ms.saturating_mul(LOCAL_DECODE_PERCENT_OF_AUDIO) / 100;
    LOCAL_FINALIZE_BASE + Duration::from_millis(decode_ms)
}

/// The fixed part of [`local_finalize_timeout`].
const LOCAL_FINALIZE_BASE: Duration = Duration::from_secs(50);

/// Decoding priced at one second per second of audio: the real-time rate
/// `asr::custom` assumes for a Whisper-class model on the user's own CPU.
const LOCAL_DECODE_PERCENT_OF_AUDIO: u64 = 100;

/// The custom-endpoint path's watchdog. Same philosophy as
/// `CLOUD_FINALIZE_TIMEOUT` — itemize every step, stack them pessimistically,
/// round up to leave at least a full second of margin — over a different set
/// of steps, because this path shares none of the realtime session's:
///   ≤60 s transcription (`asr::custom::TIMEOUT_CEILING`, the ceiling of a
///     budget that is itself scaled by utterance length, so only a ~50-second
///     dictation can actually spend it)
/// + ≤2 s looking up this install's own host for the polish
///     (`sarvam::CREDENTIAL_WAIT`, the same term `CLOUD_FINALIZE_TIMEOUT`
///     carries)
/// + ≤18 s polish (`chat::POLISH_TIMEOUT`, three times over).
///     **Three times the deadline, not one.** The timeout handed to
///     `Backend::complete` is per request, and `format::backend` can make
///     three of them against a custom host that refuses the first
///     (pinned there by `a_third_refusal_is_returned_as_the_error`).
///     `CLOUD_FINALIZE_TIMEOUT` carries the same term.
/// + ≤24.87 s the worst single route (`selection::RESOLVE_BUDGET` +
///     `chat::AGENT_TIMEOUT` + `selection::REPLACE_BUDGET` — a max over the
///     routes, not a sum: one dictation takes one route, and the agent's is
///     the worst of them, exactly as `CLOUD_FINALIZE_TIMEOUT` argues. The
///     same term that constant carries, so a change to the selection budgets
///     moves both together)
/// = 104.87 s. 106 s leaves 1.13 s for the steps nobody has itemized yet.
///
/// It is a long pill for a broken endpoint, and that asymmetry is deliberate:
/// overshooting costs seconds on a dictation that was already failing, while
/// undershooting silently discards one that *succeeded* — a transcript the
/// user's own server spent a minute producing.
///
/// `the_custom_finalize_budget_outlasts_every_step_it_itemizes` re-adds this
/// from the constants themselves.
const CUSTOM_FINALIZE_TIMEOUT: Duration = Duration::from_secs(106);

/// Whether this dictation is allowed to read what the user has selected.
///
/// Two conditions, and both are about not touching the user's document for
/// nothing:
///
/// * **the agent chord, and only it.** A plain dictation has no business
///   reading the user's selection: the answer is typed at the caret either
///   way, so the read would buy nothing and cost a cleared clipboard and a
///   synthetic Ctrl+C fired at whatever they were working in.
/// * **a key.** `routes::agent` reports a missing one *before* it consults the
///   selection at all, so without a key the capture would disturb the document
///   to inform a command that is already going to type nothing.
///
/// A free function rather than two conditions inline because the first of them
/// is a promise about the other two chords, and a promise worth making is worth
/// a test that fails when someone widens the condition. Constructing a
/// `Controller` needs an `AppHandle` and five channels, so a method on it is
/// not reachable from a unit test — the same reason `undo_decision` and
/// `injection_outcome` are free functions here.
fn should_capture_selection(chord: crate::routes::ChordKind, chat_ready: bool) -> bool {
    chord == crate::routes::ChordKind::Agent && chat_ready
}

/// Set the system clipboard on a one-shot thread. Shared by "Copy last
/// transcript" and Undo AI Edit's non-destructive fallback. Both are copies
/// the user asked for, so they are ordinary ones, unmarked.
fn copy_to_clipboard(text: String) {
    std::thread::spawn(move || {
        if let Ok(mut cb) = arboard::Clipboard::new() {
            let _ = crate::injection::set_plain_text(&mut cb, &text);
        }
    });
}

pub struct Controller {
    app: tauri::AppHandle,
    rx: Receiver<ControlMsg>,
    tx_self: Sender<ControlMsg>,
    asr_tx: Sender<AsrMsg>,
    cloud_tx: tokio::sync::mpsc::UnboundedSender<CloudCmd>,
    /// The custom-endpoint transcription worker (`asr::custom`). A third
    /// channel rather than a variant on one of the other two: each of the
    /// three transcribers owns its own thread or task, and this one is the
    /// only one that is neither a Sarvam session nor an in-process model.
    custom_tx: tokio::sync::mpsc::UnboundedSender<crate::asr::custom::SttJob>,
    gate: Arc<AtomicBool>,
    suppress: Arc<AtomicBool>,
    settings: Arc<RwLock<Settings>>,
    state: DictationState,
    buffer: Vec<f32>,
    next_req: u64,
    last_tap: Option<Instant>,
    /// When the most recent recording stopped (via `finish_recording`), for
    /// `POST_STOP_COOLDOWN`. `Instant`, not wall-clock — see the
    /// `LastInjection::at` doc for why that's fine here too: the only
    /// consequence of a stale value surviving a sleep/resume is a chord-down
    /// that should've been swallowed getting through, same as if the cooldown
    /// had simply already elapsed.
    last_stop: Option<Instant>,
    /// Audio length of the utterance being finalized, for the stats event.
    pending_duration_ms: u64,
    /// Which transcriber this dictation is using — snapshotted at
    /// begin_recording so a mid-dictation settings change can't tear the
    /// session in half. `cloud_on()` is the "is the Sarvam socket live"
    /// question the rest of this file asks of it.
    stt: SttPath,
    /// Which host this dictation's cloud work talks to, snapshotted beside
    /// `stt` and for the same reason: the Cloud session and the custom
    /// endpoint's polish both use it, and it tells the History row whether
    /// the audio went to the user's own Sarvam account or to Butterfly Labs'
    /// relay.
    stt_lane: crate::sarvam::Lane,
    /// Monotonic cloud-session counter. A `CloudError { req_id: 0, .. }` from
    /// a dead previous session must not kill the recording that replaced it.
    cloud_session: u64,
    /// Sarvam API key cache, needed to launch transforms.
    sarvam_key: crate::sarvam::SharedKey,
    /// One transform at a time; cleared by the transform thread's guard.
    /// Shared with `Backend`, where the updater reads it.
    transform_busy: Arc<AtomicBool>,
    /// The most recent final transcript, for paste-last / copy-last.
    last_text: String,
    /// Bookkeeping for Undo AI Edit's verified select-and-replace. `None`
    /// means there is nothing to undo: nothing dictated yet, the paste never
    /// landed, a previous undo consumed the record, or something else wrote
    /// to the document since (paste-last, a transform) and invalidated it.
    last_injection: Option<LastInjection>,
    /// Set for the duration of an injection, consumed at `InjectionDone`.
    /// Undo is only ever armed from here, never directly from the result
    /// arm — see `InFlight`.
    in_flight: Option<InFlight>,
    /// The window the user was looking at when this dictation started
    /// (`foreground::capture`, taken at chord-down/hands-free start — the
    /// moment the user pressed the hotkey is the moment they were looking at
    /// their intended destination). Carried through recording and
    /// finalizing, then consumed at the `FinalResult` paste so the result
    /// goes to that window even if the user clicked somewhere else while it
    /// was being transcribed — never re-queried at paste time. `None` when
    /// nothing could be captured (no foreground window, or its process
    /// couldn't be queried).
    dictation_target: Option<crate::foreground::Target>,
    /// Shared across transform calls so each rewrite doesn't pay a fresh TLS
    /// handshake (reqwest::Client is cheap to clone — internally an Arc).
    http: reqwest::Client,
    /// Live while GSMTC has media paused for the recording in progress
    /// (`audio.pause_media`). Dropping it resumes exactly what it paused, so
    /// "paused with nobody left to resume it" cannot be reached — including
    /// when the pause is still in flight at the moment capture stops.
    media_pause: Option<crate::media::PauseGuard>,
    /// Where finished dictations are filed (`FinalResult`'s arm). A cheap
    /// channel handle — `record` is fire-and-forget, so a slow or dead DB
    /// thread can never stall the paste that follows it.
    history: crate::history::Recorder,
    /// The frequency guard the field monitor reports to. Cloned onto each
    /// paste's monitor thread rather than called from here: `observe` blocks
    /// on a history-DB round trip, and the controller thread is the one thing
    /// in this app that must never block.
    learn: crate::learn::candidates::Guard,
    /// Which chord started the current dictation.
    ///
    /// Kept alongside `session_route` rather than derived from it because
    /// they answer different questions: this is what the user asked for
    /// (what the history row's `route` records), `session_route` is what the app
    /// could actually do about it. A translate dictation whose translation was
    /// skipped is a different row from a dictation that never wanted one.
    session_chord: crate::routes::ChordKind,
    /// What the current dictation will actually be put through, resolved
    /// against the config at chord-down (`routes::resolve`).
    ///
    /// Stamped at the *start* on purpose: what the user wanted is a property
    /// of the gesture, not something to infer from the transcript once it
    /// arrives — and re-resolving at finalize would let a key added
    /// mid-dictation change the answer halfway through. Read by the finalize
    /// seam, overwritten by the next `begin_recording`.
    session_route: crate::routes::Route,
    /// The notice `routes::resolve` produced at chord-down, if it had to
    /// reinterpret what the chord asked for. Held until the dictation
    /// finishes rather than flashed immediately: the user is mid-press there
    /// and the pill is busy showing the recording.
    pending_route_notice: Option<crate::routes::Notice>,
    /// Set while a deferred route job is in flight — the transcript arrived
    /// but the route has not finished with it. The controller stays in
    /// `Finalizing` under the watchdog already counting for that dictation;
    /// `ControlMsg::RouteResult` consumes this, and `set_state(Idle)` drops
    /// it, which is what makes a superseded job's answer unapplicable.
    pending_route: Option<PendingRoute>,
    /// True for every state but `Idle` — the one bit of this thread's state
    /// that anything outside it can read.
    ///
    /// It exists for the updater, which ends the process on hand-off and so
    /// has to know whether a dictation is mid-flight before it does
    /// (`updater::install_blocker`). Written at exactly one site,
    /// [`Controller::set_state`], beside the `STATE_CHANGED` emit, so the
    /// flag and the event the frontend sees can never disagree.
    dictation_busy: Arc<AtomicBool>,
    /// The selection read started when an agent chord was released, waiting
    /// for the route seam to ask what it found. `None` for every other chord:
    /// a plain dictation must never read the user's selection.
    ///
    /// Held rather than resolved at the stop because the read runs on its own
    /// thread and overlaps transcription — by the time `FinalResult` arrives
    /// the answer is almost always already sitting in the channel. See
    /// `routes::selection::begin` for why it starts at the *release* and not
    /// at the press.
    pending_selection: Option<crate::routes::selection::PendingCapture>,
    /// Messages that arrived while `collect_tail` was draining the audio
    /// tail, handled in order once the current message is done.
    deferred: std::collections::VecDeque<ControlMsg>,
}

/// What `collect_tail` does with one message that arrives while it drains
/// the audio tail.
enum TailStep {
    /// Audio from the tail: part of this recording.
    Chunk(Vec<f32>),
    /// The pump's marker: the tail is complete.
    End,
    /// Anything else, handled once the recording has stopped.
    Later(ControlMsg),
}

fn tail_step(msg: ControlMsg) -> TailStep {
    match msg {
        ControlMsg::Audio(chunk) => TailStep::Chunk(chunk),
        ControlMsg::AudioTail => TailStep::End,
        other => TailStep::Later(other),
    }
}

#[allow(clippy::too_many_arguments)]
pub fn spawn(
    app: tauri::AppHandle,
    rx: Receiver<ControlMsg>,
    tx_self: Sender<ControlMsg>,
    asr_tx: Sender<AsrMsg>,
    cloud_tx: tokio::sync::mpsc::UnboundedSender<CloudCmd>,
    custom_tx: tokio::sync::mpsc::UnboundedSender<crate::asr::custom::SttJob>,
    gate: Arc<AtomicBool>,
    suppress: Arc<AtomicBool>,
    settings: Arc<RwLock<Settings>>,
    sarvam_key: crate::sarvam::SharedKey,
    history: crate::history::Recorder,
    learn: crate::learn::candidates::Guard,
    dictation_busy: Arc<AtomicBool>,
    transform_busy: Arc<AtomicBool>,
) {
    std::thread::Builder::new()
        .name("controller".into())
        .spawn(move || {
            let mut c = Controller {
                app,
                rx,
                tx_self,
                asr_tx,
                cloud_tx,
                custom_tx,
                gate,
                suppress,
                settings,
                state: DictationState::Idle,
                buffer: Vec::new(),
                next_req: 1,
                last_tap: None,
                last_stop: None,
                pending_duration_ms: 0,
                // Overwritten at every `begin_recording`; `Local` is the one
                // value that starts no session and arms no cloud teardown, so
                // it is the safe thing to hold while idle.
                stt: SttPath::Local,
                stt_lane: crate::sarvam::Lane::default(),
                cloud_session: 0,
                sarvam_key,
                transform_busy,
                last_text: String::new(),
                last_injection: None,
                in_flight: None,
                dictation_target: None,
                http: reqwest::Client::new(),
                media_pause: None,
                history,
                learn,
                session_chord: crate::routes::ChordKind::default(),
                session_route: crate::routes::Route::default(),
                pending_route_notice: None,
                pending_route: None,
                dictation_busy,
                pending_selection: None,
                deferred: std::collections::VecDeque::new(),
            };
            loop {
                let Ok(msg) = c.rx.recv() else { break };
                c.handle(msg);
                // What arrived while `collect_tail` drained the audio, in the
                // order it arrived. Handling one can stop another recording
                // and defer more, which this same loop then picks up.
                while let Some(later) = c.deferred.pop_front() {
                    c.handle(later);
                }
            }
        })
        .expect("spawn controller thread");
}

impl Controller {
    fn settings(&self) -> Settings {
        self.settings.read().expect("settings lock").clone()
    }

    fn set_state(&mut self, s: DictationState) {
        let (name, mode) = match &s {
            DictationState::Idle => ("idle", None),
            DictationState::Recording { mode, .. } => (
                "recording",
                Some(match mode {
                    Mode::PushToTalk => "pushToTalk",
                    Mode::HandsFree => "handsFree",
                }),
            ),
            DictationState::Finalizing { .. } => ("finalizing", None),
            DictationState::Injecting => ("injecting", None),
        };
        tracing::debug!("state → {name}{}", mode.map(|m| format!(" ({m})")).unwrap_or_default());
        self.state = s;
        // Idle is the one state with no dictation in progress, so it is where
        // the per-session route bookkeeping is dropped — once, structurally,
        // instead of at each of the several arms that can end a dictation
        // without a `FinalResult` (Escape, a dead mic, a cloud error, a
        // finalize timeout, a truncated transcript, an empty one, a resumed
        // machine). Leaving any of it behind would let an unshown notice
        // surface on the back of a later dictation, or a route job answer
        // into a session that is no longer waiting for it. Everything here
        // is read before its own arm reaches Idle.
        if matches!(self.state, DictationState::Idle) {
            self.session_chord = crate::routes::ChordKind::default();
            self.session_route = crate::routes::Route::default();
            self.pending_route_notice = None;
            self.pending_route = None;
            // Dropped here for the same reason as the rest: a capture the
            // route seam never got to consult belongs to a dictation that is
            // over, and letting it survive would hand the *next* agent
            // command a selection read against a window the user has left.
            // Dropping the receiver is the whole operation — a capture thread
            // still in flight finds nobody listening and goes away.
            self.pending_selection = None;
            // And the sessions with it. A minted session is not bookkeeping, it
            // is a standing licence to overwrite a span of the user's document,
            // and the one thing that can redeem it — the deferred job
            // that edits the selection — is not cancellable: Escape and the
            // finalize watchdog both stop *waiting* for a route without
            // stopping the route, and `route_result_is_current` drops its
            // answer on arrival. That is harmless for an answer that is only
            // text to paste, but this job pastes for itself, so the licence is
            // revoked at the same moment the dictation stops being ours to
            // apply, and a late job finds nothing to redeem and replaces
            // nothing.
            //
            // Safe on the success path: the job consumes its session before it
            // answers, so by the time `finish_route` reaches Idle there is
            // nothing here to clear. Nothing mints between a capture and its
            // redemption without passing through this line first.
            crate::routes::selection::sessions().clear();
            // A switch away from Local during a dictation leaves the model
            // loaded for it (`commands::model_change`); this is where it goes.
            let provider = self.settings.read().expect("settings lock").provider;
            if unloads_at_idle(provider) {
                let _ = self.asr_tx.send(AsrMsg::Unload);
            }
        }
        // Published here, with the event, and nowhere else: the updater reads
        // it to decide whether handing the installer the bytes would kill a
        // dictation the user is in the middle of. Every state but Idle counts
        // — a finalize or an injection is as unrecoverable as the recording
        // that produced it.
        self.dictation_busy.store(name != "idle", Ordering::SeqCst);
        let _ = self
            .app
            .emit(events::STATE_CHANGED, events::StatePayload { state: name, mode });
        // The overlay is visible for every state but Idle. Re-assert TOPMOST
        // on each transition through them as the cheap, honest proxy for
        // "the foreground window may have changed" — see
        // `overlay::reassert_topmost`.
        if !matches!(self.state, DictationState::Idle) {
            overlay::reassert_topmost(&self.app);
        }
    }

    /// Whether a Sarvam realtime session is live for this dictation — a
    /// single yes/no because that is what a dozen call sites in this file
    /// need: whether there is a socket to feed, cancel or tear down.
    fn cloud_on(&self) -> bool {
        self.stt == SttPath::Cloud
    }

    /// Whether a usable Sarvam key is stored — a blank one is not a key.
    fn key_present(&self) -> bool {
        self.sarvam_key
            .read()
            .expect("key lock")
            .as_deref()
            .is_some_and(|k| !k.trim().is_empty())
    }

    /// Resolve what the chord that just went down is asking for, against the
    /// config as it stands right now.
    ///
    /// A thin wrapper over the pure `routes::resolve` so the whole decision
    /// table stays unit-testable without a `Controller`: this reads the two
    /// facts it needs (is a key stored, is a translation target configured)
    /// and does no deciding of its own.
    fn resolve_route(&self, chord: crate::routes::ChordKind) -> crate::routes::Resolved {
        let capable = self.capable();
        let target_set = !self
            .settings
            .read()
            .expect("settings lock")
            .translation
            .target_language
            .trim()
            .is_empty();
        crate::routes::resolve(chord, capable, target_set)
    }

    /// What this install can reach right now: a Sarvam key (translation's own
    /// requirement) and a chat backend of any kind (the agent's). Two facts,
    /// because chat has more than one possible host — see `routes::Capable`.
    ///
    /// The polish model is read for the resolver's sake and discarded when
    /// the custom endpoint answers, which names its own; taking it off the
    /// lock directly rather than through `settings()` avoids cloning the
    /// whole struct — dictionary, replacements, snippets, transforms — for
    /// one `String` on a path that runs at every chord-down.
    fn capable(&self) -> crate::routes::Capable {
        let sarvam_key = self.key_present();
        let (polish_model, lane) = {
            let s = self.settings.read().expect("settings lock");
            (s.sarvam.polish_model.clone(), lane_for(&s))
        };
        let key = self.sarvam_key.read().expect("key lock").clone();
        crate::routes::Capable {
            sarvam_key,
            chat: chat_capable(&lane, key.as_deref(), &polish_model, || {
                crate::auth::session::status().signed_in
            }),
        }
    }

    /// Start recording on `chord`.
    ///
    /// The route is resolved *here* rather than passed in, so the intent and
    /// the dispatch it produced can never be handed in as a mismatched pair —
    /// there is only one way to arrive at them, and it is this line. Called at
    /// the press, so "the config as it stands right now" still means
    /// chord-down.
    fn begin_recording(&mut self, mode: Mode, chord: crate::routes::ChordKind) {
        let resolved = self.resolve_route(chord);
        let s = self.settings();
        self.buffer.clear();
        // Stamped before anything else can fail: every later consumer
        // (`record_history`, the finalize seam) reads these, and a dictation
        // that started on the translate chord has to be filed as one even if
        // it ends in an error.
        self.session_chord = chord;
        self.session_route = resolved.route;
        self.pending_route_notice = resolved.notice;
        // Captured now, not at paste time: this is the moment the user
        // pressed the hotkey, which is the moment they were looking at their
        // intended destination. Transcription + formatting can take seconds,
        // long enough for focus to drift somewhere else entirely.
        self.dictation_target = crate::foreground::capture();
        self.stt = stt_path(s.provider, s.custom_endpoint.use_for_stt);
        // Beside `stt`, not inside the `cloud_on()` branch below: both are
        // this dictation's own snapshot, and `record_history` reads the pair
        // whatever path the dictation ended up taking.
        self.stt_lane = lane_for(&s);
        if self.cloud_on() {
            self.cloud_session += 1;
            let _ = self.cloud_tx.send(CloudCmd::Start {
                session: self.cloud_session,
                cfg: SessionCfg {
                    language_code: s.sarvam.language_code.clone(),
                    stream_type: s.sarvam.stream_type.clone(),
                    mode: s.sarvam.mode.clone(),
                    endpointing: match mode {
                        Mode::PushToTalk => Endpointing::Manual,
                        Mode::HandsFree => Endpointing::Vad,
                    },
                    prompt: if s.dictionary.is_empty() {
                        None
                    } else {
                        Some(s.dictionary.join(", "))
                    },
                    // Resolved here, at chord-down, and carried with the
                    // session: flipping the provider mid-sentence must not
                    // move a dictation that is already running. The
                    // credential is attached later, in the dispatcher —
                    // `Lane`'s doc comment says why.
                    lane: self.stt_lane.clone(),
                },
            });
        }
        self.gate.store(true, Ordering::Relaxed);
        if s.audio.cues {
            tones::play(Cue::Start);
        }
        if s.audio.pause_media {
            self.media_pause = Some(crate::media::pause_playing());
        }
        overlay::show(&self.app, s.overlay.offset_y);
        self.set_state(DictationState::Recording {
            mode,
            started: Instant::now(),
        });
    }

    /// Resume whatever GSMTC paused for this recording (`audio.pause_media`).
    /// Called from the two places capture stops, `finish_recording` and
    /// `cancel_recording`, and from nowhere later: the music starts again as
    /// the microphone closes, without waiting on the transcript, the polish
    /// or the paste.
    ///
    /// Dropping the guard is the whole operation, and it is correct even
    /// while the pause is still in flight: the pause thread finishes pausing,
    /// finds its guard already gone, and resumes immediately.
    fn resume_media(&mut self) {
        self.media_pause = None;
    }

    fn cancel_recording(&mut self) {
        self.gate.store(false, Ordering::Relaxed);
        self.buffer.clear();
        // The route bookkeeping is not dropped here: the `set_state(Idle)`
        // below drops it, because every other way a dictation can end needs
        // the same thing, and one place that always runs beats seven that
        // have to remember.
        if self.cloud_on() {
            let _ = self.cloud_tx.send(CloudCmd::Cancel);
        }
        self.resume_media();
        overlay::hide(&self.app);
        self.set_state(DictationState::Idle);
    }

    fn finish_recording(&mut self) {
        self.gate.store(false, Ordering::Relaxed);
        self.last_tap = None;
        // Armed here, not in `cancel_recording`: a tap-discard deliberately
        // arms `last_tap` instead so a fast second press still reaches
        // double-tap-to-hands-free. This is a stop either way — whatever the
        // speech gate below decides about the audio, the *chord* stopped.
        self.last_stop = Some(Instant::now());
        self.resume_media();
        let s = self.settings();
        if s.audio.cues {
            tones::play(Cue::Stop);
        }
        // The user's own end-of-speech instant — stamped *before*
        // `collect_tail` below, which blocks draining queued audio for up
        // to `TAIL_FLUSH_TIMEOUT_MS`. That wait happens after the hotkey
        // was released and before Sarvam ever hears about it, so it is
        // real latency from the user's perspective; carried to the cloud
        // dispatcher via `CloudCmd::Finish` so `sarvam::ws` measures
        // `drain_ms` from here, not from whenever `Finish` happens to reach
        // it — the dispatcher's receipt time would exclude exactly this
        // wait.
        let end_of_speech = Instant::now();
        self.collect_tail();
        let audio = std::mem::take(&mut self.buffer);
        // 16 kHz mono: samples / 16 = milliseconds.
        self.pending_duration_ms = (audio.len() / 16) as u64;

        // Speech gate: judged once, on the whole utterance, right here —
        // never mid-hands-free-session (this only runs at the stop that
        // ends a session, not at pauses within one). Cloud already has every
        // sample streamed live via `ControlMsg::Audio`/`collect_tail`, so a
        // `Silence` or `AllZero` outcome must tear that session down with
        // `Cancel` rather than leave it dangling, which `cancel_recording`
        // does. `Speech` and `Faint` both go on to be transcribed.
        //
        // Judged on `gate_evidence`, not the raw buffer: with cues on, the
        // start cue plays into this same recording and would keep it out of
        // `Silence` even when the user said nothing.
        let evidence = gate_evidence(&audio, s.audio.cues);
        // Checked on `evidence` — the buffer `decide` is about to window —
        // rather than the raw `audio` it was carved from: `gate_evidence`'s
        // own `MIN_TRIMMED_MS` floor keeps the two equal whenever the raw
        // recording itself was this short, and the floor guards what gets
        // windowed rather than a buffer `decide` never receives.
        if speech_gate::is_degenerate(evidence) {
            // Too short to possibly contain speech — the hotkey was tapped
            // by accident. Silent: flashing a notice over what the user
            // already knows was a stray brush would be noise, not
            // information.
            self.cancel_recording();
            return;
        }
        match speech_gate::decide(evidence) {
            speech_gate::GateOutcome::Speech => {}
            // Audible but under the speech bar. Transcribed rather than
            // discarded: see `speech_gate::GateOutcome::Faint` for why this
            // band is the transcriber's call and not the gate's.
            speech_gate::GateOutcome::Faint => {}
            speech_gate::GateOutcome::AllZero => {
                self.cancel_recording();
                self.error_flash(MUTED_MIC_NOTICE);
                self.set_state(DictationState::Idle);
                return;
            }
            speech_gate::GateOutcome::Silence => {
                self.cancel_recording();
                self.error_flash(QUIET_NOTICE);
                self.set_state(DictationState::Idle);
                return;
            }
        }

        // The selection lane — `should_capture_selection` owns the two
        // conditions and why they are what they are.
        //
        // Started here: after the speech gate has decided there is something
        // to transcribe (a stray tap must not fire a synthetic Ctrl+C at the
        // user's document), and before the audio goes out, so the read
        // overlaps transcription instead of being paid for afterwards. The
        // window it reads is the one captured at chord-down, not a fresh one
        // — where the user was looking when they pressed the key is the whole
        // point of that capture.
        // Gated on the same fact `routes::agent` will consult — a chat
        // backend exists — not on the Sarvam key alone: a custom-endpoint
        // install runs the agent, so it must also get the selection the agent
        // is about to be asked to edit.
        let capture_selection = should_capture_selection(self.session_chord, self.capable().chat);
        self.pending_selection = capture_selection.then(|| {
            crate::routes::selection::begin(self.dictation_target.clone(), self.suppress.clone())
        });

        let req_id = self.next_req;
        self.next_req += 1;
        match self.stt {
            SttPath::Cloud => {
                let _ = self.cloud_tx.send(CloudCmd::Finish {
                    req_id,
                    end_of_speech,
                    duration_ms: self.pending_duration_ms,
                });
            }
            SttPath::Local => {
                let _ = self.asr_tx.send(AsrMsg::Finalize { req_id, audio });
            }
            // The same handover the local path makes — the whole utterance,
            // once — with the configuration it was recorded under. The slot
            // is read here rather than at chord-down because this is the
            // moment the request is built, and `stt` already fixed the
            // decision that matters (whether Sarvam heard any of this).
            SttPath::Custom => {
                let language = crate::asr::custom::stt_language(&s.sarvam.language_code);
                let _ = self.custom_tx.send(crate::asr::custom::SttJob {
                    req_id,
                    audio,
                    duration_ms: self.pending_duration_ms,
                    slot: crate::endpoint::slot(),
                    language,
                    lane: self.stt_lane.clone(),
                });
            }
        }
        self.set_state(DictationState::Finalizing { req_id });

        // Watchdog: nothing downstream is allowed to hang the pill forever —
        // especially not a stalled network call. A stale req_id fires as a
        // harmless no-op.
        let timeout = match self.stt {
            SttPath::Cloud => CLOUD_FINALIZE_TIMEOUT,
            SttPath::Local => local_finalize_timeout(self.pending_duration_ms),
            SttPath::Custom => CUSTOM_FINALIZE_TIMEOUT,
        };
        let tx = self.tx_self.clone();
        std::thread::spawn(move || {
            std::thread::sleep(timeout);
            let _ = tx.send(ControlMsg::FinalizeTimeout { req_id });
        });
    }

    /// After closing the gate, consume queued audio until the pump's tail
    /// marker arrives so the last spoken words are included.
    ///
    /// Anything else that arrives meanwhile is kept in `deferred` and handled,
    /// in order, once the message that stopped the recording is done
    /// (`spawn`'s loop). Some of it cannot be lost: a History delete
    /// (`HistoryRemoved`) or a wake from sleep (`SystemResumed`) comes once.
    fn collect_tail(&mut self) {
        let deadline = Instant::now() + Duration::from_millis(TAIL_FLUSH_TIMEOUT_MS);
        loop {
            let now = Instant::now();
            if now >= deadline {
                tracing::warn!("audio tail timeout; proceeding with buffered audio");
                return;
            }
            let Ok(msg) = self.rx.recv_timeout(deadline - now) else {
                return;
            };
            match tail_step(msg) {
                TailStep::Chunk(chunk) => {
                    if self.cloud_on() {
                        let _ = self.cloud_tx.send(CloudCmd::Audio(chunk.clone()));
                    }
                    self.buffer.extend_from_slice(&chunk);
                }
                TailStep::End => return,
                TailStep::Later(msg) => self.deferred.push_back(msg),
            }
        }
    }

    /// Suppress the hook, mark Injecting, and paste `text` into the focused
    /// app on a one-shot thread. Used by finalize and paste-last.
    ///
    /// `next` says what to do with the undo bookkeeping when the paste
    /// reports back — every caller must state it, because forgetting is how
    /// a record outlives the text it describes. Whatever was armed before is
    /// dropped here regardless: this call is about to move the caret and
    /// change the document, so any existing record is stale the moment it
    /// starts.
    ///
    /// `target`, when present, is where the paste is aimed: the thread
    /// restores it to the foreground (best-effort — a failed restore still
    /// pastes, just wherever focus actually is, per `foreground::
    /// restore_foreground`'s doc) and derives the paste chord from whether
    /// it's a terminal. Callers with nothing worth targeting (paste-last with
    /// no capturable foreground window) pass `None` and get the plain
    /// non-terminal chord.
    ///
    /// The chord follows the window that will *receive* the paste, which is
    /// not the target when the restore couldn't be verified — the target is
    /// then, by definition, not foreground. Deriving Ctrl+Shift+V from a
    /// terminal we failed to reach would fire it at whatever holds focus
    /// instead, where it pastes nothing at all (Word's paste-formatting
    /// dialog, VS Code's Markdown preview) while `inject_text` still returns
    /// `Ok` and arms Undo — the dictation lost silently, and the clipboard
    /// restored over it 300 ms later.
    ///
    /// `watch_field` is auto-learn's provenance gate: `true` only when `text`
    /// is the user's own dictated words. See the monitor call site at the end
    /// of the thread, and [`TextSource`] for the rule that decides it.
    fn start_injection(
        &mut self,
        text: String,
        next: Option<InFlight>,
        target: Option<crate::foreground::Target>,
        watch_field: bool,
    ) {
        self.last_injection = None;
        self.in_flight = next;
        // Suppress the hook so our own Ctrl+V never re-triggers the chord.
        self.suppress.store(true, Ordering::Relaxed);
        self.set_state(DictationState::Injecting);
        let s = self.settings();
        let tx = self.tx_self.clone();
        // Cloned here, off the controller thread's critical path — the guard
        // is three shared handles and a channel sender.
        let learn = self.learn.clone();
        std::thread::spawn(move || {
            let terminal = match &target {
                Some(t) if crate::foreground::restore_foreground(t) => {
                    crate::foreground::is_terminal(t)
                }
                // The target is not foreground, so it is not the window
                // about to receive this paste. Ask who actually is rather
                // than aiming the target's chord at a stranger — but check
                // cheaply first whether it's still the *same process* as the
                // target (a second window of the same app is a common way
                // for `restore_foreground`'s HWND-exact check to read as
                // "failed" even though nothing meaningfully changed): a pid
                // match reuses `t`'s already-captured `class`, skipping a
                // fresh `GetClassNameW` + `QueryFullProcessImageNameW`
                // capture that would almost always agree with it anyway.
                Some(t) => {
                    tracing::warn!(
                        "foreground restore couldn't be verified; pasting into whatever holds focus"
                    );
                    if crate::foreground::current_foreground_pid() == Some(t.pid) {
                        crate::foreground::is_terminal(t)
                    } else {
                        crate::foreground::capture()
                            .as_ref()
                            .is_some_and(crate::foreground::is_terminal)
                    }
                }
                None => false,
            };
            let injected = match crate::injection::inject_text(
                &text,
                s.injection.restore_clipboard,
                s.injection.restore_delay_ms,
                terminal,
            ) {
                Ok(()) => true,
                Err(e) => {
                    // The document is unchanged, so nothing may be armed for
                    // undo: a record here would point Shift+Left at whatever
                    // the user typed themselves.
                    tracing::error!("injection failed: {e:#}");
                    false
                }
            };
            if injected && watch_field {
                // Auto-learn: watch the field this paste landed in, so a word
                // the user fixes in it can teach the app. Here rather than in
                // the `InjectionDone` arm because this is the last place that
                // still holds all three things the monitor needs — the text
                // exactly as pasted, the window it was aimed at, and whether
                // that window is a console. `InjectionDone` carries only a
                // bool, and `dictation_target` was consumed at the paste.
                //
                // `watch_field` is why this is not simply `if injected`. The
                // monitor pairs what it later reads out of the field against
                // the text this app put in, and calls the difference the
                // user's correction of a *dictation* — a claim that is only
                // true when the words were the user's own. Route answers
                // (translation, agent, wake) paste through this same call, and
                // a user reworking a model's sentence there is not teaching
                // the app how they say a word; promoted after two sightings it
                // would start rewriting their real dictations. `TextSource`
                // decides the bit and carries the full argument.
                //
                // Fire-and-forget: the monitor owns a thread, a time limit
                // and every one of its own stop conditions, and it never says
                // anything about what it read.
                crate::learn::monitor::watch(
                    crate::learn::monitor::Paste {
                        text,
                        hwnd: target.as_ref().map(crate::foreground::Target::hwnd),
                        terminal,
                        dictionary: s.dictionary,
                        enabled: s.learn.field_monitor_enabled,
                    },
                    // LOCK ORDERING: `observe` acquires the settings lock
                    // *inside* a blocking history-DB round trip, so nothing
                    // may hold that lock while calling it. Nothing here does:
                    // `s` is an owned snapshot (`Controller::settings` clones
                    // and drops the read guard on its own line), and the
                    // closure captures only the guard.
                    Box::new(move |pairs, session| {
                        learn.observe(pairs, &session);
                    }),
                );
            }
            let _ = tx.send(ControlMsg::InjectionDone { injected });
        });
    }

    /// Suppress the hook, mark Injecting, and run Undo's verified
    /// select-and-replace on a one-shot thread.
    ///
    /// The thread does not get to assume it worked: `replace_last` reads the
    /// selection back and only pastes if it is byte-for-byte `expected`, and
    /// the outcome rides home on `InjectionDone` so the controller can fall
    /// through to the clipboard fallback when it didn't.
    fn start_replace(&mut self, char_count: usize, expected: String, replacement: String) {
        self.in_flight = Some(InFlight::Undo {
            raw: replacement.clone(),
        });
        self.suppress.store(true, Ordering::Relaxed);
        self.set_state(DictationState::Injecting);
        let s = self.settings();
        let tx = self.tx_self.clone();
        std::thread::spawn(move || {
            let injected = match crate::injection::replace_last(
                char_count,
                &expected,
                &replacement,
                s.injection.restore_clipboard,
                s.injection.restore_delay_ms,
            ) {
                Ok(crate::injection::ReplaceOutcome::Replaced) => true,
                // Both of these mean the document was left exactly as it
                // was — the selection, if one was made, has been collapsed.
                Ok(crate::injection::ReplaceOutcome::Unverified) => false,
                Err(e) => {
                    tracing::error!("undo replace failed: {e:#}");
                    false
                }
            };
            let _ = tx.send(ControlMsg::InjectionDone { injected });
        });
    }

    /// The tone for the app about to receive the paste: first matching
    /// per-app rule wins, otherwise the global style.
    fn style_for_target(&self, app: Option<&str>) -> String {
        let s = self.settings();
        if let Some(app) = app {
            for rule in &s.style_rules {
                let pat = rule.app.trim().to_lowercase();
                if !pat.is_empty() && app.contains(&pat) {
                    return rule.style.clone();
                }
            }
        }
        s.style
    }

    /// File a finished dictation in the history DB (`history::Recorder`).
    ///
    /// `text` is the assembled transcript, `raw` the verbatim one from before
    /// rule cleanup or AI formatting; the History page treats `raw == text` as
    /// "no AI processing", which is already true of a dictation the formatter
    /// left alone.
    ///
    /// `outcome`/`error_code` come from the caller because two arms file rows,
    /// and what `text` means differs slightly between them:
    ///
    /// - `FinalResult` (`Done`, no code): post style, post smart-space —
    ///   exactly what gets pasted and exactly what `events::TRANSCRIPT_FINAL`
    ///   carries, so a row here and the stats feed can never disagree about
    ///   what was dictated.
    /// - `CloudTruncated` (`Failed` + `ERR_CONNECTION_LOST`): a known-
    ///   incomplete transcript that is deliberately *not* injected. It stops
    ///   at the rules → polish → guardrail pipeline's own output, without
    ///   `style::apply` or `smart_space_append`, because both of those shape
    ///   text for the window it is about to land in and this text lands
    ///   nowhere — a trailing space stored against a transcript nobody pasted
    ///   would be an artefact of a paste that never happened.
    ///
    /// An empty transcript and a recording the speech gate turned away file
    /// nothing — see the `history` module doc.
    ///
    /// Never logs either string: this is the one place both the cleaned and
    /// the verbatim transcript are in hand, and the module doc of `history`
    /// makes the same promise about its own error paths.
    fn record_history(
        &self,
        text: &str,
        raw: &str,
        app: Option<String>,
        outcome: crate::history::Outcome,
        error_code: Option<&str>,
    ) {
        self.history.record(crate::history::NewEntry {
            text: text.to_string(),
            raw_text: Some(raw.to_string()),
            outcome,
            error_code: error_code.map(str::to_string),
            provider: Some(self.stt.history_label(&self.stt_lane).into()),
            // Known exactly for the local provider. Deliberately `None` for
            // cloud: `ControlMsg::FinalResult` carries no marker for which
            // Sarvam path produced the text, and since the batch fallback
            // (`sarvam::batch::BATCH_MODEL`) answers with a different model
            // than the realtime session (`sarvam::REALTIME_MODEL`), naming
            // either one here would be a guess — and this column exists to
            // be trusted by a future "retry the same way" affordance.
            //
            // Read straight off the lock rather than through `settings()`,
            // which clones the whole struct (dictionary, replacements,
            // snippets, transforms) for one `String` — and only on the
            // branch that needs it, so the cloud path pays nothing.
            model: match self.stt {
                // Deliberately `None` for both network paths: the cloud one
                // carries no marker for which Sarvam model answered (see
                // below), and the custom endpoint's model lives in the slot,
                // not in `model.selected_id`, which names an on-device model
                // that had nothing to do with this row.
                SttPath::Cloud | SttPath::Custom => None,
                SttPath::Local => Some(
                    self.settings
                        .read()
                        .expect("settings lock")
                        .model
                        .selected_id
                        .clone(),
                ),
            },
            duration_ms: Some(self.pending_duration_ms),
            app,
            // Counted the way `dictationData.ts`'s `wordCount` counts (split
            // on whitespace runs, drop the empties) and on the same string it
            // receives, so the History page's totals match the Stats page's.
            words: Some(text.split_whitespace().count() as u32),
            // Filed under the chord the user pressed, whichever route it
            // ended up taking: a translate dictation whose translation was
            // skipped is still a translation here. The column is kept for a
            // later "run it again" button, which should ask for what the
            // user wanted, not repeat what went wrong. Plain dictation
            // stores `None` (NULL); see `Route::history_label`.
            route: self
                .session_chord
                .intent()
                .history_label()
                .map(str::to_string),
        });
    }

    /// Everything a dictation does once its final text is settled: tone,
    /// smart-space, History, the undo candidate, the stats event, the notice
    /// and the paste.
    ///
    /// One method for both ways a route can answer — inline
    /// (`RouteOutcome::Ready`) or later (`ControlMsg::RouteResult` after a
    /// `Deferred` job) — so a route that had to go to the network and one
    /// that did not are indistinguishable from here on. Splitting it was the
    /// alternative, and a second copy of this sequence is exactly how the two
    /// paths would drift.
    ///
    /// `notice` is the fresh one from whichever answer arrived; the
    /// chord-down one rides in `pending` and is the fallback.
    ///
    /// `pasted` is `routes::RouteDone::pasted`: the route already put `text`
    /// into the document itself. Only the selection lane's verify-then-replace
    /// sets it, and only a deferred route can — see that field's own doc for
    /// why the replacement cannot come back here to be pasted the ordinary
    /// way.
    ///
    /// `source` says whether `text` is the user's own dictated words or a
    /// model's, and is the only thing about it the two arrival paths are *not*
    /// allowed to be indistinguishable about: it gates auto-learn's field
    /// monitor. See [`TextSource`].
    fn finish_route(
        &mut self,
        pending: PendingRoute,
        text: Option<String>,
        notice: Option<crate::routes::Notice>,
        pasted: bool,
        source: TextSource,
    ) {
        let PendingRoute {
            dict_target,
            target_app,
            raw,
            fixes,
            guard_notice,
            route_notice,
            cut_short,
            ..
        } = pending;
        // At most one line reaches the pill. The route's own notice
        // describes what just happened and wins; the chord-down one
        // describes a skipped step decided before the user even finished
        // speaking.
        let route_notice = notice.or(route_notice);
        let Some(text) = text else {
            // The route declined to produce anything. Not a `FinalResult`
            // fallback and not a silent drop: the transcript is real and goes
            // to History (where the user can still get it), the reason goes
            // to the pill, and nothing is typed. Filed like `CloudTruncated`
            // — the verbatim transcript, no tone or smart-space, since this
            // text lands nowhere.
            self.record_history(
                &raw,
                &raw,
                target_app,
                crate::history::Outcome::Failed,
                Some(ERR_ROUTE_UNAVAILABLE),
            );
            // Deliberately not `last_text`, not `last_injection` and no
            // `events::TRANSCRIPT_FINAL`, for the same reason
            // `CloudTruncated` skips them: all three describe text this app
            // put into a document.
            self.error_flash(route_notice.as_deref().unwrap_or(ROUTE_SKIPPED_NOTICE));
            self.set_state(DictationState::Idle);
            return;
        };
        if pasted {
            // The selection lane already replaced the user's selection with
            // exactly these bytes, inside the verification window that proved
            // the selection was still there. Everything below this line is the
            // *caret* path: style and smart-space shape text for the window it
            // is about to land in, and `start_injection` would type the
            // replacement a second time.
            //
            // Skipping `start_injection` also skips auto-learn's field monitor,
            // and that is the right call rather than a casualty. The monitor
            // watches a field for the user correcting what was *dictated* into
            // it, and pairs what it sees against `raw`; here `raw` is a spoken
            // command and the text in the field is a model's rewrite of the
            // user's own prose, so every pair it produced would be noise
            // learned as vocabulary. `TextSource` is that same judgement made
            // one bit wide for the pastes that do go through `start_injection`
            // — this lane reaches the answer structurally instead.
            self.last_text = text.clone();
            self.record_history(
                &text,
                &raw,
                target_app.clone(),
                crate::history::Outcome::Done,
                completed_error_code(cut_short),
            );
            let _ = self.app.emit(
                events::TRANSCRIPT_FINAL,
                events::FinalPayload {
                    text: text.clone(),
                    duration_ms: self.pending_duration_ms,
                    app: target_app,
                    words_corrected: fixes.words_corrected,
                    dict_fixes: fixes.dict_fixes,
                },
            );
            overlay::hide(&self.app);
            // Undo AI Edit is deliberately NOT armed. `replace_last` selects
            // the characters left of the caret and pastes `raw` over them, and
            // `raw` here is the spoken *command* ("Butterfly, fix the
            // grammar") — undoing an edit to the selection that way would type
            // the instruction into the document in place of the user's own
            // paragraph. The undo of an in-place edit is the host app's own
            // Ctrl+Z, which still has the original selection on its stack.
            self.last_injection = None;
            if let Some(message) = route_notice.or(guard_notice) {
                self.error_flash(&message);
            }
            self.set_state(DictationState::Idle);
            return;
        }
        // Snippet expansions are typed as the user saved them, whatever the tone.
        let expansions: Vec<String> =
            self.settings().snippets.into_iter().map(|s| s.expansion).collect();
        let text = crate::cleanup::style::apply(
            text,
            &self.style_for_target(target_app.as_deref()),
            &expansions,
        );
        // Final assembly: whatever leaves here is exactly what gets pasted,
        // recorded as the undo target, and shown/copied as `last_text` — so
        // the trailing space has to be added before any of those, not layered
        // on after.
        let text = if self.settings().dictation.smart_space {
            smart_space_append(text)
        } else {
            text
        };
        self.last_text = text.clone();
        // File it in the history DB. Here rather than after the paste,
        // because history records what the app *produced*: a paste that never
        // lands (the clipboard failed, focus moved to something that ignores
        // Ctrl+V) is exactly when the user most needs the transcript to still
        // be findable. It also lands on the last line that still owns `raw` —
        // `next_last_injection` below consumes it. Fire-and-forget, so
        // nothing about the paste waits on SQLite.
        self.record_history(
            &text,
            &raw,
            target_app.clone(),
            crate::history::Outcome::Done,
            completed_error_code(cut_short),
        );
        // Built as one atomic value, past the empty-text guard in the
        // `FinalResult` arm: writing `raw` any earlier would let a
        // filler-only utterance ("um") desync it from `last_text` and leave
        // Undo pasting a discarded transcript into the document.
        // Handed to `start_injection` below rather than installed here — it
        // only becomes the live undo target once the paste reports that it
        // landed.
        let armed = next_last_injection(
            self.last_injection.take(),
            &text,
            raw,
            Instant::now(),
            target_app.clone(),
        );
        let _ = self.app.emit(
            events::TRANSCRIPT_FINAL,
            events::FinalPayload {
                text: text.clone(),
                duration_ms: self.pending_duration_ms,
                app: target_app,
                words_corrected: fixes.words_corrected,
                dict_fixes: fixes.dict_fixes,
            },
        );
        overlay::hide(&self.app);
        // A rejected format still injects usable text (the rule pipeline's
        // output) — but the rejection must never be silent, so flash it on
        // the pill. This never fires for a successful format: `guard_notice`
        // is only set on rejection.
        //
        // The route notice comes first when both are set: the user explicitly
        // asked for a translation or an agent and did not get it, which is a
        // bigger surprise than the formatter falling back to its own rule
        // output. Only one can show — `error_flash` owns the pill for
        // `ERROR_FLASH_MS`.
        if let Some(message) = route_notice.or(guard_notice) {
            self.error_flash(&message);
        }
        self.start_injection(text, armed.map(InFlight::Arm), dict_target, source.watch_field());
    }

    /// Undo AI Edit: probe the document and replace only what reads back as
    /// this app's own text, otherwise a clipboard fallback that never touches
    /// the document. See `undo_decision` for what makes a probe worth firing
    /// and `injection::replace_last` for the check that decides the rest.
    fn undo(&mut self) {
        let elapsed = self
            .last_injection
            .as_ref()
            .map(|r| r.at.elapsed())
            .unwrap_or_default();
        // A fresh capture, not the dictation's `dictation_target`: Undo
        // probes whatever is in front of the user right now, which may no
        // longer be the window the text was pasted into.
        let current = crate::foreground::capture();
        let current_app = current.as_ref().and_then(|t| t.app.as_deref());
        let is_terminal = current.as_ref().is_some_and(crate::foreground::is_terminal);
        match undo_decision(self.last_injection.as_ref(), elapsed, current_app, is_terminal) {
            UndoDecision::Nothing => self.error_flash("Nothing to undo"),
            UndoDecision::Replace {
                char_count,
                expected,
                replacement,
            } => {
                // `last_injection` deliberately survives the launch. It is
                // cleared at `InjectionDone` only if the replace actually
                // landed — if the readback refused, nothing on screen changed
                // and the record is exactly as valid as it was a moment ago.
                self.start_replace(char_count, expected, replacement);
            }
            UndoDecision::CopyOnly { text } => {
                // Also not cleared: nothing on screen changed, so whatever
                // made this not worth probing (stale timer aside — that
                // direction only gets worse) can still resolve itself, e.g.
                // the user clicking back into the app the text was pasted into.
                copy_to_clipboard(text);
                self.error_flash(UNDO_COPIED_NOTICE);
            }
        }
    }

    /// File the transcript a deferred route was holding as a `Failed` row
    /// with `code`, for a route that will never answer. Taken here rather
    /// than left to `set_state(Idle)`, which would drop it: the row is built
    /// from the transcript the job was launched with.
    fn file_held_transcript(&mut self, code: &str) {
        let Some(p) = self.pending_route.take() else {
            return;
        };
        self.record_history(
            held_row_text(self.session_route, &p.cleaned, &p.raw),
            &p.raw,
            p.target_app,
            crate::history::Outcome::Failed,
            Some(code),
        );
    }

    /// Flash an error message on the pill, then hide it after a delay.
    fn error_flash(&mut self, message: &str) {
        let s = self.settings();
        overlay::show(&self.app, s.overlay.offset_y);
        let _ = self.app.emit(
            events::NOTICE_ERROR,
            events::NoticePayload {
                message: message.into(),
            },
        );
        let tx = self.tx_self.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(ERROR_FLASH_MS));
            let _ = tx.send(ControlMsg::HideOverlay);
        });
    }

    fn handle(&mut self, msg: ControlMsg) {
        match msg {
            ControlMsg::ChordDown(chord) => {
                if self.last_stop.is_some_and(|t| t.elapsed() < POST_STOP_COOLDOWN) {
                    // Key chatter: a bounce right on the heels of a stop, not
                    // a deliberate new press. Checked once here rather than
                    // only in the `Idle` arm below, so it suppresses chatter
                    // wherever `ChordDown` can land — including a hands-free
                    // session's own stop-tap arm, not just a fresh start —
                    // instead of depending on the state machine having
                    // already cycled back to `Idle` by the time the bounce
                    // arrives. Does not touch `last_tap`, so a real
                    // double-tap that happens to straddle this window is
                    // unaffected — it's armed by `cancel_recording`, a path
                    // that never touches `last_stop`.
                    return;
                }
                match self.state {
                    DictationState::Idle => {
                        let double_tap_ms = self.settings().hotkey.double_tap_ms;
                        let hands_free = self
                            .last_tap
                            .is_some_and(|t| t.elapsed() < Duration::from_millis(double_tap_ms));
                        self.begin_recording(
                            if hands_free {
                                Mode::HandsFree
                            } else {
                                Mode::PushToTalk
                            },
                            chord,
                        );
                    }
                    DictationState::Recording {
                        mode: Mode::HandsFree,
                        started,
                    } => {
                        // Second tap ends a hands-free session.
                        if started.elapsed() > Duration::from_millis(400) {
                            self.finish_recording();
                        }
                    }
                    _ => {}
                }
            }
            ControlMsg::ChordUp => {
                if let DictationState::Recording {
                    mode: Mode::PushToTalk,
                    started,
                } = self.state
                {
                    if is_tap(started.elapsed(), self.settings().hotkey.min_hold_ms) {
                        // Nothing usable in it — throw it away rather than
                        // finalize silence, and remember the release: another
                        // press inside the double-tap window turns this into
                        // the first half of a double-tap.
                        self.last_tap = Some(Instant::now());
                        self.cancel_recording();
                    } else {
                        self.finish_recording();
                    }
                }
            }
            ControlMsg::Escape => {
                match escape_disposition(
                    &self.state,
                    self.pending_route.as_ref().map(|p| p.req_id),
                    self.stt == SttPath::Local,
                ) {
                    EscapeDisposition::Ignore => {}
                    EscapeDisposition::CancelRecording => {
                        self.last_tap = None;
                        self.cancel_recording();
                    }
                    // The dictation is finished and the controller is holding
                    // it; only the route is still out. Escape stops waiting
                    // rather than cancelling — there are real words in hand,
                    // and the user asking for their dictation back should get
                    // it, not lose it. The job keeps running and its answer
                    // arrives to a controller that no longer holds a
                    // `PendingRoute`, so `route_result_is_current` drops it:
                    // cancellation stays structural, and nothing can
                    // double-apply.
                    EscapeDisposition::AbortRoute => {
                        let mut pending =
                            self.pending_route.take().expect("the disposition said so");
                        let cleaned = std::mem::take(&mut pending.cleaned);
                        let (text, notice) = route_abort_outcome(self.session_route, cleaned);
                        tracing::debug!(
                            typed = text.is_some(),
                            "escape stopped a route wait; finishing with what was held"
                        );
                        // `Dictation`: an aborted wait finishes with the text
                        // the controller was already holding, which is the
                        // cleanup pipeline's own output — the job that would
                        // have replaced it with a model's is still out, and
                        // whatever it eventually answers is dropped.
                        self.finish_route(
                            pending,
                            text,
                            Some(notice),
                            false,
                            TextSource::Dictation,
                        );
                    }
                    EscapeDisposition::AbandonLocal => {
                        tracing::debug!("escape abandoned an on-device transcription");
                        self.dictation_target = None;
                        overlay::hide(&self.app);
                        self.set_state(DictationState::Idle);
                    }
                }
            }
            ControlMsg::Audio(chunk) => {
                if let DictationState::Recording { mode, started } = self.state {
                    if self.cloud_on() {
                        let _ = self.cloud_tx.send(CloudCmd::Audio(chunk.clone()));
                    }
                    // Buffered in both modes: it feeds the local ASR, and in
                    // cloud mode keeps the debug last-utterance dump possible.
                    self.buffer.extend_from_slice(&chunk);
                    // Push-to-talk safety stop: audio chunks are a reliable
                    // tick while recording (the controller loop is otherwise
                    // recv-driven, with nothing else guaranteed to arrive on
                    // a schedule). Hands-free is exempt — a long hands-free
                    // session is the feature working, not a stuck key.
                    // Finalizes exactly like a real key release: the user's
                    // words are kept, not discarded.
                    if mode == Mode::PushToTalk && started.elapsed() >= MAX_PUSH_DURATION {
                        self.finish_recording();
                    }
                }
            }
            ControlMsg::Level(level) => {
                let _ = self
                    .app
                    .emit(events::LEVEL, events::LevelPayload { level });
            }
            ControlMsg::AudioTail => {}
            ControlMsg::AudioFailed => match self.state {
                // Only a live recording depends on the microphone. A failure
                // during Finalizing/Injecting must not reset state — the
                // audio is already captured and the in-flight FinalResult
                // would be dropped as stale.
                DictationState::Recording { .. } => {
                    self.cancel_recording();
                    self.error_flash("Microphone unavailable — check your input device");
                    self.set_state(DictationState::Idle);
                }
                DictationState::Idle => {
                    self.error_flash("Microphone unavailable — check your input device");
                }
                _ => {
                    tracing::warn!("audio stream failed during finalize/inject; ignoring");
                }
            },
            ControlMsg::FinalResult {
                req_id,
                text,
                raw,
                fixes,
                notice,
                cut_short,
            } => {
                let DictationState::Finalizing { req_id: want } = self.state else {
                    return;
                };
                if want != req_id {
                    return; // stale result
                }
                if text.is_empty() {
                    self.error_flash(QUIET_NOTICE);
                    self.set_state(DictationState::Idle);
                    return;
                }
                // Tone and app come from the window saved at chord-down, not
                // from a fresh `GetForegroundWindow` here: the user can click
                // into another window while the audio is still being turned
                // into text, and the paste goes to the saved window anyway.
                let dict_target = self.dictation_target.take();
                let target_app = dict_target.as_ref().and_then(|t| t.app.clone());
                // Assembled before the route runs, and handed whole to
                // whichever path answers: a deferred route's result arrives
                // in a later message, and re-deriving any of this then would
                // describe a different moment than the dictation it belongs
                // to.
                let pending = PendingRoute {
                    req_id,
                    dict_target,
                    target_app,
                    raw,
                    fixes,
                    cleaned: text.clone(),
                    guard_notice: notice,
                    route_notice: self.pending_route_notice.take(),
                    cut_short,
                };
                // What the selection lane found, if it was asked. Resolved
                // here rather than carried further because this is the only
                // place that reads it, and it is a *borrow* into the route
                // context below — the selection is the user's document
                // content and gets copied nowhere it is not needed.
                //
                // This can block, briefly and only in the case where the read
                // has not finished yet (`RESOLVE_BUDGET`); an agent chord's
                // read starts at the stop and transcription has to land
                // before this line runs, so in practice the answer is already
                // waiting.
                let selection = self.pending_selection.take().map(|p| p.resolve());
                // The route seam, before tone and smart-space: those two
                // shape text for the window it is about to land in, and a
                // route that produces nothing to land has no use for either.
                // Everything downstream of here — history, undo, the stats
                // event, the paste — then sees one consistent string.
                let outcome = {
                    let s = self.settings();
                    crate::routes::apply(
                        self.session_route,
                        text,
                        &pending.raw,
                        &crate::routes::RouteCtx {
                            // The chord, not `session_route`: the wake-word
                            // scan may only run on a dictation the user asked
                            // to be a plain one, and a skipped translation
                            // resolves to the same `Cleanup` route.
                            chord: self.session_chord,
                            http: &self.http,
                            api_key: &self.sarvam_key,
                            target_language: &s.translation.target_language,
                            agent_name: &s.agent.name,
                            settings: &s,
                            selection: selection.as_ref(),
                        },
                    )
                };
                // Auto-learn's provenance bit, read off the outcome here
                // because this is the one place both answers are in view. Only
                // the `Ready` arm uses it; a `Deferred` outcome's answer comes
                // back through `RouteResult`, which says `Model` for itself.
                let source = TextSource::of(&outcome);
                match outcome {
                    // `pasted: false` — a synchronous route never touches the
                    // document; only a deferred one can, and only the
                    // selection lane does.
                    crate::routes::RouteOutcome::Ready { text, notice } => {
                        self.finish_route(pending, text, notice, false, source)
                    }
                    // Stay exactly where we are: `Finalizing { req_id }` is
                    // already "waiting for this transcript's final form", and
                    // the watchdog `finish_recording` armed for it is already
                    // counting. A route job inherits whatever is left of that
                    // budget rather than starting a second clock — nothing
                    // here takes an `Instant`, so nothing here can be fooled
                    // by a suspend.
                    crate::routes::RouteOutcome::Deferred(job) => {
                        let tx = self.tx_self.clone();
                        tauri::async_runtime::spawn(async move {
                            let done = job.into_future().await;
                            let _ = tx.send(ControlMsg::RouteResult {
                                req_id,
                                text: done.text,
                                notice: done.notice,
                                pasted: done.pasted,
                            });
                        });
                        self.pending_route = Some(pending);
                    }
                }
            }
            ControlMsg::RouteResult {
                req_id,
                text,
                notice,
                pasted,
            } => {
                if !route_result_is_current(
                    &self.state,
                    self.pending_route.as_ref().map(|p| p.req_id),
                    req_id,
                ) {
                    // Cancelled, superseded, or already given up on by the
                    // watchdog — and whatever ended it has already said so.
                    // Dropping the answer is the point: pasting it into
                    // whatever the user moved on to would be worse than
                    // losing it.
                    return;
                }
                let pending = self.pending_route.take().expect("checked by the guard");
                // `Model`: this message only ever carries a `Deferred` job's
                // answer, and a route defers exactly when it is going to call
                // one. Auto-learn does not watch what lands from here — see
                // `TextSource`, including the failed-translation case it
                // knowingly gives up.
                self.finish_route(pending, text, notice, pasted, TextSource::Model);
            }
            // A transcript that is real but known-incomplete: file it, say
            // so, inject nothing. See `ControlMsg::CloudTruncated` for why
            // this is neither a `FinalResult` with a notice nor a plain
            // `CloudError`.
            ControlMsg::CloudTruncated { req_id, text, raw, message } => {
                let DictationState::Finalizing { req_id: want } = self.state else {
                    return;
                };
                if want != req_id {
                    return; // stale result
                }
                // Taken, not read: this dictation is over, and leaving the
                // captured target armed would let the next `FinalResult`
                // inherit a window the user has long since left. The app
                // label still goes on the row, so History shows where the
                // dictation was headed.
                let target_app = self.dictation_target.take().and_then(|t| t.app);
                self.record_history(
                    &text,
                    &raw,
                    target_app,
                    crate::history::Outcome::Failed,
                    Some(ERR_CONNECTION_LOST),
                );
                // Deliberately not `last_text`, not `last_injection`, and no
                // `events::TRANSCRIPT_FINAL`: "Copy last transcript", Undo
                // and the Stats feed all describe text this app actually put
                // into a document, and this text never left the app.
                self.error_flash(&message);
                self.set_state(DictationState::Idle);
            }
            ControlMsg::CloudError {
                req_id,
                session,
                message,
            } => match self.state {
                // req_id 0 = the session died while the user was still
                // speaking; stop the recording rather than let them talk
                // into a dead socket. The session guard drops errors from a
                // previous session that raced the start of this one, and
                // cloud_on shields a local-provider recording (which never
                // bumps the session counter) from leftover cloud errors.
                DictationState::Recording { .. }
                    if req_id == 0 && self.cloud_on() && session == self.cloud_session =>
                {
                    self.last_tap = None;
                    self.cancel_recording();
                    self.error_flash(&message);
                    self.set_state(DictationState::Idle);
                }
                DictationState::Finalizing { req_id: want } if want == req_id => {
                    self.error_flash(&message);
                    self.set_state(DictationState::Idle);
                }
                _ => {} // stale
            },
            // The session ended while the user was still recording, with
            // words already transcribed and nothing left to transcribe what
            // comes next (`ControlMsg::CloudEnded`). Stop here, the way a
            // released key would — `finish_recording` is the path the
            // push-to-talk safety stop takes under a held chord — so the
            // result arrives now rather than after the user has talked on
            // into a closed socket. Hands-free is where that matters most:
            // there may be no release for minutes.
            ControlMsg::CloudEnded { session } => {
                if relay_end_is_current(&self.state, self.cloud_on(), session, self.cloud_session) {
                    self.finish_recording();
                }
            }
            ControlMsg::FinalizeTimeout { req_id } => {
                match finalize_timeout_disposition(
                    &self.state,
                    self.pending_route.as_ref().map(|p| p.req_id),
                    req_id,
                ) {
                    TimeoutDisposition::Stale => {}
                    TimeoutDisposition::Expire {
                        file_held_transcript,
                    } => {
                        if self.cloud_on() {
                            let _ = self.cloud_tx.send(CloudCmd::Cancel);
                        }
                        // A route job still in flight means the transcript
                        // itself arrived and is sitting in `pending_route` —
                        // only the route ran out of budget. Those words are
                        // filed rather than dropped with the job, the same
                        // way `CloudTruncated` files a fragment it refuses to
                        // paste (`held_row_text` says which text the row
                        // keeps). Otherwise the arm is the same either way:
                        // same notice, same transition, and a plain timeout
                        // files nothing because it has nothing in hand.
                        if file_held_transcript {
                            self.file_held_transcript(ERR_ROUTE_TIMEOUT);
                        }
                        self.error_flash("Timed out — try again");
                        self.set_state(DictationState::Idle);
                    }
                }
            }
            ControlMsg::InjectionDone { injected } => {
                self.suppress.store(false, Ordering::Relaxed);
                // Undo is armed here and nowhere else, so it can never point
                // at text the app didn't actually write.
                match injection_outcome(self.in_flight.take(), injected) {
                    UndoBookkeeping::Arm(rec) => self.last_injection = Some(rec),
                    UndoBookkeeping::Disarm => self.last_injection = None,
                    UndoBookkeeping::FallBackToClipboard { raw } => {
                        copy_to_clipboard(raw);
                        self.error_flash(UNDO_COPIED_NOTICE);
                    }
                }
                self.set_state(DictationState::Idle);
            }
            ControlMsg::TransformChord(i) => {
                if !matches!(self.state, DictationState::Idle) {
                    return;
                }
                if self.transform_busy.swap(true, Ordering::Relaxed) {
                    return; // one at a time; the running thread owns the flag
                }
                let s = self.settings();
                let Some(t) = s.transforms.get(i).filter(|_| s.transforms_enabled) else {
                    self.transform_busy.store(false, Ordering::Relaxed);
                    return;
                };
                // A transform rewrites a selection somewhere else in the
                // document and leaves the caret at the end of ITS output, so
                // any undo record is now describing text that is no longer in
                // front of the caret. Cleared before the thread starts, and
                // cleared even for a transform that ends up failing: giving up
                // an undo the user probably wasn't going to reach for costs
                // nothing, and the alternative is a live record pointing at
                // someone else's paragraph.
                self.last_injection = None;
                crate::transforms::spawn(
                    self.app.clone(),
                    self.tx_self.clone(),
                    self.suppress.clone(),
                    self.transform_busy.clone(),
                    self.http.clone(),
                    s.overlay.offset_y,
                    crate::transforms::TransformJob {
                        name: t.name.clone(),
                        prompt: t.prompt.clone(),
                        model: s.sarvam.polish_model.clone(),
                        api_key: self.sarvam_key.read().expect("key lock").clone(),
                        // Read here, on the controller thread, from the same
                        // snapshot as everything else in this job: the thread
                        // it is about to run on must not have to reach back
                        // into settings to find out which host it is on.
                        lane: lane_for(&s),
                        restore_clipboard: s.injection.restore_clipboard,
                        // At the chord, like a dictation's target: the model
                        // call can take seconds, and focus can move on.
                        target: crate::foreground::capture(),
                    },
                );
            }
            ControlMsg::AppShortcut(i) => {
                if !matches!(self.state, DictationState::Idle) {
                    return;
                }
                match i {
                    crate::hotkeys::SHORTCUT_PASTE_LAST => {
                        if self.last_text.is_empty() {
                            self.error_flash("Nothing dictated yet");
                        } else {
                            // `None` for undo bookkeeping: a manual re-paste
                            // is not a dictation, so it arms nothing — and
                            // `start_injection` drops any existing record,
                            // because this paste moves the caret past a
                            // second copy of the text and invalidates
                            // whatever the record described.
                            //
                            // The paste *target*, unlike the undo
                            // bookkeeping, is captured fresh here rather than
                            // reused from `dictation_target`: this shortcut
                            // has no dictation session of its own to inherit
                            // one from, so the sensible destination is
                            // wherever the user is looking right now.
                            let target = crate::foreground::capture();
                            // Not watched, for the same reason nothing is
                            // armed: this is not a dictation. `last_text` is
                            // whatever the last finish produced and carries no
                            // provenance — it can just as easily be an agent
                            // answer as spoken words — and the paste that
                            // *did* have provenance already got its 30 s
                            // window when it landed, so re-watching a second
                            // copy of the same text buys nothing worth the
                            // risk of learning from model prose.
                            self.start_injection(self.last_text.clone(), None, target, false);
                        }
                    }
                    crate::hotkeys::SHORTCUT_COPY_LAST => {
                        if self.last_text.is_empty() {
                            self.error_flash("Nothing dictated yet");
                        } else {
                            copy_to_clipboard(self.last_text.clone());
                        }
                    }
                    crate::hotkeys::SHORTCUT_SCRATCHPAD => {
                        crate::tray::show_main(&self.app);
                        // `"notes"`, not `"scratchpad"`: the Scratchpad page
                        // was replaced by Notes and the renderer's NAVIGATE
                        // handler looks the payload up in `NAV_MAIN`, silently
                        // ignoring anything it can't find. The shortcut keeps
                        // its historical name in settings and in
                        // `SHORTCUT_SCRATCHPAD`, because renaming a settings
                        // field would drop the binding a user already has.
                        let _ = self
                            .app
                            .emit(events::NAVIGATE, events::NavigatePayload { page: "notes" });
                    }
                    crate::hotkeys::SHORTCUT_UNDO_AI => self.undo(),
                    _ => {}
                }
            }
            ControlMsg::HideOverlay => {
                if matches!(self.state, DictationState::Idle) {
                    overlay::hide(&self.app);
                }
            }
            ControlMsg::SystemResumed => {
                if should_cancel_on_resume(&self.state, self.cloud_on()) {
                    // A transcript that already arrived is filed before the
                    // teardown drops it: only the route it was waiting on
                    // died with the connection.
                    if holds_route_transcript(
                        &self.state,
                        self.pending_route.as_ref().map(|p| p.req_id),
                    ) {
                        self.file_held_transcript(ERR_INTERRUPTED);
                    }
                    // `cancel_recording` already does the right thing from
                    // either `Recording` (gate/buffer/media are live) or
                    // `Finalizing` (they were already settled back in
                    // `finish_recording`, so those steps are harmless
                    // no-ops) — the same reasoning `FinalizeTimeout` relies
                    // on to send `CloudCmd::Cancel` from `Finalizing` too.
                    // Tell the user rather than let them wait out a timeout
                    // that can never resolve against a socket already dead.
                    self.cancel_recording();
                    self.error_flash(RESUME_NOTICE);
                    // `cancel_recording` already lands on `Idle`; restated
                    // explicitly here to match this file's own convention at
                    // every other error-flash-after-teardown call site (e.g.
                    // `AudioFailed`'s `Recording` arm, `CloudError`'s
                    // `Finalizing` arm) rather than leaving it implicit.
                    self.set_state(DictationState::Idle);
                }
                // `Instant` is QueryPerformanceCounter-backed on Windows and
                // does not reliably advance across S3 sleep (see
                // `LastInjection::at`'s and `last_stop`'s own doc comments):
                // a record made before a long sleep can read as seconds old
                // right after resume instead of hours old. Both docs already
                // argue the *consequence* of that is tolerable on its own —
                // this clears them proactively anyway, unconditionally
                // (regardless of `cloud_on` or `state` above: the QPC trap
                // has nothing to do with the cloud provider), so neither
                // check has to rely on a frozen clock surviving a resume it
                // was never measured across.
                self.last_stop = None;
                self.last_injection = None;
            }
            // Both cancel rather than finish: after a lock the microphone is
            // hearing the room, and a paused hook lets no key end the
            // recording. Nothing is pasted and nothing is filed.
            ControlMsg::SessionLocked | ControlMsg::Paused => {
                if cancels_on_interruption(&self.state) {
                    self.last_tap = None;
                    self.cancel_recording();
                }
            }
            ControlMsg::HistoryRemoved(removed) => {
                // Paste and Copy last transcript must not bring back a
                // dictation the user just deleted. Undo's record goes too:
                // it holds the same words.
                if forgets_last_text(&self.last_text, &removed) {
                    self.last_text.clear();
                    self.last_injection = None;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DEFAULT_MIN_HOLD: u64 = 150;

    /// Taps measured from a real session's logs: 118 ms and 201 ms. Both are
    /// halves of a double-tap; judged against `min_hold_ms` alone the 201 ms
    /// one would finalize a fifth of a second of silence and take hands-free
    /// down with it.
    #[test]
    fn real_double_tap_holds_are_taps() {
        assert!(is_tap(Duration::from_millis(118), DEFAULT_MIN_HOLD));
        assert!(is_tap(Duration::from_millis(201), DEFAULT_MIN_HOLD));
        assert!(is_tap(Duration::from_millis(300), DEFAULT_MIN_HOLD));
    }

    #[test]
    fn a_deliberate_hold_is_a_dictation() {
        assert!(!is_tap(Duration::from_millis(350), DEFAULT_MIN_HOLD));
        assert!(!is_tap(Duration::from_millis(800), DEFAULT_MIN_HOLD));
        assert!(!is_tap(Duration::from_secs(5), DEFAULT_MIN_HOLD));
    }

    /// `min_hold_ms` may only widen the tap window, never narrow it.
    #[test]
    fn min_hold_can_raise_the_bar_but_not_lower_it() {
        assert!(is_tap(Duration::from_millis(500), 600));
        assert!(!is_tap(Duration::from_millis(700), 600));
        // Below TAP_MAX the setting is ignored, so double-tap stays reachable.
        assert!(is_tap(Duration::from_millis(200), 10));
    }

    // --- Deferred routes --------------------------------------------------
    //
    // Same rationale as the Undo block below: `Controller` owns an
    // `AppHandle` and five channels, so the decisions a deferred route rests
    // on are free functions and these test those. The arms are one-line
    // callers.

    /// The happy path the whole mechanism exists for: the job answered, the
    /// dictation it belongs to is still the one being finalized, and the
    /// controller is still holding its transcript — so the answer is applied
    /// and `finish_route` reaches the paste.
    #[test]
    fn a_route_result_for_the_waiting_dictation_is_applied() {
        assert!(route_result_is_current(
            &DictationState::Finalizing { req_id: 7 },
            Some(7),
            7
        ));
    }

    /// Superseded: the user started (and finished) another dictation while
    /// the job was in flight, so the controller is finalizing a *different*
    /// request. Pasting the old answer into whatever they are doing now would
    /// be worse than losing it.
    #[test]
    fn a_route_result_from_a_superseded_dictation_is_dropped() {
        assert!(!route_result_is_current(
            &DictationState::Finalizing { req_id: 8 },
            Some(8),
            7
        ));
    }

    /// Cancelled: Escape, a dead mic, a resumed machine or the watchdog all
    /// end the dictation through `set_state(Idle)`, which drops the pending
    /// route. Both halves of the guard have to reject it — the state is no
    /// longer `Finalizing`, and there is nothing left to apply the answer to.
    #[test]
    fn a_route_result_after_the_dictation_was_cancelled_is_dropped() {
        assert!(!route_result_is_current(&DictationState::Idle, None, 7));
        assert!(!route_result_is_current(&DictationState::Injecting, None, 7));
        // The state alone is not enough: a pending route must still be held.
        assert!(!route_result_is_current(
            &DictationState::Finalizing { req_id: 7 },
            None,
            7
        ));
        // Nor is the pending id alone — this is the shape a watchdog expiry
        // leaves if the state guard were ever dropped.
        assert!(!route_result_is_current(
            &DictationState::Recording {
                mode: Mode::PushToTalk,
                started: Instant::now()
            },
            Some(7),
            7
        ));
    }

    /// A watchdog expiry with a job in flight: the transcript arrived and is
    /// being held, so it gets filed rather than thrown away with the job. The
    /// notice and the transition are the timeout's own, unchanged.
    #[test]
    fn a_timeout_with_a_route_in_flight_files_the_held_transcript() {
        assert_eq!(
            finalize_timeout_disposition(&DictationState::Finalizing { req_id: 3 }, Some(3), 3),
            TimeoutDisposition::Expire {
                file_held_transcript: true
            }
        );
    }

    /// A plain finalize timeout has nothing in hand — the transcript never
    /// arrived — and must keep filing nothing, exactly as before.
    #[test]
    fn a_plain_timeout_still_files_nothing() {
        assert_eq!(
            finalize_timeout_disposition(&DictationState::Finalizing { req_id: 3 }, None, 3),
            TimeoutDisposition::Expire {
                file_held_transcript: false
            }
        );
    }

    /// The pairing is checked, not assumed. A held route from *another*
    /// dictation is not this dictation's transcript, and filing it against
    /// this expiry would put one dictation's words on another's row.
    /// Unreachable while `set_state(Idle)` drops the pending route, but the
    /// guarantee lives in the function rather than in that invariant.
    #[test]
    fn a_timeout_never_files_someone_elses_held_transcript() {
        assert_eq!(
            finalize_timeout_disposition(&DictationState::Finalizing { req_id: 3 }, Some(4), 3),
            TimeoutDisposition::Expire {
                file_held_transcript: false
            }
        );
    }

    /// Every finalize arms a watchdog and most are outlived, so a stale one
    /// is the common case and must stay a silent no-op.
    #[test]
    fn a_stale_watchdog_does_nothing() {
        assert_eq!(
            finalize_timeout_disposition(&DictationState::Finalizing { req_id: 4 }, Some(4), 3),
            TimeoutDisposition::Stale
        );
        assert_eq!(
            finalize_timeout_disposition(&DictationState::Idle, None, 3),
            TimeoutDisposition::Stale
        );
        assert_eq!(
            finalize_timeout_disposition(&DictationState::Injecting, Some(3), 3),
            TimeoutDisposition::Stale
        );
    }

    // --- Auto-learn provenance --------------------------------------------
    //
    // The bit `finish_route` hands `start_injection`, decided by `TextSource`
    // and asserted here over outcomes built by the real `routes::apply` rather
    // than hand-made ones — the whole subtlety is *which* route answers which
    // way, and a literal `RouteOutcome` in a test would assume exactly the
    // thing worth checking. The `Deferred` job is dropped un-run, so no test
    // here makes a request.

    /// Everything a `RouteCtx` borrows — the same shape `routes::translate`'s
    /// tests use, kept local because that one is private to its module.
    fn route_parts(
        key: Option<&str>,
        dictation_language: &str,
        target: &str,
    ) -> (
        reqwest::Client,
        crate::sarvam::SharedKey,
        crate::settings::Settings,
    ) {
        let mut s = crate::settings::Settings::default();
        s.sarvam.language_code = dictation_language.into();
        s.translation.target_language = target.into();
        (
            reqwest::Client::new(),
            std::sync::Arc::new(std::sync::RwLock::new(key.map(str::to_string))),
            s,
        )
    }

    macro_rules! route_ctx {
        ($http:expr, $key:expr, $s:expr, $chord:expr) => {
            crate::routes::RouteCtx {
                chord: $chord,
                http: &$http,
                api_key: &$key,
                target_language: &$s.translation.target_language,
                agent_name: &$s.agent.name,
                settings: &$s,
                selection: None,
            }
        };
    }

    /// The ordinary case, and the one auto-learn exists for: a plain dictation
    /// pastes the user's own words, so the field it lands in is worth watching.
    #[test]
    fn a_plain_dictation_is_watched() {
        let (http, k, s) = route_parts(None, "en-IN", "");
        let outcome = crate::routes::apply(
            crate::routes::Route::Cleanup,
            "Hello there.".into(),
            "hello there",
            &route_ctx!(http, k, s, crate::routes::ChordKind::Dictation),
        );
        assert_eq!(TextSource::of(&outcome), TextSource::Dictation);
        assert!(TextSource::of(&outcome).watch_field());
    }

    /// The subtle one. A translation with no key is skipped, leaving the
    /// cleanup pipeline's output plus a notice — inline `Ready`, carrying the user's
    /// own words. It is a dictation in every way that matters to the monitor,
    /// and gating on "was this a translate chord?" instead of on how the
    /// outcome arrived would silently stop watching it.
    #[test]
    fn a_skipped_translation_is_still_a_dictation_and_is_watched() {
        let (http, k, s) = route_parts(None, "en-IN", "hi-IN");
        let outcome = crate::routes::apply(
            crate::routes::Route::Translation,
            "Hello there.".into(),
            "hello there",
            &route_ctx!(http, k, s, crate::routes::ChordKind::Translate),
        );
        assert!(
            matches!(outcome, crate::routes::RouteOutcome::Ready { .. }),
            "a skipped translation must never leave the controller thread"
        );
        assert!(TextSource::of(&outcome).watch_field());
    }

    /// The case this rule exists for: a route that goes to a model comes back
    /// `Deferred`, and what it eventually pastes is the model's prose. Nothing
    /// the user then changes in that field is a fact about how they speak, so
    /// the monitor never sees it. `ControlMsg::RouteResult` — the only way a
    /// deferred answer reaches `finish_route` — hardcodes `Model` for exactly
    /// this reason.
    #[test]
    fn a_deferred_route_answer_is_never_watched() {
        let (http, k, s) = route_parts(Some("key"), "en-IN", "hi-IN");
        let outcome = crate::routes::apply(
            crate::routes::Route::Translation,
            "Hello there.".into(),
            "hello there",
            &route_ctx!(http, k, s, crate::routes::ChordKind::Translate),
        );
        assert!(
            matches!(outcome, crate::routes::RouteOutcome::Deferred(_)),
            "a configured translation must defer — the premise of the rule"
        );
        assert_eq!(TextSource::of(&outcome), TextSource::Model);
        assert!(!TextSource::of(&outcome).watch_field());
    }

    // --- Escape -----------------------------------------------------------

    /// Escape's historical job, unchanged: throw away a live recording.
    #[test]
    fn escape_still_cancels_a_live_recording() {
        for mode in [Mode::PushToTalk, Mode::HandsFree] {
            assert_eq!(
                escape_disposition(
                    &DictationState::Recording {
                        mode,
                        started: Instant::now()
                    },
                    None,
                    false
                ),
                EscapeDisposition::CancelRecording
            );
        }
    }

    /// The behaviour that must NOT change. Escape during a plain cloud
    /// finalize has always been a no-op — the transcript is in flight and
    /// there is nothing to hand the user instead — and it stays one. Only the
    /// presence of a *held* dictation makes Escape mean anything here.
    #[test]
    fn escape_during_a_plain_cloud_finalize_is_still_a_no_op() {
        assert_eq!(
            escape_disposition(&DictationState::Finalizing { req_id: 5 }, None, false),
            EscapeDisposition::Ignore
        );
    }

    /// An on-device transcription has a watchdog that grows with the
    /// utterance, so Escape is how the user gives up on a long one. A held
    /// route still wins: those words are finished, not dropped.
    #[test]
    fn escape_abandons_an_on_device_transcription() {
        assert_eq!(
            escape_disposition(&DictationState::Finalizing { req_id: 5 }, None, true),
            EscapeDisposition::AbandonLocal
        );
        assert_eq!(
            escape_disposition(&DictationState::Finalizing { req_id: 5 }, Some(5), true),
            EscapeDisposition::AbortRoute
        );
    }

    /// The new one: the transcript already arrived and only the route is
    /// still out, so Escape stops waiting instead of doing nothing for the
    /// rest of the finalize budget.
    #[test]
    fn escape_with_a_route_in_flight_aborts_the_wait() {
        assert_eq!(
            escape_disposition(&DictationState::Finalizing { req_id: 5 }, Some(5), false),
            EscapeDisposition::AbortRoute
        );
    }

    /// Same pairing requirement as the watchdog's: a route held for a
    /// different dictation is not this one's to finish.
    #[test]
    fn escape_never_finishes_someone_elses_route() {
        assert_eq!(
            escape_disposition(&DictationState::Finalizing { req_id: 5 }, Some(6), false),
            EscapeDisposition::Ignore
        );
    }

    /// Past the route seam the text is already on its way into a document;
    /// there is nothing left for Escape to decide.
    #[test]
    fn escape_during_an_injection_does_nothing() {
        assert_eq!(
            escape_disposition(&DictationState::Injecting, Some(5), true),
            EscapeDisposition::Ignore
        );
        assert_eq!(
            escape_disposition(&DictationState::Idle, None, true),
            EscapeDisposition::Ignore
        );
    }

    /// An abandoned translation still pastes the dictation. The words went
    /// through the whole pipeline and exist; the only thing that did not
    /// happen is the translation, and the notice says exactly that.
    #[test]
    fn an_aborted_translation_pastes_the_words_it_was_holding() {
        assert_eq!(
            route_abort_outcome(crate::routes::Route::Translation, "Hello there.".into()),
            (
                Some("Hello there.".to_string()),
                crate::routes::translate::TRANSLATION_SKIPPED_NOTICE.to_string()
            )
        );
    }

    /// ...and an abandoned agent route still types nothing. Its transcript is
    /// a command, and "the user pressed Escape" is not a reason to paste one
    /// into their document as prose.
    #[test]
    fn an_aborted_agent_route_still_types_nothing() {
        let (text, notice) = route_abort_outcome(
            crate::routes::Route::Agent,
            "Butterfly, delete that paragraph.".into(),
        );
        assert_eq!(text, None, "a command must never be pasted as prose");
        assert_eq!(notice, ROUTE_SKIPPED_NOTICE);
    }

    // --- The selection lane's gate ------------------------------------------

    /// THE GATE. A plain dictation must never read what the user has
    /// selected — the answer is typed at the caret either way, so the read
    /// buys nothing and costs a cleared clipboard plus a synthetic Ctrl+C
    /// fired into whatever they were working in (a console reads that as
    /// SIGINT). The same is true of the translate chord.
    ///
    /// Asserted over the whole table rather than on the agent row alone,
    /// because the failure mode is somebody *widening* the condition, and a
    /// test that only checks the agent row would pass while doing so.
    #[test]
    fn only_the_agent_chord_ever_reads_the_users_selection() {
        for chord in [crate::routes::ChordKind::Dictation, crate::routes::ChordKind::Translate] {
            for chat_ready in [false, true] {
                assert!(
                    !should_capture_selection(chord, chat_ready),
                    "{chord:?} (chat={chat_ready}) must not touch the user's document"
                );
            }
        }
        assert!(should_capture_selection(crate::routes::ChordKind::Agent, true));
    }

    /// The second condition: with no chat backend, `routes::agent` reports
    /// that before it ever consults the selection, so a capture would disturb
    /// the document to inform a command that is already going to type
    /// nothing. It is "a backend", not "a Sarvam key": the agent can run
    /// entirely on the user's own endpoint, and gating the read on the
    /// key would leave that install's agent editing a selection it never read.
    #[test]
    fn an_agent_chord_with_no_backend_reads_nothing_either() {
        assert!(!should_capture_selection(crate::routes::ChordKind::Agent, false));
    }

    // --- The finalize budget ----------------------------------------------

    /// `CLOUD_FINALIZE_TIMEOUT`'s doc comment itemizes every step it has to
    /// outlast, and the failure mode when it does not is the watchdog
    /// throwing away a transcript that actually arrived. The terms that are
    /// importable are summed from their own constants here, so raising one of
    /// them fails this test instead of silently eating the margin; the `ws`
    /// terms are private to that module and are restated with the names the
    /// doc gives them.
    #[test]
    fn the_cloud_finalize_budget_outlasts_every_step_it_itemizes() {
        let ws_terms = Duration::from_millis(2200) * 2 // ws::ConnectBackoff, twice
            + Duration::from_secs(4) * 2               // ws::CONNECT_TIMEOUT, twice
            + Duration::from_secs(4)                   // ws::FLUSH_WAIT_FLOOR
            + Duration::from_secs(6)                   // ws::FLUSH_WAIT_CEILING
            + Duration::from_secs(1); // ws::GOODBYE_TIMEOUT
        // The route term is a MAX over the routes, not a sum of them: one
        // dictation takes exactly one route, and the ceiling has to cover the
        // worst one. Summing both would budget for a dictation that cannot
        // exist.
        let translate_route = crate::sarvam::translate::TRANSLATE_TIMEOUT;
        // The replace step is part of the agent route's own cost, not a step
        // after it: `routes::agent` awaits `run_replace` inside the deferred
        // job, so the controller is still Finalizing while it runs.
        let agent_route = crate::routes::selection::RESOLVE_BUDGET
            + crate::sarvam::chat::AGENT_TIMEOUT
            + crate::routes::selection::REPLACE_BUDGET;
        let route_term = translate_route.max(agent_route);
        assert_eq!(
            route_term, agent_route,
            "the agent route stopped being the worst one — re-derive the doc comment"
        );
        // Polish is priced at three requests, not one. `Backend::complete`'s
        // timeout is *per request*, and `format::backend` can make three of
        // them against a custom host that refuses the first — which this
        // path reaches through `endpoint::resolve_polish_backend`.
        // The bound is private to that module and restated here, the same
        // way the `ws` terms are; it is pinned there by
        // `a_third_refusal_is_returned_as_the_error`.
        let polish_requests = 3;
        let sum = ws_terms
            + crate::sarvam::batch::BATCH_TIMEOUT
            + crate::sarvam::CREDENTIAL_WAIT
            + crate::sarvam::chat::POLISH_TIMEOUT * polish_requests
            + route_term;
        assert_eq!(
            sum,
            Duration::from_millis(80_270),
            "the doc comment's arithmetic no longer adds up"
        );
        assert!(
            CLOUD_FINALIZE_TIMEOUT >= sum,
            "the watchdog would discard a successful dictation: {CLOUD_FINALIZE_TIMEOUT:?} < {sum:?}"
        );
        // The rounding rule, as a tripwire rather than prose. `>= sum` alone
        // would go on passing while the margin shrank to milliseconds; the
        // margin is what absorbs the step nobody has written down yet, so it
        // is the thing worth asserting.
        let margin = Duration::from_secs(1);
        assert!(
            CLOUD_FINALIZE_TIMEOUT >= sum + margin,
            "the ceiling is the sum rounded up to leave at least a full second: \
             {CLOUD_FINALIZE_TIMEOUT:?} < {sum:?} + {margin:?}"
        );
    }

    /// The custom-endpoint path's own budget, summed from its own constants.
    /// Every term is importable here, so raising any of them fails this test
    /// rather than silently eating the margin.
    #[test]
    fn the_custom_finalize_budget_outlasts_every_step_it_itemizes() {
        // One polish call is up to three HTTP requests on a custom backend —
        // a bound private to `format::backend` and restated here, exactly as
        // the cloud budget above restates the `ws` terms. It is pinned there
        // by `a_third_refusal_is_returned_as_the_error`.
        let polish_requests = 3;
        let polish = crate::sarvam::chat::POLISH_TIMEOUT * polish_requests;
        let route_term = crate::routes::selection::RESOLVE_BUDGET
            + crate::sarvam::chat::AGENT_TIMEOUT
            + crate::routes::selection::REPLACE_BUDGET;
        let sum = crate::asr::custom::TIMEOUT_CEILING
            + crate::sarvam::CREDENTIAL_WAIT
            + polish
            + route_term;
        assert_eq!(
            sum,
            Duration::from_millis(104_870),
            "the doc comment's arithmetic no longer adds up"
        );
        assert!(
            CUSTOM_FINALIZE_TIMEOUT >= sum,
            "the watchdog would discard a successful dictation: {CUSTOM_FINALIZE_TIMEOUT:?} < {sum:?}"
        );
        assert!(
            CUSTOM_FINALIZE_TIMEOUT - sum >= Duration::from_secs(1),
            "the margin that absorbs un-itemized steps is under a second"
        );
    }

    /// The on-device path's budget, summed from its terms. Decoding scales
    /// with the utterance, so the check runs from a tap to a twenty-minute
    /// hands-free session: none of them may be dropped by the watchdog while
    /// it is still decoding.
    #[test]
    fn the_local_finalize_budget_outlasts_every_step_it_itemizes() {
        // One model read from disk, restated with the reasoning the doc gives
        // it: under 500 MB at ~50 MB/s.
        let model_load = Duration::from_secs(10);
        // `cleanup::polish`'s TIMEOUT, private to that module.
        let polish = Duration::from_secs(4);
        let route_term = crate::routes::selection::RESOLVE_BUDGET
            + crate::sarvam::chat::AGENT_TIMEOUT
            + crate::routes::selection::REPLACE_BUDGET;
        let fixed = model_load * 2 + polish + route_term;
        assert_eq!(
            fixed,
            Duration::from_millis(48_870),
            "the doc comment's arithmetic no longer adds up"
        );
        for utterance_ms in [0, 400, 30_000, 20 * 60 * 1000] {
            let decode = Duration::from_millis(utterance_ms);
            let sum = fixed + decode;
            let ceiling = local_finalize_timeout(utterance_ms);
            assert!(
                ceiling >= sum + Duration::from_secs(1),
                "{utterance_ms} ms: {ceiling:?} leaves under a second over {sum:?}"
            );
        }
    }

    // --- Which transcriber a dictation uses -------------------------------

    /// The custom endpoint wins over the provider dropdown, both ways round:
    /// the toggle is the more specific intent, and a stale `provider` must
    /// not send audio somewhere the user thought they had left.
    #[test]
    fn the_custom_endpoint_wins_over_whichever_provider_is_selected() {
        for provider in [Provider::Sarvam, Provider::Local] {
            assert_eq!(stt_path(provider, true), SttPath::Custom, "{provider:?}");
        }
    }

    /// A settings file can carry `useForStt: true` that was written before
    /// the transcription route existed — from an import, or a hand edit. It
    /// is **honoured on arrival, not reset**: the flag has only ever
    /// meant one thing, and quietly undoing a choice the user made is what
    /// `repair()` reserves for values that became *invalid*, never for values
    /// that became *effective*. Honouring it also cannot leak anything, which
    /// is the half that makes the decision safe rather than merely principled
    /// — with no URL configured, the request is refused before a byte is sent
    /// and the pill names the fix.
    #[test]
    fn an_stt_toggle_that_predates_this_route_is_honoured_and_fails_loudly() {
        assert_eq!(stt_path(Provider::Sarvam, true), SttPath::Custom);
        let stale = crate::endpoint::CustomSlot {
            use_for_stt: true,
            ..Default::default()
        };
        assert_eq!(
            crate::endpoint::resolve_stt(&stale),
            Err(crate::endpoint::Invalid::NotConfigured),
            "an unconfigured endpoint must refuse before any audio is sent"
        );
    }

    #[test]
    fn without_the_stt_toggle_the_provider_decides_exactly_as_before() {
        assert_eq!(stt_path(Provider::Sarvam, false), SttPath::Cloud);
        assert_eq!(stt_path(Provider::Local, false), SttPath::Local);
    }

    /// Cloud is a cloud dictation: the realtime socket, the finalize
    /// timeout and the whole incremental-polish path are the same ones
    /// Bring-your-own-key takes. Only the host differs, and that is
    /// `lane_for`'s business, not `stt_path`'s.
    #[test]
    fn cloud_takes_the_cloud_path() {
        assert_eq!(stt_path(Provider::Cloud, false), SttPath::Cloud);
        assert_eq!(stt_path(Provider::Cloud, true), SttPath::Custom);
    }

    #[test]
    fn the_lane_follows_the_provider() {
        use crate::sarvam::{Lane, DEFAULT_RELAY_URL};
        let byok = crate::settings::Settings::default();
        assert_eq!(lane_for(&byok), Lane::Byok);
        let cloud = crate::settings::Settings {
            provider: Provider::Cloud,
            ..crate::settings::Settings::default()
        };
        assert_eq!(
            lane_for(&cloud),
            Lane::Cloud {
                relay: DEFAULT_RELAY_URL.into()
            }
        );
    }

    /// The hidden override, and the two ways a hand-edited value goes
    /// wrong: a trailing slash (which would double up in every route) and a
    /// blank string (which is not a URL and must not become one).
    #[test]
    fn the_hidden_relay_override_is_trimmed_and_ignored_when_blank() {
        use crate::sarvam::{Lane, DEFAULT_RELAY_URL};
        let with = |relay_url: Option<&str>| {
            lane_for(&crate::settings::Settings {
                provider: Provider::Cloud,
                cloud: crate::settings::CloudSettings {
                    relay_url: relay_url.map(str::to_string),
                },
                ..crate::settings::Settings::default()
            })
        };
        assert_eq!(
            with(Some("http://127.0.0.1:8787/")),
            Lane::Cloud {
                relay: "http://127.0.0.1:8787".into()
            }
        );
        assert_eq!(
            with(Some("   ")),
            Lane::Cloud {
                relay: DEFAULT_RELAY_URL.into()
            }
        );
    }

    /// The relay receives the sign-in bearer and the audio, so an override
    /// has to be https, or plain http to this machine for a local relay.
    /// Anything else is ignored and the shipped relay is used.
    #[test]
    fn a_relay_override_must_be_https_unless_it_is_this_machine() {
        use crate::sarvam::{Lane, DEFAULT_RELAY_URL};
        let relay = |relay_url: &str| {
            match lane_for(&crate::settings::Settings {
                provider: Provider::Cloud,
                cloud: crate::settings::CloudSettings {
                    relay_url: Some(relay_url.to_string()),
                },
                ..crate::settings::Settings::default()
            }) {
                Lane::Cloud { relay } => relay,
                Lane::Byok => panic!("a Cloud install is on the Cloud lane"),
            }
        };
        for kept in [
            "https://staging.example.workers.dev",
            "http://127.0.0.1:8787",
            "http://localhost:8787",
            "http://[::1]:8787",
        ] {
            assert_eq!(relay(kept), kept);
        }
        for refused in [
            "http://attacker.example",
            "http://192.168.1.20:8787",
            "ws://127.0.0.1:8787",
            "ftp://relay.example",
            "http://evil.example\\@127.0.0.1:8787",
            "not a url",
        ] {
            assert_eq!(relay(refused), DEFAULT_RELAY_URL, "{refused}");
        }
    }

    /// The History chip's vocabulary. The first two must be exactly what
    /// `settings::Provider` serializes to; the third has no counterpart there
    /// and must not pretend to — filing custom-endpoint audio as "sarvam"
    /// would misreport the one thing this column exists to report.
    #[test]
    fn the_history_provider_labels_match_the_settings_enum() {
        fn serialized(p: crate::settings::Provider) -> String {
            serde_json::to_value(p)
                .expect("Provider serializes")
                .as_str()
                .expect("Provider serializes as a string")
                .to_string()
        }
        use crate::sarvam::Lane;
        assert_eq!(
            SttPath::Cloud.history_label(&Lane::Byok),
            serialized(crate::settings::Provider::Sarvam)
        );
        assert_eq!(
            SttPath::Local.history_label(&Lane::Byok),
            serialized(crate::settings::Provider::Local)
        );
        assert_eq!(SttPath::Custom.history_label(&Lane::Byok), "custom");
        let all = [SttPath::Cloud, SttPath::Local, SttPath::Custom];
        let labels: std::collections::HashSet<&str> = all
            .iter()
            .map(|p| p.history_label(&Lane::Byok))
            .collect();
        assert_eq!(labels.len(), all.len(), "every path must be distinguishable");
    }

    /// A Cloud dictation is filed as "cloud", not "sarvam". That user has no
    /// Sarvam account: their audio went to Butterfly Labs' relay, and the
    /// History page is the one place they go to check what happened. The
    /// string is `settings::Provider::Cloud`'s own, so the chip stays in the
    /// settings file's vocabulary exactly as the other two do.
    #[test]
    fn a_cloud_dictation_is_filed_as_cloud() {
        use crate::sarvam::{Lane, DEFAULT_RELAY_URL};
        let cloud = Lane::Cloud {
            relay: DEFAULT_RELAY_URL.into(),
        };
        assert_eq!(
            SttPath::Cloud.history_label(&cloud),
            serde_json::to_value(crate::settings::Provider::Cloud)
                .expect("Provider serializes")
                .as_str()
                .expect("Provider serializes as a string")
        );
        assert_eq!(SttPath::Cloud.history_label(&cloud), "cloud");
        // The lane says nothing about the other two paths: a local dictation
        // is local whatever the provider dropdown was left on, and the custom
        // endpoint is the toggle that overrode the provider entirely.
        assert_eq!(SttPath::Local.history_label(&cloud), "local");
        assert_eq!(SttPath::Custom.history_label(&cloud), "custom");
    }

    /// On the Cloud lane the app holds no Sarvam key at all, so gating chat on
    /// the key would make every chat-gated feature — the voice agent,
    /// transforms, note actions — read as unavailable however signed in the
    /// user was.
    #[test]
    fn a_signed_in_cloud_install_can_reach_chat() {
        use crate::sarvam::{Lane, DEFAULT_RELAY_URL};
        let cloud = Lane::Cloud {
            relay: DEFAULT_RELAY_URL.into(),
        };
        assert!(
            chat_capable(&cloud, None, "sarvam-105b", || true),
            "a signed-in Cloud install polishes, transforms and runs the agent through the relay"
        );
        // The signed-out direction is asserted in `endpoint`, against an
        // explicit slot: the live one is process state another test may be
        // holding, and "no chat backend" is exactly the answer it changes.
    }

    /// Bring your own key is decided by the key — and the sign-in is never even
    /// consulted, because asking costs a read of the credential store at every
    /// chord-down.
    #[test]
    fn the_key_lane_never_asks_about_a_sign_in() {
        use crate::sarvam::Lane;
        let asked = std::cell::Cell::new(false);
        let ask = || {
            asked.set(true);
            true
        };
        assert_eq!(
            chat_capable(&Lane::Byok, None, "sarvam-105b", ask),
            crate::endpoint::chat_backend_exists(None, "sarvam-105b"),
            "the key lane is the resolver it has always been"
        );
        assert!(
            !asked.get(),
            "a Bring-your-own-key install must not pay for an answer about the other lane"
        );
        assert!(chat_capable(&Lane::Byok, Some("k"), "sarvam-105b", || false));
    }

    /// Only the Sarvam socket counts as "cloud on". The custom endpoint is a
    /// network path too, but nothing about a `CloudCmd` applies to it: a
    /// `Cancel` sent on its behalf would tear down a session it never opened,
    /// and the resume handler would abandon a recording that is perfectly
    /// alive (the audio is buffered locally, exactly as on the local path).
    #[test]
    fn only_the_sarvam_path_counts_as_a_live_cloud_session() {
        for (path, cloud_on) in [
            (SttPath::Cloud, true),
            (SttPath::Local, false),
            (SttPath::Custom, false),
        ] {
            assert_eq!(path == SttPath::Cloud, cloud_on, "{path:?}");
            assert_eq!(
                should_cancel_on_resume(
                    &DictationState::Recording {
                        mode: Mode::PushToTalk,
                        started: Instant::now(),
                    },
                    path == SttPath::Cloud,
                ),
                cloud_on,
                "{path:?}"
            );
        }
    }

    // --- History delete and clear reach Paste/Copy last transcript --------

    #[test]
    fn clearing_history_forgets_the_last_dictation() {
        use crate::state::HistoryRemoval;
        assert!(forgets_last_text("the meeting is at noon", &HistoryRemoval::All));
        assert!(!forgets_last_text("", &HistoryRemoval::All), "nothing to forget");
    }

    #[test]
    fn deleting_the_last_dictation_forgets_it_by_either_text() {
        use crate::state::HistoryRemoval;
        let row = |text: &str, raw: Option<&str>| HistoryRemoval::Row {
            text: text.into(),
            raw: raw.map(str::to_string),
        };
        let last = "The meeting is at noon. ";
        assert!(forgets_last_text(last, &row(last, Some("the meeting is at noon"))));
        // A row whose `text` was since edited on Home is still matched when
        // its verbatim transcript is what was pasted (nothing polished it).
        assert!(forgets_last_text("the meeting is at noon", &row("edited", Some("the meeting is at noon"))));
        assert!(!forgets_last_text(last, &row("A different dictation.", Some("a different dictation"))));
        assert!(!forgets_last_text(last, &row("A different dictation.", None)));
        assert!(!forgets_last_text("", &row("", None)), "an empty last text is nothing to forget");
    }

    // --- Undo AI Edit ---------------------------------------------------
    //
    // These test the FREE functions `undo_decision`, `next_last_injection`
    // and `injection_outcome`, not `Controller` methods: `Controller` owns an
    // `AppHandle` and five channels, so constructing one in a unit test is
    // not reasonable. The `Controller::undo` / `handle` bodies are one-line
    // callers of these.
    //
    // What is NOT covered, and cannot be: the select-read-compare-paste round
    // trip itself. `injection::replace_last` is SendInput plus a live system
    // clipboard, which needs a focused window with a caret in it — there is
    // no headless stand-in, so it is verified by hand, not by CI. The two
    // decisions that surround it (is this probe worth firing, and what does
    // its answer mean) are pure and are covered here;
    // `injection::tests::readback_matches` covers the comparison in between.

    fn sample_injection() -> LastInjection {
        LastInjection {
            text: "The meeting is at 3:30 PM.".to_string(),
            raw: "um so the meeting is at three thirty pm".to_string(),
            at: Instant::now(),
            app: Some("notepad".to_string()),
        }
    }

    #[test]
    fn undo_is_a_no_op_before_any_dictation() {
        assert_eq!(
            undo_decision(None, Duration::ZERO, None, false),
            UndoDecision::Nothing
        );
    }

    #[test]
    fn undo_is_a_no_op_when_formatting_changed_nothing() {
        let rec = LastInjection {
            text: "Hello.".to_string(),
            raw: "Hello.".to_string(),
            at: Instant::now(),
            app: Some("notepad".to_string()),
        };
        assert_eq!(
            undo_decision(Some(&rec), Duration::ZERO, Some("notepad"), false),
            UndoDecision::Nothing
        );
    }

    /// THE REGRESSION this pins: smart spacing appends its trailing space to
    /// the *final assembly*, after formatting, so `rec.text` is one space
    /// longer than `rec.raw` even when the formatter changed nothing — and
    /// under stock settings it routinely changes nothing (`smart_space`
    /// defaults on, the `formal` style is an identity, and the cloud already
    /// returns punctuated text). Compared against `rec.raw`, `Nothing` would
    /// be unreachable: Undo would fire a destructive select-read-replace probe
    /// into the user's live document whose only net effect was deleting the
    /// space the app had just added.
    ///
    /// Built the way the `FinalResult` arm builds it — through
    /// `smart_space_append` and `next_last_injection` — because the
    /// hand-assembled `LastInjection` in
    /// `undo_is_a_no_op_when_formatting_changed_nothing` cannot reach this.
    #[test]
    fn undo_is_a_no_op_when_only_smart_spacing_changed_the_text() {
        let raw = "Sounds good.";
        let text = smart_space_append(raw.to_string());
        assert_eq!(text, "Sounds good. ");
        let rec = next_last_injection(
            None,
            &text,
            raw.to_string(),
            Instant::now(),
            Some("notepad".to_string()),
        )
        .expect("a non-empty result records an undo target");

        assert_eq!(
            undo_decision(Some(&rec), Duration::ZERO, Some("notepad"), false),
            UndoDecision::Nothing
        );
    }

    /// A genuine edit still gets undone — and the replacement keeps the
    /// trailing space smart spacing added. Undoing the AI edit is not
    /// undoing the spacing: dropping it would run the user's next word
    /// straight into the last one.
    #[test]
    fn an_undo_restores_the_transcript_with_the_smart_space_intact() {
        let text = smart_space_append("Ship it Tuesday.".to_string());
        let rec = next_last_injection(
            None,
            &text,
            "so ship it tuesday".to_string(),
            Instant::now(),
            Some("notepad".to_string()),
        )
        .expect("a non-empty result records an undo target");

        assert_eq!(
            undo_decision(Some(&rec), Duration::ZERO, Some("notepad"), false),
            UndoDecision::Replace {
                char_count: "Ship it Tuesday. ".chars().count(),
                expected: "Ship it Tuesday. ".to_string(),
                replacement: "so ship it tuesday ".to_string(),
            }
        );
    }

    /// The happy path: recent, same app, ASCII-only injected text is worth
    /// probing, and the probe carries `expected` so the readback has
    /// something to compare against. Without `expected` reaching
    /// `replace_last` there is nothing to verify and the selection is deleted
    /// on trust.
    #[test]
    fn eligible_undo_probes_and_replaces() {
        let rec = sample_injection();
        assert_eq!(
            undo_decision(Some(&rec), Duration::from_secs(5), Some("notepad"), false),
            UndoDecision::Replace {
                char_count: rec.text.chars().count(),
                expected: rec.text.clone(),
                replacement: rec.raw.clone(),
            }
        );
    }

    /// `char_count` and `expected` must describe the same string: the count
    /// drives how far Shift+Left travels and `expected` decides whether what
    /// it selected is allowed to be deleted, so a drift between them turns
    /// the verification into a rubber stamp for the wrong span.
    #[test]
    fn the_probes_char_count_matches_the_text_it_verifies() {
        let rec = sample_injection();
        let UndoDecision::Replace {
            char_count,
            expected,
            ..
        } = undo_decision(Some(&rec), Duration::ZERO, Some("notepad"), false)
        else {
            panic!("a fresh same-app ASCII record must be worth probing");
        };
        assert_eq!(char_count, expected.chars().count());
        assert_eq!(expected, rec.text);
    }

    #[test]
    fn undo_falls_back_to_copy_after_the_window_expires() {
        let rec = sample_injection();
        assert_eq!(
            undo_decision(
                Some(&rec),
                UNDO_WINDOW + Duration::from_secs(1),
                Some("notepad"),
                false
            ),
            UndoDecision::CopyOnly {
                text: rec.raw.clone()
            }
        );
    }

    /// Don't fire the probe into a window the user has since switched away
    /// from: the readback synthesizes Ctrl+C, which some apps read as an
    /// interrupt rather than a copy. Note this is the *process* name, so
    /// switching between two windows of the same app still reads as a match
    /// — the readback, not this check, is what catches that.
    #[test]
    fn undo_falls_back_to_copy_when_the_foreground_app_changed() {
        let rec = sample_injection();
        assert_eq!(
            undo_decision(Some(&rec), Duration::ZERO, Some("chrome"), false),
            UndoDecision::CopyOnly {
                text: rec.raw.clone()
            }
        );
    }

    /// An app that can't be identified on either side is treated as a
    /// mismatch, not a free pass — the whole point of the check is catching
    /// "we don't know where the caret is now".
    #[test]
    fn undo_falls_back_to_copy_when_the_app_is_unknown() {
        let rec = sample_injection();
        assert_eq!(
            undo_decision(Some(&rec), Duration::ZERO, None, false),
            UndoDecision::CopyOnly {
                text: rec.raw.clone()
            }
        );
        let mut unknown_origin = sample_injection();
        unknown_origin.app = None;
        assert_eq!(
            undo_decision(Some(&unknown_origin), Duration::ZERO, Some("notepad"), false),
            UndoDecision::CopyOnly {
                text: unknown_origin.raw.clone()
            }
        );
    }

    /// When the paste went into a terminal, the same-app check passes
    /// trivially, so consoles get their own gate: the probe's Ctrl+C can be
    /// delivered to a console's foreground program as an interrupt, and no
    /// readback can undo an interrupt already sent. `is_terminal: true` short-
    /// circuits the probe even for an otherwise-eligible record — fresh, same
    /// app, ASCII and all — down to the clipboard fallback. *Which* processes
    /// and window classes count as a terminal is `foreground::is_terminal`'s
    /// job and is tested there; this only checks `undo_decision` respects the
    /// flag.
    #[test]
    fn undo_never_probes_when_the_target_is_a_terminal() {
        let rec = sample_injection();
        assert_eq!(
            undo_decision(Some(&rec), Duration::ZERO, Some("notepad"), true),
            UndoDecision::CopyOnly {
                text: rec.raw.clone()
            },
            "the probe must never fire when is_terminal is true"
        );
    }

    /// Devanagari (or any other combining-mark script): Shift+Left is not
    /// guaranteed to move exactly one Rust `char` per press, so `char_count`
    /// would select the wrong span. The readback would refuse it, but a probe
    /// designed to miscount isn't worth the clipboard round-trip.
    #[test]
    fn undo_falls_back_to_copy_for_non_ascii_injected_text() {
        let rec = LastInjection {
            text: "बैठक 3:30 बजे है।".to_string(),
            raw: "baithak teen tees baje hai".to_string(),
            at: Instant::now(),
            app: Some("notepad".to_string()),
        };
        assert_eq!(
            undo_decision(Some(&rec), Duration::ZERO, Some("notepad"), false),
            UndoDecision::CopyOnly {
                text: rec.raw.clone()
            }
        );
    }

    /// THE REGRESSION this pins: if nothing cleared the recorded injection
    /// after a successful undo, `undo_decision` would keep returning `Replace`
    /// forever and every subsequent press would append another copy of the raw
    /// transcript onto the document. The clear happens at `InjectionDone`,
    /// driven by `injection_outcome` (see
    /// `a_landed_undo_leaves_nothing_to_undo`) — this half asserts the state
    /// that clear produces really does end the chain.
    #[test]
    fn second_undo_is_a_no_op_once_the_record_is_cleared() {
        let rec = sample_injection();
        let first = undo_decision(Some(&rec), Duration::ZERO, Some("notepad"), false);
        assert!(matches!(first, UndoDecision::Replace { .. }));

        let second = undo_decision(None, Duration::ZERO, Some("notepad"), false);
        assert_eq!(second, UndoDecision::Nothing);
    }

    /// THE REGRESSION this pins: writing `last_raw` from a bare field
    /// assignment before the empty-text early return lets a filler-only
    /// utterance ("um") overwrite the raw half of the undo target while
    /// `last_text` stays pointed at the previous real dictation — two fields,
    /// desynced, both wrong. An empty result must leave a prior recording
    /// completely untouched.
    #[test]
    fn empty_result_leaves_the_previous_undo_target_untouched() {
        let prev = sample_injection();
        let prev_text = prev.text.clone();
        let prev_raw = prev.raw.clone();

        let next = next_last_injection(
            Some(prev),
            "",
            "um".to_string(),
            Instant::now(),
            Some("notepad".to_string()),
        );

        let next = next.expect("a prior recording must survive an empty result");
        assert_eq!(next.text, prev_text);
        assert_eq!(next.raw, prev_raw);
    }

    #[test]
    fn non_empty_result_replaces_the_undo_target_atomically() {
        let prev = sample_injection();
        let next = next_last_injection(
            Some(prev),
            "Ship it Tuesday.",
            "so ship it tuesday".to_string(),
            Instant::now(),
            Some("slack".to_string()),
        );

        let next = next.expect("a non-empty result must record a new undo target");
        assert_eq!(next.text, "Ship it Tuesday.");
        assert_eq!(next.raw, "so ship it tuesday");
        assert_eq!(next.app.as_deref(), Some("slack"));
    }

    // --- What the speech gate is allowed to judge -------------------------

    /// `ms` of 16 kHz audio with every sample at `level`. A constant window
    /// measures exactly at its own amplitude, so tests can place it precisely
    /// against the speech gate's constants.
    fn tone_ms(level: f32, ms: usize) -> Vec<f32> {
        vec![level; ms * SAMPLES_PER_MS]
    }

    /// With cues on, the cue-length front of a silent hold is loud enough to
    /// keep the whole recording out of `Silence`. Trimming it off lets the
    /// quiet remainder be judged for what it is.
    #[test]
    fn trimming_the_cue_restores_silence() {
        let mut audio = tone_ms(speech_gate::CLEAR_SPEECH_LEVEL, CUE_BLEED_MS);
        audio.extend(tone_ms(speech_gate::SILENCE_FLOOR / 2.0, 1500));
        assert_ne!(speech_gate::decide(&audio), speech_gate::GateOutcome::Silence);
        assert_eq!(
            speech_gate::decide(gate_evidence(&audio, true)),
            speech_gate::GateOutcome::Silence
        );
    }

    /// The trim removes only the cue: speech after it is still judged as
    /// speech.
    #[test]
    fn speech_after_the_cue_still_reads_as_speech() {
        let mut audio = tone_ms(speech_gate::CLEAR_SPEECH_LEVEL, CUE_BLEED_MS);
        audio.extend(tone_ms(speech_gate::CLEAR_SPEECH_LEVEL, 1500));
        assert_eq!(
            speech_gate::decide(gate_evidence(&audio, true)),
            speech_gate::GateOutcome::Speech
        );
    }

    /// Too little would be left to judge, so the whole utterance is judged
    /// instead — see `MIN_TRIMMED_MS` for why that error is the safe one.
    #[test]
    fn a_recording_too_short_to_trim_is_judged_whole() {
        let mut audio = tone_ms(0.01, CUE_BLEED_MS);
        audio.extend(tone_ms(0.001, MIN_TRIMMED_MS - 100));
        assert_eq!(gate_evidence(&audio, true).len(), audio.len());
    }

    /// The trim must never hand the gate an empty slice: `decide` fails open
    /// on one, which would disable the gate for exactly the shortest
    /// recordings rather than for none of them.
    #[test]
    fn a_buffer_shorter_than_the_trim_is_judged_whole() {
        let audio = tone_ms(0.01, 100);
        assert_eq!(gate_evidence(&audio, true).len(), audio.len());
    }

    /// Cues off means nothing was played into the microphone, so the gate
    /// sees every sample — including the pre-roll that carries a first
    /// syllable spoken before the chord finished going down.
    #[test]
    fn nothing_is_trimmed_when_cues_are_off() {
        let audio = tone_ms(0.01, 2000);
        assert_eq!(gate_evidence(&audio, false).len(), audio.len());
    }

    // --- Smart-space append -----------------------------------------------
    //
    // The only exception is trailing whitespace, never punctuation.

    #[test]
    fn a_result_ending_in_a_letter_gets_one_ascii_space() {
        assert_eq!(smart_space_append("Kal Pune chalte hain".to_string()), "Kal Pune chalte hain ");
    }

    /// A sentence's closing mark, in either script, is not whitespace, so the
    /// next word the user types still starts one space away.
    #[test]
    fn a_closing_mark_in_any_script_still_gets_the_space() {
        assert_eq!(smart_space_append("Kal milte hain.".to_string()), "Kal milte hain. ");
        assert_eq!(smart_space_append("Chai piyoge?".to_string()), "Chai piyoge? ");
        assert_eq!(smart_space_append("धन्यवाद।".to_string()), "धन्यवाद। ");
    }

    /// A result that already ends in a separator is returned unchanged, so a
    /// line break or a tab is never followed by a stray space.
    #[test]
    fn a_result_already_ending_in_ascii_whitespace_comes_back_unchanged() {
        assert_eq!(smart_space_append("Theek hai\r\n".to_string()), "Theek hai\r\n");
        assert_eq!(smart_space_append("Naam: Anjali\t".to_string()), "Naam: Anjali\t");
        assert_eq!(smart_space_append("Haan ji ".to_string()), "Haan ji ");
    }

    /// The test is Unicode's whitespace, not ASCII's: a no-break space, an em
    /// space or an ideographic space already separates the next word.
    #[test]
    fn a_result_ending_in_unicode_whitespace_comes_back_unchanged() {
        assert_eq!(smart_space_append("शुक्रिया\u{00A0}".to_string()), "शुक्रिया\u{00A0}");
        assert_eq!(smart_space_append("नमस्ते\u{2003}".to_string()), "नमस्ते\u{2003}");
        assert_eq!(smart_space_append("Chennai\u{3000}".to_string()), "Chennai\u{3000}");
    }

    /// Callers only pass a real result, but the empty case is pinned so that
    /// changing it is a decision rather than an accident.
    #[test]
    fn empty_text_becomes_a_single_space() {
        assert_eq!(smart_space_append(String::new()), " ");
    }

    // --- What a history row says produced it ------------------------------

    // `record_history` writes `SttPath::history_label` into a column the
    // History page renders verbatim; the assertion that those strings are
    // exactly the ones `settings::Provider` serializes to lives with the
    // rest of the `SttPath` rows, in
    // `the_history_provider_labels_match_the_settings_enum`, because the
    // vocabulary has a third word with no `Provider` counterpart.

    // --- The relay ending a Cloud session ----------------------------------

    /// The relay's end stops the recording it belongs to, and nothing else:
    /// the same guard a mid-recording `CloudError` has.
    #[test]
    fn a_relay_end_stops_only_the_live_cloud_recording_it_belongs_to() {
        let recording = DictationState::Recording {
            mode: Mode::HandsFree,
            started: Instant::now(),
        };
        assert!(relay_end_is_current(&recording, true, 4, 4));
        // A session that already ended, racing the start of this one.
        assert!(!relay_end_is_current(&recording, true, 3, 4));
        // A local recording never opened a relay session at all.
        assert!(!relay_end_is_current(&recording, false, 4, 4));
        // Already finishing: the words are on their way either way.
        assert!(!relay_end_is_current(&DictationState::Finalizing { req_id: 9 }, true, 4, 4));
        assert!(!relay_end_is_current(&DictationState::Injecting, true, 4, 4));
        assert!(!relay_end_is_current(&DictationState::Idle, true, 4, 4));
    }

    /// A dictation the relay cut short is still pasted, so its row stays
    /// `Done` — but it carries `cloud-limit`, so History shows it was
    /// cut rather than ended by the user. Every other pasted row has no code.
    #[test]
    fn a_dictation_the_relay_cut_short_is_marked_in_history() {
        assert_eq!(completed_error_code(true), Some("cloud-limit"));
        assert_eq!(completed_error_code(false), None);
    }

    // --- System resume ---------------------------------------------------

    #[test]
    fn resume_cancels_a_live_cloud_recording() {
        let state = DictationState::Recording {
            mode: Mode::PushToTalk,
            started: Instant::now(),
        };
        assert!(should_cancel_on_resume(&state, true));
    }

    #[test]
    fn resume_cancels_a_cloud_dictation_mid_finalize() {
        let state = DictationState::Finalizing { req_id: 1 };
        assert!(should_cancel_on_resume(&state, true));
    }

    /// The local provider never opens a socket that suspend can kill —
    /// nothing to invalidate.
    #[test]
    fn resume_leaves_a_local_recording_alone() {
        let state = DictationState::Recording {
            mode: Mode::HandsFree,
            started: Instant::now(),
        };
        assert!(!should_cancel_on_resume(&state, false));
    }

    /// By `Injecting`, the network round trip already finished — only a
    /// local paste thread is running, which sleep cannot break.
    #[test]
    fn resume_leaves_an_in_flight_injection_alone() {
        assert!(!should_cancel_on_resume(&DictationState::Injecting, true));
    }

    #[test]
    fn resume_is_a_no_op_when_idle() {
        assert!(!should_cancel_on_resume(&DictationState::Idle, true));
    }

    /// A resume that tears down a cloud dictation whose transcript already
    /// arrived files that transcript first; one whose route belongs to
    /// another dictation, or that holds nothing, has nothing to file.
    #[test]
    fn resume_files_a_transcript_it_was_holding_for_a_route() {
        let finalizing = DictationState::Finalizing { req_id: 4 };
        assert!(should_cancel_on_resume(&finalizing, true));
        assert!(holds_route_transcript(&finalizing, Some(4)));
        assert!(!holds_route_transcript(&finalizing, Some(3)));
        assert!(!holds_route_transcript(&finalizing, None));
        let recording = DictationState::Recording {
            mode: Mode::PushToTalk,
            started: Instant::now(),
        };
        assert!(!holds_route_transcript(&recording, Some(4)));
    }

    // --- Session lock and tray Pause ---------------------------------------

    /// Both cancel a live recording in either mode: after a lock the mic
    /// hears the room, and a paused hook lets no key end the recording.
    #[test]
    fn a_lock_or_a_pause_cancels_a_live_recording() {
        for mode in [Mode::PushToTalk, Mode::HandsFree] {
            let state = DictationState::Recording {
                mode,
                started: Instant::now(),
            };
            assert!(cancels_on_interruption(&state), "{mode:?}");
        }
    }

    /// Words already spoken are finished as usual: a recording that stopped
    /// before the lock or the pause is not the room.
    #[test]
    fn a_lock_or_a_pause_leaves_a_finished_recording_alone() {
        assert!(!cancels_on_interruption(&DictationState::Idle));
        assert!(!cancels_on_interruption(&DictationState::Finalizing { req_id: 1 }));
        assert!(!cancels_on_interruption(&DictationState::Injecting));
    }

    // --- Held transcripts ---------------------------------------------------

    /// A translation's held words are the user's dictation after the whole
    /// pipeline and are filed that way; an agent command or a wake-word
    /// dictation keeps its verbatim transcript, as a declined route does.
    #[test]
    fn a_held_transcript_is_filed_by_its_route() {
        use crate::routes::Route;
        assert_eq!(held_row_text(Route::Translation, "Cleaned.", "cleaned"), "Cleaned.");
        assert_eq!(held_row_text(Route::Agent, "Fix it.", "fix it"), "fix it");
        assert_eq!(held_row_text(Route::Cleanup, "Hey.", "hey"), "hey");
    }

    // --- The audio tail ------------------------------------------------------

    /// Audio and the tail marker belong to the drain; everything else is
    /// kept for afterwards, including the messages that arrive only once.
    #[test]
    fn the_tail_drain_keeps_every_other_message_for_later() {
        assert!(matches!(tail_step(ControlMsg::Audio(vec![0.1])), TailStep::Chunk(c) if c == [0.1]));
        assert!(matches!(tail_step(ControlMsg::AudioTail), TailStep::End));
        assert!(matches!(
            tail_step(ControlMsg::HistoryRemoved(crate::state::HistoryRemoval::All)),
            TailStep::Later(ControlMsg::HistoryRemoved(_))
        ));
        assert!(matches!(
            tail_step(ControlMsg::SystemResumed),
            TailStep::Later(ControlMsg::SystemResumed)
        ));
        assert!(matches!(
            tail_step(ControlMsg::SessionLocked),
            TailStep::Later(ControlMsg::SessionLocked)
        ));
    }

    // --- What the injection's outcome does to the bookkeeping ------------

    /// The ordinary case: the paste landed, so the text is on screen ending
    /// at the caret and Undo has something real to point at.
    #[test]
    fn a_landed_paste_arms_undo() {
        let rec = sample_injection();
        assert_eq!(
            injection_outcome(Some(InFlight::Arm(rec.clone())), true),
            UndoBookkeeping::Arm(rec)
        );
    }

    /// THE REGRESSION this pins: `start_injection` only logs a failed paste, so
    /// a record written unconditionally before `inject_text` ran would, after a
    /// locked clipboard, leave the document unchanged and Undo armed — and the
    /// next press would Shift+Left over 26 characters of text the user had
    /// typed themselves and paste the transcript on top of it. Nothing was
    /// pasted, so nothing may be armed.
    #[test]
    fn a_failed_paste_arms_nothing() {
        assert_eq!(
            injection_outcome(Some(InFlight::Arm(sample_injection())), false),
            UndoBookkeeping::Disarm
        );
    }

    /// After a verified replace the caret sits at the end of the verbatim
    /// transcript, not a formatted edit. Pressing Undo again must report
    /// "Nothing to undo" rather than stacking another copy — enforced from
    /// the outcome, not optimistically at launch.
    #[test]
    fn a_landed_undo_leaves_nothing_to_undo() {
        assert_eq!(
            injection_outcome(
                Some(InFlight::Undo {
                    raw: "um so the meeting is at three thirty pm".to_string()
                }),
                true
            ),
            UndoBookkeeping::Disarm
        );
    }

    /// The readback's whole purpose: it refused, so not one character
    /// changed. The user still gets their words — on the clipboard, with a
    /// notice — and the record survives, because "the document is exactly as
    /// it was" is also "the record is exactly as valid as it was".
    #[test]
    fn an_unverified_undo_changes_nothing_and_falls_back_to_the_clipboard() {
        let raw = "um so the meeting is at three thirty pm".to_string();
        assert_eq!(
            injection_outcome(Some(InFlight::Undo { raw: raw.clone() }), false),
            UndoBookkeeping::FallBackToClipboard { raw }
        );
    }

    /// Paste-last arms nothing (it is a re-paste, not a dictation) and the
    /// record it may have displaced is already gone — `start_injection`
    /// drops it, because that paste moved the caret past a second copy of
    /// the text and invalidated whatever the record described.
    #[test]
    fn an_injection_with_nothing_in_flight_leaves_undo_disarmed() {
        assert_eq!(injection_outcome(None, true), UndoBookkeeping::Disarm);
        assert_eq!(injection_outcome(None, false), UndoBookkeeping::Disarm);
    }
}
