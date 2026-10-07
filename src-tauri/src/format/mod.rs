//! The formatting engine: turning a raw transcript into text a person would
//! have typed. Transport, tuning and validation live here; the deterministic
//! rule stages remain in `crate::cleanup`.

pub mod backend;
pub mod guard;
pub mod level;
/// The shipped-prompt hash ratchet. `cfg(test)` in full: it exists to fail a
/// build, not to run in one, and its table would otherwise be dead weight in
/// every shipped binary.
#[cfg(test)]
mod prompt_ratchet;
pub mod timing;
