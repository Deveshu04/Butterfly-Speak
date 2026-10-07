//! Short sounds that tell the user a dictation has started or stopped, so
//! they know what happened without looking at the screen.
//!
//! Each cue is one continuous pitch sweep, not a run of separate notes. Start
//! is a quick sweep upward and Stop a slower sweep downward, so the two differ
//! in both direction and length, and a listener can tell them apart without
//! any ear for pitch. A quiet second harmonic brightens the sweep and puts
//! energy in the upper range that small laptop speakers play loudest.
//!
//! Cues are synthesised on the fly at the output device's rate; no audio file
//! ships. This module never reads settings: the controller checks
//! `audio.cues` before it asks for a cue.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// One cue: a single tone whose pitch moves from `from_hz` to `to_hz` over
/// `ms` milliseconds, by an equal musical interval every millisecond.
#[derive(Clone, Copy, Debug)]
struct Sweep {
    from_hz: f32,
    to_hz: f32,
    ms: usize,
}

/// Bottom of both sweeps: high enough that a laptop speaker still plays it.
const LOW_HZ: f32 = 560.0;
/// Top of both sweeps, a little over an octave above `LOW_HZ` and still
/// below the range where a tone turns shrill on headphones.
const HIGH_HZ: f32 = 1200.0;

/// Start: a quick rise, like something opening.
const START_SWEEP: Sweep = Sweep {
    from_hz: LOW_HZ,
    to_hz: HIGH_HZ,
    ms: 120,
};

/// Stop: a slower fall, like something settling.
const STOP_SWEEP: Sweep = Sweep {
    from_hz: HIGH_HZ,
    to_hz: LOW_HZ,
    ms: 180,
};

/// Slack for the output device coming up: `play` returns immediately, and
/// cpal has to open the WASAPI endpoint before the first sample is audible.
const OUTPUT_START_MARGIN_MS: usize = 100;

const fn longest_sweep_ms() -> usize {
    if START_SWEEP.ms > STOP_SWEEP.ms {
        START_SWEEP.ms
    } else {
        STOP_SWEEP.ms
    }
}

/// How long, from the moment `play` is called, a cue may still be sounding:
/// the longer of the two cues plus the device start-up margin. Public
/// because the controller keeps the speech gate from judging this much of
/// the start of a recording, which the start cue (or the previous
/// dictation's stop cue, carried in by the pre-roll) plays into.
pub const AUDIBLE_MS: usize = longest_sweep_ms() + OUTPUT_START_MARGIN_MS;

/// No cue sample is louder than this: 0.15, about −16.5 dBFS.
const CEILING: f32 = 0.15;
/// Level of the second harmonic relative to the fundamental.
const OVERTONE: f32 = 0.3;
/// Raised-cosine fade-in at the start of every sweep.
const FADE_IN_MS: usize = 6;
/// Raised-cosine fade-out at the end of every sweep. Much longer than the
/// fade-in, so the sweep dissolves instead of stopping short.
const FADE_OUT_MS: usize = 40;

#[derive(Clone, Copy, Debug)]
pub enum Cue {
    Start,
    Stop,
}

impl Cue {
    fn sweep(self) -> Sweep {
        match self {
            Cue::Start => START_SWEEP,
            Cue::Stop => STOP_SWEEP,
        }
    }
}

/// Samples in `ms` milliseconds at `rate`, rounded down so a rendered cue is
/// never longer than its design, but never fewer than one.
fn samples_in(ms: usize, rate: u32) -> usize {
    (ms * rate as usize / 1000).max(1)
}

/// Gain (0 to 1) of sample `i` in a sweep `len` samples long: raised-cosine
/// ramps up from exactly 0 at the first sample and down to exactly 0 at the
/// last. On a sweep too short for both ramps, the lower of the two applies.
fn envelope(i: usize, len: usize, rate: u32) -> f32 {
    // Half a cosine period rising from 0 to 1 over `steps` samples.
    let rise = |k: usize, steps: usize| -> f32 {
        if k >= steps {
            1.0
        } else {
            0.5 - 0.5 * (std::f32::consts::PI * k as f32 / steps as f32).cos()
        }
    };
    let fade_in = rise(i, samples_in(FADE_IN_MS, rate));
    let fade_out = rise(len.saturating_sub(i + 1), samples_in(FADE_OUT_MS, rate));
    fade_in.min(fade_out)
}

