//! Microphone capture and resampling.
//!
//! The cpal callback pushes raw device samples into a lock-free SPSC ring
//! buffer. The pump drains the ring, takes channel 0, resamples to 16 kHz,
//! and — while the gate is open — ships ~100 ms chunks to the controller.
//! While the gate is closed it maintains a short pre-roll so the first
//! phoneme after the hotkey press is never clipped.
//!
//! The outer loop rebuilds the stream on device switch (settings) or device
//! failure (unplug), so the app survives audio topology changes.

use crate::state::ControlMsg;
use crossbeam_channel::{Receiver, Sender};
use rtrb::RingBuffer;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

const TARGET_RATE: usize = 16000;
const CHUNK_MS: usize = 100;
/// How much audio from *before* the gate opened is shipped ahead of the live
/// stream, so a user who starts talking as they press the chord doesn't lose
/// their first syllable. Public because the speech gate has to know how much
/// of the utterance predates the recording (`controller::gate_evidence`).
pub const PREROLL_MS: usize = 300;
const RESAMPLE_BLOCK: usize = 1024;

pub enum AudioCmd {
    /// Switch input device (None = system default).
    SetDevice(Option<String>),
    /// Emit level events even while idle (mic settings page / onboarding).
    Meter(bool),
}

pub fn spawn(tx: Sender<ControlMsg>, cmd_rx: Receiver<AudioCmd>) -> Arc<AtomicBool> {
    let gate = Arc::new(AtomicBool::new(false));
    let gate2 = gate.clone();
    std::thread::Builder::new()
        .name("audio".into())
        .spawn(move || run_forever(tx, cmd_rx, gate2))
        .expect("spawn audio thread");
    gate
}

pub fn list_input_devices() -> Vec<String> {
    use cpal::traits::{DeviceTrait, HostTrait};
    let host = cpal::default_host();
    host.input_devices()
        .map(|devices| {
            devices
                .filter_map(|d| d.description().ok().map(|desc| desc.name().to_string()))
                .collect()
        })
        .unwrap_or_default()
}

fn run_forever(tx: Sender<ControlMsg>, cmd_rx: Receiver<AudioCmd>, gate: Arc<AtomicBool>) {
    // The pump must keep up even when ASR decodes saturate the CPU — losing
    // scheduler slots here garbles the capture itself.
    unsafe {
        use windows::Win32::System::Threading::{
            GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_TIME_CRITICAL,
        };
        let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_TIME_CRITICAL);
    }

    let mut device_name: Option<String> = None;
    let mut meter = false;
    let mut outage = Outage::default();

    loop {
        match run_stream(&tx, &cmd_rx, &gate, &mut device_name, &mut meter, &mut outage) {
            Ok(()) => {
                outage.recovered(); // clean rebuild (device switch)
            }
            Err(e) => {
                tracing::error!("audio stream failed: {e:#}; retrying in 2s");
                if outage.failed() {
                    let _ = tx.send(ControlMsg::AudioFailed);
                }
                // Absorb commands while waiting so device switches still land.
                if let Ok(cmd) = cmd_rx.recv_timeout(std::time::Duration::from_secs(2)) {
                    apply_cmd(cmd, &mut device_name, &mut meter);
                }
            }
        }
    }
}

/// One report per microphone outage. The rebuild loop retries every two
/// seconds, and a device that stays gone must not flash the pill each time;
/// but once a rebuilt stream delivers samples the device was back, and the
/// next failure is a new outage the user has to hear about.
#[derive(Default)]
struct Outage {
    announced: bool,
}

impl Outage {
    /// A stream failed. Returns whether to report it.
    fn failed(&mut self) -> bool {
        !std::mem::replace(&mut self.announced, true)
    }

    /// A stream is delivering samples, or was rebuilt on purpose.
    fn recovered(&mut self) {
        self.announced = false;
    }
}

fn apply_cmd(cmd: AudioCmd, device_name: &mut Option<String>, meter: &mut bool) {
    match cmd {
        AudioCmd::SetDevice(name) => *device_name = name,
        AudioCmd::Meter(on) => *meter = on,
    }
}

