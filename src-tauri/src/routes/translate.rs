//! The translate chord's last step. Cleanup has already run by the time this
//! module is called; it asks the translator for a version of the text and
//! decides what gets pasted: that version when one comes back, otherwise the
//! cleaned text with a notice.
//!
//! ## Best effort, never silent, never empty
//!
//! Translation is the *last* step of a dictation, and by the time it runs the
//! words already exist: they were transcribed, rule-cleaned, AI-formatted and
//! passed the retention guardrail. So every failure below pastes the cleaned
//! source rather than nothing — losing a finished dictation because a
//! translation call fell over is a far worse outcome than pasting it in the
//! language it was spoken. Keeping the words is not enough on its own,
//! though: a skipped step nobody hears about looks like a translation that
//! quietly did nothing. So every skipped translation also puts a notice on
//! the pill saying why the text is still in its own language.
//!
//! The one deliberate exception is a reply identical to the input
//! (`Translated::unchanged`): the text was already in the requested language,
//! so it is pasted with no notice. It counts as a success with nothing to
//! report, because someone who dictates in their target language would otherwise get
//! a warning on every dictation for text that came out as they wanted it.
//!
//! ## The retention guardrail does not run again here
//!
//! `format`'s guardrail compares the formatter's output against its input and
//! rejects a rewrite that dropped the user's content. A translation *is* a
//! structural rewrite of exactly that kind — different script, different word
//! count, different everything — so running the guardrail over it would
//! reject every correct translation. It has already run, on the cleaned source
//! this route receives. What replaces it here is [`crate::sarvam::translate`]'s
//! own guards: a blank or malformed reply is `Empty` and never replaces real
//! text, and every error path pastes the source instead.
//!
//! ## Why the source language is resolved here
//!
//! `sarvam-translate:v1` has no auto-detect, and `SarvamSettings::language_code`
//! defaults to `"auto"`. Sending `"auto"` earns an HTTP 400 — after a full
//! 10 s of the user's finalize budget, which is the whole budget this route
//! gets. So the source is resolved *before* the request
//! ([`source_language`]), and a dictation with no named language skips the
//! translation on the spot, with a notice that says what to do about it. See the note on
//! `crate::sarvam::translate::to_translate_language_code`, which deliberately
//! does not paper this over at the client.
//!
//! Nothing here logs transcript text: the `tracing` calls carry language
//! codes, character counts and typed failure discriminants. In particular a
//! [`crate::sarvam::translate::Translated`] is never `Debug`-formatted — its
//! `text` field *is* the dictation.

use super::{Notice, RouteCtx, RouteDone, RouteJob, RouteOutcome, TRANSLATOR_UNAVAILABLE_NOTICE};
use crate::sarvam::translate::{to_translate_language_code, TranslateError, Translated};

/// Shown when the translate chord fired but the dictation language is
/// `"auto"` (the shipped default) or blank.
///
/// It names the fix rather than the failure: `sarvam-translate:v1` cannot
/// detect the source language, so the user has to say what they are speaking.
/// The dictation is still pasted, cleaned, in the language it was spoken.
pub const SOURCE_UNSET_NOTICE: &str = "Set a dictation language to translate";

/// Shown when the dictation is over `sarvam::translate::MAX_INPUT_CHARS`.
///
/// Says what happened *and* what the user got, because the alternative
/// reading of a bare "too long" — that the dictation was thrown away — is the
/// thing they would otherwise go looking for.
pub const TOO_LONG_NOTICE: &str = "Too long to translate — pasted as dictated";

/// Shown when the user pressed Escape while the translation was still in
/// flight. The words the controller was holding are pasted untranslated;
/// this says why they are not in the target language.
///
/// Lives here rather than in `controller.rs` so the translate route's user
/// -facing prose is all in one file, next to the behaviour it describes.
pub const TRANSLATION_SKIPPED_NOTICE: &str = "Translation skipped";

