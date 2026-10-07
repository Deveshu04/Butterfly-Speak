//! Resumable, hash-verified model downloads.
//!
//! Stream → `models\.staging\<id>.part` (resumed via Range when present) →
//! SHA-256 verify → extract (tar.bz2) or move (single file) → `.bs-ok`
//! marker. Progress events at ~4 Hz.

use crate::settings::models_root;
use futures_util::StreamExt;
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tauri::Emitter;

/// How long a download may go without a single byte before it is abandoned
/// with an error, so a dead connection cannot leave a download showing as in
/// progress for good. Measured from the latest chunk, and also the limit on
/// connecting and on waiting for the server's answer to the request.
const IDLE_CUTOFF: Duration = Duration::from_secs(30);

/// How often a wait on the network checks the Cancel flag.
const CANCEL_POLL: Duration = Duration::from_millis(100);

/// Why [`until_cancelled`] gave up on the future it was waiting for.
#[derive(Debug, PartialEq, Eq)]
enum Interrupted {
    Cancelled,
    TimedOut,
}

/// Waits for `fut`, for at most `limit`, and stops as soon as `cancel` is
/// set. Every network wait in a download goes through this, so Cancel ends
/// the download however far it has got, and a server that goes quiet cannot
/// keep it open.
async fn until_cancelled<F: std::future::Future>(
    fut: F,
    limit: Duration,
    cancel: &AtomicBool,
) -> Result<F::Output, Interrupted> {
    tokio::pin!(fut);
    let deadline = tokio::time::sleep(limit);
    tokio::pin!(deadline);
    let mut tick = tokio::time::interval(CANCEL_POLL);
    loop {
        tokio::select! {
            out = &mut fut => return Ok(out),
            _ = &mut deadline => return Err(Interrupted::TimedOut),
            _ = tick.tick() => {
                if cancel.load(Ordering::Relaxed) {
                    return Err(Interrupted::Cancelled);
                }
            }
        }
    }
}

/// TLS/certificate errors are the one download failure not worth retrying on
/// the same network: an SSL-inspecting proxy re-signs every certificate with
/// its own (untrusted) root, and hitting Retry just fails again with the
/// exact same error — so the message sends the user to another network.
const TLS_ERROR_MESSAGE: &str = "A certificate problem stopped the download: something on this network \
     intercepts secure connections. Use another network or ask its admin.";

