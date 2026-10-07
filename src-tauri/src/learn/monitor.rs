//! Watches the text field a dictation was pasted into for a short while, to
//! catch the user fixing a misheard word by hand.
//!
//! [`watch`] starts a thread per paste. The thread waits for the paste to
//! settle, binds the element the user is typing into through UI Automation,
//! and reads its whole value once per [`POLL_INTERVAL`]. When the value has
//! changed and then held still for [`DEBOUNCE`], it is compared with the
//! pasted text by [`diff::corrections_in`], and any (heard, corrected)
//! pairs go to the sink with an id for this paste. What a pair is worth is
//! decided downstream, in [`super::candidates`].
//!
//! The watch ends on its own and without a word: when [`WINDOW`] runs out,
//! when a later paste takes over, when the element goes away or stops
//! answering, when it turns out not to hold text, or when its text grows
//! past [`MAX_FIELD_CHARS`]. Nothing is read at all when auto-learn is off,
//! when the target is a console, when the dictation had no target window, or
//! when the paste itself is over the limit.
//!
//! PRIVACY: the field can hold anything, including text that has nothing to
//! do with the dictation. It is compared in memory and dropped; it is never
//! logged, stored or sent. The only log lines here carry an error code.

use super::diff::{self, CorrectionPair};
use crate::uia::{self, UiaError};
use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime};
use unicode_normalization::UnicodeNormalization;

/// How long after the paste the field is first read, so that the first read
/// already holds the paste. The receiving app handles the paste keystroke
/// on its own schedule after `inject_text` returns; a second leaves room for
/// a slow editor, and `tests::the_paste_shows_in_notepad_before_the_first_read`
/// checks it on a real desktop. The bind after it adds up to
/// `uia::BIND_BUDGET`. Nobody fixes a word within a second of the paste, so
/// waiting this long costs no correction.
const INITIAL_READ_DELAY: Duration = Duration::from_millis(1000);

/// The time between reads after the first. Every read is a cross-process
/// call into the target app, and a change is only noticed at a read, so
/// [`DEBOUNCE`] is counted in whole polls.
const POLL_INTERVAL: Duration = Duration::from_millis(1000);

/// How long a paste stays watched, counted on the wall clock from the start
/// of [`run`]. It covers reading a dictation of one to three sentences,
/// finding a misheard word and retyping it, with [`DEBOUNCE`] and one poll
/// to spare; the budget is spelled out in
/// `tests::the_window_covers_a_slow_fix_of_a_three_sentence_dictation`.
const WINDOW: Duration = Duration::from_secs(35);

/// How long the field must hold still after a change before it is compared.
/// Two polls: a pause inside a word has to outlast both before the half
/// word can be compared, and pauses of two seconds or more almost never fall
/// inside a word; they come between words and sentences.
const DEBOUNCE: Duration = Duration::from_millis(2000);

/// The most characters a field may hold and still be read. A field longer
/// than this is a document or a log rather than somewhere a dictation is
/// being corrected, and reading it back every poll would cost the target app
/// and this app for nothing. Counted in `char`s, so the limit means the same
/// amount of text in every script.
const MAX_FIELD_CHARS: usize = 10_000;

/// Which [`watch`] is the current one. Each watch takes the next number
/// before its thread starts and stops at its next poll once the counter has
/// moved on, so a newer paste always takes over the field from an older
/// monitor.
static GENERATION: AtomicU64 = AtomicU64::new(0);

/// A paste that landed, and everything needed to decide whether it is worth
/// watching and what to compare the field against.
pub struct Paste {
    /// Exactly the string that went into the document — after style, after
    /// smart spacing. The diff baseline, and it has to be this rather than
    /// the pre-spacing transcript.
    pub text: String,
    /// The window the paste was aimed at, from `foreground::Target::hwnd`.
    /// `None` when the dictation had no captured target, which is nothing to
    /// bind to.
    pub hwnd: Option<isize>,
    /// Whether that window is a console. A shell prompt is not a correction
    /// surface — the "field" is a scrollback buffer, the user's next
    /// keystrokes are commands, and reading it back is noise at best.
    pub terminal: bool,
    /// The user's personal vocabulary, so the differ can decline to re-learn
    /// something already known. A snapshot: a word added during the window is
    /// not seen, which costs at most one redundant candidate.
    pub dictionary: Vec<String>,
    /// The auto-learn setting. Checked here rather than at the call site so
    /// "off means nothing is read" is a property of this module and has a
    /// test.
    pub enabled: bool,
}

/// Where a batch of observations goes: the pairs, and the id of the paste
/// they came from.
///
/// Injected rather than called directly so this module has no opinion about
/// what happens next — and so the state machine can be tested end to end with
/// a sink that just collects.
pub type Sink = Box<dyn Fn(Vec<CorrectionPair>, String) + Send>;