/// The concrete language the dictation was spoken in, or `None` if the app
/// does not actually know.
///
/// `"auto"` is `SarvamSettings::language_code`'s shipped default and means
/// "let the speech model work it out" — a question the *speech* endpoint can
/// answer and `/translate` cannot. `"unknown"` is the REST speech vocabulary's
/// spelling of the same thing (`batch::to_rest_language_code`) and is rejected
/// for the same reason, in case it ever reaches this setting.
///
/// Pure and total, so the whole "never let auto reach the client" rule is one
/// unit test rather than something only reachable with a live key.
pub fn source_language(code: &str) -> Option<&str> {
    let code = code.trim();
    if code.is_empty() || code.eq_ignore_ascii_case("auto") || code.eq_ignore_ascii_case("unknown") {
        return None;
    }
    Some(code)
}

/// Whether translating `source` into `target` would be a no-op.
///
/// Compared *after* `to_translate_language_code`, so the realtime/REST split
/// for Odia (`or-IN` vs `od-IN`) does not read as two different languages —
/// the dictation setting speaks realtime and the translation setting speaks
/// REST, so this is the one place the two vocabularies actually meet.
fn same_language(source: &str, target: &str) -> bool {
    to_translate_language_code(source).eq_ignore_ascii_case(to_translate_language_code(target))
}

/// Turn what the client answered into what the controller should do with it.
///
/// Pure, and separate from the request for the reason every decision in this
/// codebase is: the outcome table is the part that must not drift, and it
/// should be assertable without a socket.
///
/// `cleaned` is what every failing row pastes — see the module doc on
/// why a translation failure never pastes nothing.
fn done(result: Result<Translated, TranslateError>, cleaned: String) -> RouteDone {
    match result {
        // Never `?t` here, and never `{t:?}`: `Translated::text` IS the
        // dictation (on an unchanged reply, the user's own source string). The two
        // fields below are a bool and a count.
        Ok(t) => {
            tracing::debug!(
                unchanged = t.unchanged,
                chars = t.text.chars().count(),
                "translation finished"
            );
            RouteDone {
                text: Some(t.text),
                notice: None,
                pasted: false,
            }
        }
        // Split out only because its notice is this route's own prose rather
        // than the client's: `TranslateError::TooLong`'s message says the
        // dictation was too long and stops there, which reads like the words
        // were lost. They were not — they are being pasted.
        Err(TranslateError::TooLong) => {
            tracing::warn!(
                chars = cleaned.chars().count(),
                "dictation is over the translate cap; pasting it as dictated"
            );
            RouteDone {
                text: Some(cleaned),
                notice: Some(Notice::from(TOO_LONG_NOTICE)),
                pasted: false,
            }
        }
        // `TranslateError` carries a `NetFailure` discriminant, a status code
        // or nothing at all — no content — so this one is safe to format.
        Err(e) => {
            tracing::warn!(failure = ?e, "translation failed; pasting the dictation as spoken");
            RouteDone {
                text: Some(cleaned),
                notice: Some(Notice::from(e.user_message())),
                pasted: false,
            }
        }
    }
}

