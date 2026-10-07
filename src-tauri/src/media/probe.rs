//! "Is this actually audio, and how long is it?" — the client-side gate every
//! file import passes through before a single byte reaches Sarvam.
//!
//! A file passes three checks, cheapest first. Its extension has to be one of
//! [`ACCEPTED_EXTENSIONS`]; a container reader has to parse its headers and
//! find an audio track; and [`check_limits`] has to find it within the size
//! and length ceilings. A file that fails any of them is declined by name
//! with a [`ProbeError`] sentence, never dropped silently.
//!
//! # Why the probe is required
//!
//! A live test sent 6 kB of plain ASCII text named `not-audio.wav` through
//! the whole Sarvam batch job lifecycle. Sarvam did not reject it.
//! The job reported `job_state: "Completed"`, `successful_files_count: 1`,
//! per-file `state: "Success"`, and returned a transcript: the single word
//! `"Hello"`, with timestamps `0.0`–`0.2`.
//!
//! So the server does no format validation a client may lean on. Send it
//! something that is not audio and it will invent a plausible-looking
//! transcript, bill for it, and report success. **Nothing but this module
//! stands between a user who picked the wrong file and a fabricated note.**
//! That is why a probe failure declines the import instead of shrugging and
//! uploading anyway, and why the extension check is a cheap pre-filter rather
//! than the decision — the decision is whether a container reader can
//! actually parse the bytes.
//!
//! # What it deliberately does not do
//!
//! No decoding. [`probe`] instantiates a `FormatReader` and reads the track
//! headers, then drops it; it never pulls a packet. A 500 MB file costs the
//! same header parse a 500 kB one does.

use std::path::Path;
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::{FormatOptions, TrackType};
use symphonia::core::io::{MediaSourceStream, MediaSourceStreamOptions};
use symphonia::core::meta::MetadataOptions;

/// The extensions an import accepts, lowercase and in alphabetical order.
/// They feed the file dialog's filter, the drop check and the first step of
/// [`probe`].
///
/// An extension is here only when both of these hold:
/// - Sarvam's speech-to-text reference lists the format. It names WAV, MP3,
///   AAC, AIFF, OGG, OPUS, FLAC, MP4/M4A, AMR, WMA, WebM and raw PCM.
/// - A symphonia reader enabled in `src-tauri/Cargo.toml` can open the
///   container. The enabled readers are `wav`, `mp3`, `aac` (ADTS), `flac`,
///   `ogg`, `mkv` and `isomp4`.
///
/// The extension for each container comes from its registration: `.wav` for
/// RIFF WAVE, `.mp3` for `audio/mpeg` (RFC 3003), `.aac` for ADTS
/// (`audio/aac`), `.flac` for `audio/flac` (RFC 9639), `.ogg` and `.oga` for
/// `audio/ogg` (RFC 5334), `.opus` for Ogg Opus (RFC 7845), `.mp4` for the
/// MP4 file format (RFC 4337), and `.webm` from the WebM container
/// specification. `.m4a` is here because Sarvam names M4A itself. Other
/// registered aliases (`.adts`, `.mpg4`, `.spx`) are left out: nobody hands
/// an app a recording named that way, and `.spx` is Speex, which Sarvam does
/// not list.
///
/// `opus` stays although no Opus file can be transcribed: symphonia has no
/// Opus decoder, so the conversion refuses it by name with a sentence that
/// says what to do instead. Accepting the extension is what lets that
/// sentence reach the user; refusing it here would only say the extension is
/// wrong, which it is not.
///
/// Left out on purpose:
/// - AMR and WMA: Sarvam lists them, but no reader in this build can open
///   them, so the probe could not check that such a file is really audio.
/// - AIFF: symphonia has a reader, but it is not enabled in this build.
/// - raw PCM: it has no container at all, so nothing can verify it, and
///   Sarvam needs its codec named separately.
/// - `.mov` and `.mkv`: the readers could open them, but Sarvam does not list
///   either format.
pub const ACCEPTED_EXTENSIONS: &[&str] = &[
    "aac", "flac", "m4a", "mp3", "mp4", "oga", "ogg", "opus", "wav", "webm",
];

/// The largest file an import accepts by default: 1.5 GiB.
///
/// Sarvam publishes no size limit, so this is the app's own. The duration
/// ceiling does the real limiting for any file that states its length; the
/// size ceiling bounds how much has to be read and decoded, and it is the
/// only ceiling left for a file whose length is unknown.
///
/// Sized so a recording of the full [`MAX_IMPORT_DURATION_S`] fits in every
/// audio format people record with:
/// - 16-bit stereo WAV at 48 kHz, the heaviest common setting:
///   48 000 x 2 channels x 2 bytes = 192 000 B/s, x 7 200 s = 1 382 400 000
///   bytes (1.29 GiB). The ceiling leaves about 16% on top for header and
///   metadata chunks.
/// - 44.1 kHz stereo WAV is 1.27 GB, FLAC of either is roughly half that, and
///   320 kbps MP3 is 288 MB, so all of them fit.
/// - Video does not have to fit at full length. 1.5 GiB over two hours is
///   about 1.8 Mbit/s, which holds a typical screen-share or meeting
///   recording, but a phone camera at 15-20 Mbit/s fills it in about twelve
///   minutes; export the audio for longer ones.
///
/// What goes on the wire is the converted 16 kHz mono WAV, 32 000 B/s, so a
/// two-hour import uploads about 230 MB whatever the source weighed. At
/// `batch_job`'s 128 KiB/s floor rate that is a budget of about 30 minutes.
/// For a file that states its length, the source size only changes decode
/// time: reading 1.5 GiB of PCM from disk takes seconds, and a compressed
/// file this large runs far past two hours, so the duration ceiling refuses
/// it first.
///
/// A file whose length is unknown gets no such protection. A header-less
/// 128 kbps MP3 at this ceiling holds about 26 hours of audio, which
/// converts to about 3 GB of WAV, and the upload reads the converted file
/// whole. Only this size check stands in its way before the conversion.
pub const MAX_IMPORT_BYTES: u64 = 3 * 512 * 1024 * 1024;