/// Render `cue` as mono samples at `rate` Hz. The fundamental and its
/// overtone can peak together at `1 + OVERTONE`, so the waveform is scaled
/// by `CEILING / (1 + OVERTONE)` to keep every sample within `CEILING`.
fn synthesize(cue: Cue, rate: u32) -> Vec<f32> {
    use std::f64::consts::TAU;

    let rate = rate.max(1);
    let sweep = cue.sweep();
    let len = samples_in(sweep.ms, rate);
    let from = f64::from(sweep.from_hz);
    let ratio = f64::from(sweep.to_hz) / from;
    let scale = CEILING / (1.0 + OVERTONE);

    let mut phase = 0.0f64;
    (0..len)
        .map(|i| {
            let x = phase as f32;
            let wave = x.sin() + OVERTONE * (2.0 * x).sin();
            let hz = from * ratio.powf(i as f64 / len as f64);
            phase = (phase + TAU * hz / f64::from(rate)) % TAU;
            scale * envelope(i, len, rate) * wave
        })
        .collect()
}

/// Play `cue` on the default output device, fire-and-forget: spawns a thread
/// and returns immediately. All cpal setup happens on that thread, so a
/// missing/busy output device or an unsupported format never touches the
/// caller — it just means no tone plays.
pub fn play(cue: Cue) {
    std::thread::spawn(move || {
        if let Err(e) = play_blocking(cue) {
            tracing::debug!("dictation cue playback skipped: {e:#}");
        }
    });
}

