//! Butterfly Labs sign-in: PKCE through the system browser, the return trip on
//! the `butterflylabs://` scheme, and the refresh token in Windows Credential
//! Manager.
//!
//! The split is by lifetime, not by layer. [`pkce`] is pure arithmetic over a
//! random 32 bytes and has no state at all. [`session`] owns everything that
//! outlives a single call — the pending verifier, the access token, the stored
//! refresh token — and is the only place any of the three is spoken about.
//! [`commands`] is the thin webview-facing shell plus the deep-link listener,
//! and deliberately hands the webview no token of any kind.
//!
//! Why the browser and not a webview: a WebView2 login page would put this app
//! between the user and their Google password, hide the address bar they are
//! meant to check, and cut off every password manager and passkey on the
//! machine. Google blocks embedded-webview OAuth for exactly that reason.

pub mod commands;
pub mod pkce;
pub mod session;