/// Refuse anything longer than this. **The real ceiling is UNVERIFIED.**
/// Sarvam's current guide pages say "up to 2 hours" per file, but the
/// changelog shows the Batch API launching at 60 minutes and a July 2025 SDK
/// note still saying "files up to 1 hour long", with no entry retracting
/// either. The live tests could not settle it — the longest file sent was
/// 82 s (see [`crate::sarvam::batch_job`]'s module doc for what those runs
/// did establish), which proves nothing about the ceiling.
///
/// So this takes the published figure. If the real ceiling is lower, a long
/// upload is accepted here and then fails at Sarvam; if it is higher, a long
/// recording Sarvam would have taken is refused here. There is no setting
/// that moves it: change this constant once the ceiling is known.
pub const MAX_IMPORT_DURATION_S: f64 = 120.0 * 60.0;

/// The sample rates a recording can have, in Hz.
///
/// symphonia takes a container's stated rate on trust, and the conversion
/// sizes its resampler from it: a rate that shares no factor with 16 kHz
/// makes an FFT window as long as the rate itself, so a damaged header
/// claiming billions of hertz would ask for tens of gigabytes and abort the
/// app. 1 kHz is below any speech codec's rate; 384 kHz is the highest
/// common recording rate.
pub const SAMPLE_RATES_HZ: std::ops::RangeInclusive<u32> = 1_000..=384_000;

/// The two client-side ceilings, as a value rather than two constants read
/// inside [`check_limits`], so it stays a pure function whose boundary cases
/// the tests can set directly. The import always passes the default.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ImportLimits {
    pub max_bytes: u64,
    pub max_duration_s: f64,
}

impl Default for ImportLimits {
    fn default() -> Self {
        ImportLimits {
            max_bytes: MAX_IMPORT_BYTES,
            max_duration_s: MAX_IMPORT_DURATION_S,
        }
    }
}

/// What a container's headers said about it. Nothing here is decoded.
#[derive(Clone, Debug, PartialEq)]
pub struct Probe {
    /// Playing time in seconds, when the container states it.
    ///
    /// `Option`, not `f64`, because "I could read this file but it does not
    /// say how long it is" is a real and reasonably common answer — a VBR MP3
    /// with no Xing/VBRI header, a WebM written by a live recorder that never
    /// went back to patch its Duration element. Collapsing that into `0.0`
    /// would make an unknown length silently pass the duration ceiling while
    /// looking like a measured zero; collapsing it into an error would refuse
    /// files Sarvam would have transcribed fine. See [`check_limits`] for
    /// which of those this codebase picked.
    pub duration_s: Option<f64>,
    /// symphonia's own short name for the container that matched. Note it is
    /// the *reader's* name, not the extension, and the two differ: a `.wav`
    /// reports `"wave"` and a `.webm` reports `"matroska"`. The full set this
    /// build can report is `wave`, `mp3`, `flac`, `ogg`, `matroska`,
    /// `isomp4`, `aac`.
    ///
    /// Diagnostic only — it is never sent to Sarvam, which auto-detects
    /// everything except raw PCM.
    pub container: &'static str,
    /// Sample rate in Hz, when the track headers state it. Diagnostic only,
    /// for the same reason: Sarvam's 16 kHz figure is a recommendation for
    /// compressed formats and a hard requirement only for raw PCM, which
    /// [`ACCEPTED_EXTENSIONS`] does not accept.
    pub sample_rate: Option<u32>,
}

/// Every way an import can be declined before it starts. Each carries a
/// user-facing sentence, in the same style as
/// [`crate::sarvam::net_error::NetFailure::user_message`]: say what happened
/// and what it means, never a code or a library's error text.
#[derive(Clone, Debug, PartialEq)]
pub enum ProbeError {
    /// The filename's extension is not one the app accepts.
    UnsupportedExtension { ext: String },
    /// The file could not be opened or read at all.
    Unreadable,
    /// No registered container reader recognized the bytes — the case the
    /// module doc is about. Includes an empty file and a text file with an
    /// audio extension.
    NotMedia,
    /// A container was recognized but it has no audio track (a video-only
    /// MP4, most plausibly).
    NoAudioTrack,
    /// The file is larger than [`ImportLimits::max_bytes`].
    TooLarge { bytes: u64, limit: u64 },
    /// The file is longer than [`ImportLimits::max_duration_s`].
    TooLong { seconds: f64, limit_s: f64 },
}

impl ProbeError {
    pub fn user_message(&self) -> String {
        match self {
            ProbeError::UnsupportedExtension { ext } if ext.is_empty() => {
                "That file's name has no extension, so there's no telling what kind of recording \
                 it is — a WAV, MP3, M4A or FLAC copy will work"
                    .to_string()
            }
            ProbeError::UnsupportedExtension { ext } => format!(
                "Files ending in .{ext} aren't a format Butterfly Speak imports — a WAV, MP3, M4A \
                 or FLAC copy of the recording will work"
            ),
            ProbeError::Unreadable => {
                "Couldn't open that file — it may have been moved, renamed or locked by another app"
                    .to_string()
            }
            ProbeError::NotMedia => {
                "That file doesn't look like audio Butterfly Speak can read — it may be damaged, or \
                 have the wrong extension"
                    .to_string()
            }
            ProbeError::NoAudioTrack => {
                "That file has no audio track — only the video, or nothing at all".to_string()
            }
            ProbeError::TooLarge { bytes, limit } => {
                let unit = SizeUnit::for_bytes(*limit);
                let cap = format_size(*limit, unit);
                format!(
                    "That file is {} and the limit is {cap} — try a shorter recording",
                    size_beside_limit(*bytes, unit, &cap)
                )
            }
            ProbeError::TooLong { seconds, limit_s } => {
                let cap = format_duration(*limit_s);
                format!(
                    "That recording is {} long and the limit is {cap} — try a shorter recording",
                    duration_beside_limit(*seconds, &cap)
                )
            }
        }
    }
}

