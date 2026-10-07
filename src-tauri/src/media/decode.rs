//! Turn whatever the user picked into the one thing Sarvam's Batch API
//! actually transcribes: 16 kHz mono 16-bit PCM WAV.
//!
//! # The measurement this module exists for
//!
//! Sarvam's speech-to-text docs list WAV, MP3, AAC, AIFF, OGG, OPUS, FLAC,
//! MP4/M4A, AMR, WMA, WebM and raw PCM. The Batch **job** endpoint does not
//! honour that list. Controlled uploads of the same 60 s of speech through
//! the exact five-call lifecycle [`crate::sarvam::batch_job`] uses:
//!
//! | Sent as                      | Transcript |
//! |------------------------------|------------|
//! | 16 kHz mono PCM WAV          | 1327 chars, `en-IN` |
//! | 48 kHz **stereo** PCM WAV    | 0 chars |
//! | 16 kHz mono MP3              | 0 chars |
//! | audio-only M4A (AAC)         | 0 chars |
//! | audio-only WebM (Opus)       | 0 chars |
//! | raw `.mp4` (H.264 + AAC)     | four spaces |
//!
//! **Every failure reported success**: `job_state: "Completed"`,
//! `successful_files_count: 1`, `failed_files_count: 0`, per-file
//! `state: "Success"`, `error_message: ""`. There is no error field that
//! reveals it; the only tell is a nonsense `language_code` (`ml-IN`, `kn-IN`
//! for English speech) beside empty text. So the server cannot be asked what
//! it will accept — the client has to send the one shape that is known to
//! work, every time.
//!
//! That makes this module the difference between a working import and a
//! silent one, for **every** format and not merely for video.
//!
//! # Why in-process, and not ffmpeg
//!
//! Converting with an external tool such as ffmpeg would mean shipping and
//! updating a second executable beside the app, and handing the user's file
//! to it. Nothing here needs that:
//! symphonia already demuxes these containers for [`super::probe`], rubato
//! already resamples the live capture path ([`crate::audio`]), and both are
//! pure Rust that links cleanly against this build's static CRT. The only
//! thing that had to change was the symphonia feature list, which was
//! deliberately decoder-free while nothing decoded a sample.
//!
//! # The gap this cannot close
//!
//! **symphonia has no Opus decoder**, and there is no pure-Rust one to reach
//! for. `.opus` files and most `.webm` recordings (browsers record Opus) fail
//! here, permanently, and [`DecodeError::UnsupportedCodec`] says so by name
//! and says what to do about it. That is a worse outcome than converting
//! them, and a far better one than the empty, billed, "successful" note the
//! measurement above describes.
//!
//! # Relationship to `probe`
//!
//! [`super::probe::probe`] reads headers and decodes nothing; this decodes
//! and writes. The container is therefore opened and probed twice per import.
//! That is deliberate: the probe is a bounded header parse that runs on files
//! this module must never be handed (a text file wearing a `.wav` extension),
//! and keeping the two independent means the gate cannot be skipped by a
//! future caller that only wants the conversion.

use std::fs::File;
use std::io::{BufWriter, Seek, SeekFrom, Write};
use std::path::Path;

use symphonia::core::codecs::audio::{well_known as wk, AudioCodecId, AudioDecoderOptions};
use symphonia::core::errors::Error as SymphoniaError;
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::{FormatOptions, TrackType};
use symphonia::core::io::{MediaSourceStream, MediaSourceStreamOptions};
use symphonia::core::meta::MetadataOptions;

/// The one sample rate Sarvam's batch endpoint reliably transcribes. Same
/// number as [`crate::audio`]'s realtime target, arrived at independently:
/// there it is what the realtime WebSocket wants, here it is what the
/// measurement above says the job API wants.
pub const TARGET_RATE_HZ: u32 = 16_000;

/// Input frames handed to the resampler per call.
///
/// Four times [`crate::audio`]'s 1024 because the tradeoff is the opposite
/// one: that path is sizing for latency on a live stream, this one is sizing
/// for throughput on a file that may run two hours. Larger blocks mean fewer
/// FFTs and fewer per-block allocations; the cost is a bigger working set,
/// and 4096 f32 frames is 16 kB.
const RESAMPLE_BLOCK: usize = 4096;

/// What a finished conversion produced.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Converted {
    /// Playing time of the **written** file, measured from the frames that
    /// were actually decoded rather than read from a container header.
    ///
    /// This is the reason the struct exists. [`super::probe::Probe`]'s
    /// duration is whatever the container claims, and it is routinely absent
    /// (a VBR MP3 with no Xing header) or wrong (a WebM a live recorder never
    /// went back to patch). An import whose length is unknown gets the
    /// 30-minute poll ceiling instead of a length-scaled deadline; once the
    /// file has been decoded end to end, the length is no longer a guess.
    pub duration_s: f64,
    /// Frames written at [`TARGET_RATE_HZ`], mono.
    pub frames: u64,
    /// The rate the source decoded at, as the decoder reported it — not as
    /// the container header claimed. Diagnostic.
    pub source_rate: u32,
    /// Channels before the downmix. Diagnostic.
    pub source_channels: u16,
    /// Whether the resampler ran at all. False for a source already at
    /// [`TARGET_RATE_HZ`], which is passed through untouched.
    pub resampled: bool,
}

/// Every way a conversion can stop. Each carries a finished sentence for the
/// user, in the same style as [`super::probe::ProbeError::user_message`]: say
/// what happened and what it means, never a code and never a library's own
/// error text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DecodeError {
    /// The source could not be opened or read.
    Unreadable,
    /// No registered container reader recognized the bytes, or the audio
    /// decoded at a sample rate outside [`super::probe::SAMPLE_RATES_HZ`].
    NotMedia,
    /// A container was recognized but carries no audio track.
    NoAudioTrack,
    /// The audio track's codec has no decoder in this build. **Opus is the
    /// permanent case** — see the module doc — and the codec is named so the
    /// message can be acted on rather than merely read.
    UnsupportedCodec { codec: &'static str },
    /// Decoding finished having produced no audio frames at all.
    NoAudio,
    /// The destination could not be written, or the resampler refused the
    /// source's rate. Both are "nothing is wrong with your file, and the
    /// conversion still did not happen", which is one sentence to the user
    /// and two different log lines here.
    ConversionFailed,
    /// The import was cancelled mid-conversion.
    ///
    /// Carries a message for completeness only: the import path maps this
    /// straight onto its own cancelled state, which is deliberately never
    /// phrased as a failure.
    Cancelled,
}