fn play_blocking(cue: Cue) -> anyhow::Result<()> {
    use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

    let host = cpal::default_host();
    let device = host
        .default_output_device()
        .ok_or_else(|| anyhow::anyhow!("no default output device"))?;
    let supported = device.default_output_config()?;
    let channels = supported.channels() as usize;
    let rate = supported.sample_rate();

    let mono = synthesize(cue, rate);
    let frames = mono.len();
    let interleaved: Arc<Vec<f32>> = Arc::new(
        mono.into_iter()
            .flat_map(|s| std::iter::repeat(s).take(channels))
            .collect(),
    );
    let pos = Arc::new(AtomicUsize::new(0));

    let config: cpal::StreamConfig = supported.clone().into();
    let err_cb = |e| tracing::debug!("dictation cue output stream error: {e}");

    // Pulls the next `out.len()` interleaved samples starting from a shared
    // cursor, converting from the f32 render buffer to the device's sample
    // format; once the buffer is exhausted (the tail of playback, and the
    // one-shot stream is never rebuilt) it pads with silence rather than
    // reading out of bounds.
    macro_rules! fill {
        ($ty:ty, $conv:expr) => {{
            let data = interleaved.clone();
            let pos = pos.clone();
            move |out: &mut [$ty], _: &cpal::OutputCallbackInfo| {
                let start = pos.fetch_add(out.len(), Ordering::Relaxed);
                for (i, sample) in out.iter_mut().enumerate() {
                    *sample = data.get(start + i).copied().map($conv).unwrap_or_default();
                }
            }
        }};
    }

    let stream = match supported.sample_format() {
        cpal::SampleFormat::F32 => {
            device.build_output_stream(config, fill!(f32, |s: f32| s), err_cb, None)?
        }
        cpal::SampleFormat::I16 => device.build_output_stream(
            config,
            fill!(i16, |s: f32| (s * 32768.0).clamp(-32768.0, 32767.0) as i16),
            err_cb,
            None,
        )?,
        cpal::SampleFormat::U16 => device.build_output_stream(
            config,
            fill!(u16, |s: f32| ((s * 32768.0) + 32768.0).clamp(0.0, 65535.0) as u16),
            err_cb,
            None,
        )?,
        f => anyhow::bail!("unsupported output sample format {f:?}"),
    };
    stream.play()?;

    // Block this one-shot thread, not the caller, until the tone has had
    // time to play out; a small margin covers scheduling jitter in the
    // backend before the stream actually starts producing sound.
    let duration = Duration::from_secs_f32(frames as f32 / rate as f32 + 0.05);
    std::thread::sleep(duration);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::speech_gate::{window_level, SILENCE_FLOOR, WINDOW_SAMPLES};

    const RATES: [u32; 3] = [16_000, 44_100, 48_000];
    const CUES: [Cue; 2] = [Cue::Start, Cue::Stop];

    /// Loss between the laptop speaker and its own microphone that the start
    /// cue must survive and still reach the gate's floor: 30 dB covers a low
    /// system volume plus the acoustic path around the chassis.
    const SPEAKER_TO_MIC_LOSS_DB: f32 = 30.0;

    /// Sign changes in a stretch of the render. The overtone is too weak to
    /// add any of its own, so this counts the fundamental's cycles.
    fn crossings(samples: &[f32]) -> usize {
        samples
            .windows(2)
            .filter(|pair| (pair[0] < 0.0) != (pair[1] < 0.0))
            .count()
    }

    #[test]
    fn rendered_length_matches_the_design_at_every_rate() {
        for cue in CUES {
            for rate in RATES {
                let rendered = synthesize(cue, rate).len() as f64;
                let designed = cue.sweep().ms as f64 * rate as f64 / 1000.0;
                assert!(
                    (rendered - designed).abs() <= 1.0,
                    "{cue:?} at {rate} Hz: {rendered} samples, designed {designed}"
                );
            }
        }
    }

    #[test]
    fn no_sample_exceeds_the_ceiling() {
        for cue in CUES {
            for rate in RATES {
                let render = synthesize(cue, rate);
                assert!(!render.is_empty());
                let loudest = render.iter().fold(0.0f32, |m, s| m.max(s.abs()));
                assert!(loudest <= CEILING + 1e-6, "{cue:?} at {rate} Hz: {loudest}");
            }
        }
    }

    #[test]
    fn every_sweep_fades_in_from_zero_and_out_to_zero() {
        for rate in RATES {
            let fade_in = samples_in(FADE_IN_MS, rate);
            let fade_out = samples_in(FADE_OUT_MS, rate);
            for cue in CUES {
                let len = samples_in(cue.sweep().ms, rate);

                assert_eq!(envelope(0, len, rate), 0.0);
                assert_eq!(envelope(len - 1, len, rate), 0.0);
                // Each fade takes its full length to reach full gain.
                assert!(envelope(fade_in - 1, len, rate) < 1.0);
                assert_eq!(envelope(fade_in, len, rate), 1.0);
                assert!(envelope(len - fade_out, len, rate) < 1.0);
                assert_eq!(envelope(len - 1 - fade_out, len, rate), 1.0);
                // Gain never jumps: neighbouring samples differ by a small step.
                for i in 1..len {
                    let step = (envelope(i, len, rate) - envelope(i - 1, len, rate)).abs();
                    assert!(step < 0.05, "step {step} at sample {i} of {len} ({rate} Hz)");
                }

                let render = synthesize(cue, rate);
                assert_eq!(render.len(), len);
                assert_eq!(render[0], 0.0);
                assert!(render[len - 1].abs() < 1e-6, "{cue:?} at {rate} Hz");
            }
        }
    }

    #[test]
    fn start_rises_quickly_and_stop_falls_slowly() {
        assert_ne!(synthesize(Cue::Start, 16_000), synthesize(Cue::Stop, 16_000));
        for rate in RATES {
            let start = synthesize(Cue::Start, rate);
            let stop = synthesize(Cue::Stop, rate);
            // Stop lasts half as long again as Start.
            assert!(stop.len() * 2 >= start.len() * 3, "{rate} Hz");

            for (cue, render, rises) in [(Cue::Start, &start, true), (Cue::Stop, &stop, false)] {
                let (first, second) = render.split_at(render.len() / 2);
                let (first, second) = (crossings(first), crossings(second));
                // Over a sweep of about 2.1x in pitch, the upper half runs
                // about 1.46x as many cycles as the lower half.
                let (lower, upper) = if rises { (first, second) } else { (second, first) };
                assert!(
                    upper as f32 > lower as f32 * 1.3,
                    "{cue:?} at {rate} Hz: {first} then {second} crossings"
                );
            }
        }
    }

    #[test]
    fn the_start_cue_would_keep_a_recording_out_of_silence() {
        let render = synthesize(Cue::Start, 16_000);
        assert!(render.len() >= WINDOW_SAMPLES);
        let level = window_level(&render[..WINDOW_SAMPLES]);
        let needed = SILENCE_FLOOR * 10f32.powf(SPEAKER_TO_MIC_LOSS_DB / 20.0);
        assert!(level >= needed, "cue window level {level}, needed {needed}");
    }

    #[test]
    fn the_audible_window_covers_both_rendered_cues() {
        assert!(AUDIBLE_MS <= 400);
        for rate in RATES {
            for cue in CUES {
                let rendered_ms = (synthesize(cue, rate).len() * 1000).div_ceil(rate as usize);
                assert!(rendered_ms < AUDIBLE_MS, "{cue:?} at {rate} Hz: {rendered_ms} ms");
                assert!(rendered_ms + OUTPUT_START_MARGIN_MS <= AUDIBLE_MS);
            }
        }
    }

    #[test]
    fn tiny_sample_rates_render_without_panicking() {
        for rate in [1, 7, 50, 999] {
            for cue in CUES {
                let render = synthesize(cue, rate);
                assert_eq!(render.len(), samples_in(cue.sweep().ms, rate));
                assert!(!render.is_empty());
                assert!(render.iter().all(|s| s.is_finite()));
            }
        }
    }
}
