//! Where a finished dictation goes.
//!
//! Every dictation carries an *intent*, stamped by the chord that started it:
//! the push-to-talk chord means "clean this up", the translate chord means
//! "clean it up and translate it", the voice-agent chord means "this is a
//! command addressed to the assistant". The intent is decided once, at
//! chord-down (`resolve`), and carried through recording and finalization;
//! `apply` is the single seam in the controller's `FinalResult` arm where the
//! intent becomes text — or a deliberate refusal to type anything.
//!
//! ## Why resolution is a separate, pure function
//!
//! The failure it exists to rule out: a command addressed to the agent that
//! falls through to the cleanup model when the agent is unreachable. The
//! cleanup prompt is hardened never to carry out a dictated instruction, so
//! "Butterfly, delete that paragraph" would come back polished into prose and
//! be typed into the user's document, with nothing shown or logged.
//!
//! The rule below has two lines and no silent branch:
//!
//! - **Translate keeps the words.** If the translator can't be used, the
//!   cleaned text is pasted in the language it was spoken, and a notice says
//!   the translation was skipped.
//! - **The agent has no fallback.** A command gets the agent's answer, or
//!   nothing is typed at all and a notice says why.
//!
//! ## The one route that is decided late
//!
//! Everything above is settled at chord-down. The wake word cannot be: whether
//! a dictation addressed the agent is a fact about the words, and the words do
//! not exist yet. So `apply`'s `Cleanup` arm asks `wake::upgrade` once, on the
//! verbatim transcript, and a detection is handed to the same `agent::apply`
//! the chord uses, so the agent's no-fallback rule above covers the wake phrase
//! too. It is off by default and only the plain push-to-talk chord is ever
//! scanned; `routes::wake` holds both gates and the reasoning.
//!
//! Nothing here ever logs transcript text. `apply` sees both the cleaned and
//! the verbatim string; its `tracing` calls carry lengths and configuration,
//! never content.
//!
//! ## Deferred routes
//!
//! `apply` is synchronous and runs on the controller thread, which is the
//! single consumer of `ControlMsg` — a blocking network call there stalls the
//! pill, the hotkeys' effects and every subsequent chord. Routes that need
//! the network therefore return `RouteOutcome::Deferred(RouteJob)`: the
//! controller spawns the job on the app's async runtime, keeps waiting inside
//! the finalize state it is already in (under the watchdog already counting
//! for that dictation), and applies the answer — delivered as
//! `ControlMsg::RouteResult` — through the identical path a `Ready` outcome
//! takes, so no route has to touch the state machine.

mod agent;
pub mod selection;
pub mod translate;
mod wake;

use std::future::Future;
use std::pin::Pin;

/// Which chord started a dictation. The hook stamps this at chord-down;
/// `resolve` turns it into a `Route` once the config is known.
///
/// Deliberately distinct from `Route`: what the user *asked for* and what
/// the app can actually *do* are different questions, and conflating them is
/// how a skipped step goes unnoticed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ChordKind {
    /// The ordinary push-to-talk / hands-free dictation chord.
    #[default]
    Dictation,
    /// "Translate dictation" — dictate, then translate before pasting.
    Translate,
    /// "Voice agent" — the dictation is a command addressed to the assistant.
    Agent,
}

impl ChordKind {
    /// The route this chord asks for, before `resolve` weighs it against what
    /// the config can actually do.
    ///
    /// This — not the resolved route — is what a history row's `route` stores:
    /// the column exists so a future "retry this the same way" affordance can
    /// re-run what the user *meant*. A translate dictation whose translation
    /// was skipped because no key was stored should run as a translation once
    /// there is one; running it as plain dictation would only repeat what
    /// went wrong.
    pub fn intent(self) -> Route {
        match self {
            ChordKind::Dictation => Route::Cleanup,
            ChordKind::Translate => Route::Translation,
            ChordKind::Agent => Route::Agent,
        }
    }
}

/// What a finished dictation is put through. `Cleanup` is the plain
/// dictation path and the default for anything unstamped.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Route {
    /// Rule cleanup + AI formatting only — the historical single route.
    #[default]
    Cleanup,
    /// Cleanup, then translate into `translation.target_language`.
    Translation,
    /// The transcript is a command for the voice agent, not text to type.
    Agent,
}