impl DecodeError {
    pub fn user_message(&self) -> String {
        match self {
            // Same words as the probe's for the same situation. A user who
            // meets this twice should not be told two different things.
            DecodeError::Unreadable => {
                "Couldn't open that file — it may have been moved, renamed or locked by another app"
                    .to_string()
            }
            DecodeError::NotMedia => {
                "That file doesn't look like audio Butterfly Speak can read — it may be damaged, or \
                 have the wrong extension"
                    .to_string()
            }
            DecodeError::NoAudioTrack => {
                "That file has no audio track — only the video, or nothing at all".to_string()
            }
            // WAV specifically, and not "WAV or MP3": MP3 reaches Sarvam
            // fine and comes back empty (see the module doc), so suggesting
            // it would send the user round a loop that ends in a billed,
            // successful, blank note.
            DecodeError::UnsupportedCodec { codec } => format!(
                "Butterfly Speak can't decode {codec} audio — convert the recording to a WAV file \
                 and import that instead"
            ),
            DecodeError::NoAudio => {
                "That recording decoded to no audio at all — the file may be empty or damaged"
                    .to_string()
            }
            DecodeError::ConversionFailed => {
                "Butterfly Speak couldn't prepare that recording for upload — check there's free \
                 disk space and try again"
                    .to_string()
            }
            DecodeError::Cancelled => "That import was cancelled.".to_string(),
        }
    }
}

/// A display name for a codec this build cannot decode.
///
/// Only the ones an accepted container can plausibly carry are named. The
/// fallback is a word that keeps [`DecodeError::user_message`] grammatical
/// rather than a hex id the user cannot act on; the id goes to the log
/// instead, where a support report can find it.
fn codec_name(id: AudioCodecId) -> &'static str {
    match id {
        wk::CODEC_ID_OPUS => "Opus",
        wk::CODEC_ID_SPEEX => "Speex",
        wk::CODEC_ID_MP1 => "MPEG Layer 1",
        wk::CODEC_ID_MP2 => "MPEG Layer 2",
        wk::CODEC_ID_AC3 => "AC-3",
        wk::CODEC_ID_EAC3 => "E-AC-3",
        wk::CODEC_ID_AC4 => "Dolby AC-4",
        wk::CODEC_ID_TRUEHD => "Dolby TrueHD",
        wk::CODEC_ID_DCA => "DTS",
        wk::CODEC_ID_WMA => "Windows Media Audio",
        wk::CODEC_ID_WAVPACK => "WavPack",
        wk::CODEC_ID_MONKEYS_AUDIO => "Monkey's Audio",
        wk::CODEC_ID_MUSEPACK => "Musepack",
        wk::CODEC_ID_TTA => "True Audio",
        wk::CODEC_ID_ATRAC1 | wk::CODEC_ID_ATRAC3 | wk::CODEC_ID_ATRAC3PLUS
        | wk::CODEC_ID_ATRAC9 => "ATRAC",
        _ => "this",
    }
}

/// Convert `src` to a 16 kHz mono 16-bit PCM WAV at `dst`.
///
/// `dst` is created (or truncated) and is left on disk only on success —
/// every error path removes it, so a caller that fails never has to reason
/// about a half-written file. The caller still owns deleting it afterwards;
/// the import path does that with an RAII guard.
///
/// `stop` is consulted before the file is opened and then once per packet.
/// It is a `Fn() -> bool` rather than the import queue's own `CancelToken` so
/// that this module stays a leaf `media` can own without depending on
/// `import`; per-packet is fine-grained enough to matter (a packet is tens of
/// milliseconds of audio) and cheap enough to ignore (the import path's
/// implementation is one atomic read). A caller with nothing to cancel passes
/// `&|| false`.
///
/// Blocking, and unapologetically so: a two-hour recording is minutes of CPU.
/// The import path runs it on `spawn_blocking`.
pub fn to_wav_16k_mono(
    src: &Path,
    dst: &Path,
    stop: &dyn Fn() -> bool,
) -> Result<Converted, DecodeError> {
    match convert(src, dst, stop) {
        Ok(converted) => Ok(converted),
        Err(e) => {
            // A partial WAV is worse than none: it would upload, transcribe
            // and produce a confidently truncated note.
            let _ = std::fs::remove_file(dst);
            Err(e)
        }
    }
}