/// Run the translate route over a finished, cleaned dictation.
///
/// Returns [`RouteOutcome::Deferred`] whenever it is actually going to call
/// Sarvam: `routes::apply` runs on the controller thread, which is the single
/// consumer of `ControlMsg`, and a 10 s blocking call there would stall the
/// pill, the hotkeys and every subsequent chord. Every *skip* is decided
/// on the spot instead and comes back `Ready` — none of them needs the
/// network, and the ones that would otherwise spend the whole budget earning
/// a 400 are exactly the ones worth answering instantly.
pub fn apply(cleaned: String, ctx: &RouteCtx<'_>) -> RouteOutcome {
    // `routes::resolve` already checked both of these at chord-down, and a
    // dictation that failed either never reaches this arm. They are checked
    // again because Settings is live: a key can be cleared or a target blanked
    // while the user is still speaking, and the finalize seam is the moment
    // that actually matters.
    let Some(api_key) = ctx.api_key().filter(|k| !k.trim().is_empty()) else {
        tracing::debug!("translate route has no API key; pasting the dictation as spoken");
        return skip_translation(cleaned, TRANSLATOR_UNAVAILABLE_NOTICE);
    };
    let target = ctx.target_language.trim();
    if target.is_empty() {
        tracing::debug!("translate route has no target language; pasting the dictation as spoken");
        return skip_translation(cleaned, TRANSLATOR_UNAVAILABLE_NOTICE);
    }
    // The hard one. See the module doc: `"auto"` costs the entire 10 s budget
    // to earn an HTTP 400, so it is answered here instead of at the client.
    let Some(source) = source_language(&ctx.settings.sarvam.language_code) else {
        // `target_language`, not `target`: `target` is `tracing`'s own
        // reserved field name and a bare one reads as a subscriber filter.
        tracing::debug!(
            target_language = target,
            "no dictation language is set, and this model has no auto-detect; \
             pasting the dictation as spoken"
        );
        return skip_translation(cleaned, SOURCE_UNSET_NOTICE);
    };
    if same_language(source, target) {
        // The unchanged-reply row without the round trip, and load-bearing
        // rather than merely thrifty: sent live, this exact pair makes
        // `sarvam-translate:v1` answer **HTTP 400**
        // (`invalid_request_error`, "Source and target languages must be
        // different."), not the input handed back. So without this branch a
        // user whose dictation language equals their translation target
        // would get a failure flash on every translate chord instead of
        // their own words. Silent, exactly like the unchanged reply it
        // stands in for.
        tracing::debug!(
            source_language = source,
            target_language = target,
            "nothing to translate between; pasting as spoken"
        );
        return RouteOutcome::Ready {
            text: Some(cleaned),
            notice: None,
        };
    }

    // Everything the job needs, owned: it outlives this `RouteCtx`. The
    // client is `Arc`-backed and the key was already copied out of the lock,
    // so this is two small allocations and a refcount.
    let http = ctx.http.clone();
    let source = source.to_string();
    let target = target.to_string();
    tracing::debug!(
        source_language = %source,
        target_language = %target,
        chars = cleaned.chars().count(),
        "translating"
    );
    RouteOutcome::Deferred(RouteJob::new(async move {
        let result =
            crate::sarvam::translate::translate(&http, &api_key, &cleaned, &source, &target).await;
        done(result, cleaned)
    }))
}