impl Route {
    /// The value stored in a history row's `route`.
    ///
    /// `None` (SQL NULL) for `Cleanup`: the column records a deliberate
    /// *non-default* intent, so a future "retry this the same way" affordance
    /// can tell "the user asked for a translation" from "this is just a
    /// dictation" without every historical row having to be backfilled.
    pub fn history_label(self) -> Option<&'static str> {
        match self {
            Route::Cleanup => None,
            Route::Translation => Some("translation"),
            Route::Agent => Some("agent"),
        }
    }
}

/// A short user-facing line for the overlay pill's error flash
/// (`controller::error_flash`), which is the only channel this app has for
/// "what you asked for is not what happened".
///
/// An alias rather than a newtype on purpose: every consumer — `error_flash`,
/// `ControlMsg::FinalResult`'s own `notice`, `events::NoticePayload` — already
/// speaks `String`, and a wrapper would earn nothing but `.0`s at each seam.
pub type Notice = String;

/// Shown when the translate chord fired but there is nothing to translate
/// with. The dictation is still pasted — cleaned, in the language it was
/// spoken — which is why this is a notice and not a refusal.
pub const TRANSLATOR_UNAVAILABLE_NOTICE: &str = "Translation unavailable — cleaned instead";

/// Shown when the voice agent cannot run. Nothing is typed: a command the
/// agent never saw must not reach the document as polished prose.
pub const AGENT_UNAVAILABLE_NOTICE: &str = "Voice agent unavailable — nothing typed";

/// What this install can reach at the moment a chord goes down.
///
/// Two facts rather than one because a custom endpoint separates them: an
/// install can have a working chat backend and no Sarvam key at all, or a
/// Sarvam key and a broken endpoint. Passed as a struct so neither can be
/// handed in as the other at a call site: with one `bool` for both, a
/// custom-endpoint install without a Sarvam key was told the agent was
/// unavailable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Capable {
    /// A Sarvam API key is stored. `sarvam::translate` is a Sarvam-only
    /// endpoint, so this is the translate chord's own requirement.
    pub sarvam_key: bool,
    /// Some chat backend resolves — Sarvam, or the custom endpoint
    /// (`endpoint::chat_backend_exists`).
    pub chat: bool,
}

/// The intent a dictation starts with, plus whatever the user needs to be
/// told about how it was reinterpreted.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Resolved {
    pub route: Route,
    /// Set only when the resolved route is not the one the chord asked for,
    /// or when the asked-for route is known to be unable to run. Flashed when
    /// the dictation finishes, not at chord-down — the user is mid-press
    /// there, and the pill is showing the recording.
    pub notice: Option<Notice>,
}

/// Decide what a dictation started by `chord` will actually be put through.
///
/// Pure and total, so the whole table is a unit test rather than something
/// only reachable with a live API key and a bound chord.
///
/// - `capable` — which halves of the cloud this install can actually reach.
///   The two routes need different things and it took custom endpoints to
///   make that visible: translation is a *Sarvam-specific* endpoint
///   (`sarvam::translate`, its own model and its own request shape), so it
///   needs a Sarvam key and nothing else can stand in; the agent is an
///   ordinary chat completion, so it runs on the user's own endpoint just as
///   well.
/// - `target_set` — `translation.target_language` is non-blank. A blank
///   target is how "translation is not configured" is expressed; there is no
///   separate enable flag to disagree with it.
pub fn resolve(chord: ChordKind, capable: Capable, target_set: bool) -> Resolved {
    let Capable {
        sarvam_key: key_present,
        chat: chat_ready,
    } = capable;
    match chord {
        ChordKind::Dictation => Resolved {
            route: Route::Cleanup,
            notice: None,
        },
        ChordKind::Translate => {
            if key_present && target_set {
                Resolved {
                    route: Route::Translation,
                    notice: None,
                }
            } else {
                // The translation is skipped and the cleaned words go out,
                // always with a notice.
                Resolved {
                    route: Route::Cleanup,
                    notice: Some(TRANSLATOR_UNAVAILABLE_NOTICE.into()),
                }
            }
        }
        // Stays `Agent` even when it cannot run: `apply` refuses to produce
        // text for an agent route, and that refusal is the whole point.
        // Resolving to `Cleanup` here would type the command as dictation.
        ChordKind::Agent => Resolved {
            route: Route::Agent,
            notice: (!chat_ready).then(|| AGENT_UNAVAILABLE_NOTICE.into()),
        },
    }
}