fn convert(src: &Path, dst: &Path, stop: &dyn Fn() -> bool) -> Result<Converted, DecodeError> {
    if stop() {
        return Err(DecodeError::Cancelled);
    }

    let file = File::open(src).map_err(|e| {
        // The path is never logged: a user's file path is as personal as the
        // recording in it. `io::ErrorKind` is a fieldless enum.
        tracing::warn!(kind = ?e.kind(), "import conversion could not open the source");
        DecodeError::Unreadable
    })?;
    let mss = MediaSourceStream::new(Box::new(file), MediaSourceStreamOptions::default());

    let mut hint = Hint::new();
    if let Some(ext) = src.extension().and_then(|e| e.to_str()) {
        // A hint, not a claim: a `.wav` that is really an MP3 still decodes
        // as an MP3, the same way it probes as one.
        hint.with_extension(&ext.to_ascii_lowercase());
    }

    let mut format = symphonia::default::get_probe()
        .probe(
            &hint,
            mss,
            FormatOptions::default(),
            MetadataOptions::default(),
        )
        .map_err(|e| {
            // Safe to interpolate: every `symphonia::core::errors::Error`
            // payload is a `&'static str` the library wrote, or an io error.
            tracing::warn!("import conversion found no readable container: {e}");
            DecodeError::NotMedia
        })?;

    let track = format
        .default_track(TrackType::Audio)
        .or_else(|| {
            format
                .tracks()
                .iter()
                .find(|t| t.codec_params.as_ref().is_some_and(|p| p.is_audio()))
        })
        .ok_or(DecodeError::NoAudioTrack)?;
    let track_id = track.id;
    let params = track
        .codec_params
        .as_ref()
        .and_then(|p| p.audio())
        .ok_or(DecodeError::NoAudioTrack)?
        .clone();

    // Asked of the registry *before* the decoder is built, so an absent codec
    // is named from its id rather than inferred from a construction failure
    // that could equally mean corrupt parameters.
    if symphonia::default::get_codecs()
        .get_audio_decoder(params.codec)
        .is_none()
    {
        tracing::warn!(codec = %params.codec, "import conversion has no decoder for this codec");
        return Err(DecodeError::UnsupportedCodec {
            codec: codec_name(params.codec),
        });
    }
    let mut decoder = symphonia::default::get_codecs()
        .make_audio_decoder(&params, &AudioDecoderOptions::default())
        .map_err(|e| {
            tracing::warn!(codec = %params.codec, "import conversion could not start a decoder: {e}");
            DecodeError::UnsupportedCodec {
                codec: codec_name(params.codec),
            }
        })?;

    let mut writer = WavWriter::create(dst)?;
    let mut resampler: Option<rubato::FftFixedIn<f32>> = None;
    let mut source_rate = 0u32;
    let mut source_channels = 0u16;
    // Frames of the resampler's own group delay still to be dropped from the
    // front of its output; see the flush block at the end of this function.
    let mut skip_out = 0usize;
    // Mono frames handed to the resampler. The output's length is measured
    // against this rather than against the container's own frame count,
    // which is routinely absent and occasionally wrong.
    let mut source_frames = 0u64;

    // Two reused scratch buffers, both bounded by one decoded packet:
    // `copy_to_vec_interleaved` resizes to exactly the sample count, and
    // `mono` never holds more than one block plus one packet.
    let mut interleaved: Vec<f32> = Vec::new();
    let mut mono: Vec<f32> = Vec::with_capacity(RESAMPLE_BLOCK * 2);

    loop {
        if stop() {
            return Err(DecodeError::Cancelled);
        }
        let packet = match format.next_packet() {
            Ok(Some(p)) => p,
            Ok(None) => break,
            // A truncated file. Whatever decoded before the cut is real
            // audio and worth keeping; the import's own coverage banner is
            // what tells the user a transcript came up short.
            Err(SymphoniaError::IoError(e))
                if e.kind() == std::io::ErrorKind::UnexpectedEof =>
            {
                break
            }
            // Chained streams (a concatenated OGG). Only the first physical
            // stream is converted, which is what a player would also show
            // without being told to go looking.
            Err(SymphoniaError::ResetRequired) => break,
            Err(e) => {
                tracing::warn!("import conversion stopped reading packets early: {e}");
                break;
            }
        };
        if packet.track_id != track_id {
            continue;
        }

        let decoded = match decoder.decode(&packet) {
            Ok(buf) => buf,
            // Both are documented as "discard this packet and carry on".
            Err(SymphoniaError::DecodeError(_)) | Err(SymphoniaError::IoError(_)) => continue,
            Err(e) => {
                tracing::warn!("import conversion stopped decoding early: {e}");
                break;
            }
        };
        if decoded.is_empty() {
            continue;
        }

        let channels = decoded.num_planes().max(1);
        if source_rate == 0 {
            source_rate = decoded.spec().rate();
            source_channels = channels.min(u16::MAX as usize) as u16;
            // Checked here as well as in the probe: this is the rate the
            // resampler is built from, and one outside the range could ask
            // it for more memory than the machine has.
            if !super::probe::SAMPLE_RATES_HZ.contains(&source_rate) {
                tracing::warn!(source_rate, "import conversion decoded audio at a sample rate no recording has");
                return Err(DecodeError::NotMedia);
            }
            if source_rate != TARGET_RATE_HZ {
                use rubato::Resampler;
                let rs = rubato::FftFixedIn::<f32>::new(
                    source_rate as usize,
                    TARGET_RATE_HZ as usize,
                    RESAMPLE_BLOCK,
                    2,
                    1,
                )
                .map_err(|e| {
                    tracing::warn!("import conversion could not build a resampler: {e}");
                    DecodeError::ConversionFailed
                })?;
                // An FFT resampler's output lags its input by half an output
                // window. Dropping exactly that many frames from the front
                // realigns the conversion with the recording, which is what
                // lets the tail be cut at the recording's true length below
                // instead of at "whatever came out".
                skip_out = rs.output_delay();
                resampler = Some(rs);
            }
            tracing::debug!(
                source_rate,
                source_channels,
                resampling = resampler.is_some(),
                "import conversion started"
            );
        }

        // Averaged, not channel-0. `crate::audio` deliberately keeps only the
        // first channel because a laptop mic array's second one is often
        // phase-inverted for noise cancellation, so (L+R)/2 cancels speech.
        // An imported file is not a mic array: it is a mix somebody made, and
        // dropping a channel from one can drop a speaker.
        decoded.copy_to_vec_interleaved(&mut interleaved);
        for frame in interleaved.chunks_exact(channels) {
            mono.push(frame.iter().sum::<f32>() / channels as f32);
        }
        source_frames += (interleaved.len() / channels) as u64;

        match resampler.as_mut() {
            Some(rs) => {
                use rubato::Resampler;
                while mono.len() >= RESAMPLE_BLOCK {
                    let block: Vec<f32> = mono.drain(..RESAMPLE_BLOCK).collect();
                    let out = rs.process(&[block], None).map_err(|e| {
                        tracing::warn!("import conversion could not resample a block: {e}");
                        DecodeError::ConversionFailed
                    })?;
                    emit(&mut writer, &out[0], &mut skip_out, None)?;
                }
            }
            None => {
                emit(&mut writer, &mono, &mut skip_out, None)?;
                mono.clear();
            }
        }
    }

    // The tail, and the flush that the tail alone does not achieve.
    //
    // An FFT resampler holds back up to one FFT window of input: feeding it
    // the last partial block still leaves ~`fft_size_in` frames inside it,
    // so without the flush below, the final ~40 ms of every resampled import
    // would be lost. Zeros are pushed through until the output reaches the
    // recording's true length — `source_frames` scaled by the rate ratio —
    // and then it is cut there, so the padding never reaches the file.
    match resampler.as_mut() {
        None => emit(&mut writer, &mono, &mut skip_out, None)?,
        Some(rs) => {
            use rubato::Resampler;
            let expected = ((source_frames as f64) * f64::from(TARGET_RATE_HZ)
                / f64::from(source_rate))
            .round() as u64;
            if !mono.is_empty() {
                let out = rs
                    .process_partial(Some(&[std::mem::take(&mut mono)]), None)
                    .map_err(|e| {
                        tracing::warn!("import conversion could not resample the tail: {e}");
                        DecodeError::ConversionFailed
                    })?;
                emit(&mut writer, &out[0], &mut skip_out, Some(expected))?;
            }
            // Bounded, because "push zeros until enough comes out" must not
            // become "push zeros forever" if a rate ratio makes the arithmetic
            // above unreachable. Two or three passes is the normal case; the
            // worst plausible one (a rate coprime with 16 kHz, so one FFT
            // window is a whole second) needs a dozen.
            const FLUSH_PASSES: usize = 64;
            for _ in 0..FLUSH_PASSES {
                if writer.frames >= expected {
                    break;
                }
                let out = rs.process_partial::<Vec<f32>>(None, None).map_err(|e| {
                    tracing::warn!("import conversion could not flush the resampler: {e}");
                    DecodeError::ConversionFailed
                })?;
                if out[0].is_empty() {
                    break;
                }
                emit(&mut writer, &out[0], &mut skip_out, Some(expected))?;
            }
        }
    }

    let frames = writer.finish()?;
    if frames == 0 {
        return Err(DecodeError::NoAudio);
    }
    let duration_s = frames as f64 / f64::from(TARGET_RATE_HZ);
    tracing::info!(
        frames,
        source_rate,
        source_channels,
        "import conversion wrote a 16 kHz mono WAV"
    );
    Ok(Converted {
        duration_s,
        frames,
        source_rate,
        source_channels,
        resampled: resampler.is_some(),
    })
}