/// The unit a size is written in. Binary steps with the familiar labels,
/// the way File Explorer's Size column counts, so the number in a refusal is
/// the number the user sees next to the file.
#[derive(Clone, Copy, Debug, PartialEq)]
enum SizeUnit {
    Bytes,
    Kb,
    Mb,
    Gb,
}

impl SizeUnit {
    const KIB: u64 = 1024;
    const MIB: u64 = 1024 * 1024;
    const GIB: u64 = 1024 * 1024 * 1024;

    /// The largest unit `bytes` reaches at least one of.
    fn for_bytes(bytes: u64) -> SizeUnit {
        match bytes {
            b if b >= Self::GIB => SizeUnit::Gb,
            b if b >= Self::MIB => SizeUnit::Mb,
            b if b >= Self::KIB => SizeUnit::Kb,
            _ => SizeUnit::Bytes,
        }
    }
}

/// `bytes` in `unit`, to three significant figures (at least the whole
/// number), trailing zeros dropped: "512 bytes", "1.5 KB", "12.3 MB",
/// "1.5 GB".
///
/// The unit is a parameter so a refusal can write the file's size and the
/// limit in the same one. A 1.52 GB file against a 1.5 GB limit reads as
/// two numbers a person can compare, not "1.52 GB" against "1536 MB".
fn format_size(bytes: u64, unit: SizeUnit) -> String {
    format_size_finer(bytes, unit, 0)
}

/// [`format_size`] with `extra` more decimal places than it would use.
fn format_size_finer(bytes: u64, unit: SizeUnit, extra: usize) -> String {
    let (divisor, label) = match unit {
        SizeUnit::Bytes => return format!("{bytes} bytes"),
        SizeUnit::Kb => (SizeUnit::KIB, "KB"),
        SizeUnit::Mb => (SizeUnit::MIB, "MB"),
        SizeUnit::Gb => (SizeUnit::GIB, "GB"),
    };
    let value = bytes as f64 / divisor as f64;
    let decimals = extra
        + if value < 10.0 {
            2
        } else if value < 100.0 {
            1
        } else {
            0
        };
    let mut text = format!("{value:.decimals$}");
    if text.contains('.') {
        text = text.trim_end_matches('0').trim_end_matches('.').to_string();
    }
    format!("{text} {label}")
}

/// A refused file's size, written so it never reads the same as the limit
/// it broke. The usual precision can round a file just over the limit onto
/// the limit's own figure ("1.5 GB" against "1.5 GB"), so this adds a
/// decimal place at a time, up to three, until the two differ, and past that
/// says "just over".
fn size_beside_limit(bytes: u64, unit: SizeUnit, cap: &str) -> String {
    (0..=3)
        .map(|extra| format_size_finer(bytes, unit, extra))
        .find(|size| size != cap)
        .unwrap_or_else(|| format!("just over {cap}"))
}

/// Whole minutes and hours, never a bare second count: "7200 seconds" is not
/// a thing a person recognizes as the two-hour limit they just hit.
fn format_duration(seconds: f64) -> String {
    let total_min = (seconds / 60.0).round() as i64;
    if total_min < 60 {
        let m = total_min.max(1);
        return format!("{m} min");
    }
    let h = total_min / 60;
    let m = total_min % 60;
    if m == 0 {
        format!("{h} hr")
    } else {
        format!("{h} hr {m} min")
    }
}

/// `seconds` to the whole second, zero parts left out: "2 hr 20 sec",
/// "30 min 5 sec".
fn format_duration_to_the_second(seconds: f64) -> String {
    let total = seconds.round() as i64;
    let parts = [(total / 3600, "hr"), (total % 3600 / 60, "min"), (total % 60, "sec")];
    parts
        .iter()
        .filter(|(n, _)| *n > 0)
        .map(|(n, unit)| format!("{n} {unit}"))
        .collect::<Vec<_>>()
        .join(" ")
}

/// A refused recording's length, written so it never reads the same as the
/// limit it broke, the way [`size_beside_limit`] does for a size: to the
/// minute, then to the second, and past that "just over".
fn duration_beside_limit(seconds: f64, cap: &str) -> String {
    [format_duration(seconds), format_duration_to_the_second(seconds)]
        .into_iter()
        .find(|length| length != cap)
        .unwrap_or_else(|| format!("just over {cap}"))
}

/// Whether `path`'s extension is one of [`ACCEPTED_EXTENSIONS`], ignoring
/// case. A path with no extension is not accepted. This is the cheap check
/// the drop and the queue use; it says nothing about the file's contents,
/// which [`probe`] checks.
pub fn is_accepted_extension(path: &Path) -> bool {
    extension_of(path).is_some_and(|e| ACCEPTED_EXTENSIONS.contains(&e.as_str()))
}

fn extension_of(path: &Path) -> Option<String> {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
}

