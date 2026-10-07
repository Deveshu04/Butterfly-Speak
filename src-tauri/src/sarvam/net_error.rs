//! Errno → prose: a typed classification of "why can't we reach Sarvam" so
//! the overlay can name the likely cause instead of one generic message, and
//! so `ws::ConnectBackoff` can tell a blip worth a short automatic retry from
//! a failure that will not clear on its own.
//!
//! Classification is on typed discriminants only — `std::io::ErrorKind`,
//! `raw_os_error()` for the Winsock codes `ErrorKind` doesn't have a variant
//! for, and `tokio_tungstenite`'s own `Error` enum — never on `.to_string()`
//! text, which varies by OS locale and library version.

use tokio_tungstenite::tungstenite::Error as WsError;

/// Why a connection never got an answer: four causes this app can name (the
/// name lookup, a refusal, a timeout, the TLS handshake), a server that
/// answered with a 5xx, and everything else that is plainly a network
/// failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NetFailure {
    /// The host name never became an address. Far more often a PC with no
    /// network, or something on this machine or network declining the lookup
    /// (a DNS filter, an ad blocker, a VPN's resolver), than an outage.
    NameNotResolved,
    /// The connection was refused or reset, which a firewall or VPN usually
    /// explains.
    Refused,
    /// Nothing answered within the OS's or this app's own deadline.
    Timeout,
    /// The TLS handshake failed. The usual causes are a clock set far enough
    /// off that the server's certificate looks invalid, and a proxy that
    /// re-signs traffic with a certificate of its own.
    Tls,
    /// The server answered but said "not right now" (a 5xx) — the service,
    /// not the network, and worth a short automatic retry rather than a
    /// permanent-looking error.
    ServiceUnavailable,
    /// Some other network-level failure. Still a real "couldn't reach
    /// Sarvam", just not one this table can say anything more specific
    /// about.
    Other,
}

impl NetFailure {
    /// The sentence the overlay shows for this failure.
    pub fn user_message(self) -> &'static str {
        match self {
            NetFailure::NameNotResolved => {
                "Couldn't look up Sarvam's address — you may be offline, or a DNS filter or VPN is blocking it"
            }
            NetFailure::Refused => {
                "Sarvam refused the connection — a firewall or VPN may be in the way"
            }
            NetFailure::Timeout => "Sarvam didn't answer in time — check your network or VPN",
            NetFailure::Tls => {
                "Sarvam's certificate wasn't accepted — check this PC's date, or a network that inspects traffic"
            }
            NetFailure::ServiceUnavailable => "Sarvam is temporarily unavailable — try again in a moment",
            NetFailure::Other => "Couldn't reach Sarvam — check your internet connection",
        }
    }

    /// [`user_message`](Self::user_message) for a socket that is not talking
    /// to Sarvam.
    ///
    /// Cloud mode puts Butterfly Labs' relay in front of the same realtime
    /// socket, and these six sentences are the most likely thing a Cloud user
    /// ever sees — they are what an aeroplane, a captive portal or a
    /// corporate proxy produces. Naming Sarvam there names a company that
    /// user has no account with and no way to fix.
    ///
    /// Only the host differs; `the_two_forms_say_the_same_thing_about_sarvam`
    /// pins that, so the copy below cannot drift from the one above. The
    /// other two callers of `user_message` (`sarvam::translate`,
    /// `sarvam::batch_job`) are Sarvam-only routes and are deliberately left
    /// on it.
    pub fn user_message_for(self, host: &str) -> String {
        // "Sarvam's" but "Butterfly Labs'": a name that already ends in an s
        // takes the bare apostrophe.
        let owner = if host.ends_with('s') {
            format!("{host}'")
        } else {
            format!("{host}'s")
        };
        match self {
            NetFailure::NameNotResolved => format!(
                "Couldn't look up {owner} address — you may be offline, or a DNS filter or VPN is blocking it"
            ),
            NetFailure::Refused => {
                format!("{host} refused the connection — a firewall or VPN may be in the way")
            }
            NetFailure::Timeout => {
                format!("{host} didn't answer in time — check your network or VPN")
            }
            NetFailure::Tls => format!(
                "{owner} certificate wasn't accepted — check this PC's date, or a network that inspects traffic"
            ),
            NetFailure::ServiceUnavailable => {
                format!("{host} is temporarily unavailable — try again in a moment")
            }
            NetFailure::Other => {
                format!("Couldn't reach {host} — check your internet connection")
            }
        }
    }

    /// Worth `ws::ConnectBackoff` scheduling a short, bounded pause before
    /// the next utterance's connect attempt. `NameNotResolved` and `Tls` are
    /// left out: each nearly always comes from a setting on this machine or
    /// network (a filter, a clock, a proxy) that stays put while the user
    /// waits, so an automatic retry would only delay the same failure.
    pub fn is_retryable(self) -> bool {
        matches!(
            self,
            NetFailure::Refused | NetFailure::Timeout | NetFailure::ServiceUnavailable | NetFailure::Other
        )
    }
}