/// Write `samples`, first dropping whatever remains of the stream-wide
/// `skip` (the resampler's group delay) and then stopping at `cap` total
/// frames (the recording's true length).
///
/// One function rather than two because the two are the same idea seen from
/// each end: the resampled stream is the recording shifted later by
/// `output_delay` frames, so the file wants the window that starts `skip`
/// frames in and is `cap` frames long. Splitting them invites a fix to one
/// end that quietly unbalances the other.
fn emit(
    writer: &mut WavWriter,
    samples: &[f32],
    skip: &mut usize,
    cap: Option<u64>,
) -> Result<(), DecodeError> {
    let dropped = (*skip).min(samples.len());
    *skip -= dropped;
    let mut slice = &samples[dropped..];
    if let Some(cap) = cap {
        let room = cap.saturating_sub(writer.frames) as usize;
        if slice.len() > room {
            slice = &slice[..room];
        }
    }
    writer.write_frames(slice)
}

// ---------------------------------------------------------------------------
// The WAV writer.
// ---------------------------------------------------------------------------

/// Canonical 44-byte RIFF/WAVE header for 16-bit mono PCM at
/// [`TARGET_RATE_HZ`], carrying `data_len` bytes of samples.
fn wav_header(data_len: u32) -> [u8; 44] {
    let mut h = [0u8; 44];
    let rate = TARGET_RATE_HZ;
    h[0..4].copy_from_slice(b"RIFF");
    h[4..8].copy_from_slice(&(36 + data_len).to_le_bytes());
    h[8..12].copy_from_slice(b"WAVE");
    h[12..16].copy_from_slice(b"fmt ");
    h[16..20].copy_from_slice(&16u32.to_le_bytes()); // PCM fmt chunk size
    h[20..22].copy_from_slice(&1u16.to_le_bytes()); // WAVE_FORMAT_PCM
    h[22..24].copy_from_slice(&1u16.to_le_bytes()); // mono
    h[24..28].copy_from_slice(&rate.to_le_bytes());
    h[28..32].copy_from_slice(&(rate * 2).to_le_bytes()); // byte rate
    h[32..34].copy_from_slice(&2u16.to_le_bytes()); // block align
    h[34..36].copy_from_slice(&16u16.to_le_bytes()); // bits per sample
    h[36..40].copy_from_slice(b"data");
    h[40..44].copy_from_slice(&data_len.to_le_bytes());
    h
}

/// Streams i16 samples to a file and patches the two length fields at the
/// end.
///
/// # Why patch-back rather than buffer-then-write
///
/// A RIFF header states the sizes up front, and the size is only known once
/// the last frame is decoded. The two ways out are to hold the whole
/// conversion in RAM until the length is known, or to write a placeholder,
/// stream, and seek back over 8 bytes. This takes the second.
///
/// The first is not affordable here. [`super::probe::MAX_IMPORT_DURATION_S`]
/// admits a two-hour recording; two hours at 16 kHz mono 16-bit is ~230 MB of
/// PCM, on a machine with 7.7 GB that is also holding the decoder, and
/// [`crate::sarvam::batch_job::JobClient::upload`] is *already* going to read
/// the finished file into a `Bytes` to put it on the wire. Buffering here
/// would mean two full copies live at once for no gain. Streaming keeps the
/// resident cost at one `BufWriter` — 64 kB — plus one packet.
///
/// The cost of the choice is that a crash mid-conversion leaves a file whose
/// header says zero bytes of audio. Nothing reads it: the conversion's own
/// error path deletes the destination, and the import path holds an RAII
/// guard that deletes it again.
struct WavWriter {
    file: BufWriter<File>,
    frames: u64,
    /// Reused so a packet's worth of samples does not allocate per call.
    bytes: Vec<u8>,
}