/// Whether `err`'s source chain contains a certificate-validation failure.
/// Looks for `rustls::Error`'s own `InvalidCertificate` discriminant — never
/// message text — since `reqwest`'s `rustls-tls` backend surfaces
/// TLS failures as a `rustls::Error` wrapped by `io::Error` and then by
/// hyper/reqwest's own error types.
///
/// `source()` alone cannot reach it. `io::Error`'s `source()` returns its
/// *payload's* source rather than the payload itself, so the walk dead-ends
/// at the first `io::Error` in the chain, and `get_ref()` is the only way
/// past — twice over, because there are two such layers: `tokio-rustls`
/// wraps the `rustls::Error` in an `io::Error`, then `hyper-rustls` wraps
/// that in another. Measured against untrusted-root/self-signed/expired
/// endpoints, the real chain is
///
/// ```text
/// reqwest::Error  "error sending request for url"
///   -> hyper_util::client::legacy::Error  "client error (Connect)"
///     -> io::Error (Other)      [get_ref]
///       -> io::Error (InvalidData)  [get_ref]
///         -> rustls::Error  "invalid peer certificate: UnknownIssuer"
/// ```
///
/// A `source()`-only walk returns `false` for all three, which would make
/// this whole classification — and [`TLS_ERROR_MESSAGE`] with it —
/// unreachable.
fn is_tls_error(err: &(dyn std::error::Error + 'static)) -> bool {
    let mut cur: Option<&(dyn std::error::Error + 'static)> = Some(err);
    while let Some(e) = cur {
        if let Some(rustls_err) = e.downcast_ref::<rustls::Error>() {
            return matches!(rustls_err, rustls::Error::InvalidCertificate(_));
        }
        // Prefer the payload over `source()` for an `io::Error`: the payload
        // is the layer below, and `source()` skips straight past it.
        cur = match e.downcast_ref::<std::io::Error>().and_then(|io| io.get_ref()) {
            Some(payload) => Some(payload),
            None => e.source(),
        };
    }
    false
}

/// Turn a `reqwest` failure into the download's error, substituting the
/// SSL-inspection message for a TLS/certificate failure so a user
/// hitting Retry on the same network isn't left guessing. Never touches the
/// `.part` file — unlike the checksum-mismatch path, this is a connection
/// failure, not the wrong bytes, so resume-on-retry keeps working once the
/// network stops intercepting TLS.
fn classify_reqwest_error(e: reqwest::Error) -> anyhow::Error {
    if is_tls_error(&e) {
        anyhow::anyhow!(TLS_ERROR_MESSAGE)
    } else {
        anyhow::Error::new(e)
    }
}

pub const PROGRESS_EVENT: &str = "models://progress";

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProgressPayload {
    pub id: String,
    pub phase: String, // downloading | verifying | extracting | done | error | cancelled
    pub downloaded: u64,
    pub total: u64,
    pub bytes_per_sec: u64,
    pub message: Option<String>,
}

pub struct Job {
    pub id: String,
    pub url: String,
    pub sha256: String,
    pub total_bytes: u64,
    pub kind: JobKind,
}

pub enum JobKind {
    /// tar.bz2 archive that extracts to `dir_name` under the models root.
    Archive { dir_name: String },
    /// Single file stored as `file_name` under the models root.
    File { file_name: String },
}

fn emit(app: &tauri::AppHandle, p: ProgressPayload) {
    let _ = app.emit(PROGRESS_EVENT, p);
}

pub async fn run(app: tauri::AppHandle, job: Job, cancel: Arc<AtomicBool>) {
    let id = job.id.clone();
    match run_inner(&app, &job, &cancel).await {
        Ok(()) => emit(
            &app,
            ProgressPayload {
                id,
                phase: "done".into(),
                downloaded: job.total_bytes,
                total: job.total_bytes,
                bytes_per_sec: 0,
                message: None,
            },
        ),
        Err(e) => {
            let cancelled = cancel.load(Ordering::Relaxed);
            tracing::warn!("download {id} ended: {e:#} (cancelled: {cancelled})");
            emit(
                &app,
                ProgressPayload {
                    id,
                    phase: if cancelled { "cancelled" } else { "error" }.into(),
                    downloaded: 0,
                    total: job.total_bytes,
                    bytes_per_sec: 0,
                    message: Some(format!("{e:#}")),
                },
            );
        }
    }
}

async fn run_inner(
    app: &tauri::AppHandle,
    job: &Job,
    cancel: &Arc<AtomicBool>,
) -> anyhow::Result<()> {
    let root = models_root();
    let staging = root.join(".staging");
    std::fs::create_dir_all(&staging)?;
    let part = staging.join(format!("{}.part", job.id));

    // Resume support: hash + count existing bytes first.
    let (mut hasher, mut downloaded) = read_part(&part)?;
    // Whether the bytes checked at the end include some from an earlier
    // attempt. Cleared whenever the part is thrown away.
    let mut resumed = downloaded > 0;

    // A part as long as the whole file has nothing left to fetch, and every
    // server answers a Range past the end with 416. A quit, a crash or a full
    // disk during install leaves exactly that behind, so it is checked
    // before anything is requested: the right bytes go straight to install,
    // anything else is thrown away and fetched again from the start.
    let mut whole = part_is_whole(downloaded, job.total_bytes);
    if whole && !digest_matches(&hasher, &job.sha256) {
        (hasher, downloaded) = discard_part(&part);
        resumed = false;
        whole = false;
    }

    if !whole {
        let client = download_client()?;
        let mut resp = request(&client, &job.url, downloaded, cancel, IDLE_CUTOFF).await?;
        // The same answer for a part the catalog's size did not flag: the
        // server has nothing after it.
        if downloaded > 0 && resp.status() == reqwest::StatusCode::RANGE_NOT_SATISFIABLE {
            if digest_matches(&hasher, &job.sha256) {
                whole = true;
            } else {
                (hasher, downloaded) = discard_part(&part);
                resumed = false;
                resp = request(&client, &job.url, 0, cancel, IDLE_CUTOFF).await?;
            }
        }
        if !whole {
            let resp = resp.error_for_status().map_err(classify_reqwest_error)?;
            // If the server ignored the Range header, start over.
            if downloaded > 0 && resp.status() != reqwest::StatusCode::PARTIAL_CONTENT {
                (hasher, downloaded) = discard_part(&part);
                resumed = false;
            }
            (hasher, downloaded) =
                stream_into(app, job, cancel, &part, resp, hasher, downloaded).await?;
        }
    }

    // Verify.
    emit(
        app,
        ProgressPayload {
            id: job.id.clone(),
            phase: "verifying".into(),
            downloaded,
            total: job.total_bytes,
            bytes_per_sec: 0,
            message: None,
        },
    );
    let hash = format!("{:x}", hasher.finalize());
    verify(&job.id, &hash, &job.sha256, &part, resumed)?;

    // Install.
    emit(
        app,
        ProgressPayload {
            id: job.id.clone(),
            phase: "extracting".into(),
            downloaded,
            total: job.total_bytes,
            bytes_per_sec: 0,
            message: None,
        },
    );
    install(&root, &staging, &part, &job.id, &job.kind, &hash)
}

/// The bytes already in `part`, hashed, and how many there are. An absent
/// part is an empty one.
fn read_part(part: &Path) -> anyhow::Result<(Sha256, u64)> {
    let mut hasher = Sha256::new();
    let mut len: u64 = 0;
    if part.exists() {
        let mut f = std::fs::File::open(part)?;
        let mut buf = vec![0u8; 1 << 20];
        loop {
            let n = f.read(&mut buf)?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
            len += n as u64;
        }
    }
    Ok((hasher, len))
}

/// Delete `part` and start the count over.
fn discard_part(part: &Path) -> (Sha256, u64) {
    let _ = std::fs::remove_file(part);
    (Sha256::new(), 0)
}

/// Whether a part of `len` bytes already holds the whole `total`-byte file,
/// so there is nothing left to ask the server for.
fn part_is_whole(len: u64, total: u64) -> bool {
    total > 0 && len >= total
}

/// Whether the bytes hashed so far are the file the catalog names. A job
/// with no checksum never matches: bytes that cannot be checked are fetched
/// again rather than trusted.
fn digest_matches(hasher: &Sha256, sha256: &str) -> bool {
    !sha256.is_empty() && format!("{:x}", hasher.clone().finalize()) == sha256
}

/// Shown when a download fetched whole in one go does not match the
/// catalog's checksum. HTTPS keeps the bytes from changing on the way, so the
/// file on the server is not the one this version of the app was built
/// against, and downloading it again would fetch the same file.
const CHANGED_UPSTREAM_MESSAGE: &str = "The file on the server no longer matches the one this \
     version of Butterfly Speak expects. Updating the app fixes this.";

/// Shown when a download resumed from an earlier attempt does not match. The
/// earlier bytes can be at fault (a crash can leave a zero-filled tail), and
/// the part is deleted, so a fresh download can succeed.
const RESUMED_MISMATCH_MESSAGE: &str =
    "The download didn't match the expected file and has been deleted. Download it again.";

/// Checks a finished download's `hash` against the catalog's `expected` one.
/// A mismatch deletes the part, so nothing resumes from the wrong bytes, and
/// the message depends on whether bytes from an earlier attempt were part of
/// it (`resumed`). A job with no checksum is not checked.
fn verify(id: &str, hash: &str, expected: &str, part: &Path, resumed: bool) -> anyhow::Result<()> {
    if expected.is_empty() || hash == expected {
        return Ok(());
    }
    let _ = std::fs::remove_file(part);
    tracing::warn!(
        "download {id} does not match its checksum (expected {expected}, got {hash}, resumed {resumed})"
    );
    if resumed {
        anyhow::bail!(RESUMED_MISMATCH_MESSAGE)
    }
    anyhow::bail!(CHANGED_UPSTREAM_MESSAGE)
}

/// How long connecting may take. Shorter than [`IDLE_CUTOFF`], which bounds
/// the whole wait for an answer, so a host that cannot be reached is reported
/// as a connection failure rather than as a server that never answered.
const CONNECT_CUTOFF: Duration = Duration::from_secs(10);

/// The HTTP client for one download.
fn download_client() -> anyhow::Result<reqwest::Client> {
    Ok(reqwest::Client::builder().connect_timeout(CONNECT_CUTOFF).build()?)
}

/// `GET url`, from byte `from` on when it is not zero, waiting at most
/// `limit` for the server's answer and no longer than it takes to press
/// Cancel. The status is left to the caller: a 416 is an answer this module
/// acts on, not a failure.
async fn request(
    client: &reqwest::Client,
    url: &str,
    from: u64,
    cancel: &AtomicBool,
    limit: Duration,
) -> anyhow::Result<reqwest::Response> {
    let mut req = client.get(url);
    if from > 0 {
        req = req.header("Range", format!("bytes={from}-"));
    }
    match until_cancelled(req.send(), limit, cancel).await {
        Ok(sent) => sent.map_err(classify_reqwest_error),
        Err(Interrupted::Cancelled) => anyhow::bail!("cancelled"),
        Err(Interrupted::TimedOut) => anyhow::bail!(
            "The download could not start: the server sent no answer for {} seconds.",
            limit.as_secs()
        ),
    }
}

/// Append `resp`'s body to `part`, carrying on the hash and the count.
async fn stream_into(
    app: &tauri::AppHandle,
    job: &Job,
    cancel: &Arc<AtomicBool>,
    part: &Path,
    resp: reqwest::Response,
    mut hasher: Sha256,
    mut downloaded: u64,
) -> anyhow::Result<(Sha256, u64)> {
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(part)?;
    let mut stream = resp.bytes_stream();
    let mut last_emit = Instant::now();
    let mut window_bytes: u64 = 0;

    loop {
        if cancel.load(Ordering::Relaxed) {
            anyhow::bail!("cancelled");
        }
        // Re-armed every iteration, so it measures time since the *last*
        // chunk, not since the download started.
        let chunk = match until_cancelled(stream.next(), IDLE_CUTOFF, cancel).await {
            Ok(Some(chunk)) => chunk.map_err(classify_reqwest_error)?,
            Ok(None) => break, // stream ended normally
            Err(Interrupted::Cancelled) => anyhow::bail!("cancelled"),
            Err(Interrupted::TimedOut) => anyhow::bail!(
                "The download stopped: nothing arrived for {} seconds.",
                IDLE_CUTOFF.as_secs()
            ),
        };
        file.write_all(&chunk)?;
        hasher.update(&chunk);
        downloaded += chunk.len() as u64;
        window_bytes += chunk.len() as u64;

        if last_emit.elapsed().as_millis() >= 250 {
            let bps = (window_bytes as f64 / last_emit.elapsed().as_secs_f64()) as u64;
            emit(
                app,
                ProgressPayload {
                    id: job.id.clone(),
                    phase: "downloading".into(),
                    downloaded,
                    total: job.total_bytes,
                    bytes_per_sec: bps,
                    message: None,
                },
            );
            last_emit = Instant::now();
            window_bytes = 0;
        }
    }
    file.flush()?;
    Ok((hasher, downloaded))
}

/// Put a verified download in place under `root`.
///
/// An archive is unpacked into `staging` first and its folder, marked with
/// `.bs-ok`, is renamed into place only once it is whole, so a quit or a
/// failure part-way leaves no half-extracted folder that looks installed. A
/// failed extraction also deletes the part: the next Download starts clean
/// instead of meeting the same bytes again.
fn install(
    root: &Path,
    staging: &Path,
    part: &Path,
    id: &str,
    kind: &JobKind,
    hash: &str,
) -> anyhow::Result<()> {
    match kind {
        JobKind::Archive { dir_name } => {
            let unpack = staging.join(format!("{id}.unpack"));
            let _ = std::fs::remove_dir_all(&unpack);
            let placed = extract_tar_bz2(part, &unpack).and_then(|()| {
                let unpacked = unpack.join(dir_name);
                if !unpacked.is_dir() {
                    anyhow::bail!("archive did not contain expected folder {dir_name}");
                }
                std::fs::write(unpacked.join(".bs-ok"), hash)?;
                let dest = root.join(dir_name);
                if dest.exists() {
                    std::fs::remove_dir_all(&dest)?;
                }
                std::fs::rename(&unpacked, &dest)?;
                Ok(())
            });
            let _ = std::fs::remove_dir_all(&unpack);
            if placed.is_err() {
                let _ = std::fs::remove_file(part);
            }
            placed?;
        }
        JobKind::File { file_name } => {
            let dest = root.join(file_name);
            if dest.exists() {
                std::fs::remove_file(&dest)?;
            }
            std::fs::rename(part, &dest)?;
        }
    }
    let _ = std::fs::remove_file(part);
    Ok(())
}

/// Remove the unpack folders a quit or a crash left in the staging folder
/// during an install. Called once at startup rather than from a download:
/// two downloads can run at once, each unpacking into its own folder there,
/// and one must not sweep the other's. A `.part` file stays: it is what a
/// Download resumes from.
pub fn sweep_leftover_unpacks() {
    let removed = sweep_unpacks_in(&models_root().join(".staging"));
    if removed > 0 {
        tracing::info!("removed {removed} half-installed model folder(s) left by an earlier run");
    }
}

fn sweep_unpacks_in(staging: &Path) -> usize {
    let Ok(entries) = std::fs::read_dir(staging) else {
        return 0;
    };
    entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir() && p.extension().is_some_and(|x| x == "unpack"))
        .filter(|p| std::fs::remove_dir_all(p).is_ok())
        .count()
}

fn extract_tar_bz2(archive: &Path, dest: &Path) -> anyhow::Result<()> {
    let file = std::fs::File::open(archive)?;
    let decoder = bzip2::read::BzDecoder::new(file);
    let mut tar = tar::Archive::new(decoder);
    tar.unpack(dest)?;
    Ok(())
}

/// A picker model (or support artifact) counts as installed when its folder
/// carries the `.bs-ok` marker, which [`install`] writes only after the
/// download was verified and the whole folder unpacked. Model files alone
/// prove neither.
pub fn dir_installed(dir_name: &str) -> bool {
    dir_installed_in(&models_root(), dir_name)
}

fn dir_installed_in(root: &Path, dir_name: &str) -> bool {
    root.join(dir_name).join(".bs-ok").exists()
}

pub fn file_installed(file_name: &str, expected_bytes: u64) -> bool {
    let path = models_root().join(file_name);
    std::fs::metadata(&path)
        .map(|m| m.len() >= expected_bytes / 2)
        .unwrap_or(false)
}

pub fn delete_dir(dir_name: &str) -> anyhow::Result<()> {
    let dir = models_root().join(dir_name);
    if dir.exists() {
        std::fs::remove_dir_all(&dir)?;
    }
    Ok(())
}

pub fn model_dir(dir_name: &str) -> PathBuf {
    models_root().join(dir_name)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Stands in for the `reqwest::Error` -> `hyper_util` head of the real
    /// chain: two types with no public constructor, whose only relevant
    /// behaviour is an ordinary `source()` hop.
    #[derive(Debug)]
    struct Wrapped(Box<dyn std::error::Error + Send + Sync + 'static>);

    impl std::fmt::Display for Wrapped {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "wrapped: {}", self.0)
        }
    }

    impl std::error::Error for Wrapped {
        fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
            Some(self.0.as_ref())
        }
    }

    /// The exact chain `reqwest` + `rustls-tls` produces against a
    /// certificate it can't validate, reproduced layer for layer: two
    /// nested `io::Error`s between the transport error and the
    /// `rustls::Error` — `tokio-rustls` builds the inner one
    /// (`io::Error::new(InvalidData, rustls_err)`), `hyper-rustls` wraps
    /// that in the outer one.
    fn real_tls_chain(cert: rustls::CertificateError) -> Wrapped {
        let tokio_rustls = std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            rustls::Error::InvalidCertificate(cert),
        );
        let hyper_rustls = std::io::Error::new(std::io::ErrorKind::Other, tokio_rustls);
        Wrapped(Box::new(hyper_rustls))
    }

    #[test]
    fn a_certificate_error_is_classified_as_tls() {
        let err = rustls::Error::InvalidCertificate(rustls::CertificateError::UnknownIssuer);
        assert!(is_tls_error(&err));
    }

    /// THE REGRESSION this pins: `io::Error::source()` returns its
    /// *payload's* source rather than the payload, so a `source()`-only walk
    /// dead-ends at the first `io::Error` and returns false for every real
    /// certificate failure. A fixture that hands the `rustls::Error` straight
    /// to `source()` omits exactly that hop and passes while the feature is
    /// dead. `get_ref()` is what gets past it, and it has to do so twice.
    #[test]
    fn a_certificate_error_is_found_through_the_real_nested_io_error_chain() {
        assert!(is_tls_error(&real_tls_chain(
            rustls::CertificateError::UnknownIssuer
        )));
        assert!(is_tls_error(&real_tls_chain(
            rustls::CertificateError::Expired
        )));
    }

    /// The std behaviour the classification hinges on, pinned directly: an
    /// `io::Error`'s `source()` is its *payload's* source, not the payload,
    /// so the one layer holding the `rustls::Error` is invisible to a
    /// `source()`-only walk. `get_ref()` is the only way to it.
    #[test]
    fn an_io_errors_source_skips_its_own_payload() {
        let io = std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            rustls::Error::InvalidCertificate(rustls::CertificateError::UnknownIssuer),
        );
        assert!(std::error::Error::source(&io).is_none());
        assert!(io.get_ref().is_some());
    }

    /// An SSL-inspecting proxy is a connection-time failure, so the chain
    /// this has to survive is a transport one — but an ordinary transport
    /// failure through the same nested `io::Error` shape must still come out
    /// false, or every refused connection would be blamed on certificates.
    #[test]
    fn a_transport_failure_through_the_same_shape_is_not_tls() {
        let inner = std::io::Error::new(
            std::io::ErrorKind::ConnectionRefused,
            "connection refused",
        );
        let outer = std::io::Error::new(std::io::ErrorKind::Other, inner);
        assert!(!is_tls_error(&Wrapped(Box::new(outer))));
    }

    /// Not every `rustls::Error` reached through the real chain is a
    /// certificate problem either — only `InvalidCertificate` earns the
    /// SSL-inspection message.
    #[test]
    fn a_non_certificate_rustls_error_deep_in_the_chain_is_not_tls() {
        let inner = std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            rustls::Error::NoCertificatesPresented,
        );
        let outer = std::io::Error::new(std::io::ErrorKind::Other, inner);
        assert!(!is_tls_error(&Wrapped(Box::new(outer))));
    }

    /// Not every `rustls::Error` is a certificate problem — only
    /// `InvalidCertificate` gets the SSL-inspection message.
    #[test]
    fn a_non_certificate_rustls_error_is_not_classified_as_tls() {
        let err = rustls::Error::NoCertificatesPresented;
        assert!(!is_tls_error(&err));
    }

    /// Classification is by type, not message text: an ordinary error must
    /// not accidentally match through message-text sniffing.
    #[test]
    fn an_unrelated_error_is_not_classified_as_tls() {
        let err = std::io::Error::new(std::io::ErrorKind::TimedOut, "connection timed out");
        assert!(!is_tls_error(&err));
    }

    /// `reqwest::Error` has no public constructor, so `classify_reqwest_error`
    /// cannot be driven from a test; the sentence it returns is pinned here.
    /// Retrying on the same network fails the same way, so the sentence must
    /// point elsewhere, and it has to fit the model card's one error line.
    #[test]
    fn tls_error_message_names_the_problem_and_a_way_out() {
        assert!(TLS_ERROR_MESSAGE.contains("certificate problem"), "{TLS_ERROR_MESSAGE}");
        assert!(TLS_ERROR_MESSAGE.contains("another network"), "{TLS_ERROR_MESSAGE}");
        let lower = TLS_ERROR_MESSAGE.to_lowercase();
        assert!(!lower.contains("try again") && !lower.contains("retry"), "{TLS_ERROR_MESSAGE}");
        assert!(TLS_ERROR_MESSAGE.chars().count() < 140, "{} chars", TLS_ERROR_MESSAGE.chars().count());
    }

    // -- a server that never answers -----------------------------------------

    /// A loopback listener that accepts a connection and then says nothing,
    /// never closing it: what a network that takes the TCP connection but
    /// never completes TLS or sends headers looks like from here.
    async fn silent_server() -> std::net::SocketAddr {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind loopback listener");
        let addr = listener.local_addr().expect("local addr");
        tokio::spawn(async move {
            let held = listener.accept().await;
            std::future::pending::<()>().await;
            drop(held);
        });
        addr
    }

    /// Waiting on such a server for good would hold the download at
    /// "downloading" until the app restarts, with every retry refused, so
    /// the wait for its answer has a limit.
    #[tokio::test]
    async fn a_server_that_never_answers_cannot_hold_a_download_open() {
        let addr = silent_server().await;
        let cancel = AtomicBool::new(false);
        let outcome = tokio::time::timeout(
            Duration::from_secs(5),
            request(
                &download_client().unwrap(),
                &format!("http://{addr}/m.bin"),
                0,
                &cancel,
                Duration::from_millis(300),
            ),
        )
        .await;
        let err = outcome
            .expect("the request is still waiting on a silent server")
            .expect_err("a server that never answers is a failure");
        assert!(err.to_string().contains("could not start"), "{err:#}");
    }

    /// And Cancel ends that wait at once, long before the limit.
    #[tokio::test]
    async fn cancel_ends_a_request_the_server_never_answers() {
        let addr = silent_server().await;
        let cancel = std::sync::Arc::new(AtomicBool::new(false));
        let pressed = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(200)).await;
            pressed.store(true, Ordering::Relaxed);
        });
        let started = Instant::now();
        let err = request(
            &download_client().unwrap(),
            &format!("http://{addr}/m.bin"),
            0,
            &cancel,
            Duration::from_secs(60),
        )
        .await
        .expect_err("a cancelled request is a failure");
        assert_eq!(err.to_string(), "cancelled");
        assert!(started.elapsed() < Duration::from_secs(5), "{:?}", started.elapsed());
    }

    /// A body that stops arriving is cut off by the same limit, and Cancel
    /// reaches it too.
    #[tokio::test]
    async fn a_stalled_wait_ends_at_its_limit_or_at_cancel() {
        let cancel = AtomicBool::new(false);
        let stalled = std::future::pending::<()>();
        assert_eq!(
            until_cancelled(stalled, Duration::from_millis(200), &cancel).await,
            Err(Interrupted::TimedOut)
        );
        cancel.store(true, Ordering::Relaxed);
        let stalled = std::future::pending::<()>();
        assert_eq!(
            until_cancelled(stalled, Duration::from_secs(60), &cancel).await,
            Err(Interrupted::Cancelled)
        );
        cancel.store(false, Ordering::Relaxed);
        assert_eq!(until_cancelled(async { 7 }, Duration::from_secs(60), &cancel).await, Ok(7));
    }

    // -- checking what arrived ------------------------------------------------

    /// A whole download with the wrong checksum is the server's file having
    /// changed, which another attempt cannot fix, so the message points to an
    /// update rather than a retry. The part goes, so nothing resumes from it.
    #[test]
    fn a_checksum_mismatch_names_an_update_and_deletes_the_part() {
        let root = TempDir::new();
        let part = root.0.join(".staging").join("m.part");
        std::fs::write(&part, b"someone else's file").unwrap();

        let err = verify("m", &sha256_hex(b"someone else's file"), &sha256_hex(b"the model"), &part, false)
            .expect_err("a mismatch is a failure");
        assert_eq!(err.to_string(), CHANGED_UPSTREAM_MESSAGE);
        assert!(!part.exists());
        let lower = CHANGED_UPSTREAM_MESSAGE.to_lowercase();
        assert!(lower.contains("no longer matches") && lower.contains("updating the app"), "{lower}");
        assert!(!lower.contains("retry") && !lower.contains("try again") && !lower.contains("corrupt"), "{lower}");
        assert!(CHANGED_UPSTREAM_MESSAGE.chars().count() < 140);

        std::fs::write(&part, b"the model").unwrap();
        verify("m", &sha256_hex(b"the model"), &sha256_hex(b"the model"), &part, false).unwrap();
        assert!(part.exists(), "a verified part stays for the install");
    }

    /// A resumed download that does not match may owe it to its earlier
    /// bytes, not to the server, so the message asks for a fresh download
    /// instead of blaming the server's file.
    #[test]
    fn a_resumed_mismatch_asks_for_a_fresh_download() {
        let root = TempDir::new();
        let part = root.0.join(".staging").join("m.part");
        std::fs::write(&part, b"good start\0\0\0\0").unwrap();

        let err = verify("m", &sha256_hex(b"good start\0\0\0\0"), &sha256_hex(b"the model"), &part, true)
            .expect_err("a mismatch is a failure");
        assert_eq!(err.to_string(), RESUMED_MISMATCH_MESSAGE);
        assert!(!part.exists(), "the bad part is deleted so the next download starts clean");
        let lower = RESUMED_MISMATCH_MESSAGE.to_lowercase();
        assert!(lower.contains("download it again") && !lower.contains("server"), "{lower}");
    }

    // -- a part left behind, and installing -----------------------------------

    /// A folder of its own under the temp directory, removed on drop.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            let dir = std::env::temp_dir().join(format!("bs-models-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(dir.join(".staging")).unwrap();
            Self(dir)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn sha256_hex(bytes: &[u8]) -> String {
        let mut h = Sha256::new();
        h.update(bytes);
        format!("{:x}", h.finalize())
    }

    /// A tar.bz2 holding `dir/model.int8.onnx`.
    fn archive_with(dir: &str) -> Vec<u8> {
        let mut out = Vec::new();
        {
            let enc = bzip2::write::BzEncoder::new(&mut out, bzip2::Compression::default());
            let mut tar = tar::Builder::new(enc);
            let data = b"onnx bytes";
            let mut header = tar::Header::new_gnu();
            header.set_size(data.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            tar.append_data(&mut header, format!("{dir}/model.int8.onnx"), &data[..])
                .unwrap();
            tar.into_inner().unwrap().finish().unwrap();
        }
        out
    }

    /// A part as long as the file has nothing left to fetch: a Range past its
    /// end is answered 416, so such a part goes to verification instead of
    /// to the server.
    #[test]
    fn a_part_as_long_as_the_file_is_whole() {
        assert!(part_is_whole(100, 100));
        assert!(part_is_whole(120, 100));
        assert!(!part_is_whole(99, 100));
        assert!(!part_is_whole(0, 100));
        assert!(!part_is_whole(10, 0), "no size in the catalog proves nothing");
    }

    /// A whole part goes straight to install only when it is the right file.
    #[test]
    fn a_whole_part_is_installed_only_when_its_digest_matches() {
        let mut h = Sha256::new();
        h.update(b"the model");
        assert!(digest_matches(&h, &sha256_hex(b"the model")));
        assert!(!digest_matches(&h, &sha256_hex(b"another model")));
        assert!(!digest_matches(&h, ""), "bytes with no checksum are not trusted");
        // Peeking does not consume the running hash.
        h.update(b"!");
        assert!(digest_matches(&h, &sha256_hex(b"the model!")));
    }

    #[test]
    fn a_verified_archive_lands_whole_and_marked() {
        let root = TempDir::new();
        let staging = root.0.join(".staging");
        let part = staging.join("m.part");
        let bytes = archive_with("model-dir");
        std::fs::write(&part, &bytes).unwrap();
        let kind = JobKind::Archive {
            dir_name: "model-dir".into(),
        };
        install(&root.0, &staging, &part, "m", &kind, &sha256_hex(&bytes)).unwrap();
        assert!(root.0.join("model-dir/model.int8.onnx").is_file());
        assert!(dir_installed_in(&root.0, "model-dir"));
        assert!(!part.exists());
        assert!(!staging.join("m.unpack").exists());
    }

    /// An extraction that fails leaves nothing that looks installed and no
    /// part to meet again on the next Download.
    #[test]
    fn a_failed_extraction_deletes_the_part_and_installs_nothing() {
        let root = TempDir::new();
        let staging = root.0.join(".staging");
        let part = staging.join("m.part");
        std::fs::write(&part, b"not an archive").unwrap();
        let kind = JobKind::Archive {
            dir_name: "model-dir".into(),
        };
        assert!(install(&root.0, &staging, &part, "m", &kind, "x").is_err());
        assert!(!part.exists());
        assert!(!dir_installed_in(&root.0, "model-dir"));

        // The archive's folder under another name is a failure too.
        let bytes = archive_with("other-dir");
        std::fs::write(&part, &bytes).unwrap();
        assert!(install(&root.0, &staging, &part, "m", &kind, "x").is_err());
        assert!(!root.0.join("other-dir").exists());
        assert!(!dir_installed_in(&root.0, "model-dir"));
    }

    /// What an install interrupted by a quit leaves in staging is swept; a
    /// part a Download can resume from is kept.
    #[test]
    fn leftover_unpack_folders_are_swept_and_parts_kept() {
        let root = TempDir::new();
        let staging = root.0.join(".staging");
        std::fs::create_dir_all(staging.join("a.unpack/model-dir")).unwrap();
        std::fs::create_dir_all(staging.join("b.unpack")).unwrap();
        std::fs::write(staging.join("c.part"), b"half a download").unwrap();
        assert_eq!(sweep_unpacks_in(&staging), 2);
        assert!(!staging.join("a.unpack").exists());
        assert!(!staging.join("b.unpack").exists());
        assert!(staging.join("c.part").is_file());
        assert_eq!(sweep_unpacks_in(&root.0.join("missing")), 0);
    }

    /// Model files without the marker are a folder something else put there
    /// or an extraction that never finished; neither is an install.
    #[test]
    fn model_files_without_the_marker_are_not_installed() {
        let root = TempDir::new();
        let dir = root.0.join("model-dir");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("encoder.int8.onnx"), b"half").unwrap();
        assert!(!dir_installed_in(&root.0, "model-dir"));
        std::fs::write(dir.join(".bs-ok"), "hash").unwrap();
        assert!(dir_installed_in(&root.0, "model-dir"));
    }
}