/// Paste the cleaned source, and say why it is not translated.
fn skip_translation(cleaned: String, notice: &str) -> RouteOutcome {
    RouteOutcome::Ready {
        text: Some(cleaned),
        notice: Some(Notice::from(notice)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sarvam::net_error::NetFailure;
    use std::sync::{Arc, RwLock};

    /// Everything a `RouteCtx` borrows, built so a test can name only the
    /// three things this route actually reads.
    fn parts(
        key: Option<&str>,
        dictation_language: &str,
        target: &str,
    ) -> (
        reqwest::Client,
        crate::sarvam::SharedKey,
        crate::settings::Settings,
    ) {
        let mut s = crate::settings::Settings::default();
        s.sarvam.language_code = dictation_language.into();
        s.translation.target_language = target.into();
        (
            reqwest::Client::new(),
            Arc::new(RwLock::new(key.map(str::to_string))),
            s,
        )
    }

    macro_rules! ctx {
        ($http:expr, $key:expr, $s:expr) => {
            RouteCtx {
                chord: crate::routes::ChordKind::Translate,
                http: &$http,
                api_key: &$key,
                settings: &$s,
                target_language: &$s.translation.target_language,
                agent_name: &$s.agent.name,
                selection: None,
            }
        };
    }

    // --- The source language ----------------------------------------------

    #[test]
    fn a_named_dictation_language_is_the_source() {
        assert_eq!(source_language("hi-IN"), Some("hi-IN"));
        assert_eq!(source_language("en-IN"), Some("en-IN"));
        // The realtime spelling of Odia survives; the REST rewrite happens at
        // the client, not here.
        assert_eq!(source_language("or-IN"), Some("or-IN"));
        assert_eq!(source_language("  ta-IN  "), Some("ta-IN"));
    }

    /// THE requirement this function exists for. `"auto"` is the shipped
    /// default of `SarvamSettings::language_code`, and `sarvam-translate:v1`
    /// answers it with a 400 after burning the entire 10 s translate budget.
    #[test]
    fn auto_is_never_a_source_language() {
        for code in ["auto", "AUTO", "Auto", "unknown", "", "   "] {
            assert_eq!(source_language(code), None, "code {code:?}");
        }
    }

    // --- The outcome table -------------------------------------------------

    /// Row 1: a real translation is what gets pasted, with nothing to say
    /// about it.
    #[test]
    fn a_real_translation_is_what_gets_pasted() {
        let out = done(
            Ok(Translated {
                text: "नमस्ते".into(),
                unchanged: false,
            }),
            "hello".into(),
        );
        assert_eq!(
            out,
            RouteDone {
                text: Some("नमस्ते".into()),
                notice: None,
                pasted: false,
            }
        );
    }

    /// Row 2: a reply identical to the input means the text was already in
    /// the requested language. On this row the client hands back the caller's
    /// own source string, so the paste is the cleaned source, with no notice:
    /// anyone dictating in their target language would otherwise get one on
    /// every dictation.
    #[test]
    fn an_unchanged_reply_pastes_the_source_silently() {
        let cleaned = "बैठक कल सुबह दस बजे है।";
        let out = done(
            Ok(Translated {
                text: cleaned.into(),
                unchanged: true,
            }),
            cleaned.into(),
        );
        assert_eq!(
            out,
            RouteDone {
                text: Some(cleaned.into()),
                notice: None,
                pasted: false,
            }
        );
    }

    /// Row 3: over the model's character cap. The words exist, so they are
    /// pasted; the notice says they were pasted as dictated so the user is not
    /// left hunting for a dictation they think was dropped.
    #[test]
    fn too_long_pastes_the_dictation_and_says_why() {
        let out = done(Err(TranslateError::TooLong), "a very long dictation".into());
        assert_eq!(
            out,
            RouteDone {
                text: Some("a very long dictation".into()),
                notice: Some(TOO_LONG_NOTICE.into()),
                pasted: false,
            }
        );
    }

    /// Row 4: everything else borrows the client's own prose, so a DNS
    /// failure, a rejected key and a 429 do not collapse into one shrug.
    #[test]
    fn any_other_failure_borrows_the_clients_own_prose() {
        for err in [
            TranslateError::Net(NetFailure::NameNotResolved),
            TranslateError::Net(NetFailure::Timeout),
            TranslateError::Empty,
            TranslateError::Http { status: 401 },
            TranslateError::Http { status: 429 },
            TranslateError::Http { status: 503 },
        ] {
            let expected = err.user_message();
            let out = done(Err(err.clone()), "the words".into());
            assert_eq!(
                out,
                RouteDone {
                    text: Some("the words".into()),
                    notice: Some(expected.into()),
                    pasted: false,
                },
                "error {err:?}"
            );
        }
    }

    /// The rule the whole module is shaped around, asserted over every
    /// failure the client can produce: a translation that did not happen must
    /// never cost the user their dictation.
    #[test]
    fn no_failure_ever_pastes_nothing() {
        for err in [
            TranslateError::Net(NetFailure::NameNotResolved),
            TranslateError::Net(NetFailure::Refused),
            TranslateError::Net(NetFailure::Timeout),
            TranslateError::Net(NetFailure::Tls),
            TranslateError::Net(NetFailure::ServiceUnavailable),
            TranslateError::Net(NetFailure::Other),
            TranslateError::TooLong,
            TranslateError::Empty,
            TranslateError::Http { status: 400 },
            TranslateError::Http { status: 500 },
        ] {
            let out = done(Err(err.clone()), "the words".into());
            assert_eq!(
                out.text.as_deref(),
                Some("the words"),
                "error {err:?} must still paste the dictation"
            );
            assert!(out.notice.is_some(), "error {err:?} must not be silent");
        }
    }

    // --- What `apply` decides without touching the network -----------------

    /// The key can be cleared from Settings between chord-down (where
    /// `routes::resolve` checks it) and the finalize seam. Nothing hangs on
    /// that race: the translation is skipped here too, and never with a
    /// request.
    #[test]
    fn a_missing_key_skips_the_translation_without_a_request() {
        for key in [None, Some(""), Some("   ")] {
            let (http, k, s) = parts(key, "en-IN", "hi-IN");
            let RouteOutcome::Ready { text, notice } = apply("Hello there.".into(), &ctx!(http, k, s))
            else {
                panic!("a skipped translation must never leave the controller thread");
            };
            assert_eq!(text.as_deref(), Some("Hello there."));
            assert_eq!(notice.as_deref(), Some(TRANSLATOR_UNAVAILABLE_NOTICE));
        }
    }

    /// A blank `translation.target_language` is how "translation is not
    /// configured" is expressed (`settings::TranslationSettings`).
    #[test]
    fn a_blank_target_skips_the_translation_without_a_request() {
        let (http, k, s) = parts(Some("key"), "en-IN", "   ");
        let RouteOutcome::Ready { text, notice } = apply("Hello there.".into(), &ctx!(http, k, s))
        else {
            panic!("a skipped translation must never leave the controller thread");
        };
        assert_eq!(text.as_deref(), Some("Hello there."));
        assert_eq!(notice.as_deref(), Some(TRANSLATOR_UNAVAILABLE_NOTICE));
    }

    /// THE guard. `"auto"` is the shipped default, and letting it through
    /// costs the user their whole finalize budget to be told no. The
    /// translation is skipped at once instead, with a notice that names the
    /// fix.
    #[test]
    fn an_auto_dictation_language_skips_the_translation_instead_of_calling_sarvam() {
        let (http, k, s) = parts(Some("key"), "auto", "hi-IN");
        let RouteOutcome::Ready { text, notice } = apply("Hello there.".into(), &ctx!(http, k, s))
        else {
            panic!("auto must never reach the client — that is a 10 s HTTP 400");
        };
        assert_eq!(text.as_deref(), Some("Hello there."));
        assert_eq!(notice.as_deref(), Some(SOURCE_UNSET_NOTICE));
    }

    /// Translating a language into itself is the unchanged-reply row without
    /// the round trip: the same paste, the same silence, ten seconds sooner. Odia is
    /// the case worth pinning — the dictation setting spells it `or-IN` and
    /// the translation setting spells it `od-IN`, and those are one language.
    #[test]
    fn translating_a_language_into_itself_skips_the_round_trip() {
        for (source, target) in [("hi-IN", "hi-IN"), ("or-IN", "od-IN"), ("od-IN", "or-IN")] {
            let (http, k, s) = parts(Some("key"), source, target);
            let RouteOutcome::Ready { text, notice } = apply("Hello there.".into(), &ctx!(http, k, s))
            else {
                panic!("{source} → {target} has nothing to ask Sarvam");
            };
            assert_eq!(text.as_deref(), Some("Hello there."), "{source} → {target}");
            assert_eq!(
                notice, None,
                "{source} → {target} needs no translating, so it is silent"
            );
        }
    }

    /// The one row that actually goes to the network must hand back a job
    /// rather than answer: `routes::apply` runs on the controller thread and a
    /// 10 s call there stalls the pill, the hotkeys and every later chord.
    /// The job is dropped un-run, so no request is made.
    #[test]
    fn a_configured_translation_defers_instead_of_blocking_the_controller() {
        let (http, k, s) = parts(Some("key"), "en-IN", "hi-IN");
        assert!(
            matches!(
                apply("Hello there.".into(), &ctx!(http, k, s)),
                RouteOutcome::Deferred(_)
            ),
            "a network call must never run on the controller thread"
        );
    }

    // --- Prose -------------------------------------------------------------

    /// The three lines this route can put on the pill, pinned. They are the
    /// only thing the user ever sees of it.
    #[test]
    fn the_notices_say_what_they_are_supposed_to_say() {
        assert_eq!(SOURCE_UNSET_NOTICE, "Set a dictation language to translate");
        assert_eq!(TOO_LONG_NOTICE, "Too long to translate — pasted as dictated");
        assert_eq!(TRANSLATION_SKIPPED_NOTICE, "Translation skipped");
    }
}