impl WavWriter {
    fn create(path: &Path) -> Result<WavWriter, DecodeError> {
        let file = File::create(path).map_err(|e| {
            tracing::warn!(kind = ?e.kind(), "import conversion could not create its output");
            DecodeError::ConversionFailed
        })?;
        let mut file = BufWriter::with_capacity(64 * 1024, file);
        file.write_all(&wav_header(0)).map_err(io_failed)?;
        Ok(WavWriter {
            file,
            frames: 0,
            bytes: Vec::new(),
        })
    }

    fn write_frames(&mut self, samples: &[f32]) -> Result<(), DecodeError> {
        self.bytes.clear();
        self.bytes.reserve(samples.len() * 2);
        for &s in samples {
            self.bytes.extend_from_slice(&to_i16(s).to_le_bytes());
        }
        self.file.write_all(&self.bytes).map_err(io_failed)?;
        self.frames += samples.len() as u64;
        Ok(())
    }

    /// Flush, patch the RIFF and data lengths, and return the frame count.
    fn finish(self) -> Result<u64, DecodeError> {
        let data_len = self.frames.saturating_mul(2);
        // 4 GB of 16 kHz mono PCM is 37 hours, far past the import ceiling —
        // but a header that silently wrapped would describe a file nobody
        // could read, so it is refused rather than truncated.
        let data_len = u32::try_from(data_len).map_err(|_| {
            tracing::warn!("import conversion produced more PCM than a RIFF header can state");
            DecodeError::ConversionFailed
        })?;
        let mut file = self.file.into_inner().map_err(|e| {
            tracing::warn!(kind = ?e.error().kind(), "import conversion could not flush its output");
            DecodeError::ConversionFailed
        })?;
        let header = wav_header(data_len);
        file.seek(SeekFrom::Start(0)).map_err(io_failed)?;
        file.write_all(&header).map_err(io_failed)?;
        file.flush().map_err(io_failed)?;
        Ok(self.frames)
    }
}

fn io_failed(e: std::io::Error) -> DecodeError {
    tracing::warn!(kind = ?e.kind(), "import conversion could not write its output");
    DecodeError::ConversionFailed
}

/// f32 sample to 16-bit PCM.
///
/// The scale is 32768 and not 32767 because that is the divisor symphonia
/// itself uses converting an integer sample to `f32`
/// (`symphonia-core/src/audio/conv.rs`: `s as f32 / 32_768.0`). Matching it
/// makes a 16-bit source round-trip bit-exact; scaling by 32767 would shift
/// every sample by up to one LSB for no reason. The clamp is what keeps
/// `+1.0` from a float source — and only that — at `i16::MAX`.
fn to_i16(s: f32) -> i16 {
    let scaled = (s * 32_768.0).round();
    scaled.clamp(i16::MIN as f32, i16::MAX as f32) as i16
}

#[cfg(test)]
mod tests {
    use super::*;

    // ------------------------------------------------------------ fixtures --

    /// A genuine RIFF/WAVE file: 16-bit PCM at `rate` Hz with `channels`
    /// channels and `frames` frames, each channel carrying `sample(frame,
    /// channel)`. Synthesized rather than checked in, so the fixture cannot
    /// drift from what the test claims it is.
    fn wav_bytes(
        rate: u32,
        channels: u16,
        frames: u32,
        sample: impl Fn(u32, u16) -> i16,
    ) -> Vec<u8> {
        let block_align = channels * 2;
        let data_len = frames * u32::from(block_align);
        let mut v = Vec::with_capacity(44 + data_len as usize);
        v.extend_from_slice(b"RIFF");
        v.extend_from_slice(&(36 + data_len).to_le_bytes());
        v.extend_from_slice(b"WAVE");
        v.extend_from_slice(b"fmt ");
        v.extend_from_slice(&16u32.to_le_bytes());
        v.extend_from_slice(&1u16.to_le_bytes()); // WAVE_FORMAT_PCM
        v.extend_from_slice(&channels.to_le_bytes());
        v.extend_from_slice(&rate.to_le_bytes());
        v.extend_from_slice(&(rate * u32::from(block_align)).to_le_bytes());
        v.extend_from_slice(&block_align.to_le_bytes());
        v.extend_from_slice(&16u16.to_le_bytes());
        v.extend_from_slice(b"data");
        v.extend_from_slice(&data_len.to_le_bytes());
        for f in 0..frames {
            for c in 0..channels {
                v.extend_from_slice(&sample(f, c).to_le_bytes());
            }
        }
        v
    }

    /// A 440 Hz tone at `rate`, identical in every channel.
    fn tone(rate: u32) -> impl Fn(u32, u16) -> i16 {
        move |f, _| {
            let t = f as f64 / f64::from(rate);
            ((t * 440.0 * std::f64::consts::TAU).sin() * 8000.0) as i16
        }
    }

    // ---- a minimal WebM, hand-built ------------------------------------
    //
    // The same idea as `probe`'s fixture, and for the same reason: the
    // demuxer's init needs only an EBML header, a Segment, an Info and a
    // Tracks, so a container with a chosen codec can be synthesized without
    // an encoder and without a binary blob in the repo.

    fn ebml_size(n: usize) -> Vec<u8> {
        let mut v = vec![0x01u8];
        v.extend_from_slice(&(n as u64).to_be_bytes()[1..]);
        v
    }