/// Classify a failure from `tokio_tungstenite::connect_async`. The
/// `WsError::Http` case (a real HTTP response with a status code, e.g. 401
/// or a bad-parameter 400) is handled by `ws::run_session` itself before
/// this is ever reached — those are answers from Sarvam, not network
/// failures — so this only covers the cases that mean the request never got
/// a response at all.
pub fn classify_ws_error(e: &WsError) -> NetFailure {
    match e {
        WsError::Io(io_err) => classify_io_error(io_err),
        WsError::Tls(_) => NetFailure::Tls,
        _ => NetFailure::Other,
    }
}

/// Classify a bare `io::Error`, e.g. from a `tokio::time::timeout` wrapper
/// firing on its own (no inner error to inspect) or a lower-level socket
/// failure.
///
/// Checked before the `ErrorKind` match below, not after: this build's TLS
/// stack (`tokio-tungstenite`'s `rustls-tls-webpki-roots` feature) never
/// produces `WsError::Tls` for a failed handshake — in
/// `tokio-tungstenite-0.24.0/src/tls.rs`'s rustls branch, `connect_async`
/// maps every handshake failure through `Error::Io`, so it always arrives
/// here, as a bare `io::Error`. And `tokio-rustls` reports the failure as
/// `io::ErrorKind::InvalidData` (`tokio-rustls-0.26.4/src/common/mod.rs:115`)
/// — a kind with no dedicated arm below, and not a signal exact enough to
/// give one of its own (`InvalidData` covers plenty of non-TLS failures too)
/// — wrapping the actual `rustls::Error`, which `is_tls_error` below finds.
pub fn classify_io_error(e: &std::io::Error) -> NetFailure {
    use std::io::ErrorKind;
    if is_tls_error(e) {
        return NetFailure::Tls;
    }
    match e.kind() {
        ErrorKind::ConnectionRefused | ErrorKind::ConnectionReset | ErrorKind::ConnectionAborted => {
            NetFailure::Refused
        }
        ErrorKind::TimedOut => NetFailure::Timeout,
        _ => classify_by_os_error(e),
    }
}