/// Everything a route module needs to turn a transcript into its output.
///
/// Borrowed, not owned: it is built at the finalize seam from state the
/// controller already holds, used once, and dropped. Extend it freely —
/// adding a field is a one-line change for every route.
pub struct RouteCtx<'a> {
    /// Which chord started this dictation.
    ///
    /// Carried alongside the resolved `Route` rather than derived from it
    /// because exactly one decision needs the difference and gets it wrong
    /// otherwise: the wake-word scan (`wake::upgrade`), which may only run on
    /// a dictation the user asked to be *plain*. A translate chord with no key
    /// resolves to `Route::Cleanup` too, and its words are text for the
    /// document whatever they say.
    pub chord: ChordKind,
    /// The app's shared HTTP client, so a route call reuses the TLS pool
    /// rather than paying a fresh handshake (see `Controller::http`).
    ///
    /// Cloned into the deferred job rather than borrowed: `reqwest::Client`
    /// is `Arc`-backed, so the clone is a refcount bump and the pool is the
    /// same one.
    pub http: &'a reqwest::Client,
    /// The Sarvam key cache. A handle rather than a copy: the key can be
    /// rotated from Settings while a dictation is in flight.
    pub api_key: &'a crate::sarvam::SharedKey,
    /// The settings snapshot taken for this dictation, so a mid-dictation
    /// change cannot tear a route in half.
    pub settings: &'a crate::settings::Settings,
    /// The language to translate into, already resolved from settings (and,
    /// later, from any per-app rule).
    pub target_language: &'a str,
    /// What the user calls the voice agent.
    pub agent_name: &'a str,
    /// What the selection lane found when the agent chord was released, or
    /// `None` when no capture was attempted — which is every chord but the
    /// agent's, since a plain dictation must never read the user's selection.
    ///
    /// Borrowed like everything else here: the selection is the user's
    /// document content and gets read once, by the one route that asked for
    /// it. See `selection::plan` for what each outcome means.
    pub selection: Option<&'a selection::Capture>,
}

impl RouteCtx<'_> {
    /// The stored Sarvam key, if there is one. Never logged.
    pub fn api_key(&self) -> Option<String> {
        self.api_key.read().ok().and_then(|k| k.clone())
    }
}

/// What a deferred route eventually produced — `RouteOutcome::Ready`'s two
/// fields delivered later, plus the one thing only a deferred route can say.
///
/// Becomes `ControlMsg::RouteResult` verbatim; the controller applies it
/// through the identical code path a `Ready` outcome takes, so a route that
/// had to go to the network and one that did not are indistinguishable from
/// the paste's point of view.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RouteDone {
    pub text: Option<String>,
    pub notice: Option<Notice>,
    /// The route already put `text` into the document itself, so the caller
    /// must file the row and stop — pasting again would type it twice.
    ///
    /// Exactly one route sets this: the selection lane's verify-then-replace
    /// (`agent::apply` under `Plan::EditSelection`). It cannot hand the
    /// replacement back to be pasted the ordinary way, for two reasons that
    /// are both load-bearing. The paste has to land *inside* the window the
    /// verification opened — a re-read that proves the selection is still
    /// live, then a controller round trip, then a paste, is a promise about a
    /// document that has had time to change. And the ordinary path runs tone
    /// and smart-space over what it types, both of which are shaped for text
    /// arriving at a caret; a replacement is the model's exact bytes standing
    /// in for a span the user chose, and a trailing space appended to it is a
    /// character they did not ask for.
    ///
    /// `false` for every other route and for `Default`, so nothing that never
    /// touches the document has to know this exists.
    pub pasted: bool,
}

/// Work a route needs done off the controller thread.
///
/// The controller is the single consumer of `ControlMsg`: the pill, the
/// hotkeys' effects, `InjectionDone` and every subsequent chord are all
/// behind one `recv` loop, so a blocking network call at the route seam
/// stalls the entire app for the length of the request. A job is handed to
/// the app's async runtime instead and answers with `ControlMsg::RouteResult`.
///
/// `'static` on purpose: the future outlives the `RouteCtx` it was built
/// from, so a route arm clones what it needs (the `reqwest::Client` and the
/// key handle are both `Arc`-backed and cheap; the settings snapshot is
/// already a clone).
pub struct RouteJob {
    future: Pin<Box<dyn Future<Output = RouteDone> + Send + 'static>>,
}

