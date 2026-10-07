//! Learning from the user's own corrections.
//!
//! When the user fixes a misheard word by hand in text the app just pasted,
//! the app can learn to write that word right next time. Three parts:
//!
//! - [`monitor`] watches the field a paste went into for a short while and
//!   reports the words the user replaced there.
//! - [`diff`] compares the pasted text with the field and decides which
//!   replaced words look like fixes of a mishearing.
//! - [`candidates`] counts each fix across dictations and turns one into an
//!   automatic replacement rule once it has been made in two separate
//!   pastes; the user can take the rule back under Dictionary → Corrections.
//!
//! Edits to a dictation in the Home feed are learned from separately, in the
//! webview (`src/lib/learn.ts`), with the same thresholds.

pub mod candidates;
pub mod diff;
pub mod monitor;