/// Whether `err`'s source chain contains a TLS handshake failure, found by
/// the `rustls::Error` type, never by message text (the same technique as
/// `models::downloader::is_tls_error`). An `io::Error`'s `source()` returns
/// its *payload's* source rather than the payload itself, so a
/// `source()`-only walk dead-ends at the first `io::Error` in the chain
/// (`tokio-rustls` wraps the `rustls::Error` in exactly one); `get_ref()` is
/// the only way past. Unlike `downloader::is_tls_error`, this does not narrow
/// to `InvalidCertificate`: any failed handshake is `NetFailure::Tls`.
fn is_tls_error(err: &(dyn std::error::Error + 'static)) -> bool {
    let mut cur: Option<&(dyn std::error::Error + 'static)> = Some(err);
    while let Some(e) = cur {
        if e.downcast_ref::<rustls::Error>().is_some() {
            return true;
        }
        // Prefer the payload over `source()` for an `io::Error`: the payload
        // is the layer below, and `source()` skips straight past it (see
        // this function's own doc comment).
        cur = match e.downcast_ref::<std::io::Error>().and_then(|io| io.get_ref()) {
            Some(payload) => Some(payload),
            None => e.source(),
        };
    }
    false
}

/// Rust's `ErrorKind` has no variant for "DNS resolution failed" — a
/// getaddrinfo failure surfaces as a bare OS error with no more specific
/// kind. The Winsock codes below are the actual, stable numeric
/// discriminants (not text), verified against Microsoft's WinSock error
/// code reference: `WSAHOST_NOT_FOUND` (11001), `WSATRY_AGAIN` (11002),
/// `WSANO_RECOVERY` (11003) and `WSANO_DATA` (11004) are every "the name
/// server says this host doesn't exist / won't answer" outcome
/// `getaddrinfo` can report on Windows.
fn classify_by_os_error(e: &std::io::Error) -> NetFailure {
    const WSAHOST_NOT_FOUND: i32 = 11001;
    const WSATRY_AGAIN: i32 = 11002;
    const WSANO_RECOVERY: i32 = 11003;
    const WSANO_DATA: i32 = 11004;
    match e.raw_os_error() {
        Some(WSAHOST_NOT_FOUND) | Some(WSATRY_AGAIN) | Some(WSANO_RECOVERY) | Some(WSANO_DATA) => {
            NetFailure::NameNotResolved
        }
        _ => NetFailure::Other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Error as IoError, ErrorKind};

    #[test]
    fn connection_refused_is_refused_and_retryable() {
        let e = WsError::Io(IoError::from(ErrorKind::ConnectionRefused));
        assert_eq!(classify_ws_error(&e), NetFailure::Refused);
        assert!(classify_ws_error(&e).is_retryable());
    }

    #[test]
    fn connection_reset_is_refused() {
        let e = WsError::Io(IoError::from(ErrorKind::ConnectionReset));
        assert_eq!(classify_ws_error(&e), NetFailure::Refused);
    }

    #[test]
    fn timed_out_kind_is_timeout_and_retryable() {
        let e = WsError::Io(IoError::from(ErrorKind::TimedOut));
        assert_eq!(classify_ws_error(&e), NetFailure::Timeout);
        assert!(classify_ws_error(&e).is_retryable());
    }

    #[test]
    fn a_winsock_name_lookup_code_is_name_not_resolved_and_not_retryable() {
        for code in [11001, 11002, 11003, 11004] {
            let e = WsError::Io(IoError::from_raw_os_error(code));
            assert_eq!(
                classify_ws_error(&e),
                NetFailure::NameNotResolved,
                "code {code}"
            );
            assert!(!classify_ws_error(&e).is_retryable(), "code {code}");
        }
    }

    /// The real chain this build's stack produces:
    /// `tokio_tungstenite::connect_async`'s rustls branch never returns
    /// `WsError::Tls` for a failed handshake (`tls.rs`'s `Err(e) =>
    /// Err(Error::Io(e))`) — it always arrives here as `WsError::Io`,
    /// wrapping the exact `io::Error::new(InvalidData, rustls_err)` shape
    /// `tokio-rustls` itself constructs. A corporate proxy doing TLS
    /// inspection with an untrusted root is exactly this case.
    #[test]
    fn a_tls_handshake_failure_wrapped_the_way_tokio_rustls_wraps_it_is_classified_as_tls() {
        let rustls_err = rustls::Error::InvalidCertificate(rustls::CertificateError::UnknownIssuer);
        let io_err = IoError::new(ErrorKind::InvalidData, rustls_err);
        let e = WsError::Io(io_err);
        assert_eq!(classify_ws_error(&e), NetFailure::Tls);
        assert!(!classify_ws_error(&e).is_retryable());
    }

    /// Not every `rustls::Error` reaching a handshake failure is a
    /// certificate problem, but this classification (unlike
    /// `models::downloader::is_tls_error`'s narrower one) does not need to
    /// tell them apart — `NetFailure::Tls` already covers "the TLS handshake
    /// itself failed" generically.
    #[test]
    fn a_non_certificate_rustls_error_is_still_classified_as_tls() {
        let io_err = IoError::new(ErrorKind::InvalidData, rustls::Error::NoCertificatesPresented);
        let e = WsError::Io(io_err);
        assert_eq!(classify_ws_error(&e), NetFailure::Tls);
    }

    /// An ordinary `InvalidData` `io::Error` with no `rustls::Error`
    /// anywhere in its source chain must not be swept into `Tls` just
    /// because it shares the same `ErrorKind` — the classification is the
    /// discriminant, never the kind alone.
    #[test]
    fn an_invaliddata_io_error_without_a_rustls_source_is_not_tls() {
        let io_err = IoError::new(ErrorKind::InvalidData, "not a TLS problem");
        let e = WsError::Io(io_err);
        assert_ne!(classify_ws_error(&e), NetFailure::Tls);
    }

    /// `io::Error::source()` returns its *payload's* source, not the payload
    /// itself — a `source()`-only walk would dead-end here and never see the
    /// `rustls::Error` this pins directly, the same trap
    /// `models::downloader::is_tls_error`'s own regression test documents.
    #[test]
    fn an_io_errors_source_skips_its_own_payload() {
        let io_err = IoError::new(
            ErrorKind::InvalidData,
            rustls::Error::InvalidCertificate(rustls::CertificateError::UnknownIssuer),
        );
        assert!(std::error::Error::source(&io_err).is_none());
        assert!(io_err.get_ref().is_some());
    }

    /// A Winsock code that is neither one of the four DNS codes nor one
    /// `std::io::Error::kind()` already maps to a specific `ErrorKind` on
    /// its own (unlike, say, `WSAECONNREFUSED` (10061), which Windows'
    /// own `ErrorKind` translation recognizes as `ConnectionRefused`
    /// before `classify_by_os_error` is ever reached) must not be swept
    /// into `NameNotResolved` — the match is exact codes, not a range.
    #[test]
    fn an_unrelated_os_error_code_is_other() {
        let e = WsError::Io(IoError::from_raw_os_error(999_999));
        assert_eq!(classify_ws_error(&e), NetFailure::Other);
    }

    #[test]
    fn an_unclassified_io_error_is_other_and_still_retryable_by_default() {
        let e = WsError::Io(IoError::from(ErrorKind::BrokenPipe));
        assert_eq!(classify_ws_error(&e), NetFailure::Other);
        assert!(classify_ws_error(&e).is_retryable());
    }

    #[test]
    fn every_failure_has_a_distinct_user_message() {
        let all = [
            NetFailure::NameNotResolved,
            NetFailure::Refused,
            NetFailure::Timeout,
            NetFailure::Tls,
            NetFailure::ServiceUnavailable,
            NetFailure::Other,
        ];
        let messages: std::collections::HashSet<&str> = all.iter().map(|f| f.user_message()).collect();
        assert_eq!(messages.len(), all.len(), "every variant must say something different");
    }

    /// The two forms are one table written twice, which is a drift waiting to
    /// happen — so the drift is what this test forbids. Told the host it has
    /// always assumed, `user_message_for` must produce `user_message` to the
    /// byte, on every variant.
    #[test]
    fn the_two_forms_say_the_same_thing_about_sarvam() {
        for failure in [
            NetFailure::NameNotResolved,
            NetFailure::Refused,
            NetFailure::Timeout,
            NetFailure::Tls,
            NetFailure::ServiceUnavailable,
            NetFailure::Other,
        ] {
            assert_eq!(
                failure.user_message_for("Sarvam"),
                failure.user_message(),
                "{failure:?}"
            );
        }
    }

    #[test]
    fn dns_and_tls_are_not_retryable_a_few_seconds_will_not_fix_them() {
        assert!(!NetFailure::NameNotResolved.is_retryable());
        assert!(!NetFailure::Tls.is_retryable());
    }
}