/// Read `path`'s container headers.
///
/// Errors are values, never panics: every input to this function is a path a
/// user picked, so "the file is nonsense" is an ordinary Tuesday and has to
/// arrive as a [`ProbeError`] the UI can phrase. symphonia's readers are
/// parsing untrusted bytes here, which is also why the probe depth stays at
/// the library default (1 MB) rather than being raised — a malformed file
/// gets a bounded scan and a `NotMedia`, not an unbounded one.
pub fn probe(path: &Path) -> Result<Probe, ProbeError> {
    let ext = extension_of(path).unwrap_or_default();
    if !ACCEPTED_EXTENSIONS.contains(&ext.as_str()) {
        return Err(ProbeError::UnsupportedExtension { ext });
    }

    let file = std::fs::File::open(path).map_err(|e| {
        // Path never logged: a user's file path is as personal as the
        // recording in it. The io kind is a fieldless enum.
        tracing::warn!(kind = ?e.kind(), "import probe could not open the file");
        ProbeError::Unreadable
    })?;
    let mss = MediaSourceStream::new(Box::new(file), MediaSourceStreamOptions::default());

    // The extension is a hint, not a claim the reader has to honour — a
    // `.wav` that is really an MP3 still probes as an MP3, which is the whole
    // point of sniffing the bytes rather than trusting the name.
    let mut hint = Hint::new();
    hint.with_extension(&ext);

    let reader = symphonia::default::get_probe()
        .probe(
            &hint,
            mss,
            FormatOptions::default(),
            MetadataOptions::default(),
        )
        .map_err(|e| {
            // `{e}` is safe here in a way it is not for a serde error: every
            // `symphonia::core::errors::Error` payload is a `&'static str`
            // written by the library or an `io::Error`, never a slice of the
            // file's own bytes.
            tracing::warn!("import probe found no readable container: {e}");
            ProbeError::NotMedia
        })?;

    let track = reader
        .default_track(TrackType::Audio)
        .or_else(|| {
            reader
                .tracks()
                .iter()
                .find(|t| t.codec_params.as_ref().is_some_and(|p| p.is_audio()))
        })
        .ok_or(ProbeError::NoAudioTrack)?;

    let sample_rate = track
        .codec_params
        .as_ref()
        .and_then(|p| p.audio())
        .and_then(|a| a.sample_rate);
    // A stated 0 passes: an MP4 sample entry cannot hold a rate above
    // 65 535 Hz and holds 0 instead, and the conversion checks the rate the
    // decoder reports before it builds anything from it.
    if let Some(rate) = sample_rate.filter(|r| *r != 0 && !SAMPLE_RATES_HZ.contains(r)) {
        tracing::warn!(rate, "import probe found a sample rate no recording has");
        return Err(ProbeError::NotMedia);
    }

    // Three sources, most specific first.
    //
    // 1. The track's own stated duration, in timebase units. Preferred over
    //    `num_frames` on the library's own advice: a track's timebase is not
    //    always the reciprocal of its frame rate, so the two disagree
    //    slightly, and the container's figure is the one a player displays.
    // 2. Playable frames over the sample rate.
    // 3. The **media**'s duration rather than the track's.
    //
    // Step 3 is not belt-and-braces, it is the only thing that makes WebM
    // work. symphonia's Matroska demuxer populates `MediaInfo` and *never*
    // touches `Track::duration` or `Track::num_frames`
    // (`symphonia-format-mkv-0.6.1/src/demuxer.rs:322-329` builds the
    // `MediaInfo` from the segment's own Duration element; nothing in that
    // file assigns either track field). Without this branch every single
    // `.webm` probes as "length unknown" — and since `check_limits` lets an
    // unknown length through, a three-hour screen recording would sail past
    // the ceiling, be uploaded, be billed, and then be waited on for the
    // full 30-minute deadline.
    let media = reader.media_info();
    let duration_s = track
        .time_base
        .zip(track.duration)
        .and_then(|(tb, d)| tb.calc_duration(d))
        .map(|t| t.as_secs_f64())
        .or_else(|| {
            let frames = track.num_frames?;
            let rate = sample_rate?;
            (rate > 0).then(|| frames as f64 / rate as f64)
        })
        .or_else(|| {
            media
                .time_base
                .zip(media.duration)
                .and_then(|(tb, d)| tb.calc_duration(d))
                .map(|t| t.as_secs_f64())
        })
        .filter(|s| s.is_finite() && *s >= 0.0);

    let container = reader.format_info().short_name;
    tracing::debug!(
        container,
        sample_rate,
        has_duration = duration_s.is_some(),
        "import probe read a container"
    );
    Ok(Probe {
        duration_s,
        container,
        sample_rate,
    })
}