impl RouteJob {
    /// Wrap a route's async work. Nothing runs until the controller spawns it.
    pub fn new(work: impl Future<Output = RouteDone> + Send + 'static) -> Self {
        Self {
            future: Box::pin(work),
        }
    }

    /// Hand the work to the caller's runtime. Consuming, so a job can only be
    /// launched once.
    pub fn into_future(self) -> Pin<Box<dyn Future<Output = RouteDone> + Send + 'static>> {
        self.future
    }
}

impl std::fmt::Debug for RouteJob {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("RouteJob(..)")
    }
}

/// What a route produced, or a promise to produce it.
///
/// Deliberately not `Clone`/`PartialEq`: `Deferred` owns a future, and an
/// outcome is consumed exactly once by the arm that received it.
#[derive(Debug)]
pub enum RouteOutcome {
    /// Decided without leaving the controller thread.
    Ready {
        /// The text to paste. `None` means paste **nothing** — the route
        /// deliberately declined, and the caller must not paste the cleaned
        /// transcript in its place.
        text: Option<String>,
        /// Surfaced through the existing pill flash.
        notice: Option<Notice>,
    },
    /// Not decided yet. The controller spawns this and waits inside the
    /// finalize state it is already in, under the watchdog already counting
    /// for it; the answer arrives as `ControlMsg::RouteResult`.
    Deferred(RouteJob),
}