/// Build and pump one stream. Returns Ok on deliberate rebuild (device
/// switch), Err on stream failure.
fn run_stream(
    tx: &Sender<ControlMsg>,
    cmd_rx: &Receiver<AudioCmd>,
    gate: &Arc<AtomicBool>,
    device_name: &mut Option<String>,
    meter: &mut bool,
    outage: &mut Outage,
) -> anyhow::Result<()> {
    use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

    let host = cpal::default_host();
    let device = match device_name.as_deref() {
        Some(want) => host
            .input_devices()?
            .find(|d| {
                d.description()
                    .map(|desc| desc.name() == want)
                    .unwrap_or(false)
            })
            .or_else(|| host.default_input_device()),
        None => host.default_input_device(),
    }
    .ok_or_else(|| anyhow::anyhow!("no input device available"))?;

    let supported = device.default_input_config()?;
    let channels = supported.channels() as usize;
    let in_rate = supported.sample_rate() as usize;
    tracing::info!(
        "mic: {:?}, {} ch @ {} Hz, {:?}",
        device
            .description()
            .map(|d| d.name().to_string())
            .unwrap_or_default(),
        channels,
        in_rate,
        supported.sample_format()
    );

    // 10 s of headroom: if the pump is ever starved long enough to overflow
    // this, samples get dropped and speech turns to garble — so drops are
    // counted and loudly logged rather than silently swallowed.
    let (mut prod, mut cons) = RingBuffer::<f32>::new(in_rate * channels * 10);
    let dropped = Arc::new(AtomicUsize::new(0));
    let dropped_w = dropped.clone();
    let failed = Arc::new(AtomicBool::new(false));
    let failed_w = failed.clone();

    let err_cb = move |e| {
        tracing::error!("cpal stream error: {e}");
        failed_w.store(true, Ordering::Relaxed);
    };
    let config: cpal::StreamConfig = supported.clone().into();
    macro_rules! push_all {
        ($data:ident, $conv:expr) => {
            for &s in $data {
                if prod.push($conv(s)).is_err() {
                    dropped_w.fetch_add(1, Ordering::Relaxed);
                }
            }
        };
    }
    let stream = match supported.sample_format() {
        cpal::SampleFormat::F32 => device.build_input_stream(
            config,
            move |data: &[f32], _| push_all!(data, |s| s),
            err_cb,
            None,
        )?,
        cpal::SampleFormat::I16 => device.build_input_stream(
            config,
            move |data: &[i16], _| push_all!(data, |s: i16| s as f32 / 32768.0),
            err_cb,
            None,
        )?,
        cpal::SampleFormat::U16 => device.build_input_stream(
            config,
            move |data: &[u16], _| push_all!(data, |s: u16| (s as f32 - 32768.0) / 32768.0),
            err_cb,
            None,
        )?,
        f => anyhow::bail!("unsupported sample format {f:?}"),
    };
    stream.play()?;

    let mut resampler = if in_rate != TARGET_RATE {
        Some(rubato::FftFixedIn::<f32>::new(
            in_rate,
            TARGET_RATE,
            RESAMPLE_BLOCK,
            2,
            1,
        )?)
    } else {
        None
    };

    let chunk_out = TARGET_RATE * CHUNK_MS / 1000;
    let preroll_cap = TARGET_RATE * PREROLL_MS / 1000;
    let mut mono: Vec<f32> = Vec::with_capacity(RESAMPLE_BLOCK * 4);
    let mut out: Vec<f32> = Vec::with_capacity(chunk_out * 4);
    let mut preroll: VecDeque<f32> = VecDeque::with_capacity(preroll_cap + chunk_out);
    let mut was_open = false;

    loop {
        // Commands / failure checks.
        while let Ok(cmd) = cmd_rx.try_recv() {
            let switching = matches!(cmd, AudioCmd::SetDevice(_));
            apply_cmd(cmd, device_name, meter);
            if switching {
                return Ok(()); // rebuild with the new device
            }
        }
        if failed.load(Ordering::Relaxed) {
            anyhow::bail!("stream reported an error (device unplugged?)");
        }

        // Drain ring → mono, keeping only channel 0. Averaging is wrong for
        // laptop mic arrays: the second channel is often phase-inverted for
        // noise cancellation, so (L+R)/2 cancels speech to near-silence.
        let avail = cons.slots();
        let frames = avail / channels;
        if frames > 0 {
            outage.recovered();
        }
        if frames > in_rate / 2 {
            tracing::warn!(
                "audio pump starved: drained {:.2}s at once (CPU oversubscribed?)",
                frames as f32 / in_rate as f32
            );
        }
        let lost = dropped.swap(0, Ordering::Relaxed);
        if lost > 0 {
            tracing::error!("capture ring overflowed: {lost} samples dropped — audio is garbled");
        }
        for _ in 0..frames {
            let mut first = 0.0f32;
            for c in 0..channels {
                let s = cons.pop().unwrap_or(0.0);
                if c == 0 {
                    first = s;
                }
            }
            mono.push(first);
        }

        // Resample full blocks → 16 kHz.
        if let Some(rs) = resampler.as_mut() {
            use rubato::Resampler;
            while mono.len() >= RESAMPLE_BLOCK {
                let block: Vec<f32> = mono.drain(..RESAMPLE_BLOCK).collect();
                let processed = rs.process(&[block], None)?;
                out.extend_from_slice(&processed[0]);
            }
        } else {
            out.append(&mut mono);
        }

        let open = gate.load(Ordering::Relaxed);
        if open && !was_open {
            // Gate just opened: ship the pre-roll first.
            let pre: Vec<f32> = preroll.drain(..).collect();
            if !pre.is_empty() {
                let _ = tx.send(ControlMsg::Audio(pre));
            }
        } else if !open && was_open {
            // Gate just closed: flush whatever is buffered, then mark the tail.
            if !out.is_empty() {
                let _ = tx.send(ControlMsg::Audio(std::mem::take(&mut out)));
            }
            let _ = tx.send(ControlMsg::AudioTail);
        }
        was_open = open;

        while out.len() >= chunk_out {
            let chunk: Vec<f32> = out.drain(..chunk_out).collect();
            let rms = (chunk.iter().map(|s| s * s).sum::<f32>() / chunk.len() as f32).sqrt();
            if open {
                let _ = tx.send(ControlMsg::Level(rms));
                let _ = tx.send(ControlMsg::Audio(chunk));
            } else {
                if *meter {
                    let _ = tx.send(ControlMsg::Level(rms));
                }
                for s in chunk {
                    if preroll.len() >= preroll_cap {
                        preroll.pop_front();
                    }
                    preroll.push_back(s);
                }
            }
        }

        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A device that stays gone is one report, however many rebuilds fail;
    /// a device that came back and was lost again is a second one.
    #[test]
    fn a_mic_lost_again_after_it_came_back_is_reported_again() {
        let mut outage = Outage::default();
        assert!(outage.failed(), "the first loss is reported");
        assert!(!outage.failed(), "a rebuild that fails again is the same outage");
        outage.recovered();
        assert!(outage.failed(), "lost again after delivering samples");
    }
}
