//! Decides, from signal level alone, whether a finished recording holds
//! anything worth transcribing.
//!
//! The controller asks once per dictation, after the key is released and
//! before any cloud or local transcription is spent on the audio. There is no
//! model here: the recording is cut into short windows, each window's level
//! is measured, and the loudest window is compared with two fixed levels.
//! Only a recording with no audible window at all (`Silence`) or with every
//! sample exactly zero (`AllZero`) is thrown away; anything audible goes on
//! to the transcriber, which is a far better judge of quiet speech than a
//! level check.

use std::ops::Range;

/// One gate window: 100 ms @ 16 kHz mono. Also the floor below which a
/// recording is discarded outright rather than judged — see `is_degenerate`.
pub const WINDOW_SAMPLES: usize = 1600;

/// Window level (RMS) at or above which a window counts as audible. A
/// recording whose every window sits below it is `Silence`.
///
/// 7.0e-4 is about −63 dBFS. On the built-in laptop mic at Windows' default
/// input level, silent holds peaked at about −70 dBFS and quiet one-word
/// dictations at −49 to −47 dBFS, so this sits about 7 dB over room tone
/// and about 14 dB under the quietest word.
pub(crate) const SILENCE_FLOOR: f32 = 7.0e-4;

/// Window level (RMS) at or above which a recording is `Speech` rather than
/// `Faint`: −40 dBFS, the level of a word spoken at a normal volume into a
/// laptop mic from arm's length. A headset mic sits well above it. Only the
/// loudest window is compared, so one clearly spoken syllable is enough.
pub(crate) const CLEAR_SPEECH_LEVEL: f32 = 1.0e-2;

/// The gate's verdict on one recording.
///
/// Only `Silence` and `AllZero` make the controller discard the recording.
/// The two errors cost very different amounts: discarding a real sentence
/// loses the user's words, while passing an empty recording costs one round
/// trip that ends at the same "Didn't catch that" notice. The gate therefore
/// throws away only what it is sure of.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GateOutcome {
    /// The loudest window reaches `CLEAR_SPEECH_LEVEL`. Transcribed.
    Speech,
    /// At least one window reaches `SILENCE_FLOOR`, but none reaches
    /// `CLEAR_SPEECH_LEVEL`: a quiet mic, a soft voice, a whisper. Still
    /// transcribed, because only the transcriber can tell a quiet word from
    /// a noise, and a wrongly discarded word costs the user their sentence.
    /// Kept apart from `Speech` so the log shows how often it happens.
    Faint,
    /// No window reaches `SILENCE_FLOOR`. Discarded with the "Didn't catch
    /// that" notice.
    Silence,
    /// Every sample is exactly zero (`-0.0` included). On Windows this nearly
    /// always means the input device is muted, so it gets its own notice.
    /// Any nonzero sample, however small, rules this outcome out.
    AllZero,
}

/// The level of one span of samples: its RMS, accumulated in `f64`. The
/// span's own length is the divisor, so a merged span longer than a window
/// is measured on the same scale as a regular one. An empty span has level 0.
pub(crate) fn window_level(span: &[f32]) -> f32 {
    if span.is_empty() {
        return 0.0;
    }
    let energy: f64 = span.iter().map(|&s| f64::from(s) * f64::from(s)).sum();
    (energy / span.len() as f64).sqrt() as f32
}

/// True when the recording is too short to judge: fewer than one window's
/// worth of samples. It counts samples, not wall-clock time. The controller
/// ends such a recording without a notice, as a key brushed by accident.
pub fn is_degenerate(samples: &[f32]) -> bool {
    samples.len() < WINDOW_SAMPLES
}

/// The shortest final partial window that is judged on its own. A shorter
/// remainder is folded into the full window before it, so a handful of stray
/// samples at the end cannot decide the outcome by themselves.
const MIN_TRAILING_WINDOW_SAMPLES: usize = WINDOW_SAMPLES / 2;