/// Run `route` over a finished dictation.
///
/// `cleaned` is the pipeline's output (rule cleanup + AI formatting, before
/// tone and smart-space, which are shaped for the window the text lands in);
/// `raw` is the verbatim transcript.
///
/// `Cleanup` is a passthrough.
/// `Translation` is [`translate::apply`]; `Agent` is [`agent::apply`]. A
/// route that has to go to the network returns `Deferred` rather than
/// blocking here: the controller already handles it, so no route has to touch
/// the state machine to get there.
pub fn apply(route: Route, cleaned: String, raw: &str, ctx: &RouteCtx<'_>) -> RouteOutcome {
    match route {
        // The one route that can still turn into another one, and the only
        // place it can happen: a plain dictation that turns out to have
        // addressed the agent by name. `wake::upgrade` answers `None` for
        // every dictation on the shipped settings — see that function for the
        // two gates — and a detection hands the *command* (the transcript with
        // the wake prefix removed) to the same `agent::apply` the chord uses,
        // which is what extends its no-fallback rule over this path.
        Route::Cleanup => match wake::upgrade(raw, ctx) {
            Some(command) => agent::apply(command, raw, ctx),
            None => RouteOutcome::Ready {
                text: Some(cleaned),
                notice: None,
            },
        },
        // Every branch of this one is in `translate`, including the skips:
        // whether a translation can run at all is a question about language
        // codes and keys, and it belongs next to the call it guards.
        Route::Translation => translate::apply(cleaned, ctx),
        Route::Agent => agent::apply(cleaned, raw, ctx),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, RwLock};

    fn ctx_settings() -> crate::settings::Settings {
        crate::settings::Settings::default()
    }

    /// The pre-custom-endpoint world, where "has a Sarvam key" and "can chat"
    /// were the same bit. Every row that predates the split still reads that
    /// way; the rows below that exercise the split spell both fields out.
    fn both(present: bool) -> Capable {
        Capable {
            sarvam_key: present,
            chat: present,
        }
    }

    // --- resolve: the whole table -----------------------------------------

    /// The ordinary chord never routes anywhere but cleanup, whatever the
    /// translation/agent config happens to say.
    #[test]
    fn the_dictation_chord_is_always_cleanup_and_never_notices() {
        for key in [false, true] {
            for target in [false, true] {
                assert_eq!(
                    resolve(ChordKind::Dictation, both(key), target),
                    Resolved {
                        route: Route::Cleanup,
                        notice: None
                    },
                    "key={key} target={target}"
                );
            }
        }
    }

    #[test]
    fn translation_with_a_key_and_a_target_routes_to_translation() {
        assert_eq!(
            resolve(ChordKind::Translate, both(true), true),
            Resolved {
                route: Route::Translation,
                notice: None
            }
        );
    }

    /// The rule's first line: with no key for the translator, the words go
    /// out as an ordinary dictation and a notice explains why.
    #[test]
    fn translation_without_a_key_is_skipped_with_a_notice() {
        assert_eq!(
            resolve(ChordKind::Translate, both(false), true),
            Resolved {
                route: Route::Cleanup,
                notice: Some(TRANSLATOR_UNAVAILABLE_NOTICE.into())
            }
        );
    }

    /// No target chosen (the setting is blank) gets the same paste and the
    /// same notice as a missing key.
    #[test]
    fn translation_without_a_target_is_skipped_with_a_notice() {
        assert_eq!(
            resolve(ChordKind::Translate, both(true), false),
            Resolved {
                route: Route::Cleanup,
                notice: Some(TRANSLATOR_UNAVAILABLE_NOTICE.into())
            }
        );
    }

    #[test]
    fn the_agent_chord_with_a_key_routes_to_the_agent() {
        assert_eq!(
            resolve(ChordKind::Agent, both(true), false),
            Resolved {
                route: Route::Agent,
                notice: None
            }
        );
    }

    /// The rule's second line, and the defect this table exists to not have:
    /// an agent dictation that cannot reach the agent must NOT become a
    /// cleanup. It stays `Agent` so `apply` refuses to produce text, and it
    /// carries the notice that says so.
    #[test]
    fn an_unusable_agent_keeps_its_route_and_types_nothing() {
        let r = resolve(ChordKind::Agent, both(false), true);
        assert_eq!(r.route, Route::Agent, "an agent dictation stays an agent dictation");
        assert_eq!(r.notice.as_deref(), Some(AGENT_UNAVAILABLE_NOTICE));
    }

    /// A custom-only install (its own chat endpoint, no Sarvam account) can
    /// run the agent: it is an ordinary chat completion.
    #[test]
    fn the_agent_runs_on_a_custom_endpoint_with_no_sarvam_key() {
        let r = resolve(
            ChordKind::Agent,
            Capable {
                sarvam_key: false,
                chat: true,
            },
            false,
        );
        assert_eq!(r.route, Route::Agent);
        assert_eq!(r.notice, None, "nothing to warn about — it can run");
    }

    /// The other half of the same split: translation is a Sarvam-only
    /// endpoint, so a working custom chat backend does not rescue it.
    #[test]
    fn translation_still_needs_sarvam_even_with_a_custom_chat_backend() {
        let r = resolve(
            ChordKind::Translate,
            Capable {
                sarvam_key: false,
                chat: true,
            },
            true,
        );
        assert_eq!(r.route, Route::Cleanup);
        assert_eq!(r.notice.as_deref(), Some(TRANSLATOR_UNAVAILABLE_NOTICE));
    }

    /// And a Sarvam key with a broken custom endpoint is not a working agent:
    /// `endpoint::resolve` refuses rather than switching hosts once that endpoint
    /// owns the STT, so `chat` is the only field that can answer this.
    #[test]
    fn a_sarvam_key_alone_does_not_make_the_agent_reachable() {
        let r = resolve(
            ChordKind::Agent,
            Capable {
                sarvam_key: true,
                chat: false,
            },
            false,
        );
        assert_eq!(r.notice.as_deref(), Some(AGENT_UNAVAILABLE_NOTICE));
    }

    // --- apply -------------------------------------------------------------

    /// The cleanup route hands the cleaned transcript straight back,
    /// unchanged and with nothing to say about it.
    #[test]
    fn cleanup_is_an_exact_passthrough() {
        let http = reqwest::Client::new();
        let key: crate::sarvam::SharedKey = Arc::new(RwLock::new(Some("k".into())));
        let s = ctx_settings();
        let ctx = RouteCtx {
            chord: ChordKind::Dictation,
            http: &http,
            api_key: &key,
            settings: &s,
            target_language: &s.translation.target_language,
            agent_name: &s.agent.name,
            selection: None,
        };
        let RouteOutcome::Ready { text, notice } =
            apply(Route::Cleanup, "Hello there.".into(), "hello there", &ctx)
        else {
            panic!("cleanup makes no network call and must never defer");
        };
        assert_eq!(text.as_deref(), Some("Hello there."));
        assert_eq!(notice, None);
    }

    /// The `Translation` arm is wired to `translate::apply` and not to a stub
    /// — asserted through the one branch that is observable without a key or
    /// a socket: the shipped `sarvam.language_code` default is `"auto"`, which
    /// the translate route refuses to send. The rest of that arm's table lives
    /// in `translate`'s own tests.
    #[test]
    fn the_translation_arm_reaches_the_translate_route() {
        let http = reqwest::Client::new();
        let key: crate::sarvam::SharedKey = Arc::new(RwLock::new(Some("k".into())));
        let s = ctx_settings();
        assert_eq!(s.sarvam.language_code, "auto", "the premise of this test");
        let ctx = RouteCtx {
            chord: ChordKind::Translate,
            http: &http,
            api_key: &key,
            settings: &s,
            target_language: &s.translation.target_language,
            agent_name: &s.agent.name,
            selection: None,
        };
        let RouteOutcome::Ready { text, notice } =
            apply(Route::Translation, "Hello there.".into(), "hello there", &ctx)
        else {
            panic!("an unnameable source language must never reach the network");
        };
        assert_eq!(text.as_deref(), Some("Hello there."));
        assert_eq!(
            notice.as_deref(),
            Some(translate::SOURCE_UNSET_NOTICE),
            "the stub arm's notice would be TRANSLATOR_UNAVAILABLE_NOTICE"
        );
    }

    /// An agent route that cannot run never yields text. Pasting the cleaned
    /// form of a command is the failure mode this whole module is shaped
    /// around, so it is asserted at the seam as well as inside
    /// `routes::agent`.
    ///
    /// Unchanged in substance since the agent arm was a stub — only the key
    /// is now absent rather than present, because with a key the arm makes a
    /// network call and answers later (`Deferred`, pinned below). The
    /// no-text-plus-a-notice shape is the same one either way.
    #[test]
    fn an_agent_route_yields_no_text_and_always_says_why() {
        let http = reqwest::Client::new();
        let key: crate::sarvam::SharedKey = Arc::new(RwLock::new(None));
        let s = ctx_settings();
        let ctx = RouteCtx {
            chord: ChordKind::Agent,
            http: &http,
            api_key: &key,
            settings: &s,
            target_language: &s.translation.target_language,
            agent_name: &s.agent.name,
            selection: None,
        };
        let RouteOutcome::Ready { text, notice } = apply(
            Route::Agent,
            "Butterfly, delete that paragraph.".into(),
            "butterfly delete that paragraph",
            &ctx,
        ) else {
            panic!("an agent with no key has nothing to wait for");
        };
        assert_eq!(text, None, "a command must never be pasted as prose");
        assert_eq!(notice.as_deref(), Some(AGENT_UNAVAILABLE_NOTICE));
    }

    /// ...and a *usable* agent answers off the controller thread. `apply`
    /// runs inside the single `ControlMsg` consumer, so an arm that made its
    /// chat call inline would freeze the pill, the hotkeys and every
    /// subsequent chord for the length of the request.
    #[test]
    fn a_usable_agent_route_defers_rather_than_answering_inline() {
        let http = reqwest::Client::new();
        let key: crate::sarvam::SharedKey = Arc::new(RwLock::new(Some("k".into())));
        let s = ctx_settings();
        let ctx = RouteCtx {
            chord: ChordKind::Agent,
            http: &http,
            api_key: &key,
            settings: &s,
            target_language: &s.translation.target_language,
            agent_name: &s.agent.name,
            selection: None,
        };
        assert!(matches!(
            apply(
                Route::Agent,
                "Butterfly, delete that paragraph.".into(),
                "butterfly delete that paragraph",
                &ctx,
            ),
            RouteOutcome::Deferred(_)
        ));
    }

    // --- the wake word at the seam ------------------------------------------
    //
    // The matcher's own table is in `routes::wake`. What is asserted here is
    // the wiring: which dictations are scanned at all, and what a detection
    // does to the route.

    /// A `RouteCtx` over `s`, on `chord`, with no selection — the shape all
    /// four wake tests need and the only thing that varies between them.
    fn wake_ctx<'a>(
        chord: ChordKind,
        http: &'a reqwest::Client,
        key: &'a crate::sarvam::SharedKey,
        s: &'a crate::settings::Settings,
    ) -> RouteCtx<'a> {
        RouteCtx {
            chord,
            http,
            api_key: key,
            settings: s,
            target_language: &s.translation.target_language,
            agent_name: &s.agent.name,
            // Structural, not incidental: a plain dictation never starts a
            // selection read (`Controller::finish_recording` gates it on the
            // agent chord), so a wake invocation always types at the cursor.
            selection: None,
        }
    }

    /// OFF BY DEFAULT. The shipped settings scan nothing, so a dictation that
    /// happens to open with the agent's name is typed like any other.
    #[test]
    fn the_wake_word_is_off_in_the_shipped_settings() {
        let http = reqwest::Client::new();
        let key: crate::sarvam::SharedKey = Arc::new(RwLock::new(Some("k".into())));
        let s = ctx_settings();
        assert!(!s.agent.wake_word_enabled, "the premise of this test");
        let ctx = wake_ctx(ChordKind::Dictation, &http, &key, &s);
        let RouteOutcome::Ready { text, notice } = apply(
            Route::Cleanup,
            "Butterfly, delete that paragraph.".into(),
            "butterfly delete that paragraph",
            &ctx,
        ) else {
            panic!("a scan that never runs cannot reach the network");
        };
        assert_eq!(text.as_deref(), Some("Butterfly, delete that paragraph."));
        assert_eq!(notice, None);
    }

    /// Turned on, an addressed name turns the dictation into an agent command
    /// — which means a network call, off the controller thread.
    #[test]
    fn an_enabled_wake_word_sends_a_plain_dictation_to_the_agent() {
        let http = reqwest::Client::new();
        let key: crate::sarvam::SharedKey = Arc::new(RwLock::new(Some("k".into())));
        let mut s = ctx_settings();
        s.agent.wake_word_enabled = true;
        let ctx = wake_ctx(ChordKind::Dictation, &http, &key, &s);
        assert!(matches!(
            apply(
                Route::Cleanup,
                "Butterfly, delete that paragraph.".into(),
                "butterfly delete that paragraph",
                &ctx,
            ),
            RouteOutcome::Deferred(_)
        ));
    }

    /// A wake-phrase command with the agent unreachable is never handed to the
    /// cleanup model, which would polish "Butterfly, delete that paragraph"
    /// into prose and type it into the user's document. It types nothing and
    /// says why, the same answer the agent chord gives.
    #[test]
    fn a_wake_word_with_no_key_types_nothing_and_never_cleans_the_command_up() {
        let http = reqwest::Client::new();
        let key: crate::sarvam::SharedKey = Arc::new(RwLock::new(None));
        let mut s = ctx_settings();
        s.agent.wake_word_enabled = true;
        let ctx = wake_ctx(ChordKind::Dictation, &http, &key, &s);
        let RouteOutcome::Ready { text, notice } = apply(
            Route::Cleanup,
            "Butterfly, delete that paragraph.".into(),
            "butterfly delete that paragraph",
            &ctx,
        ) else {
            panic!("with no key there is nothing to defer to");
        };
        assert_eq!(text, None, "the command must never reach the document");
        assert_eq!(notice.as_deref(), Some(AGENT_UNAVAILABLE_NOTICE));
    }

    /// ...and the same dictation on the *translate* chord is not scanned at
    /// all. A translation with no key resolves to `Route::Cleanup`, which
    /// is the arm the scan lives in — reading the resolved route instead of
    /// the chord would silently swallow the user's words here.
    #[test]
    fn a_skipped_translation_is_never_wake_scanned() {
        let http = reqwest::Client::new();
        let key: crate::sarvam::SharedKey = Arc::new(RwLock::new(None));
        let mut s = ctx_settings();
        s.agent.wake_word_enabled = true;
        let ctx = wake_ctx(ChordKind::Translate, &http, &key, &s);
        let RouteOutcome::Ready { text, notice } = apply(
            Route::Cleanup,
            "Butterfly, delete that paragraph.".into(),
            "butterfly delete that paragraph",
            &ctx,
        ) else {
            panic!("an unscanned dictation makes no network call");
        };
        assert_eq!(
            text.as_deref(),
            Some("Butterfly, delete that paragraph."),
            "the dictation is pasted as cleaned, untranslated and unscanned"
        );
        assert_eq!(notice, None);
    }

    /// THE RAW/CLEANED RULING, both directions. Detection reads the verbatim
    /// transcript: the cleaned string has been through a language model whose
    /// job is to rewrite words, and whether the wake phrase survives that is
    /// not a fact about what the user said.
    #[test]
    fn the_scan_reads_the_raw_transcript_and_not_the_cleaned_one() {
        let http = reqwest::Client::new();
        let key: crate::sarvam::SharedKey = Arc::new(RwLock::new(None));
        let mut s = ctx_settings();
        s.agent.wake_word_enabled = true;
        let ctx = wake_ctx(ChordKind::Dictation, &http, &key, &s);

        // Polish dropped the wake phrase; the user still said it.
        let RouteOutcome::Ready { text, .. } = apply(
            Route::Cleanup,
            "Delete that paragraph.".into(),
            "butterfly delete that paragraph",
            &ctx,
        ) else {
            panic!("with no key there is nothing to defer to");
        };
        assert_eq!(text, None, "the raw transcript addressed the agent");

        // ...and the mirror image: polish put a name in that was never spoken.
        let RouteOutcome::Ready { text, .. } = apply(
            Route::Cleanup,
            "Butterfly, delete that paragraph.".into(),
            "delete that paragraph",
            &ctx,
        ) else {
            panic!("an unaddressed dictation makes no network call");
        };
        assert_eq!(
            text.as_deref(),
            Some("Butterfly, delete that paragraph."),
            "the user never addressed the agent, so this is text"
        );
    }

    /// The command handed to the agent is the raw transcript with the wake
    /// prefix removed — asserted on `wake::command` because `Deferred` hides
    /// the string it was built with. `routes::wake` holds the rest of the
    /// strip table.
    #[test]
    fn the_agent_receives_the_command_without_the_wake_prefix() {
        assert_eq!(
            wake::command("hey butterfly delete that paragraph", "Butterfly").as_deref(),
            Some("delete that paragraph")
        );
    }

    // --- the deferred mechanism -------------------------------------------

    /// The trivial job the mechanism is built for: a route that has to go to
    /// the network hands back a future, the controller runs it off its own
    /// thread, and what comes out is the same `{ text, notice }` a `Ready`
    /// outcome carries — so the paste path cannot tell the two apart.
    #[test]
    fn a_deferred_job_runs_off_thread_and_answers_in_the_ready_shape() {
        let outcome = RouteOutcome::Deferred(RouteJob::new(async {
            RouteDone {
                text: Some("नमस्ते".into()),
                notice: None,
                pasted: false,
            }
        }));
        let RouteOutcome::Deferred(job) = outcome else {
            panic!("built as deferred");
        };
        // Exactly what the controller does with it — `tauri::async_runtime`
        // is the runtime the app already owns (`transforms.rs` precedent).
        let done = tauri::async_runtime::block_on(job.into_future());
        assert_eq!(done.text.as_deref(), Some("नमस्ते"));
        assert_eq!(done.notice, None);
    }

    /// A deferred route may also decline, and it must be able to say so the
    /// same way a synchronous one does — `text: None` means paste nothing,
    /// wherever the decision was made.
    #[test]
    fn a_deferred_job_can_decline_with_a_notice() {
        let job = RouteJob::new(async {
            RouteDone {
                text: None,
                notice: Some(AGENT_UNAVAILABLE_NOTICE.into()),
                pasted: false,
            }
        });
        let done = tauri::async_runtime::block_on(job.into_future());
        assert_eq!(done.text, None);
        assert_eq!(done.notice.as_deref(), Some(AGENT_UNAVAILABLE_NOTICE));
    }

    // --- history vocabulary ------------------------------------------------

    /// The history `route` column's vocabulary, pinned: NULL for cleanup, the
    /// lowercase route name otherwise. The History UI and any future retry
    /// affordance both read these strings.
    #[test]
    fn history_labels_are_null_for_cleanup_and_named_otherwise() {
        assert_eq!(Route::Cleanup.history_label(), None);
        assert_eq!(Route::Translation.history_label(), Some("translation"));
        assert_eq!(Route::Agent.history_label(), Some("agent"));
    }

    /// Anything unstamped is a plain dictation.
    #[test]
    fn the_default_route_is_cleanup() {
        assert_eq!(Route::default(), Route::Cleanup);
        assert_eq!(ChordKind::default(), ChordKind::Dictation);
    }

    /// A translate dictation whose translation was skipped is still filed as
    /// a translation: the history column keeps the chord the user pressed, so
    /// running it again asks for the translation again.
    #[test]
    fn the_history_label_follows_the_chord_not_the_route_taken() {
        let skipped = resolve(ChordKind::Translate, both(false), true);
        assert_eq!(skipped.route, Route::Cleanup, "dispatch skips the translation");
        assert_eq!(
            ChordKind::Translate.intent().history_label(),
            Some("translation"),
            "history does not"
        );
        assert_eq!(ChordKind::Dictation.intent().history_label(), None);
        assert_eq!(ChordKind::Agent.intent().history_label(), Some("agent"));
    }
}