    fn elem(id: &[u8], payload: &[u8]) -> Vec<u8> {
        let mut v = id.to_vec();
        v.extend_from_slice(&ebml_size(payload.len()));
        v.extend_from_slice(payload);
        v
    }

    fn ebml_uint(n: u64) -> Vec<u8> {
        n.to_be_bytes().to_vec()
    }

    fn ebml_float(x: f64) -> Vec<u8> {
        x.to_be_bytes().to_vec()
    }

    /// A WebM carrying one track of `codec_id` with TrackType `track_type`
    /// (2 = audio, 1 = video). No Clusters, so nothing decodes — which is all
    /// these tests need, since both of them fail before the first packet.
    fn webm_bytes(codec_id: &[u8], track_type: u64) -> Vec<u8> {
        let header = elem(
            &[0x1A, 0x45, 0xDF, 0xA3],
            &[
                elem(&[0x42, 0x86], &ebml_uint(1)),
                elem(&[0x42, 0xF7], &ebml_uint(1)),
                elem(&[0x42, 0xF2], &ebml_uint(4)),
                elem(&[0x42, 0xF3], &ebml_uint(8)),
                elem(&[0x42, 0x82], b"webm"),
                elem(&[0x42, 0x87], &ebml_uint(2)),
                elem(&[0x42, 0x85], &ebml_uint(2)),
            ]
            .concat(),
        );
        let info = elem(
            &[0x15, 0x49, 0xA9, 0x66],
            &[
                elem(&[0x2A, 0xD7, 0xB1], &ebml_uint(1_000_000)),
                elem(&[0x44, 0x89], &ebml_float(5_000.0)),
                // symphonia fails the whole read without these two.
                elem(&[0x4D, 0x80], b"butterfly-speak-test"),
                elem(&[0x57, 0x41], b"butterfly-speak-test"),
            ]
            .concat(),
        );
        let audio = elem(
            &[0xE1],
            &[
                elem(&[0xB5], &ebml_float(48_000.0)), // SamplingFrequency
                elem(&[0x9F], &ebml_uint(2)),         // Channels
            ]
            .concat(),
        );
        let mut track_children = vec![
            elem(&[0xD7], &ebml_uint(1)),                // TrackNumber
            elem(&[0x73, 0xC5], &ebml_uint(1)),          // TrackUID
            elem(&[0x83], &ebml_uint(track_type)),       // TrackType
            elem(&[0x86], codec_id),                     // CodecID
        ];
        if track_type == 2 {
            track_children.push(audio);
        }
        let track = elem(&[0xAE], &track_children.concat());
        let tracks = elem(&[0x16, 0x54, 0xAE, 0x6B], &track);
        let segment = elem(&[0x18, 0x53, 0x80, 0x67], &[info, tracks].concat());
        [header, segment].concat()
    }

    /// A uniquely named file under the OS temp dir, deleted on drop so a
    /// failing assertion cannot leave litter behind.
    struct TempPath(std::path::PathBuf);

    impl TempPath {
        fn with(name: &str, bytes: &[u8]) -> TempPath {
            let p = TempPath::empty(name);
            let mut f = File::create(&p.0).expect("create the temp fixture");
            f.write_all(bytes).expect("write the temp fixture");
            p
        }