/// Start watching the field `paste` landed in. Returns immediately; the
/// watching happens on its own thread and stops on its own.
pub fn watch(paste: Paste, sink: Sink) {
    if !worth_watching(&paste) {
        return;
    }
    // Claimed before the thread starts, so an older monitor is superseded the
    // moment this paste is known to be worth watching rather than whenever
    // the OS gets round to scheduling us.
    let generation = GENERATION.fetch_add(1, Ordering::SeqCst) + 1;
    // Per-paste, so a consumer downstream can tell "the user fixed two words
    // in one dictation" from "the user made the same fix in two dictations" —
    // the second is evidence, the first is one edit seen twice.
    let session = uuid::Uuid::new_v4().to_string();
    if let Err(e) = std::thread::Builder::new()
        .name("field-monitor".into())
        .spawn(move || {
            let mut live = Live {
                handle: None,
                generation,
            };
            run(&mut live, &paste, &session, &sink);
        })
    {
        // The error is a thread-spawn failure (an OS resource limit); it
        // cannot contain field text.
        tracing::debug!("field monitor could not start ({e})");
    }
}

/// Whether a landed paste is worth watching at all — the cheap refusals, all
/// of which are ordinary rather than errors.
fn worth_watching(paste: &Paste) -> bool {
    paste.enabled
        && !paste.terminal
        && paste.hwnd.is_some()
        && !paste.text.trim().is_empty()
        && !over_cap(&paste.text)
}

/// The desktop, as this module needs it: a clock, a way to wait, a bind and a
/// read, and whether this monitor is still the current one.
///
/// A trait so the whole state machine — the delay ladder, the window, the
/// debounce, every stop condition — is exercisable without COM, without a
/// desktop and without sleeping, by a stub whose clock only moves when
/// [`Desktop::sleep`] is called.
trait Desktop {
    fn now(&self) -> SystemTime;
    fn sleep(&mut self, d: Duration);
    /// Bind the element the user is typing into inside `hwnd`.
    fn bind(&mut self, hwnd: isize) -> Result<(), UiaError>;
    /// The bound element's whole current value. `Ok(None)` means it is not a
    /// text field at all.
    ///
    /// PRIVACY: the returned `String` is the user's field content.
    fn value(&mut self) -> Result<Option<String>, UiaError>;
    /// Whether a later paste has taken over.
    fn superseded(&self) -> bool;
}

/// The real one.
struct Live {
    handle: Option<uia::UiaHandle>,
    generation: u64,
}

impl Desktop for Live {
    fn now(&self) -> SystemTime {
        SystemTime::now()
    }

    fn sleep(&mut self, d: Duration) {
        std::thread::sleep(d);
    }

    fn bind(&mut self, hwnd: isize) -> Result<(), UiaError> {
        // Blocks for up to `uia::BIND_BUDGET` — the retry ladder is in there.
        // Fine here and nowhere near as fine in the selection lane: this runs
        // on the field-monitor thread, after a settle delay nobody is waiting
        // on either.
        //
        // Any handle already held is dropped by the assignment, which releases
        // its element on the UIA thread. `run` binds once, so in practice
        // there is none.
        self.handle = Some(uia::element_for_hwnd(hwnd)?);
        Ok(())
    }

    fn value(&mut self) -> Result<Option<String>, UiaError> {
        match &self.handle {
            Some(h) => uia::focused_value(h),
            None => Err(UiaError::Unavailable),
        }
    }

    fn superseded(&self) -> bool {
        GENERATION.load(Ordering::SeqCst) != self.generation
    }
}

/// The state machine.
///
/// Every exit is a silent return. There is no failure this module can report
/// that anyone could act on: an elevated target, a password box, a window
/// that closed, a control with no text pattern and a user who simply did not
/// edit anything are all the same outcome — no observations — and all of them
/// are the ordinary case rather than a defect.
///
/// One deliberate omission: a change still inside its debounce when the
/// window expires is **dropped, not flushed**. Flushing would diff a field
/// the user is still typing into, and [`DEBOUNCE`] exists precisely because
/// that is how a half-typed word gets learned. The cost is the last
/// [`DEBOUNCE`] of the window being dead to a correction that starts in it.
fn run<D: Desktop>(d: &mut D, paste: &Paste, session: &str, sink: &Sink) {
    if !worth_watching(paste) {
        return;
    }
    let Some(hwnd) = paste.hwnd else { return };
    // Wall-clock, not `Instant`. `Instant` is QueryPerformanceCounter-backed
    // on Windows and does not reliably advance across S3 sleep, so a machine
    // suspended mid-window would wake and keep reading a stranger's field for
    // what is left of the window in real time. The other direction — a clock stepped
    // *backwards* mid-window — makes `duration_since` fail, and that is
    // treated as "expired" below so the failure closes the window rather than
    // extending it indefinitely.
    let started = d.now();
    // Normalized once: the app the text was pasted into may store it in a
    // different Unicode form than the one that was put on the clipboard. With
    // both sides composed the same way, a field that differs from the paste
    // only in form is not a change, and the words handed on are spelled the
    // way the candidate store keeps them.
    let baseline = nfc(&paste.text);

    d.sleep(INITIAL_READ_DELAY);
    if d.superseded() {
        return;
    }

    // ONE ask. `uia::element_for_hwnd` retries on its own, and a loop here on
    // top of it would multiply its attempts and its waiting on a background
    // thread reading a field the user may already have moved on from.
    //
    // Because the waiting happens below this module, a paste that arrives
    // mid-bind does not stop this monitor until the bind returns. That costs
    // one wasted read at most: the check at the top of the poll loop still
    // fires before anything is diffed, and a superseded monitor emits no
    // pairs either way.
    match d.bind(hwnd) {
        Ok(()) => {}
        // Silent, always, and never distinguished: `Unavailable` is what a
        // password box, an elevated window, a target that will not answer and
        // a target that is not ready yet all look like from here, and saying
        // which would itself be a disclosure.
        Err(UiaError::Unavailable) => return,
        Err(e @ UiaError::Com(_)) => {
            // An HRESULT nobody has classified yet. It carries nothing but
            // the code — see `UiaError`'s own tripwire — and it is worth one
            // line so it can be looked at.
            tracing::debug!("field monitor: {e}");
            return;
        }
    }

    // The field as it stands now. This is what "changed" is measured
    // against — NOT the diff baseline, which is the pasted text. The two
    // differ whenever the field holds a document the paste landed in the
    // middle of, and the diff engine finds the paste inside it.
    let Some(mut last_seen) = read(d) else { return };

    let mut changed_at: Option<SystemTime> = None;
    let mut emitted: HashSet<(String, String)> = HashSet::new();

    loop {
        d.sleep(POLL_INTERVAL);
        if d.superseded() {
            return;
        }
        let now = d.now();
        if now.duration_since(started).unwrap_or(WINDOW) >= WINDOW {
            return;
        }
        let Some(value) = read(d) else { return };

        if value != last_seen {
            last_seen = value;
            changed_at = Some(now);
            continue;
        }
        let Some(at) = changed_at else { continue };
        if now.duration_since(at).unwrap_or_default() < DEBOUNCE {
            continue;
        }
        changed_at = None;

        let pairs = diff::corrections_in(&baseline, &last_seen, &paste.dictionary);
        // The window keeps running after a learn — a user often fixes a
        // second word a few seconds after the first — so a pair already sent
        // must not be sent again when the next quiet period diffs the same
        // field against the same baseline.
        let fresh: Vec<CorrectionPair> = pairs
            .into_iter()
            .filter(|p| emitted.insert((p.from.to_lowercase(), p.to.to_lowercase())))
            .collect();
        if !fresh.is_empty() {
            sink(fresh, session.to_string());
        }
    }
}