/// The stretches of a `len`-sample recording that are measured one by one:
/// consecutive `WINDOW_SAMPLES` windows from sample 0, then any remainder.
/// A remainder of at least `MIN_TRAILING_WINDOW_SAMPLES` is its own span; a
/// shorter one extends the last full window. A recording shorter than one
/// window is a single span.
fn spans(len: usize) -> impl Iterator<Item = Range<usize>> {
    let full = len / WINDOW_SAMPLES;
    let rest = len % WINDOW_SAMPLES;
    let (regular, last) = if full == 0 {
        (0, Some(0..len))
    } else if rest == 0 {
        (full, None)
    } else if rest >= MIN_TRAILING_WINDOW_SAMPLES {
        (full, Some(full * WINDOW_SAMPLES..len))
    } else {
        (full - 1, Some((full - 1) * WINDOW_SAMPLES..len))
    };
    (0..regular)
        .map(|i| i * WINDOW_SAMPLES..(i + 1) * WINDOW_SAMPLES)
        .chain(last)
}

/// Judge a whole finished recording (16 kHz mono `f32`).
///
/// An empty slice is `Speech`: with nothing measured, the gate does not
/// claim the user was quiet. A recording containing NaN or an infinity is
/// also `Speech`, since broken input is not evidence of silence; a warning
/// is logged instead of the usual verdict line.
///
/// Every outcome other than `Speech` writes one `speech gate verdict` info
/// line with the levels behind it, never the samples themselves.
pub fn decide(samples: &[f32]) -> GateOutcome {
    if samples.is_empty() {
        return GateOutcome::Speech;
    }
    if samples.iter().any(|s| !s.is_finite()) {
        tracing::warn!("speech gate saw non-finite samples; passing the recording through");
        return GateOutcome::Speech;
    }

    let mut windows = 0usize;
    let mut audible_windows = 0usize;
    let mut loudest_window = 0.0f32;
    for span in spans(samples.len()) {
        let level = window_level(&samples[span]);
        windows += 1;
        if level >= SILENCE_FLOOR {
            audible_windows += 1;
        }
        loudest_window = loudest_window.max(level);
    }
    let loudest_sample = samples.iter().fold(0.0f32, |m, s| m.max(s.abs()));

    let outcome = if loudest_sample == 0.0 {
        GateOutcome::AllZero
    } else if loudest_window >= CLEAR_SPEECH_LEVEL {
        GateOutcome::Speech
    } else if loudest_window >= SILENCE_FLOOR {
        GateOutcome::Faint
    } else {
        GateOutcome::Silence
    };

    if outcome != GateOutcome::Speech {
        tracing::info!(
            ?outcome,
            loudest_window,
            loudest_sample,
            windows,
            audible_windows,
            silence_floor = SILENCE_FLOOR,
            clear_speech_level = CLEAR_SPEECH_LEVEL,
            "speech gate verdict"
        );
    }
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build `n` windows' worth of samples where every sample in window `i`
    /// equals `windows[i]` — so that window's rms and peak are both exactly
    /// that value, letting tests target the verified threshold values
    /// precisely instead of fighting floating-point synthesis.
    fn samples_from_constant_windows(windows: &[f32]) -> Vec<f32> {
        windows
            .iter()
            .flat_map(|&v| std::iter::repeat(v).take(WINDOW_SAMPLES))
            .collect()
    }

    /// The largest `f32` strictly below a positive `level`.
    fn just_below(level: f32) -> f32 {
        f32::from_bits(level.to_bits() - 1)
    }

    /// Position in the order Silence < Faint < Speech.
    fn rank(outcome: GateOutcome) -> u8 {
        match outcome {
            GateOutcome::Silence => 0,
            GateOutcome::Faint => 1,
            GateOutcome::Speech => 2,
            GateOutcome::AllZero => panic!("all-zero recordings have no rank"),
        }
    }

    // --- Window level -----------------------------------------------------

    #[test]
    fn a_constant_window_measures_at_its_own_amplitude() {
        for level in [SILENCE_FLOOR, CLEAR_SPEECH_LEVEL, -SILENCE_FLOOR / 3.0] {
            assert_eq!(window_level(&vec![level; WINDOW_SAMPLES]), level.abs());
        }
        assert_eq!(window_level(&[]), 0.0);
    }

    // --- `decide` / `is_degenerate`, over actual sample buffers ---

    #[test]
    fn degenerate_buffers_are_flagged_by_sample_count_not_duration() {
        assert!(is_degenerate(&[]));
        assert!(is_degenerate(&vec![1.0; WINDOW_SAMPLES - 1]));
        assert!(!is_degenerate(&vec![0.0; WINDOW_SAMPLES]));
    }

    #[test]
    fn decide_fails_open_on_an_empty_buffer() {
        assert_eq!(decide(&[]), GateOutcome::Speech);
    }

    #[test]
    fn decide_windows_a_full_buffer_and_reports_all_zero() {
        let samples = vec![0.0f32; WINDOW_SAMPLES * 3];
        assert_eq!(decide(&samples), GateOutcome::AllZero);
    }

    #[test]
    fn decide_windows_a_full_buffer_and_reports_speech() {
        // A steady tone well above every threshold on both axes.
        let samples: Vec<f32> = (0..WINDOW_SAMPLES * 2)
            .map(|i| 0.3 * (i as f32 * 0.1).sin())
            .collect();
        assert_eq!(decide(&samples), GateOutcome::Speech);
    }

    #[test]
    fn negative_zero_counts_as_zero() {
        let samples = vec![-0.0f32; WINDOW_SAMPLES * 2];
        assert_eq!(decide(&samples), GateOutcome::AllZero);
    }

    #[test]
    fn windows_at_several_levels_all_under_the_floor_are_silence() {
        let samples = samples_from_constant_windows(&[
            SILENCE_FLOOR / 10.0,
            SILENCE_FLOOR / 2.0,
            -SILENCE_FLOOR / 4.0,
            just_below(SILENCE_FLOOR),
            SILENCE_FLOOR / 100.0,
        ]);
        assert_eq!(decide(&samples), GateOutcome::Silence);
    }

    #[test]
    fn two_windows_at_fractions_of_the_floor_are_silence() {
        let samples = samples_from_constant_windows(&[SILENCE_FLOOR / 3.0, SILENCE_FLOOR / 2.0]);
        assert_eq!(decide(&samples), GateOutcome::Silence);
    }

    #[test]
    fn the_floor_itself_is_audible_and_just_under_it_is_not() {
        let at = samples_from_constant_windows(&[0.0, SILENCE_FLOOR, 0.0]);
        assert_eq!(decide(&at), GateOutcome::Faint);

        let under = samples_from_constant_windows(&[0.0, just_below(SILENCE_FLOOR), 0.0]);
        assert_eq!(decide(&under), GateOutcome::Silence);
    }

    #[test]
    fn a_loudest_window_just_over_the_floor_is_faint_never_silence() {
        for loudest in [SILENCE_FLOOR * 1.01, SILENCE_FLOOR * 2.0, SILENCE_FLOOR * 5.0] {
            let samples = samples_from_constant_windows(&[
                SILENCE_FLOOR / 2.0,
                loudest,
                SILENCE_FLOOR / 4.0,
            ]);
            assert_eq!(decide(&samples), GateOutcome::Faint, "loudest window {loudest}");
        }
    }

    #[test]
    fn clear_speech_level_splits_speech_from_faint() {
        let at = samples_from_constant_windows(&[SILENCE_FLOOR * 2.0, CLEAR_SPEECH_LEVEL]);
        assert_eq!(decide(&at), GateOutcome::Speech);

        let under = samples_from_constant_windows(&[
            SILENCE_FLOOR * 2.0,
            just_below(CLEAR_SPEECH_LEVEL),
        ]);
        assert_eq!(decide(&under), GateOutcome::Faint);
    }

    #[test]
    fn windows_at_clear_speech_level_are_speech() {
        let samples = samples_from_constant_windows(&[
            CLEAR_SPEECH_LEVEL,
            CLEAR_SPEECH_LEVEL * 3.0,
            -CLEAR_SPEECH_LEVEL * 2.0,
        ]);
        assert_eq!(decide(&samples), GateOutcome::Speech);
    }

    #[test]
    fn one_clear_window_among_inaudible_ones_is_speech() {
        let samples = samples_from_constant_windows(&[
            SILENCE_FLOOR / 2.0,
            SILENCE_FLOOR / 5.0,
            CLEAR_SPEECH_LEVEL,
            SILENCE_FLOOR / 2.0,
            0.0,
        ]);
        assert_eq!(decide(&samples), GateOutcome::Speech);
    }

    #[test]
    fn the_smallest_nonzero_sample_is_silence_not_all_zero() {
        let mut samples = vec![0.0f32; WINDOW_SAMPLES * 2];
        samples[WINDOW_SAMPLES + 7] = f32::MIN_POSITIVE;
        assert_eq!(decide(&samples), GateOutcome::Silence);
    }

    #[test]
    fn a_quiet_trailing_partial_window_is_silence_not_all_zero() {
        let mut samples = vec![0.0f32; WINDOW_SAMPLES];
        samples.extend(vec![SILENCE_FLOOR / 2.0; MIN_TRAILING_WINDOW_SAMPLES]);
        assert_eq!(decide(&samples), GateOutcome::Silence);
    }

    #[test]
    fn a_one_sample_tail_is_merged_and_cannot_decide_alone() {
        // On its own, one sample at the clear-speech level is Speech...
        let tail = CLEAR_SPEECH_LEVEL;
        assert_eq!(decide(&[tail]), GateOutcome::Speech);

        // ...but folded into the silent window before it, the merged span
        // measures far under the floor.
        let mut samples = vec![0.0f32; WINDOW_SAMPLES];
        samples.push(tail);
        assert!(window_level(&samples) < SILENCE_FLOOR);
        assert_eq!(decide(&samples), GateOutcome::Silence);
    }

    #[test]
    fn a_tail_of_exactly_half_a_window_is_judged_on_its_own() {
        let mut samples = vec![SILENCE_FLOOR / 2.0; WINDOW_SAMPLES];
        samples.extend(vec![SILENCE_FLOOR * 1.2; MIN_TRAILING_WINDOW_SAMPLES]);

        // Merged into one span, the two would measure under the floor.
        assert!(window_level(&samples) < SILENCE_FLOOR);
        // Apart, the tail reaches the floor by itself.
        assert_eq!(decide(&samples), GateOutcome::Faint);
    }

    #[test]
    fn a_tail_just_under_half_a_window_is_merged() {
        let mut samples = vec![SILENCE_FLOOR / 2.0; WINDOW_SAMPLES];
        samples.extend(vec![SILENCE_FLOOR * 1.2; MIN_TRAILING_WINDOW_SAMPLES - 1]);
        assert_eq!(decide(&samples), GateOutcome::Silence);
    }

    #[test]
    fn a_recording_shorter_than_one_window_is_one_span() {
        let short = vec![SILENCE_FLOOR * 2.0; WINDOW_SAMPLES / 4];
        assert_eq!(decide(&short), GateOutcome::Faint);
        let quiet = vec![SILENCE_FLOOR / 2.0; WINDOW_SAMPLES / 4];
        assert_eq!(decide(&quiet), GateOutcome::Silence);
    }

    #[test]
    fn raising_the_level_never_lowers_the_outcome() {
        let tone = |amplitude: f32| -> Vec<f32> {
            (0..WINDOW_SAMPLES * 3)
                .map(|i| amplitude * (i as f32 * 0.07).sin())
                .collect()
        };
        let recordings = vec![
            samples_from_constant_windows(&[SILENCE_FLOOR / 8.0, SILENCE_FLOOR / 3.0]),
            samples_from_constant_windows(&[SILENCE_FLOOR / 2.0, just_below(SILENCE_FLOOR)]),
            samples_from_constant_windows(&[0.0, SILENCE_FLOOR * 1.5, 0.0]),
            samples_from_constant_windows(&[SILENCE_FLOOR, just_below(CLEAR_SPEECH_LEVEL)]),
            samples_from_constant_windows(&[SILENCE_FLOOR / 2.0, CLEAR_SPEECH_LEVEL]),
            tone(SILENCE_FLOOR / 4.0),
            tone(CLEAR_SPEECH_LEVEL / 3.0),
            tone(CLEAR_SPEECH_LEVEL * 4.0),
        ];
        for mut samples in recordings {
            let mut previous = rank(decide(&samples));
            for _ in 0..12 {
                for s in samples.iter_mut() {
                    *s *= 2.0;
                }
                let now = rank(decide(&samples));
                assert!(now >= previous, "doubling moved the outcome down");
                previous = now;
            }
        }
    }

    #[test]
    fn non_finite_samples_fail_open() {
        for bad in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            let mut samples = vec![0.0f32; WINDOW_SAMPLES * 3];
            samples[WINDOW_SAMPLES + 3] = bad;
            let outcome = decide(&samples);
            assert_ne!(outcome, GateOutcome::Silence, "{bad}");
            assert_ne!(outcome, GateOutcome::AllZero, "{bad}");
        }
        let mut quiet = vec![SILENCE_FLOOR / 4.0; WINDOW_SAMPLES * 2];
        quiet[10] = f32::NAN;
        assert_ne!(decide(&quiet), GateOutcome::Silence);
    }
}