        fn empty(name: &str) -> TempPath {
            let unique = format!(
                "bs-decode-{}-{:?}-{name}",
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .expect("system clock is after the epoch")
                    .as_nanos(),
                std::thread::current().id()
            );
            TempPath(std::env::temp_dir().join(unique))
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempPath {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    /// The header fields a reader actually acts on.
    struct Header {
        format: u16,
        channels: u16,
        rate: u32,
        byte_rate: u32,
        block_align: u16,
        bits: u16,
        data_len: u32,
        riff_len: u32,
    }

    fn read_header(bytes: &[u8]) -> Header {
        let u16_at = |o: usize| u16::from_le_bytes([bytes[o], bytes[o + 1]]);
        let u32_at = |o: usize| {
            u32::from_le_bytes([bytes[o], bytes[o + 1], bytes[o + 2], bytes[o + 3]])
        };
        assert_eq!(&bytes[0..4], b"RIFF");
        assert_eq!(&bytes[8..12], b"WAVE");
        assert_eq!(&bytes[12..16], b"fmt ");
        assert_eq!(&bytes[36..40], b"data");
        Header {
            format: u16_at(20),
            channels: u16_at(22),
            rate: u32_at(24),
            byte_rate: u32_at(28),
            block_align: u16_at(32),
            bits: u16_at(34),
            data_len: u32_at(40),
            riff_len: u32_at(4),
        }
    }

    /// [`to_wav_16k_mono`] for a test that is not about cancellation.
    fn convert_file(src: &Path, dst: &Path) -> Result<Converted, DecodeError> {
        to_wav_16k_mono(src, dst, &|| false)
    }

    fn samples_of(bytes: &[u8]) -> Vec<i16> {
        bytes[44..]
            .chunks_exact(2)
            .map(|c| i16::from_le_bytes([c[0], c[1]]))
            .collect()
    }

    // --------------------------------------------------------------- tests --

    /// The whole point of the module. 48 kHz stereo is the shape a screen
    /// recorder or a phone produces, and the measurement in the module doc
    /// says Sarvam returns nothing at all for it.
    #[test]
    fn a_48_khz_stereo_wav_becomes_a_16_khz_mono_wav() {
        let src = TempPath::with("stereo.wav", &wav_bytes(48_000, 2, 24_000, tone(48_000)));
        let dst = TempPath::empty("out.wav");

        let converted = convert_file(src.path(), dst.path()).expect("a real WAV must convert");
        assert_eq!(converted.source_rate, 48_000);
        assert_eq!(converted.source_channels, 2);
        assert!(converted.resampled);

        let out = std::fs::read(dst.path()).expect("the conversion must leave a file");
        let h = read_header(&out);
        assert_eq!(h.format, 1, "WAVE_FORMAT_PCM");
        assert_eq!(h.channels, 1);
        assert_eq!(h.rate, 16_000);
        assert_eq!(h.bits, 16);
        assert_eq!(h.block_align, 2);
        assert_eq!(h.byte_rate, 32_000);

        // The header's lengths must describe the bytes that are actually
        // there — the patch-back step is the only thing that makes them.
        assert_eq!(h.data_len as usize, out.len() - 44);
        assert_eq!(h.riff_len as usize, out.len() - 8);
        assert_eq!(converted.frames * 2, u64::from(h.data_len));

        // 24 000 frames at 48 kHz is 0.5 s, so exactly 8000 at 16 kHz — not
        // "about". An FFT resampler holds back a whole window of input, and
        // before the flush at the end of `convert` this came out at 7320:
        // the last 42 ms of every resampled import, silently dropped. Exact
        // is the only assertion that catches that coming back.
        assert_eq!(converted.frames, 8_000);
        assert!(
            (converted.duration_s - 0.5).abs() < 1e-9,
            "expected 0.5 s, got {} s",
            converted.duration_s
        );

        // And symphonia itself has to agree it is a WAV, since Sarvam's
        // reader is the one that matters and this is the closest stand-in.
        let probed = super::super::probe::probe(dst.path()).expect("the output must probe");
        assert_eq!(probed.container, "wave");
        assert_eq!(probed.sample_rate, Some(16_000));
    }

    /// 44.1 kHz is not a whole multiple of 16 kHz, so the length arithmetic
    /// has nowhere to hide. One second in must still be one second out.
    #[test]
    fn a_44_1_khz_source_keeps_its_exact_length_through_the_resampler() {
        let src = TempPath::with("cd-rate.wav", &wav_bytes(44_100, 1, 44_100, tone(44_100)));
        let dst = TempPath::empty("out.wav");

        let converted = convert_file(src.path(), dst.path()).expect("must convert");
        assert!(converted.resampled);
        assert_eq!(converted.frames, 16_000, "one second in, one second out");
    }

    /// Already the target shape: the resampler must not run, because running
    /// it would resample 16 kHz to 16 kHz and lose a little of the recording
    /// to the FFT for nothing.
    #[test]
    fn a_16_khz_mono_wav_passes_through_without_being_resampled() {
        let input = wav_bytes(16_000, 1, 4_000, tone(16_000));
        let src = TempPath::with("already.wav", &input);
        let dst = TempPath::empty("out.wav");

        let converted = convert_file(src.path(), dst.path()).expect("must convert");
        assert!(!converted.resampled, "16 kHz mono must not be resampled");
        assert_eq!(converted.frames, 4_000, "every frame must survive");
        assert_eq!(converted.source_rate, 16_000);
        assert_eq!(converted.source_channels, 1);

        // Bit-exact, not merely close: `to_i16` scales by the same 32768
        // symphonia divided by, so a 16-bit source round-trips unchanged.
        let out = std::fs::read(dst.path()).expect("read the output");
        assert_eq!(samples_of(&out), samples_of(&input));
    }

    /// Averaging, not channel-0. Two channels that differ must both reach the
    /// mix — a stereo interview with one speaker per channel is exactly the
    /// recording an import gets handed.
    #[test]
    fn two_channels_are_averaged_rather_than_one_being_dropped() {
        // Left is a constant 1000, right a constant -600; the mean is 200.
        let src = TempPath::with(
            "split.wav",
            &wav_bytes(16_000, 2, 1_000, |_, c| if c == 0 { 1_000 } else { -600 }),
        );
        let dst = TempPath::empty("out.wav");

        let converted = convert_file(src.path(), dst.path()).expect("must convert");
        assert_eq!(converted.frames, 1_000);
        let out = std::fs::read(dst.path()).expect("read the output");
        let samples = samples_of(&out);
        assert!(
            samples.iter().all(|s| (*s - 200).abs() <= 1),
            "every frame must be the mean of the two channels, got {:?}",
            &samples[..4]
        );
    }

    /// The permanent gap, stated as a test so it cannot regress into a crash
    /// or a silent empty note. If symphonia ever ships an Opus decoder this
    /// fails, which is the right way to find out.
    #[test]
    fn an_opus_track_is_refused_by_name_and_told_to_convert_to_wav() {
        let src = TempPath::with("browser.webm", &webm_bytes(b"A_OPUS", 2));
        let dst = TempPath::empty("out.wav");

        let err = convert_file(src.path(), dst.path()).expect_err("Opus cannot be decoded");
        assert_eq!(err, DecodeError::UnsupportedCodec { codec: "Opus" });
        let message = err.user_message();
        assert!(message.contains("Opus"), "{message}");
        assert!(message.contains("WAV"), "{message}");
        // Never MP3: it uploads fine and comes back empty (module doc).
        assert!(!message.contains("MP3"), "{message}");
        assert!(
            !dst.path().exists(),
            "a refused conversion must leave no file behind"
        );
    }

    #[test]
    fn a_container_with_no_audio_track_is_refused_before_any_decoding() {
        let src = TempPath::with("silent-movie.webm", &webm_bytes(b"V_VP8", 1));
        let dst = TempPath::empty("out.wav");
        assert_eq!(
            convert_file(src.path(), dst.path()),
            Err(DecodeError::NoAudioTrack)
        );
    }

    /// The same input `probe` exists to refuse — 6 kB of ASCII wearing a
    /// `.wav` extension. It must never reach a decoder either.
    #[test]
    fn plain_text_wearing_a_wav_extension_is_not_media() {
        let text = "this is plain text, not audio, ".repeat(200);
        let src = TempPath::with("not-audio.wav", text.as_bytes());
        let dst = TempPath::empty("out.wav");
        assert_eq!(
            convert_file(src.path(), dst.path()),
            Err(DecodeError::NotMedia)
        );
        assert!(!dst.path().exists());
    }

    /// symphonia hands on whatever rate a WAV header claims, and a rate that
    /// shares no factor with 16 kHz makes the resampler's FFT as long as the
    /// rate itself: a header claiming 4 294 967 291 Hz asked for tens of
    /// gigabytes and aborted the app. A rate no recording has is refused
    /// before the resampler is built, and leaves no file behind.
    #[test]
    fn a_source_at_an_impossible_sample_rate_is_refused_before_the_resampler() {
        for rate in [500u32, 4_294_967_291] {
            let mut bytes = wav_bytes(16_000, 1, 1_600, tone(16_000));
            bytes[24..28].copy_from_slice(&rate.to_le_bytes());
            let src = TempPath::with("odd-rate.wav", &bytes);
            let dst = TempPath::empty("out.wav");
            assert_eq!(
                convert_file(src.path(), dst.path()),
                Err(DecodeError::NotMedia),
                "{rate} Hz"
            );
            assert!(!dst.path().exists(), "{rate} Hz");
        }
    }

    #[test]
    fn a_missing_source_is_unreadable_rather_than_a_crash() {
        let src = TempPath::empty("does-not-exist.wav");
        let dst = TempPath::empty("out.wav");
        assert_eq!(
            convert_file(src.path(), dst.path()),
            Err(DecodeError::Unreadable)
        );
    }

    /// A container that reads but decodes to nothing must not produce a
    /// zero-length WAV — that file would upload, "succeed", and become an
    /// empty note.
    #[test]
    fn a_container_that_decodes_to_nothing_fails_rather_than_writing_an_empty_wav() {
        let src = TempPath::with("silence.wav", &wav_bytes(16_000, 1, 0, |_, _| 0));
        let dst = TempPath::empty("out.wav");
        assert_eq!(
            convert_file(src.path(), dst.path()),
            Err(DecodeError::NoAudio)
        );
        assert!(
            !dst.path().exists(),
            "an empty conversion must leave no file to upload"
        );
    }

    /// Cancellation is checked before the file is even opened, so a run
    /// cancelled while the item was queued does no work at all.
    #[test]
    fn an_already_cancelled_conversion_does_nothing() {
        let src = TempPath::with("talk.wav", &wav_bytes(48_000, 2, 48_000, tone(48_000)));
        let dst = TempPath::empty("out.wav");
        assert_eq!(
            to_wav_16k_mono(src.path(), dst.path(), &|| true),
            Err(DecodeError::Cancelled)
        );
        assert!(!dst.path().exists());
    }

    /// ...and once it has started, so a two-hour file does not have to finish
    /// before the Cancel button means anything.
    #[test]
    fn a_conversion_cancelled_partway_stops_and_leaves_no_file() {
        let src = TempPath::with("long.wav", &wav_bytes(48_000, 2, 240_000, tone(48_000)));
        let dst = TempPath::empty("out.wav");
        let packets = std::cell::Cell::new(0u32);
        let result = to_wav_16k_mono(src.path(), dst.path(), &|| {
            packets.set(packets.get() + 1);
            packets.get() > 3
        });
        assert_eq!(result, Err(DecodeError::Cancelled));
        assert!(packets.get() > 1, "the stop hook must be polled per packet");
        assert!(!dst.path().exists());
    }

    #[test]
    fn every_refusal_says_something_different_and_names_no_path() {
        let all = [
            DecodeError::Unreadable,
            DecodeError::NotMedia,
            DecodeError::NoAudioTrack,
            DecodeError::UnsupportedCodec { codec: "Opus" },
            DecodeError::NoAudio,
            DecodeError::ConversionFailed,
            DecodeError::Cancelled,
        ];
        let messages: std::collections::HashSet<String> =
            all.iter().map(|e| e.user_message()).collect();
        assert_eq!(messages.len(), all.len());
        for e in &all {
            let m = e.user_message();
            assert!(!m.is_empty());
            assert!(
                !m.contains('\\') && !m.contains(":/"),
                "a refusal must not carry a path: {m}"
            );
        }
    }

    /// The fallback has to keep the sentence grammatical, because a codec
    /// nobody named is the case nobody will be looking at when it happens.
    #[test]
    fn an_unnamed_codec_still_produces_a_readable_sentence() {
        assert_eq!(codec_name(wk::CODEC_ID_OPUS), "Opus");
        assert_eq!(codec_name(wk::CODEC_ID_WMA), "Windows Media Audio");
        let unknown = codec_name(wk::CODEC_ID_SBC);
        let message = DecodeError::UnsupportedCodec { codec: unknown }.user_message();
        assert!(
            message.starts_with("Butterfly Speak can't decode this audio"),
            "{message}"
        );
    }

    /// The scale is 32768 because that is symphonia's own divisor; the clamp
    /// is only there for float sources that reach the rails.
    #[test]
    fn sixteen_bit_samples_round_trip_through_f32_unchanged() {
        for x in [i16::MIN, -32_767, -1_000, -1, 0, 1, 1_000, 32_766, i16::MAX] {
            assert_eq!(to_i16(f32::from(x) / 32_768.0), x, "{x} did not round-trip");
        }
        assert_eq!(to_i16(1.0), i16::MAX, "a float +1.0 must clamp, not wrap");
        assert_eq!(to_i16(-1.0), i16::MIN);
        assert_eq!(to_i16(9.0), i16::MAX);
        assert_eq!(to_i16(-9.0), i16::MIN);
    }

    /// The header the placeholder is patched into, pinned literally: Sarvam's
    /// reader gets exactly these bytes, and the measurement says the fields
    /// are the difference between a transcript and an empty note.
    #[test]
    fn the_written_header_is_16_khz_mono_16_bit_pcm() {
        let h = read_header(&wav_header(1_000));
        assert_eq!(h.format, 1);
        assert_eq!(h.channels, 1);
        assert_eq!(h.rate, 16_000);
        assert_eq!(h.bits, 16);
        assert_eq!(h.block_align, 2);
        assert_eq!(h.byte_rate, 32_000);
        assert_eq!(h.data_len, 1_000);
        assert_eq!(h.riff_len, 1_036);
    }
}
