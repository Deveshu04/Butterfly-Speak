//! PKCE (RFC 7636) verifier and S256 challenge.
//!
//! The verifier is the one secret in the sign-in flow that never leaves this
//! process. The authorization code travels back over a custom URL scheme any
//! program on the machine could in principle claim, so the code alone has to
//! be worthless: GoTrue will only exchange it for a token when the caller can
//! also present the verifier whose SHA-256 it saw at `/authorize` time.

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use sha2::{Digest, Sha256};

/// A fresh code verifier: 32 random bytes, base64url without padding.
///
/// 43 characters — the shortest length RFC 7636 §4.1 allows, and 256 bits of
/// entropy, which is the number that matters. Base64url's alphabet is a subset
/// of RFC 3986's `unreserved` set, so the result needs no further escaping in
/// the token request body.
pub fn verifier() -> String {
    let mut bytes = [0u8; 32];
    // A verifier built from a weak source would let anyone who sees the
    // authorization code guess it, which is the whole attack PKCE exists to
    // stop — so a failure here is not something to paper over with a
    // fallback. `getrandom` reads the OS CSPRNG (BCryptGenRandom on Windows)
    // and only fails if that is unavailable, which no running desktop is.
    getrandom::fill(&mut bytes).expect("the OS random source");
    URL_SAFE_NO_PAD.encode(bytes)
}

/// The S256 challenge for `verifier`: base64url-nopad(SHA-256(ASCII(verifier))).
pub fn challenge(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// RFC 7636 appendix B vector.
    #[test]
    fn the_s256_challenge_matches_the_rfc_vector() {
        assert_eq!(
            challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn a_verifier_is_43_to_128_unreserved_chars_and_unique() {
        let v = verifier();
        assert!((43..=128).contains(&v.len()));
        assert!(v
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-._~".contains(c)));
        assert_ne!(v, verifier());
    }
}