/// One poll's worth of reading: `Some` to carry on with, `None` to stop.
///
/// Every "stop" is silent except an unclassified `HRESULT`, which is the one
/// thing here worth a log line and carries nothing else.
fn read<D: Desktop>(d: &mut D) -> Option<String> {
    match d.value() {
        Ok(Some(v)) if over_cap(&v) => None,
        Ok(Some(v)) => Some(nfc(&v)),
        // Neither ValuePattern nor TextPattern: not a text field, so there is
        // nothing here to come back for.
        Ok(None) => None,
        Err(UiaError::Unavailable) => None,
        Err(e @ UiaError::Com(_)) => {
            tracing::debug!("field monitor: {e}");
            None
        }
    }
}

/// Whether `s` is longer than [`MAX_FIELD_CHARS`] characters.
///
/// The byte length is checked first because it is free and a `char` is at
/// least one byte, so anything shorter than the cap in bytes cannot be longer
/// than it in `char`s. Past that, `nth` stops counting at the cap instead of
/// walking a whole document.
fn over_cap(s: &str) -> bool {
    s.len() > MAX_FIELD_CHARS && s.chars().nth(MAX_FIELD_CHARS).is_some()
}

/// Canonical composition, applied to both sides before anything compares
/// them.
///
/// The clipboard round trip and the receiving app's own storage are both free
/// to hand back a different-but-equivalent normalization of what was pasted:
/// `क़` as one codepoint or as `क` plus a nukta, `é` as one or as `e` plus an
/// acute. Uncompared-for, that reads as the user having replaced the word.
fn nfc(s: &str) -> String {
    s.nfc().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    /// A desktop whose clock only moves when something sleeps, and whose
    /// field only changes when the script says so.
    struct Stub {
        now: SystemTime,
        slept: Vec<Duration>,
        binds: VecDeque<Result<(), UiaError>>,
        bind_calls: u32,
        /// Answers for the next reads, in order.
        reads: VecDeque<Result<Option<String>, UiaError>>,
        /// What the field keeps saying once `reads` runs dry — i.e. the value
        /// the last scripted read established. A field that is not being
        /// typed into holds still, and the debounce depends on that.
        steady: Result<Option<String>, UiaError>,
        value_calls: u32,
        taken_over: bool,
        /// Pretend a newer paste arrived once this many reads have happened.
        taken_over_after_reads: Option<u32>,
    }

    impl Stub {
        fn new(reads: Vec<Result<Option<String>, UiaError>>) -> Self {
            Stub {
                now: SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000),
                slept: Vec::new(),
                binds: VecDeque::new(),
                bind_calls: 0,
                reads: reads.into(),
                steady: Ok(None),
                value_calls: 0,
                taken_over: false,
                taken_over_after_reads: None,
            }
        }

        /// The common script: the field holds the paste, then holds `edited`.
        fn editing(pasted: &str, edited: &str) -> Self {
            Stub::new(vec![
                Ok(Some(pasted.to_string())),
                Ok(Some(edited.to_string())),
            ])
        }
    }

    impl Desktop for Stub {
        fn now(&self) -> SystemTime {
            self.now
        }

        fn sleep(&mut self, d: Duration) {
            self.slept.push(d);
            self.now += d;
        }

        fn bind(&mut self, _hwnd: isize) -> Result<(), UiaError> {
            self.bind_calls += 1;
            self.binds.pop_front().unwrap_or(Ok(()))
        }

        fn value(&mut self) -> Result<Option<String>, UiaError> {
            self.value_calls += 1;
            if let Some(next) = self.reads.pop_front() {
                self.steady = next;
            }
            self.steady.clone()
        }

        fn superseded(&self) -> bool {
            self.taken_over
                || self
                    .taken_over_after_reads
                    .is_some_and(|n| self.value_calls >= n)
        }
    }

    /// A sink that collects, so a test can assert on what left the monitor
    /// and nothing else can.
    #[derive(Clone, Default)]
    struct Collector(Arc<Mutex<Vec<(Vec<CorrectionPair>, String)>>>);

    impl Collector {
        fn sink(&self) -> Sink {
            let batches = self.0.clone();
            Box::new(move |pairs, session| {
                batches.lock().expect("collector lock").push((pairs, session))
            })
        }

        fn batches(&self) -> Vec<(Vec<CorrectionPair>, String)> {
            self.0.lock().expect("collector lock").clone()
        }

        fn pairs(&self) -> Vec<(String, String)> {
            self.batches()
                .into_iter()
                .flat_map(|(pairs, _)| pairs)
                .map(|p| (p.from, p.to))
                .collect()
        }
    }

    /// A dictation with one misheard name, and the field once the user has
    /// fixed it.
    const PASTED: &str = "Please send the Kubernetes notes to Sidharth before lunch";
    const FIXED: &str = "Please send the Kubernetes notes to Siddharth before lunch";

    fn paste(text: &str) -> Paste {
        Paste {
            text: text.to_string(),
            hwnd: Some(0x1234),
            terminal: false,
            dictionary: Vec::new(),
            enabled: true,
        }
    }

    fn watch_with(stub: &mut Stub, p: &Paste) -> Collector {
        let got = Collector::default();
        run(stub, p, "session-under-test", &got.sink());
        got
    }

    #[test]
    fn a_correction_typed_into_the_field_reaches_the_sink_after_the_debounce() {
        let mut stub = Stub::editing(PASTED, FIXED);
        let got = watch_with(&mut stub, &paste(PASTED));

        assert_eq!(got.pairs(), vec![("Sidharth".to_string(), "Siddharth".to_string())]);
        assert_eq!(got.batches().len(), 1, "one quiet period, one batch");
        assert_eq!(got.batches()[0].1, "session-under-test");
    }

    /// Whole polls in the debounce.
    fn debounce_polls() -> usize {
        assert_eq!(
            DEBOUNCE.as_millis() % POLL_INTERVAL.as_millis(),
            0,
            "the debounce is a whole number of polls"
        );
        (DEBOUNCE.as_millis() / POLL_INTERVAL.as_millis()) as usize
    }

    /// How many polls read the field before the window closes. Poll `k`
    /// reads at `INITIAL_READ_DELAY + k * POLL_INTERVAL` after the start, and
    /// only while that is short of `WINDOW`.
    fn polls_in_window() -> usize {
        let open = (WINDOW - INITIAL_READ_DELAY).as_millis();
        let poll = POLL_INTERVAL.as_millis();
        let whole = (open / poll) as usize;
        if open % poll == 0 {
            whole - 1
        } else {
            whole
        }
    }

    /// The first read waits for the paste to settle, and every read after it
    /// is one poll later.
    #[test]
    fn the_first_read_waits_for_the_paste_to_settle_and_later_reads_are_one_poll_apart() {
        let mut stub = Stub::editing(PASTED, FIXED);
        let got = watch_with(&mut stub, &paste(PASTED));

        assert_eq!(stub.slept.first(), Some(&INITIAL_READ_DELAY));
        assert!(stub.slept.len() > 2, "{:?}", stub.slept);
        assert!(
            stub.slept[1..].iter().all(|&d| d == POLL_INTERVAL),
            "{:?}",
            stub.slept
        );
        assert_eq!(got.batches().len(), 1);
    }

    /// A pause inside a word that is shorter than the debounce never gets
    /// the half word compared, even though the half word would pass every
    /// test the diff engine applies.
    #[test]
    fn a_half_typed_correction_is_never_what_gets_learned() {
        const HALF: &str = "Please send the Kubernetes notes to Siddhar before lunch";
        assert_eq!(
            diff::corrections_in(PASTED, HALF, &[]),
            vec![CorrectionPair {
                from: "Sidharth".into(),
                to: "Siddhar".into(),
            }],
            "the half word would be learned if it were ever compared"
        );

        // The half word stays for as many polls as it can without the
        // debounce running out, and then the word is finished.
        let hesitation = debounce_polls() - 1;
        assert!(hesitation >= 1, "a hesitation of at least one poll");
        let mut reads = vec![Ok(Some(PASTED.to_string()))];
        reads.extend(vec![Ok(Some(HALF.to_string())); 1 + hesitation]);
        reads.push(Ok(Some(FIXED.to_string())));
        let mut stub = Stub::new(reads);
        let got = watch_with(&mut stub, &paste(PASTED));

        assert_eq!(got.pairs(), vec![("Sidharth".to_string(), "Siddharth".to_string())]);
        assert_eq!(got.batches().len(), 1);
    }

    /// The window's budget: a slow but ordinary fix of a three-sentence
    /// dictation, then the debounce, then one poll for the last change to be
    /// seen, all inside the window, which still reads as a short while.
    #[test]
    fn the_window_covers_a_slow_fix_of_a_three_sentence_dictation() {
        let noticing = Duration::from_millis(1_500);
        // Forty-five words proofread at 150 words a minute.
        let reading = Duration::from_secs(45 * 60 / 150);
        let reaching_the_word = Duration::from_secs(3);
        // Ten letters, 300 ms apart, and one hesitation.
        let typing = Duration::from_millis(10 * 300);
        let hesitating = Duration::from_secs(2);
        let fix = noticing + reading + reaching_the_word + typing + hesitating;

        assert!(fix + DEBOUNCE + POLL_INTERVAL <= WINDOW, "{fix:?}");
        assert!(WINDOW <= Duration::from_secs(60));
    }

    #[test]
    fn a_field_left_holding_exactly_what_was_pasted_teaches_nothing() {
        let mut stub = Stub::new(vec![Ok(Some(PASTED.to_string()))]);
        let got = watch_with(&mut stub, &paste(PASTED));
        assert!(got.batches().is_empty());
    }

    /// `Unavailable` is the answer for a password box, an elevated target and
    /// a window that will not talk to us. It is never distinguished and never
    /// explained — the monitor just stops.
    ///
    /// **And it asks exactly once.** The retry ladder is
    /// `uia::element_for_hwnd`'s now; this module had its own copy first, and
    /// a copy left behind would have compounded to nine attempts and 3.6 s
    /// here while the selection lane, which has no loop of its own, got three.
    /// The two assertions below are that tripwire: one call, and not one wait
    /// of the length the ladder uses.
    #[test]
    fn an_unavailable_bind_stops_silently_after_exactly_one_ask() {
        let mut stub = Stub::editing(PASTED, FIXED);
        stub.binds = vec![Err(UiaError::Unavailable)].into();

        let got = watch_with(&mut stub, &paste(PASTED));

        assert_eq!(
            stub.bind_calls, 1,
            "the retry belongs to uia::element_for_hwnd; a loop here doubles it up"
        );
        assert!(
            !stub.slept.contains(&crate::uia::BIND_RETRY_DELAY),
            "and so does the waiting: {:?}",
            stub.slept
        );
        assert!(got.batches().is_empty());
    }

    #[test]
    fn an_unavailable_first_read_stops_the_monitor() {
        let mut stub = Stub::new(vec![Err(UiaError::Unavailable)]);
        let got = watch_with(&mut stub, &paste(PASTED));
        assert!(got.batches().is_empty());
    }

    /// The element went away mid-window (the user closed the tab, the app
    /// exited). Stop — and in particular do not keep polling for the rest of
    /// the window.
    #[test]
    fn an_element_that_disappears_mid_window_stops_the_monitor() {
        let mut stub = Stub::new(vec![
            Ok(Some(PASTED.to_string())),
            Err(UiaError::Unavailable),
            Ok(Some(FIXED.to_string())),
        ]);
        let got = watch_with(&mut stub, &paste(PASTED));

        assert!(got.batches().is_empty());
        // Settle delay + exactly one poll: the run stopped at the failed read
        // rather than working through the window.
        assert_eq!(stub.slept.len(), 2);
    }

    /// Not a text field at all. A different thing from an error, and the same
    /// response.
    #[test]
    fn an_element_with_no_text_pattern_stops_the_monitor() {
        let mut stub = Stub::new(vec![Ok(None)]);
        let got = watch_with(&mut stub, &paste(PASTED));
        assert!(got.batches().is_empty());
    }

    #[test]
    fn a_field_holding_a_whole_document_is_not_a_correction_surface() {
        let huge = "क".repeat(MAX_FIELD_CHARS + 1);
        let mut stub = Stub::new(vec![Ok(Some(huge))]);
        let got = watch_with(&mut stub, &paste(PASTED));

        assert!(got.batches().is_empty());
        assert_eq!(stub.slept.len(), 1, "stopped at the first read");
    }

    #[test]
    fn a_field_that_grows_past_the_cap_mid_window_stops_the_monitor() {
        let mut stub = Stub::new(vec![
            Ok(Some(PASTED.to_string())),
            Ok(Some("x".repeat(MAX_FIELD_CHARS + 1))),
            Ok(Some(FIXED.to_string())),
        ]);
        let got = watch_with(&mut stub, &paste(PASTED));
        assert!(got.batches().is_empty());
    }

    /// The cap is in characters, not bytes: 10 000 Devanagari characters are
    /// 30 000 bytes and must still be under it.
    #[test]
    fn the_cap_counts_characters_not_bytes() {
        let at_cap = "क".repeat(MAX_FIELD_CHARS);
        assert_eq!(at_cap.len(), MAX_FIELD_CHARS * 3);
        assert!(!over_cap(&at_cap));
        assert!(over_cap(&format!("{at_cap}क")));
        assert!(!over_cap(""));
    }

    #[test]
    fn an_over_cap_paste_is_never_watched() {
        let mut stub = Stub::editing(PASTED, FIXED);
        let got = watch_with(&mut stub, &paste(&"word ".repeat(MAX_FIELD_CHARS)));

        assert_eq!(stub.bind_calls, 0);
        assert!(stub.slept.is_empty(), "not even the settle delay is paid");
        assert!(got.batches().is_empty());
    }

    /// A shell prompt is not a correction surface, and reading one back is
    /// noise. Nothing is bound, so nothing is read.
    #[test]
    fn a_terminal_target_is_never_watched() {
        let mut stub = Stub::editing(PASTED, FIXED);
        let mut p = paste(PASTED);
        p.terminal = true;
        let got = watch_with(&mut stub, &p);

        assert_eq!(stub.bind_calls, 0);
        assert!(stub.slept.is_empty());
        assert!(got.batches().is_empty());
    }

    #[test]
    fn the_setting_being_off_means_the_field_is_never_read() {
        let mut stub = Stub::editing(PASTED, FIXED);
        let mut p = paste(PASTED);
        p.enabled = false;
        let got = watch_with(&mut stub, &p);

        assert_eq!(stub.bind_calls, 0);
        assert!(stub.slept.is_empty());
        assert!(got.batches().is_empty());
    }

    #[test]
    fn a_dictation_with_no_captured_target_is_never_watched() {
        let mut stub = Stub::editing(PASTED, FIXED);
        let mut p = paste(PASTED);
        p.hwnd = None;
        let got = watch_with(&mut stub, &p);

        assert_eq!(stub.bind_calls, 0);
        assert!(got.batches().is_empty());
    }

    /// The window keeps running after a learn, because a user often fixes a
    /// second word a few seconds after the first. That is what makes this
    /// necessary: the next quiet period diffs the same field against the same
    /// baseline and finds the same pair again.
    #[test]
    fn one_paste_cannot_teach_the_same_correction_twice() {
        let mut stub = Stub::new(vec![
            Ok(Some(PASTED.to_string())),
            Ok(Some(FIXED.to_string())),
            // ...quiet, so the first batch goes out...
            Ok(Some(FIXED.to_string())),
            Ok(Some(FIXED.to_string())),
            // ...then the user adds a word, which is a fresh change and a
            // fresh quiet period, and the same substitution is still there.
            Ok(Some(format!("{FIXED} today "))),
        ]);
        let got = watch_with(&mut stub, &paste(PASTED));

        assert_eq!(got.batches().len(), 1, "the second quiet period adds nothing");
        assert_eq!(got.pairs(), vec![("Sidharth".to_string(), "Siddharth".to_string())]);
    }

    /// A correction made after the window has closed is not this paste's to
    /// learn: the monitor has stopped reading by then.
    #[test]
    fn a_correction_made_after_the_window_closes_is_not_learned() {
        let steady = polls_in_window();
        let mut reads = vec![Ok(Some(PASTED.to_string())); 1 + steady];
        reads.push(Ok(Some(FIXED.to_string())));
        let mut stub = Stub::new(reads);
        let got = watch_with(&mut stub, &paste(PASTED));

        assert!(got.batches().is_empty());
        assert_eq!(stub.reads.len(), 1, "the late correction was never read");
        let watched: Duration = stub.slept.iter().sum();
        assert!(
            watched >= WINDOW && watched < WINDOW + 2 * POLL_INTERVAL,
            "stopped {watched:?} after the start"
        );
    }

    /// A change still inside its debounce when the window ends is **dropped,
    /// not flushed**: flushing would compare a field the user may still be
    /// typing into. The same edit one poll earlier settles in time and is
    /// learned.
    #[test]
    fn a_change_still_inside_its_debounce_when_the_window_ends_is_dropped_not_flushed() {
        // The field holds the paste until poll `k`, which reads the fix.
        fn edit_at_poll(k: usize) -> Stub {
            let mut reads = vec![Ok(Some(PASTED.to_string())); k];
            reads.push(Ok(Some(FIXED.to_string())));
            Stub::new(reads)
        }
        // A fix read at poll `k` is compared at poll `k + debounce_polls()`,
        // and the last poll that reads is `polls_in_window()`.
        let in_time = polls_in_window() - debounce_polls();
        let too_late = in_time + 1;
        assert!(too_late <= polls_in_window(), "the late fix is still read");

        let mut late = edit_at_poll(too_late);
        let got = watch_with(&mut late, &paste(PASTED));
        assert!(late.reads.is_empty(), "the late fix was read");
        assert!(got.batches().is_empty(), "and dropped when the window closed");

        let mut early = edit_at_poll(in_time);
        let got = watch_with(&mut early, &paste(PASTED));
        assert_eq!(got.pairs(), vec![("Sidharth".to_string(), "Siddharth".to_string())]);
    }

    /// A later paste takes over. An older monitor that kept going would see
    /// the newer paste land in its field and diff it as though the user had
    /// made that edit.
    #[test]
    fn a_superseded_monitor_stops_before_it_binds_anything() {
        let mut stub = Stub::editing(PASTED, FIXED);
        stub.taken_over = true;
        let got = watch_with(&mut stub, &paste(PASTED));

        assert_eq!(stub.bind_calls, 0);
        assert!(got.batches().is_empty());
    }

    #[test]
    fn a_monitor_superseded_mid_window_stops_at_its_next_poll() {
        let mut stub = Stub::editing(PASTED, FIXED);
        // The newer paste arrives just after the first read, so the edit on
        // the following poll is never even looked at.
        stub.taken_over_after_reads = Some(1);
        let got = watch_with(&mut stub, &paste(PASTED));

        assert_eq!(stub.bind_calls, 1, "it did bind, and did read once");
        assert_eq!(stub.value_calls, 1);
        assert!(got.batches().is_empty());
    }

    /// An HRESULT nobody has classified is the one thing worth a log line,
    /// and it still stops the monitor.
    #[test]
    fn an_unclassified_com_error_stops_the_monitor() {
        let mut stub = Stub::editing(PASTED, FIXED);
        stub.binds = vec![Err(UiaError::Com(0x8000_4005u32 as i32))].into();
        let got = watch_with(&mut stub, &paste(PASTED));

        assert_eq!(stub.bind_calls, 1, "an unknown HRESULT is not retried");
        assert!(got.batches().is_empty());
    }

    /// The clipboard round trip and the receiving app are both free to hand
    /// back a different-but-equivalent normalization of what was pasted.
    /// `क़` is U+0958 precomposed, and U+0915 U+093C decomposed; NFC settles
    /// on the decomposed form for this character (it is a composition
    /// exclusion), so both sides converge either way.
    ///
    /// The nukta word differs from the paste scalar for scalar, right next
    /// to the real correction. It must not read as a second substitution;
    /// only the real correction comes out.
    #[test]
    fn an_equivalent_normalization_of_the_paste_is_not_an_edit() {
        let pasted = "\u{0958}लम Sidharth said hi";
        let edited = "\u{0915}\u{093C}लम Siddharth said hi";
        assert_ne!(pasted, edited.replace("Siddharth", "Sidharth"));

        let mut stub = Stub::editing(pasted, edited);
        let got = watch_with(&mut stub, &paste(pasted));

        assert_eq!(
            got.pairs(),
            vec![("Sidharth".to_string(), "Siddharth".to_string())],
            "only the real correction, and it survives"
        );
    }

    /// The same, at the seam: a field that differs from the paste only by
    /// normalization is not a change at all, so nothing is ever diffed.
    #[test]
    fn a_purely_normalization_level_difference_is_not_even_a_change() {
        let pasted = "\u{0958}लम खरीदी";
        let decomposed = "\u{0915}\u{093C}लम खरीदी";
        assert_ne!(pasted, decomposed);
        assert_eq!(nfc(pasted), nfc(decomposed));

        let mut stub = Stub::editing(pasted, decomposed);
        let got = watch_with(&mut stub, &paste(pasted));
        assert!(got.batches().is_empty());
    }

    /// Words the user already has are not re-learned — the dictionary reaches
    /// the differ.
    #[test]
    fn the_users_dictionary_reaches_the_differ() {
        let mut stub = Stub::editing(PASTED, FIXED);
        let mut p = paste(PASTED);
        p.dictionary = vec!["siddharth".to_string()];
        let got = watch_with(&mut stub, &p);

        assert!(got.batches().is_empty());
    }

    /// Live end-to-end — needs a real interactive desktop (a window manager,
    /// a focused window, working `SendInput`), so it is `#[ignore]`d and
    /// never runs in `cargo test --lib`.
    ///
    /// Run it by hand with:
    ///
    /// ```text
    /// cargo test --lib the_monitor_learns_a_correction_made_in_notepad -- --ignored --nocapture
    /// ```
    ///
    /// Do not touch the keyboard while it runs: it launches Notepad, pastes
    /// through the real clipboard, then selects all and pastes the corrected
    /// text over it, which is what a user retyping the word looks like to
    /// UIA. It prints counts and verdicts only — the strings involved are
    /// this test's own constants, but a test of a privacy-sensitive path
    /// should read like one.
    ///
    /// Windows 11's Notepad is single-instance and tabbed, so it may open a
    /// tab in a window that already existed, asynchronously — hence the wait
    /// for the foreground to hold still, copied from `uia`'s live test, where
    /// the reason is written out at length.
    #[test]
    #[ignore = "needs a real interactive desktop; see the doc comment for the invocation"]
    fn the_monitor_learns_a_correction_made_in_notepad() {
        use std::process::Command;
        use std::thread;

        let mut notepad = Command::new("notepad.exe")
            .spawn()
            .expect("launch notepad.exe");
        let (pid, hwnd) = settled_notepad_window().expect("notepad settled in the foreground");
        println!("notepad pid={pid} hwnd={hwnd:#x}");

        crate::injection::inject_text(PASTED, true, 100, false).expect("paste into notepad");

        let got = Collector::default();
        watch(
            Paste {
                text: PASTED.to_string(),
                hwnd: Some(hwnd),
                terminal: false,
                dictionary: Vec::new(),
                enabled: true,
            },
            got.sink(),
        );

        // Let the settle delay, the bind and the first read happen, then
        // "retype" the word: select all and paste the corrected text over it.
        thread::sleep(INITIAL_READ_DELAY + crate::uia::BIND_BUDGET + Duration::from_millis(500));
        select_all();
        thread::sleep(Duration::from_millis(200));
        crate::injection::inject_text(FIXED, true, 100, false).expect("correct the field");

        // One poll to notice the change, the debounce, one more poll for
        // where the reads happen to fall, and room for the paste itself.
        thread::sleep(POLL_INTERVAL + DEBOUNCE + POLL_INTERVAL + Duration::from_secs(2));

        let batches = got.batches();
        println!("sink batches: {}", batches.len());
        for (pairs, session) in &batches {
            println!("  batch: {} pair(s), session {session}", pairs.len());
        }
        // Compared as a boolean, never asserted with `assert_eq!` on the
        // pairs themselves. What comes back is field-derived by construction,
        // and `assert_eq!` renders both sides into the failure output — so the
        // one run that goes wrong would be the one run that prints the
        // contents of whatever field it had actually bound to. The verdict is
        // the only thing that leaves.
        let learned = got.pairs();
        let as_expected = learned == vec![("Sidharth".to_string(), "Siddharth".to_string())];
        println!("learned exactly the expected pair: {as_expected}");

        let _ = notepad.kill(); // TerminateProcess: no "save changes?" prompt
        let _ = notepad.wait();

        assert!(
            as_expected,
            "the correction made in the real field should have reached the sink \
             as exactly one Sidharth -> Siddharth pair; got {} pair(s) in {} batch(es)",
            learned.len(),
            batches.len()
        );
    }

    /// Live, like the test above, and run the same way with its own name:
    /// pastes into Notepad several times and times how long each paste takes
    /// to show in the field after `inject_text` returns. Every paste has to
    /// show before [`INITIAL_READ_DELAY`] runs out, so the monitor's first
    /// read holds it. Prints timings only.
    #[test]
    #[ignore = "needs a real interactive desktop; see the doc comment for the invocation"]
    fn the_paste_shows_in_notepad_before_the_first_read() {
        use std::process::Command;
        use std::time::Instant;

        let mut notepad = Command::new("notepad.exe")
            .spawn()
            .expect("launch notepad.exe");
        let (_pid, hwnd) = settled_notepad_window().expect("notepad settled in the foreground");
        let handle = crate::uia::element_for_hwnd(hwnd).expect("bind the notepad field");

        let mut slowest = Duration::ZERO;
        let mut missed = 0;
        for round in 0..6 {
            let text = format!("{PASTED} ({round})");
            select_all();
            std::thread::sleep(Duration::from_millis(250));
            crate::injection::inject_text(&text, true, 100, false).expect("paste into notepad");
            let returned = Instant::now();
            let mut shown = None;
            while returned.elapsed() < 3 * INITIAL_READ_DELAY {
                if let Ok(Some(v)) = crate::uia::focused_value(&handle) {
                    if v.trim_end() == text {
                        shown = Some(returned.elapsed());
                        break;
                    }
                }
                std::thread::sleep(Duration::from_millis(2));
            }
            match shown {
                Some(t) => {
                    println!("paste {round}: shown after {} ms", t.as_millis());
                    slowest = slowest.max(t);
                }
                None => {
                    println!("paste {round}: not shown");
                    missed += 1;
                }
            }
        }
        println!("slowest: {} ms", slowest.as_millis());

        let _ = notepad.kill();
        let _ = notepad.wait();

        assert_eq!(missed, 0, "a paste never showed in the field");
        assert!(
            slowest < INITIAL_READ_DELAY,
            "a paste took {slowest:?} to show, past the settle delay"
        );
    }

    /// Poll until Notepad has been the foreground window under the *same*
    /// handle for ten consecutive reads. Lifted from `uia`'s live test, which
    /// explains at length why a shorter streak catches the wrong window.
    fn settled_notepad_window() -> Option<(u32, isize)> {
        use std::thread;
        const STABLE_SAMPLES: u32 = 10;
        const SAMPLE_MS: u64 = 150;

        let foreground = || unsafe {
            windows::Win32::UI::WindowsAndMessaging::GetForegroundWindow().0 as isize
        };
        let mut previous: Option<isize> = None;
        let mut streak = 0u32;
        for _ in 0..100 {
            thread::sleep(Duration::from_millis(SAMPLE_MS));
            let Some(target) = crate::foreground::capture() else {
                previous = None;
                streak = 0;
                continue;
            };
            if target.app.as_deref() != Some("notepad") {
                previous = None;
                streak = 0;
                continue;
            }
            let hwnd = foreground();
            if previous == Some(hwnd) {
                streak += 1;
                if streak >= STABLE_SAMPLES {
                    return Some((target.pid, hwnd));
                }
            } else {
                streak = 0;
            }
            previous = Some(hwnd);
        }
        None
    }

    /// Ctrl+A, hand-built for the same reason `uia`'s live test builds its
    /// own: `injection`'s key helpers are private to that module. Safe only
    /// because neither Ctrl nor A is an E0-prefixed key.
    fn select_all() {
        use windows::Win32::UI::Input::KeyboardAndMouse::{
            SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_KEYUP, VIRTUAL_KEY,
            VK_CONTROL,
        };
        const VK_A: VIRTUAL_KEY = VIRTUAL_KEY(0x41);
        fn key(vk: VIRTUAL_KEY, up: bool) -> INPUT {
            INPUT {
                r#type: INPUT_KEYBOARD,
                Anonymous: INPUT_0 {
                    ki: KEYBDINPUT {
                        wVk: vk,
                        wScan: 0,
                        dwFlags: if up {
                            KEYEVENTF_KEYUP
                        } else {
                            Default::default()
                        },
                        time: 0,
                        dwExtraInfo: 0,
                    },
                },
            }
        }
        let inputs = [
            key(VK_CONTROL, false),
            key(VK_A, false),
            key(VK_A, true),
            key(VK_CONTROL, true),
        ];
        let _ = unsafe { SendInput(&inputs, std::mem::size_of::<INPUT>() as i32) };
    }
}