/// Apply the two client-side ceilings to an already-probed file.
///
/// **An unknown duration passes.** That is a decision, not an oversight: the
/// duration ceiling is itself unverified (see [`MAX_IMPORT_DURATION_S`]), so
/// refusing a file because the app could not measure it would be refusing on
/// the strength of a number the app is not sure about in the first place. The size
/// ceiling still applies, and a file large enough to be worth worrying about
/// will fail that one (see [`MAX_IMPORT_BYTES`]). The unknown is logged so a
/// support report can say so.
pub fn check_limits(p: &Probe, size_bytes: u64, limits: &ImportLimits) -> Result<(), ProbeError> {
    if size_bytes > limits.max_bytes {
        return Err(ProbeError::TooLarge {
            bytes: size_bytes,
            limit: limits.max_bytes,
        });
    }
    match p.duration_s {
        Some(s) if s > limits.max_duration_s => Err(ProbeError::TooLong {
            seconds: s,
            limit_s: limits.max_duration_s,
        }),
        Some(_) => Ok(()),
        None => {
            tracing::info!(
                container = p.container,
                "container states no duration; the length ceiling could not be applied"
            );
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    // ---- a minimal WebM, hand-built ------------------------------------
    //
    // Small enough to synthesize because the demuxer's init only needs the
    // EBML header, a Segment, an Info and a Tracks
    // (`symphonia-format-mkv-0.6.1/src/demuxer.rs:86-160` breaks out of its
    // top-level scan at the first Cluster, and never requires one). No
    // encoder and no checked-in binary blob, so the fixture cannot drift
    // from what the test says it is.

    /// An EBML element size, always in the 8-byte form: a leading `0x01`
    /// marker byte then 7 big-endian length bytes. Valid for any length this
    /// test produces, and it avoids a width calculation the fixture would
    /// otherwise have to get right for every element.
    fn ebml_size(n: usize) -> Vec<u8> {
        let mut v = vec![0x01u8];
        v.extend_from_slice(&(n as u64).to_be_bytes()[1..]);
        v
    }

    /// One element: its ID bytes (EBML IDs carry their own width), its size,
    /// then its payload.
    fn elem(id: &[u8], payload: &[u8]) -> Vec<u8> {
        let mut v = id.to_vec();
        v.extend_from_slice(&ebml_size(payload.len()));
        v.extend_from_slice(payload);
        v
    }

    /// EBML unsigned integers may be 1-8 bytes; 8 is always legal.
    fn ebml_uint(n: u64) -> Vec<u8> {
        n.to_be_bytes().to_vec()
    }

    /// EBML floats must be exactly 4 or 8 bytes.
    fn ebml_float(x: f64) -> Vec<u8> {
        x.to_be_bytes().to_vec()
    }

    /// A WebM whose Segment>Info states a duration of
    /// `duration_ticks * timecode_scale_ns` nanoseconds, carrying one Opus
    /// audio track. The duration deliberately lives ONLY here — on the
    /// media, never on the track — because that is exactly what real
    /// Matroska files do and what this fixture exists to reproduce.
    fn webm_bytes(duration_ticks: f64, timecode_scale_ns: u64, sample_rate: f64) -> Vec<u8> {
        let header = elem(
            &[0x1A, 0x45, 0xDF, 0xA3],
            &[
                elem(&[0x42, 0x86], &ebml_uint(1)), // EBMLVersion
                elem(&[0x42, 0xF7], &ebml_uint(1)), // EBMLReadVersion
                elem(&[0x42, 0xF2], &ebml_uint(4)), // EBMLMaxIDLength
                elem(&[0x42, 0xF3], &ebml_uint(8)), // EBMLMaxSizeLength
                elem(&[0x42, 0x82], b"webm"),       // DocType
                elem(&[0x42, 0x87], &ebml_uint(2)), // DocTypeVersion
                elem(&[0x42, 0x85], &ebml_uint(2)), // DocTypeReadVersion
            ]
            .concat(),
        );
        let info = elem(
            &[0x15, 0x49, 0xA9, 0x66],
            &[
                elem(&[0x2A, 0xD7, 0xB1], &ebml_uint(timecode_scale_ns)), // TimestampScale
                elem(&[0x44, 0x89], &ebml_float(duration_ticks)),         // Duration
                // MuxingApp and WritingApp are not optional to symphonia:
                // `segment.rs:1032,1035` fail the whole read with
                // "missing info muxing app" / "missing info writing app"
                // rather than defaulting them. Every real muxer writes both.
                elem(&[0x4D, 0x80], b"butterfly-speak-test"), // MuxingApp
                elem(&[0x57, 0x41], b"butterfly-speak-test"), // WritingApp
            ]
            .concat(),
        );
        let audio = elem(
            &[0xE1],
            &[
                elem(&[0xB5], &ebml_float(sample_rate)), // SamplingFrequency
                elem(&[0x9F], &ebml_uint(1)),            // Channels
            ]
            .concat(),
        );
        let track = elem(
            &[0xAE],
            &[
                elem(&[0xD7], &ebml_uint(1)),       // TrackNumber
                elem(&[0x73, 0xC5], &ebml_uint(1)), // TrackUID
                elem(&[0x83], &ebml_uint(2)),       // TrackType = audio
                elem(&[0x86], b"A_OPUS"),           // CodecID
                audio,
            ]
            .concat(),
        );
        let tracks = elem(&[0x16, 0x54, 0xAE, 0x6B], &track);
        let segment = elem(&[0x18, 0x53, 0x80, 0x67], &[info, tracks].concat());
        [header, segment].concat()
    }

    /// A minimal but genuine RIFF/WAVE file: 16-bit mono PCM at `rate` Hz
    /// holding `frames` samples of a quiet tone. Synthesized rather than
    /// checked in, so the fixture cannot drift from what the test claims it
    /// is, and so no binary blob enters the repo for a header parse.
    fn wav_bytes(rate: u32, frames: u32) -> Vec<u8> {
        let data_len = frames * 2;
        let mut v = Vec::with_capacity(44 + data_len as usize);
        v.extend_from_slice(b"RIFF");
        v.extend_from_slice(&(36 + data_len).to_le_bytes());
        v.extend_from_slice(b"WAVE");
        v.extend_from_slice(b"fmt ");
        v.extend_from_slice(&16u32.to_le_bytes()); // PCM fmt chunk size
        v.extend_from_slice(&1u16.to_le_bytes()); // WAVE_FORMAT_PCM
        v.extend_from_slice(&1u16.to_le_bytes()); // mono
        v.extend_from_slice(&rate.to_le_bytes());
        v.extend_from_slice(&(rate * 2).to_le_bytes()); // byte rate
        v.extend_from_slice(&2u16.to_le_bytes()); // block align
        v.extend_from_slice(&16u16.to_le_bytes()); // bits per sample
        v.extend_from_slice(b"data");
        v.extend_from_slice(&data_len.to_le_bytes());
        for i in 0..frames {
            let s = ((i as f64 * 0.05).sin() * 4000.0) as i16;
            v.extend_from_slice(&s.to_le_bytes());
        }
        v
    }

    /// Writes `bytes` to a uniquely named file under the OS temp dir and
    /// deletes it on drop, so a failing assertion cannot leave litter behind.
    struct TempFile(std::path::PathBuf);

    impl TempFile {
        fn new(name: &str, bytes: &[u8]) -> TempFile {
            // Nanos + the thread id keep two tests that ask for the same name
            // from colliding when the suite runs in parallel.
            let unique = format!(
                "bs-probe-{}-{:?}-{name}",
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .expect("system clock is after the epoch")
                    .as_nanos(),
                std::thread::current().id()
            );
            let path = std::env::temp_dir().join(unique);
            let mut f = std::fs::File::create(&path).expect("create the temp fixture");
            f.write_all(bytes).expect("write the temp fixture");
            drop(f);
            TempFile(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempFile {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    #[test]
    fn a_real_wav_probes_to_its_duration_and_rate() {
        // 16 kHz x 32 000 frames = exactly 2 s.
        let f = TempFile::new("two-seconds.wav", &wav_bytes(16_000, 32_000));
        let p = probe(f.path()).expect("a well-formed WAV must probe");
        assert_eq!(p.sample_rate, Some(16_000));
        // symphonia names the RIFF reader "wave", not "wav" — the container's
        // name, not the extension's. Pinned so the distinction is on record.
        assert_eq!(p.container, "wave");
        let secs = p.duration_s.expect("a PCM WAV states its length");
        assert!(
            (secs - 2.0).abs() < 0.01,
            "expected ~2 s, got {secs} s"
        );
    }

    /// A non-16 kHz rate must survive unchanged — nothing in this path
    /// resamples, and Sarvam auto-detects everything but raw PCM.
    #[test]
    fn a_44_1_khz_wav_keeps_its_own_rate() {
        let f = TempFile::new("cd-rate.wav", &wav_bytes(44_100, 44_100));
        let p = probe(f.path()).expect("a 44.1 kHz WAV must probe");
        assert_eq!(p.sample_rate, Some(44_100));
        let secs = p.duration_s.expect("a PCM WAV states its length");
        assert!((secs - 1.0).abs() < 0.01, "expected ~1 s, got {secs} s");
    }

    /// symphonia's Matroska demuxer states the duration on `MediaInfo` and
    /// never on the track, so a probe that reads only `Track::duration` and
    /// `Track::num_frames` returns `None` for **every** `.webm`. Because
    /// `check_limits` deliberately lets an unknown length through, that
    /// silently uncapped the whole format: a three-hour screen recording
    /// would have been uploaded, billed, and then waited on for the full
    /// 30-minute deadline.
    ///
    /// 5000 ticks x 1 ms = 5 s.
    #[test]
    fn a_webm_reports_its_duration_from_the_media_not_the_track() {
        let f = TempFile::new("screen.webm", &webm_bytes(5_000.0, 1_000_000, 48_000.0));
        let p = probe(f.path()).expect("a well-formed WebM must probe");
        assert_eq!(p.container, "matroska");
        let secs = p
            .duration_s
            .expect("a WebM states its length on the media, and the probe must find it there");
        assert!((secs - 5.0).abs() < 0.01, "expected ~5 s, got {secs} s");
    }

    /// The consequence, stated as its own test so the reason the branch
    /// exists cannot be refactored away: a long WebM has to actually hit the
    /// duration ceiling. Before the `media_info()` fallback this passed
    /// `check_limits` with `duration_s: None`.
    #[test]
    fn a_three_hour_webm_is_refused_by_the_duration_ceiling() {
        // 10 800 000 ticks x 1 ms = 3 h.
        let f = TempFile::new("long.webm", &webm_bytes(10_800_000.0, 1_000_000, 48_000.0));
        let p = probe(f.path()).expect("a well-formed WebM must probe");
        let limits = ImportLimits::default();
        assert!(
            matches!(check_limits(&p, 10_000, &limits), Err(ProbeError::TooLong { .. })),
            "a 3 h WebM must not slip past the ceiling as an unknown length"
        );
    }

    /// A non-default timestamp scale must still resolve, since the duration
    /// is stated in those ticks rather than in seconds.
    #[test]
    fn a_webm_with_a_non_default_timestamp_scale_still_resolves() {
        // 200 ticks x 100 ms = 20 s.
        let f = TempFile::new("scaled.webm", &webm_bytes(200.0, 100_000_000, 48_000.0));
        let p = probe(f.path()).expect("must probe");
        let secs = p.duration_s.expect("must state a length");
        assert!((secs - 20.0).abs() < 0.05, "expected ~20 s, got {secs} s");
    }

    /// The whole reason this module exists. A live test showed Sarvam
    /// transcribes this exact input — 6 kB of ASCII named `.wav` — as the
    /// word "Hello" and reports success. If this test ever goes green
    /// by returning `Ok`, the fabricated-transcript path is open again.
    #[test]
    fn plain_text_wearing_a_wav_extension_is_declined_not_uploaded() {
        let text = "this is plain text, not audio, ".repeat(200);
        let f = TempFile::new("not-audio.wav", text.as_bytes());
        assert_eq!(probe(f.path()), Err(ProbeError::NotMedia));
    }

    #[test]
    fn an_empty_file_is_declined_rather_than_panicking() {
        let f = TempFile::new("empty.wav", b"");
        assert_eq!(probe(f.path()), Err(ProbeError::NotMedia));
    }

    /// A WAV header promising 2 s of samples that the file then does not
    /// contain. The header parse still succeeds — that is what a probe is —
    /// so this pins that a truncated file does not panic and does not report
    /// a length it cannot back up with bytes.
    #[test]
    fn a_truncated_wav_does_not_panic() {
        let mut bytes = wav_bytes(16_000, 32_000);
        bytes.truncate(60);
        let f = TempFile::new("truncated.wav", &bytes);
        // Either answer is acceptable; a panic is not.
        match probe(f.path()) {
            Ok(p) => assert_eq!(p.container, "wave"),
            Err(e) => assert!(matches!(e, ProbeError::NotMedia | ProbeError::NoAudioTrack)),
        }
    }

    /// symphonia takes a WAV header's sample rate on trust, and the
    /// conversion sizes its resampler from it. A header claiming a rate no
    /// recording has is declined here, as not audio.
    #[test]
    fn a_wav_claiming_an_impossible_sample_rate_is_not_media() {
        for rate in [500u32, 4_294_967_291] {
            let mut bytes = wav_bytes(16_000, 1_600);
            bytes[24..28].copy_from_slice(&rate.to_le_bytes());
            let f = TempFile::new("odd-rate.wav", &bytes);
            assert_eq!(probe(f.path()), Err(ProbeError::NotMedia), "{rate} Hz");
        }
        for rate in [8_000u32, 384_000] {
            let f = TempFile::new("real-rate.wav", &wav_bytes(rate, 1_600));
            assert_eq!(probe(f.path()).map(|p| p.sample_rate), Ok(Some(rate)), "{rate} Hz");
        }
    }

    /// A stated rate of 0 is not refused here. An MP4 sample entry cannot
    /// hold a rate above 65 535 Hz, so ffmpeg writes 0 there for an 88.2 or
    /// 96 kHz AAC `.m4a`, whose decoder then reads the real rate from the
    /// stream itself. The conversion checks the rate the decoder reports.
    #[test]
    fn a_stated_sample_rate_of_zero_is_left_to_the_conversion() {
        let mut bytes = wav_bytes(16_000, 1_600);
        bytes[24..28].copy_from_slice(&0u32.to_le_bytes());
        let f = TempFile::new("zero-rate.wav", &bytes);
        assert_eq!(probe(f.path()).map(|p| p.sample_rate), Ok(Some(0)));
    }

    #[test]
    fn a_missing_file_is_unreadable_not_a_crash() {
        let path = std::env::temp_dir().join("bs-probe-does-not-exist-9f3a2b.wav");
        assert_eq!(probe(&path), Err(ProbeError::Unreadable));
    }

    #[test]
    fn an_unsupported_extension_is_refused_before_the_file_is_even_opened() {
        // No file is created, and the error is still the extension one rather
        // than `Unreadable` — proof the check happens first.
        let path = std::env::temp_dir().join("bs-probe-nonexistent.mov");
        assert_eq!(
            probe(&path),
            Err(ProbeError::UnsupportedExtension { ext: "mov".into() })
        );
    }

    #[test]
    fn a_file_with_no_extension_at_all_is_refused_not_guessed() {
        let path = std::env::temp_dir().join("bs-probe-no-extension");
        assert_eq!(
            probe(&path),
            Err(ProbeError::UnsupportedExtension { ext: String::new() })
        );
    }

    #[test]
    fn extension_matching_is_case_insensitive() {
        assert!(is_accepted_extension(Path::new("C:/x/RECORDING.WAV")));
        assert!(is_accepted_extension(Path::new("C:/x/clip.Mp3")));
        assert!(!is_accepted_extension(Path::new("C:/x/clip.mov")));
        assert!(!is_accepted_extension(Path::new("C:/x/no-extension")));
    }

    #[test]
    fn the_accepted_extensions_are_the_documented_list() {
        assert_eq!(
            ACCEPTED_EXTENSIONS,
            &["aac", "flac", "m4a", "mp3", "mp4", "oga", "ogg", "opus", "wav", "webm"]
        );
        let unique: std::collections::HashSet<&str> = ACCEPTED_EXTENSIONS.iter().copied().collect();
        assert_eq!(unique.len(), ACCEPTED_EXTENSIONS.len(), "no entry twice");
        for ext in ACCEPTED_EXTENSIONS {
            assert_eq!(*ext, ext.to_ascii_lowercase(), "{ext} must be lowercase");
        }
        for excluded in ["amr", "wma", "aiff", "pcm", "mov", "mkv"] {
            assert!(
                !ACCEPTED_EXTENSIONS.contains(&excluded),
                "{excluded} must stay out of the list"
            );
        }
    }

    /// The refusal names formats that always convert, and never an
    /// Opus-only container as the fix.
    #[test]
    fn the_extension_refusal_suggests_formats_that_convert() {
        for ext in ["mov", ""] {
            let m = ProbeError::UnsupportedExtension { ext: ext.into() }.user_message();
            for suggested in ["WAV", "MP3", "M4A", "FLAC"] {
                assert!(m.contains(suggested), "{m}");
                assert!(ACCEPTED_EXTENSIONS.contains(&suggested.to_ascii_lowercase().as_str()));
            }
            assert!(!m.contains("Opus") && !m.contains("OPUS") && !m.contains("WebM"), "{m}");
        }
        assert!(ProbeError::UnsupportedExtension { ext: "mov".into() }
            .user_message()
            .contains(".mov"));
    }

    fn probed(duration_s: Option<f64>) -> Probe {
        Probe {
            duration_s,
            container: "wav",
            sample_rate: Some(16_000),
        }
    }

    #[test]
    fn the_default_limits_are_the_documented_numbers() {
        assert_eq!(
            ImportLimits::default(),
            ImportLimits {
                max_bytes: MAX_IMPORT_BYTES,
                max_duration_s: MAX_IMPORT_DURATION_S,
            }
        );
        assert_eq!(MAX_IMPORT_BYTES, 1_610_612_736);
        // The derivation: a full-length 48 kHz 16-bit stereo WAV fits.
        let full_length_wav = 44 + (MAX_IMPORT_DURATION_S as u64) * 48_000 * 2 * 2;
        assert!(
            check_limits(&probed(Some(MAX_IMPORT_DURATION_S)), full_length_wav, &ImportLimits::default())
                .is_ok()
        );
    }

    #[test]
    fn a_file_at_exactly_the_size_ceiling_is_allowed() {
        let d = ImportLimits::default();
        assert_eq!(check_limits(&probed(Some(1.0)), d.max_bytes, &d), Ok(()));
        assert_eq!(
            check_limits(&probed(Some(1.0)), d.max_bytes + 1, &d),
            Err(ProbeError::TooLarge {
                bytes: d.max_bytes + 1,
                limit: d.max_bytes
            })
        );
    }

    #[test]
    fn a_file_at_exactly_the_duration_ceiling_is_allowed() {
        let d = ImportLimits::default();
        assert_eq!(check_limits(&probed(Some(7_200.0)), 1_000, &d), Ok(()));
        assert_eq!(
            check_limits(&probed(Some(7_200.5)), 1_000, &d),
            Err(ProbeError::TooLong {
                seconds: 7_200.5,
                limit_s: 7_200.0
            })
        );
    }

    /// The documented decision, pinned: a container that does not state its
    /// length must not be refused on the strength of a ceiling that is itself
    /// unverified.
    #[test]
    fn an_unknown_duration_passes_the_length_ceiling() {
        let d = ImportLimits::default();
        assert_eq!(check_limits(&probed(None), 1_000, &d), Ok(()));
    }

    /// ...but it does not buy a pass on the size ceiling too.
    #[test]
    fn an_unknown_duration_still_fails_the_size_ceiling() {
        let d = ImportLimits::default();
        assert!(check_limits(&probed(None), d.max_bytes + 1, &d).is_err());
    }

    /// The ceiling is read from the value passed in, not from the constant:
    /// a higher one admits the file the default refused.
    #[test]
    fn raising_the_duration_override_admits_a_file_the_default_refused() {
        let default = ImportLimits::default();
        let three_hours = probed(Some(3.0 * 3_600.0));
        assert!(check_limits(&three_hours, 1_000, &default).is_err());

        let raised = ImportLimits {
            max_duration_s: 4.0 * 3_600.0,
            ..default
        };
        assert_eq!(check_limits(&three_hours, 1_000, &raised), Ok(()));
    }

    #[test]
    fn every_decline_says_something_different_and_names_no_path() {
        let all = [
            ProbeError::UnsupportedExtension { ext: "mov".into() },
            ProbeError::Unreadable,
            ProbeError::NotMedia,
            ProbeError::NoAudioTrack,
            ProbeError::TooLarge {
                bytes: 600 * 1024 * 1024,
                limit: MAX_IMPORT_BYTES,
            },
            ProbeError::TooLong {
                seconds: 10_000.0,
                limit_s: MAX_IMPORT_DURATION_S,
            },
        ];
        let messages: std::collections::HashSet<String> =
            all.iter().map(|e| e.user_message()).collect();
        assert_eq!(
            messages.len(),
            all.len(),
            "every decline must say something different"
        );
        for e in &all {
            let m = e.user_message();
            assert!(!m.is_empty());
            assert!(
                !m.contains('\\') && !m.contains(":/"),
                "a decline must not carry a path: {m}"
            );
        }
    }

    #[test]
    fn the_ceilings_are_phrased_in_units_a_person_reads() {
        let over = ProbeError::TooLarge {
            bytes: MAX_IMPORT_BYTES + 20 * 1024 * 1024,
            limit: MAX_IMPORT_BYTES,
        }
        .user_message();
        assert!(over.contains("1.52 GB"), "{over}");
        assert!(over.contains("limit is 1.5 GB"), "{over}");
        assert!(!over.contains("bytes"), "{over}");

        // A file and a limit either side of a unit step still share one unit.
        let close = ProbeError::TooLarge {
            bytes: 1_100 * 1024 * 1024,
            limit: 1_000 * 1024 * 1024,
        }
        .user_message();
        assert!(close.contains("1100 MB") && close.contains("1000 MB"), "{close}");

        // A file that rounds onto the limit's figure gets more digits, and
        // one too close for any digits says so in words.
        let nearly = ProbeError::TooLarge {
            bytes: MAX_IMPORT_BYTES + 4 * 1024 * 1024,
            limit: MAX_IMPORT_BYTES,
        }
        .user_message();
        assert!(nearly.contains("file is 1.504 GB"), "{nearly}");
        assert!(nearly.contains("limit is 1.5 GB"), "{nearly}");
        let barely = ProbeError::TooLarge {
            bytes: MAX_IMPORT_BYTES + 1,
            limit: MAX_IMPORT_BYTES,
        }
        .user_message();
        assert!(barely.contains("file is just over 1.5 GB"), "{barely}");

        let long = ProbeError::TooLong {
            seconds: 3.0 * 3_600.0,
            limit_s: MAX_IMPORT_DURATION_S,
        }
        .user_message();
        assert!(long.contains("3 hr"), "{long}");
        assert!(long.contains("2 hr"), "{long}");
        assert!(!long.contains("7200") && !long.contains("10800"), "{long}");
    }

    /// There is no setting that moves the length ceiling, so the refusal must
    /// not send the user looking for one. And a recording just over the limit
    /// must not read as exactly the limit.
    #[test]
    fn a_recording_just_over_the_length_limit_says_by_how_much() {
        let over = |seconds: f64| {
            ProbeError::TooLong {
                seconds,
                limit_s: MAX_IMPORT_DURATION_S,
            }
            .user_message()
        };
        let m = over(7_220.0);
        assert!(m.contains("recording is 2 hr 20 sec long"), "{m}");
        assert!(m.contains("limit is 2 hr"), "{m}");
        assert!(!m.contains("Settings"), "{m}");
        let m = over(7_200.4);
        assert!(m.contains("recording is just over 2 hr long"), "{m}");
        let m = over(7_290.0);
        assert!(m.contains("recording is 2 hr 2 min long"), "{m}");
    }

    #[test]
    fn durations_read_the_way_a_clock_does() {
        assert_eq!(format_duration(30.0), "1 min");
        assert_eq!(format_duration(90.0), "2 min");
        assert_eq!(format_duration(3_600.0), "1 hr");
        assert_eq!(format_duration(5_400.0), "1 hr 30 min");
        assert_eq!(format_duration(7_200.0), "2 hr");
    }

    /// Each unit step, and one value inside each band.
    #[test]
    fn sizes_read_the_way_file_explorer_counts_them() {
        let show = |b: u64| format_size(b, SizeUnit::for_bytes(b));
        assert_eq!(show(0), "0 bytes");
        assert_eq!(show(512), "512 bytes");
        assert_eq!(show(1023), "1023 bytes");
        assert_eq!(show(1024), "1 KB");
        assert_eq!(show(1536), "1.5 KB");
        assert_eq!(show(1024 * 1024), "1 MB");
        assert_eq!(show(12_900_000), "12.3 MB");
        assert_eq!(show(250 * 1024 * 1024), "250 MB");
        assert_eq!(show(1024 * 1024 * 1024), "1 GB");
        assert_eq!(show(MAX_IMPORT_BYTES), "1.5 GB");
        assert_eq!(show(20 * 1024 * 1024 * 1024), "20 GB");
    }
}
