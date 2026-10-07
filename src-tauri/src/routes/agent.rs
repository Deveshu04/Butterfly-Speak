//! The voice-agent route: the dictation was a command, so what gets typed is
//! the agent's answer — or nothing at all, with a reason.
//!
//! ## The one rule
//!
//! An agent route never yields the transcript. Not the cleaned form, not the
//! verbatim form, not on a missing key, a dead network, an empty reply or a
//! truncated one. Handing a command to the cleanup model instead, whose
//! prompt is hardened to *write commands down instead of running them*, would
//! put "Butterfly, delete that paragraph" into the user's document as a
//! polished sentence with nothing shown or logged. `routes::resolve` already
//! keeps the route `Agent` when the agent can't run, and no early exit below
//! hands back any words. `text: None` plus a notice is the only shape this module
//! produces when it cannot deliver an answer.
//!
//! ## Why the guardrail does not apply here
//!
//! `format::guard` compares a formatter's output against its input and
//! rejects it if too much changed — the right check for a pass that promises
//! to preserve the speaker's words. An agent that is asked to summarise five
//! sentences into one, or to answer a question, *must* fail that comparison.
//! So the agent's output is guarded on its own terms instead: it must be
//! non-empty, and it must have reached its end marker.
//!
//! ## The selection lane
//!
//! An agent command spoken with text selected is a command *about that text*.
//! `routes::selection` reads the selection when the chord is released and
//! hands the result here as `RouteCtx::selection`; [`apply`] consults it once,
//! at the point the job is built, and `selection::plan` picks one of three
//! paths for everything downstream:
//!
//! - **type at the cursor** — nothing selected, or no window whose selection
//!   the answer could overwrite. The command goes to the model on its own and
//!   the answer is typed at the cursor.
//! - **edit the selection** — the selection rides into the prompt as the
//!   document the command applies to, the model is asked for the whole
//!   replacement, and `routes::selection::replace` re-reads the document and
//!   byte-compares it before pasting the reply over the selection. Nothing is
//!   typed at the cursor on this path, and nothing is typed at all unless the
//!   document still says exactly what it said when the user spoke.
//! - **refuse** — nothing is typed and nothing is asked of the model. The
//!   point of refusing is that the LLM call does not happen: an answer
//!   produced against a selection the app could not read has nowhere safe to
//!   go.
//!
//! ## The replacement is not touched
//!
//! What gets pasted on the selected path is the model's reply **byte for
//! byte**: untrimmed (see [`replacement_of`]), and past every text pass this
//! app owns. Snippet expansion, tone and smart-space all run on the transcript
//! or on text bound for a caret, and all three are skipped here.
//!
//! Of the three, snippets would do the most harm: a reply that contains one of
//! the user's triggers (say "addr") would be expanded on its way in, and this
//! path promises to paste the model's reply unchanged.
//!
//! Nothing here logs transcript text. The spoken command, the agent's reply
//! and the captured selection are all transcript-class; the `tracing` calls
//! carry counts, opaque session ids and reasons.

use super::{selection, RouteCtx, RouteDone, RouteJob, RouteOutcome, AGENT_UNAVAILABLE_NOTICE};
use crate::format::backend::Backend;

/// The model answered, and the answer was empty (or whitespace).
///
/// Typing the transcript instead would type the user's own command out when
/// they asked for an answer, so nothing is typed.
pub const AGENT_EMPTY_NOTICE: &str = "Agent returned nothing — nothing typed";

/// The reply never reached its end marker, so it ends mid-thought —
/// either `max_tokens` ran out or the transport dropped the tail while still
/// reporting success (see `ChatReply::strip_end_marker`). Half an
/// answer pasted into a document is worse than none, and unlike the polish
/// path there is no rule-pipeline output to paste instead: the only other
/// text on hand is the command itself.
pub const AGENT_TRUNCATED_NOTICE: &str = "Agent reply was cut off — nothing typed";

/// Where the agent's chat call is going, at the point the job is built.
///
/// Two shapes because the two lanes learn their credential at different
/// moments. Bring-your-own-key resolves the whole backend on the controller
/// thread, exactly as it always has — the key is already in memory. The Cloud
/// lane cannot: its bearer is an `await` (and possibly a token refresh) away,
/// and [`apply`] runs on the controller thread, the single consumer of
/// `ControlMsg`. So the lane travels into the job and the backend is resolved
/// there, one step before the request that needs it.
///
/// Whether a call can be made *at all* is still decided synchronously, in
/// `apply`, on both lanes — that answer needs no credential
/// (`endpoint::chat_available`), and it has to come before the selection is
/// consulted.
enum ChatTarget {
    Ready(Backend),
    Cloud {
        lane: crate::sarvam::Lane,
        sarvam_key: Option<String>,
        model: String,
    },
}

/// So every existing caller — the tests included — keeps passing a `Backend`.
impl From<Backend> for ChatTarget {
    fn from(backend: Backend) -> Self {
        ChatTarget::Ready(backend)
    }
}

impl ChatTarget {
    /// The backend, or the sentence to decline with.
    async fn resolve(self) -> Result<Backend, String> {
        let (lane, sarvam_key, model) = match self {
            ChatTarget::Ready(backend) => return Ok(backend),
            ChatTarget::Cloud {
                lane,
                sarvam_key,
                model,
            } => (lane, sarvam_key, model),
        };
        crate::endpoint::chat_backend_for(&lane, sarvam_key.as_deref(), &model)
            .await
            .map_err(|why| match why {
                // The same two sentences `apply`'s own gate would have used,
                // for the same two reasons — this is the identical decision,
                // re-made a moment later with the credential in hand.
                crate::endpoint::ChatUnavailable::Backend(
                    crate::endpoint::Unavailable::NoSarvamKey,
                ) => AGENT_UNAVAILABLE_NOTICE.to_string(),
                crate::endpoint::ChatUnavailable::Backend(
                    crate::endpoint::Unavailable::CustomEndpoint(invalid),
                ) => invalid.message().to_string(),
                crate::endpoint::ChatUnavailable::SignIn(sentence) => sentence,
            })
    }
}

/// The verify-then-replace step, as something [`Edit`] can hold.
///
/// `Send` and implicitly `'static`: it crosses onto the thread `run_replace`
/// spawns, because everything it does — two foreground switches, a synthetic
/// copy, a paste — blocks.
type Replace = Box<dyn Fn(&selection::SessionId, &str) -> selection::Replacement + Send>;

/// An edit to the user's selection, and everything it takes to land one.
///
/// `replace` is injected rather than called by name so that everything
/// upstream of the document — the marker check, the empty-edit guard, what
/// reaches History, which notice the user sees — is testable. The real thing
/// needs a focused window, a live selection and a system clipboard, none of
/// which a `cargo test` run has; the shape of the decisions around it is
/// exactly what a test should be pinning anyway.
struct Edit {
    /// The token minted when the selection was captured. Single-use, and
    /// burned by the first redemption whatever it decides.
    session: selection::SessionId,
    /// PRIVACY: the user's document content, on its way to the prompt and
    /// nowhere else.
    text: String,
    /// `routes::selection::replace`, pre-loaded with the injection settings
    /// this dictation's snapshot carried.
    replace: Replace,
}

/// What an edit to the selection will paste, or `None` if the model produced
/// nothing worth pasting.
///
/// THE UNTRIMMED-PASTE PROPERTY. The emptiness test is on the **trimmed**
/// reply and the value handed back is the **untrimmed** one, and the
/// asymmetry is deliberate on both sides. A reply of nothing but whitespace
/// is a failed edit, not an instruction to blank the user's paragraph. But
/// whitespace *inside* a real reply is part of the replacement: it stands in
/// for a span the user chose, which may well have started with two spaces of
/// indentation or ended with the newline that separates it from the next
/// line, and the prompt has just told the model to preserve exactly that.
///
/// This is the one place the agent route does not trim, and the contrast with
/// the type-at-the-cursor path in [`job`] is the whole reason it is a named
/// function rather than an inline condition: there the reply is inserted at a
/// caret, where a leading newline from the model (Sarvam sends them) lands in
/// the document as a blank line nobody asked for; here it is substituted for
/// a span whose shape the app has promised to keep.
///
/// **In practice only the leading whitespace can survive**, and that is not
/// this function's doing. `ChatReply::strip_end_marker` `trim_end()`s
/// what it hands back, because sarvam-105b punctuates the marker as the last
/// word of a sentence in a third of live replies and the tolerance that
/// accommodates it cannot tell a model's own trailing newline from the one it
/// wrote before our sentinel. Keeping the trailing end would mean rejecting
/// every reply with a newline before the marker as truncated, a third of them
/// on this backend. `only_the_leading_whitespace_can_survive_the_marker_strip`
/// pins that trade.
fn replacement_of(reply: &str) -> Option<&str> {
    (!reply.trim().is_empty()).then_some(reply)
}

/// Hand a finished dictation to the voice agent.
///
/// `command` is the text the agent is asked to act on; `raw` stays the
/// verbatim spoken transcript and is what History files as the row's
/// `raw_text`. The two callers choose `command` differently, and both are
/// right about their own path:
///
/// * **The agent chord** (`routes::apply`'s `Agent` arm) passes the
///   pipeline's *cleaned* output. The rule stages have already applied the
///   user's own replacements and spoken-command handling to it, so proper
///   nouns are spelt the way that user spells them before the model reasons
///   about them, and the polish pass cannot have executed the command (its
///   prompt forbids exactly that). The whole dictation is the command, so
///   nothing has to be removed from it.
/// * **The wake word** (`routes::wake`) passes the *raw* transcript with the
///   wake prefix stripped off. A prefix can only be removed from the string it
///   was found in, and detection has to read the raw one — see that module.
///   What is given up is the rule pipeline's spelling fixes, which reach the
///   model anyway as the dictionary in its own system turn.
///
/// Always `Deferred` once a backend resolves: this runs on the controller thread,
/// the single consumer of `ControlMsg`, and a blocking chat call here would
/// stall the pill, the hotkeys and every subsequent chord for the length of
/// the request.
pub fn apply(command: String, raw: &str, ctx: &RouteCtx<'_>) -> RouteOutcome {
    // The gate is "is there a chat backend", not "is there a Sarvam key": the
    // agent is a chat-completions call, and that call can go to the user's
    // own endpoint. An install with no Sarvam account at all runs the agent
    // instead of being told to add a key it will never have.
    // `routes::resolve` and the selection lane are gated on the same fact, so
    // the chord-time notice and this refusal cannot disagree.
    let lane = crate::controller::lane_for(ctx.settings);
    let sarvam_key = ctx.api_key();
    // The same chat model the cloud polish pass uses — one configured model
    // per install, so a user who has switched models has switched this too.
    // Ignored when the custom endpoint answers: it names its own.
    let model = &ctx.settings.sarvam.polish_model;
    let chat = match &lane {
        // Unchanged: the key is in memory, so the whole backend resolves here.
        crate::sarvam::Lane::Byok => {
            crate::endpoint::resolve_polish_backend(sarvam_key.as_deref(), model)
                .map(ChatTarget::from)
        }
        // The relay's bearer is an await away and this is the controller
        // thread, so only the *question* is answered here — and it needs no
        // credential: a signed-in Cloud install always has a host for chat.
        crate::sarvam::Lane::Cloud { .. } => {
            crate::endpoint::chat_available(crate::auth::session::status().signed_in).map(|()| {
                ChatTarget::Cloud {
                    lane: lane.clone(),
                    sarvam_key: sarvam_key.clone(),
                    model: model.clone(),
                }
            })
        }
    };
    let chat = match chat {
        Ok(chat) => chat,
        Err(why) => {
            // Not a switch to cleanup and not a silent skip: the chord asked for the
            // agent, the agent cannot be reached, and the command is not text
            // to type.
            tracing::debug!(
                reason = ?why,
                raw_chars = raw.chars().count(),
                "voice agent has no chat backend; typing nothing"
            );
            return RouteOutcome::Ready {
                text: None,
                notice: Some(match why {
                    crate::endpoint::Unavailable::NoSarvamKey => AGENT_UNAVAILABLE_NOTICE,
                    // Names the thing the user can actually fix.
                    crate::endpoint::Unavailable::CustomEndpoint(invalid) => invalid.message(),
                }
                .into()),
            };
        }
    };
    // Consulted after the backend check, deliberately: with no backend nothing runs
    // at all, and "voice agent unavailable" is the fix the user needs to hear
    // before anything about their selection.
    let edit = match selection::plan(ctx.selection) {
        selection::Plan::TypeAtCursor => None,
        // No model call. A selection may be sitting live in the document and
        // the read did not settle whether it is — so an answer typed at the
        // cursor would land on top of it. Asking anyway and then discarding
        // the reply would spend the user's quota to arrive at the same
        // silence.
        selection::Plan::Refuse(notice) => {
            tracing::debug!("selection capture did not settle; typing nothing");
            return RouteOutcome::Ready {
                text: None,
                notice: Some(notice.into()),
            };
        }
        selection::Plan::EditSelection(s) => {
            // The id, never the text — see the module's privacy note. This
            // is the session the verify-then-replace step will redeem.
            tracing::debug!(
                session = %s.session,
                selection_chars = s.chars,
                "agent command carries a captured selection"
            );
            // Read off the dictation's own settings snapshot here, not inside
            // the closure: a user who toggles "restore clipboard" while the
            // model is answering must not change the terms of the paste that
            // is already in flight.
            let restore_clipboard = ctx.settings.injection.restore_clipboard;
            let restore_delay_ms = ctx.settings.injection.restore_delay_ms;
            Some(Edit {
                session: s.session.clone(),
                text: s.text.clone(),
                replace: Box::new(move |session, replacement| {
                    selection::replace(session, replacement, restore_clipboard, restore_delay_ms)
                }),
            })
        }
    };
    RouteOutcome::Deferred(job(
        // Cloned, not borrowed: the job outlives the `RouteCtx` it was built
        // from. The client is `Arc`-backed, so this keeps the TLS pool.
        ctx.http.clone(),
        chat,
        ctx.agent_name.to_string(),
        ctx.settings.dictionary.clone(),
        command,
        edit,
        // The prompts the user saved on the Prompts page, cloned for the same reason
        // everything else here is: the job outlives the `RouteCtx`. A test
        // run passes its unsaved draft the same way — see
        // `sarvam::chat::AgentPrompts`.
        ctx.settings.prompts.agent.clone(),
        ctx.settings.prompts.selection_rules.clone(),
    ))
}

/// The network half, split from `apply` so a test can point it at a loopback
/// server without a live Sarvam key — the same seam `sarvam::chat`'s own
/// round-trip tests use.
///
/// `edit` is the selection lane's half, owned rather than borrowed for the
/// same reason everything else here is: the job outlives the `RouteCtx`. Its
/// presence switches both the prompt (`sarvam::chat::agent` builds the
/// turns for an edit to the selection and raises the token budget) and what
/// happens to the reply — pasted over a verified selection here, handed back to
/// the caller to type at the cursor when there is none.
#[allow(clippy::too_many_arguments)]
fn job(
    http: reqwest::Client,
    chat: impl Into<ChatTarget>,
    agent_name: String,
    dictionary: Vec<String>,
    command: String,
    edit: Option<Edit>,
    brief_override: Option<String>,
    selection_rules_override: Option<String>,
) -> RouteJob {
    let chat = chat.into();
    RouteJob::new(async move {
        // On every lane but Cloud this is already in hand and resolves
        // without awaiting anything; on Cloud it is the bearer, fetched once,
        // here, a step before the request that carries it.
        let backend = match chat.resolve().await {
            Ok(backend) => backend,
            Err(notice) => {
                tracing::debug!("voice agent has no chat backend; typing nothing");
                return decline(notice);
            }
        };
        match crate::sarvam::chat::agent(
            &http,
            &backend,
            &agent_name,
            &dictionary,
            &command,
            edit.as_ref().map(|e| e.text.as_str()),
            crate::sarvam::chat::AgentPrompts {
                brief: brief_override.as_deref(),
                selection_rules: selection_rules_override.as_deref(),
            },
        )
        .await
        {
            Err(e) => {
                // `{e:#}` — the whole anyhow chain. reqwest's top-level
                // Display for a send-phase timeout is the same generic string
                // it uses for a refused connection. Through `redact_urls`:
                // reqwest's Display embeds the request URL, which for the
                // custom slot is a pasted string that can carry a credential
                // (`https://user:pw@host`, `?api_key=`).
                let reason = crate::format::backend::redact_urls(&format!("{e:#}"));
                tracing::warn!("voice agent call failed: {reason}");
                decline(crate::sarvam::chat::failure_sentence(&e).unwrap_or(AGENT_UNAVAILABLE_NOTICE))
            }
            // MARKER TRUNCATION, and it matters more on the selection path
            // than anywhere else in this app: a reply that stopped mid-sentence
            // is still a *complete-looking* replacement, and pasting it over
            // the selection would delete the rest of the user's paragraph and
            // put half a sentence in its place. `sarvam::chat::agent` rewrites
            // `finish_reason` to "length" when the reply does not end with the
            // marker it minted, so a transport that dropped the tail while
            // reporting success lands here too.
            Ok(reply) if reply.was_truncated() => {
                tracing::warn!(
                    finish_reason = %reply.finish_reason,
                    reply_chars = reply.text.chars().count(),
                    "voice agent reply was truncated; typing nothing"
                );
                decline(AGENT_TRUNCATED_NOTICE)
            }
            Ok(reply) => match edit {
                // No selection: the reply is inserted at the caret, so it is
                // trimmed. Tone and smart-space run after this, and a leading
                // newline from the model (Sarvam sends them) would land in the
                // document. The emptiness check has to be on the trimmed form,
                // or a reply of one newline reads as an answer.
                None => {
                    let text = reply.text.trim();
                    if text.is_empty() {
                        tracing::warn!("voice agent returned an empty reply; typing nothing");
                        return decline(AGENT_EMPTY_NOTICE);
                    }
                    RouteDone {
                        text: Some(text.to_string()),
                        notice: None,
                        pasted: false,
                    }
                }
                Some(edit) => {
                    // THE EMPTY-EDIT GUARD. A model that answers with
                    // whitespace has not edited the selection down to nothing,
                    // it has failed — and "replace the paragraph with a
                    // space" is the single most destructive thing this route
                    // could do. Note what is *kept*: the untrimmed reply. See
                    // `replacement_of`.
                    let Some(replacement) = replacement_of(&reply.text) else {
                        tracing::warn!(
                            session = %edit.session,
                            "the reply for the selection was blank; leaving the selection as it was"
                        );
                        return decline(AGENT_EMPTY_NOTICE);
                    };
                    let replacement = replacement.to_string();
                    match run_replace(edit, replacement.clone()).await {
                        selection::Replacement::Replaced => RouteDone {
                            text: Some(replacement),
                            notice: None,
                            // Already in the document, and it had to be: the
                            // paste has to happen inside the verification
                            // window the replace step opened, not a controller
                            // round trip later.
                            pasted: true,
                        },
                        selection::Replacement::Declined(notice) => decline(notice),
                    }
                }
            },
        }
    })
}

/// Run the blocking verify-then-replace somewhere other than the async
/// runtime.
///
/// A foreground switch, a full synthetic-copy round trip and a paste with its
/// clipboard restore — the better part of a second of sleeping in the worst
/// case — have no business on a tokio worker, which is shared with every other
/// request the app has in flight. Its own thread, like every other
/// synthetic-input path here (`controller::start_injection`,
/// `controller::start_replace`, `transforms::spawn`).
///
/// A thread that dies without answering declines, like everything else in this
/// module: the paste either happened and said so, or it did not happen.
async fn run_replace(edit: Edit, replacement: String) -> selection::Replacement {
    let (tx, rx) = tokio::sync::oneshot::channel();
    std::thread::spawn(move || {
        let _ = tx.send((edit.replace)(&edit.session, &replacement));
    });
    rx.await.unwrap_or(selection::Replacement::Declined(
        selection::SELECTION_PASTE_FAILED_NOTICE,
    ))
}

/// Type nothing, and say why. The only failure shape this module has.
///
/// `impl Into<String>` rather than `&str` because one of the reasons is not a
/// constant: the Cloud lane's credential failure arrives as a finished
/// sentence from `auth::session`, which is the one place that knows whether
/// this user needs to sign in again or is simply offline.
fn decline(notice: impl Into<String>) -> RouteDone {
    RouteDone {
        text: None,
        notice: Some(notice.into()),
        pasted: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, RwLock};

    const COMMAND: &str = "Butterfly, delete that paragraph.";

    /// What the stub replace was asked to paste, and what it answered.
    ///
    /// A `Mutex<Option<String>>` rather than a channel because the assertion
    /// is "these exact bytes reached the document", which is only worth making
    /// after the job has finished and the recorder is the only thing that
    /// still knows.
    #[derive(Default)]
    struct Pasted(Arc<std::sync::Mutex<Option<String>>>);

    impl Pasted {
        /// A replace that records what it was handed and reports `outcome`.
        fn stub(&self, outcome: selection::Replacement) -> Replace {
            let seen = self.0.clone();
            Box::new(move |_, replacement| {
                *seen.lock().expect("pasted lock") = Some(replacement.to_string());
                outcome.clone()
            })
        }

        fn seen(&self) -> Option<String> {
            self.0.lock().expect("pasted lock").clone()
        }
    }

    /// An [`Edit`] whose replace is `edit`'s stub, over `selection` as the
    /// captured text.
    fn an_edit(selection: &str, replace: Replace) -> Edit {
        Edit {
            session: selection::test_session("a-session"),
            text: selection.to_string(),
            replace,
        }
    }

    /// Runs the route's own job against a loopback server that answers one
    /// chat request with `content`. `content` may embed `{marker}`, which is
    /// replaced with the end marker the request actually carried — the
    /// stub cannot know it in advance, exactly like a real model.
    async fn run_against_stub(content: impl Into<String>) -> RouteDone {
        run_against_stub_with(content, None).await.0
    }

    /// [`run_against_stub`] plus the request body the stub actually received,
    /// so the selection's trip into the prompt can be asserted on the wire
    /// rather than on the builder.
    async fn run_against_stub_with(
        content: impl Into<String>,
        edit: Option<Edit>,
    ) -> (RouteDone, String) {
        let content = content.into();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind loopback listener");
        let addr = listener.local_addr().expect("local addr");
        let (seen_tx, seen_rx) = tokio::sync::oneshot::channel::<String>();
        tokio::spawn(async move {
            if let Ok((socket, _)) = listener.accept().await {
                let body = read_request_body(&socket).await;
                let _ = seen_tx.send(body.clone());
                // Mirrors format::backend::mint_end_marker's shape.
                let marker = regex::Regex::new(r"<<[A-Z2-9]{4}>>")
                    .unwrap()
                    .find(&body)
                    .expect("the request must carry an end marker")
                    .as_str()
                    .to_string();
                let reply = serde_json::json!({
                    "choices": [{"finish_reason": "stop",
                                 "message": {"content": content.replace("{marker}", &marker)}}],
                    "usage": {"prompt_tokens": 5, "completion_tokens": 3},
                })
                .to_string();
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    reply.len(),
                    reply
                );
                let bytes = response.into_bytes();
                let mut written = 0;
                while written < bytes.len() {
                    socket.writable().await.expect("socket writable");
                    match socket.try_write(&bytes[written..]) {
                        Ok(n) => written += n,
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => continue,
                        Err(e) => panic!("failed writing stub response: {e}"),
                    }
                }
            }
        });
        let mut backend = Backend::sarvam("k", "sarvam-105b");
        backend.base_url = format!("http://{addr}");
        let done = job(
            reqwest::Client::new(),
            backend,
            "Butterfly".into(),
            Vec::new(),
            COMMAND.into(),
            edit,
            None,
            None,
        )
        .into_future()
        .await;
        (done, seen_rx.await.unwrap_or_default())
    }

    /// Enough of HTTP/1.1 to find the body — same hand-rolled reader
    /// `sarvam::chat`'s round-trip tests use, for the same reason (no HTTP
    /// server dependency in this crate).
    async fn read_request_body(socket: &tokio::net::TcpStream) -> String {
        let mut buf = Vec::new();
        let mut chunk = [0u8; 8192];
        loop {
            if let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                let headers = String::from_utf8_lossy(&buf[..end]);
                let len: usize = headers
                    .lines()
                    .filter_map(|l| l.split_once(':'))
                    .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
                    .and_then(|(_, v)| v.trim().parse().ok())
                    .unwrap_or(0);
                if buf.len() >= end + 4 + len {
                    return String::from_utf8_lossy(&buf[end + 4..end + 4 + len]).to_string();
                }
            }
            socket.readable().await.expect("socket readable");
            match socket.try_read(&mut chunk) {
                Ok(0) => return String::from_utf8_lossy(&buf).to_string(),
                Ok(n) => buf.extend_from_slice(&chunk[..n]),
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => continue,
                Err(e) => panic!("failed reading stub request: {e}"),
            }
        }
    }

    // --- Never cleanup ------------------------------------------------------

    /// The defect this route exists to not have. With no key the agent cannot
    /// run, and the answer must be "nothing, and here is why" — never the
    /// command itself, cleaned into prose and typed into the document.
    #[test]
    fn an_agent_with_no_key_types_nothing_and_says_why() {
        let http = reqwest::Client::new();
        let api_key: crate::sarvam::SharedKey = Arc::new(RwLock::new(None));
        let s = crate::settings::Settings::default();
        let ctx = RouteCtx {
            chord: crate::routes::ChordKind::Agent,
            http: &http,
            api_key: &api_key,
            target_language: &s.translation.target_language,
            agent_name: &s.agent.name,
            settings: &s,
            selection: None,
        };
        let RouteOutcome::Ready { text, notice } =
            apply(COMMAND.into(), "butterfly delete that paragraph", &ctx)
        else {
            panic!("with no key there is nothing to defer to");
        };
        assert_eq!(text, None, "a command must never be pasted as prose");
        assert_eq!(notice.as_deref(), Some(AGENT_UNAVAILABLE_NOTICE));
    }

    /// The network call must never happen on the controller thread — it is
    /// the single consumer of `ControlMsg`, so a blocking request there
    /// freezes the pill, the hotkeys and every subsequent chord.
    #[test]
    fn a_usable_agent_defers_instead_of_blocking_the_controller() {
        let http = reqwest::Client::new();
        let api_key: crate::sarvam::SharedKey = Arc::new(RwLock::new(Some("k".into())));
        let s = crate::settings::Settings::default();
        let ctx = RouteCtx {
            chord: crate::routes::ChordKind::Agent,
            http: &http,
            api_key: &api_key,
            target_language: &s.translation.target_language,
            agent_name: &s.agent.name,
            settings: &s,
            selection: None,
        };
        assert!(
            matches!(
                apply(COMMAND.into(), "butterfly delete that paragraph", &ctx),
                RouteOutcome::Deferred(_)
            ),
            "an agent call must be handed to the runtime, never run inline"
        );
    }

    /// Every way the call can fail ends in the same shape: no text. This is
    /// the assertion that would fail if anyone ever "helpfully" made an
    /// unreachable agent paste the transcript instead.
    #[tokio::test]
    async fn a_dead_endpoint_declines_rather_than_pasting_the_command() {
        // Bound but never listening, so a connect is refused. The socket lives
        // to the end of the test: a port released here could be handed to
        // another test's listener, which would then answer this request.
        let socket = tokio::net::TcpSocket::new_v4().expect("create socket");
        socket
            .bind("127.0.0.1:0".parse().expect("loopback address"))
            .expect("bind loopback socket");
        let addr = socket.local_addr().expect("local addr");

        let mut backend = Backend::sarvam("k", "sarvam-105b");
        backend.base_url = format!("http://{addr}");
        let done = job(
            reqwest::Client::new(),
            backend,
            "Butterfly".into(),
            Vec::new(),
            COMMAND.into(),
            None,
            None,
            None,
        )
        .into_future()
        .await;
        assert_eq!(done.text, None, "an unreachable agent must type nothing");
        assert_eq!(done.notice.as_deref(), Some(AGENT_UNAVAILABLE_NOTICE));
    }

    // --- The output guard ---------------------------------------------------

    /// The answer is what gets typed, and it arrives trimmed: Sarvam prefixes
    /// replies with a newline often enough that the parser's own fixture has
    /// one, and tone plus smart-space run on whatever leaves here.
    #[tokio::test]
    async fn an_agent_answer_is_what_gets_typed() {
        let done = run_against_stub("\nShipped two things this week.{marker}").await;
        assert_eq!(done.text.as_deref(), Some("Shipped two things this week."));
        assert_eq!(done.notice, None, "a working route has nothing to explain");
    }

    /// An empty answer is not an answer, and the transcript is not typed in
    /// its place: that would type the user's own command out.
    #[tokio::test]
    async fn an_empty_reply_types_nothing_and_says_so() {
        for content in ["{marker}", "   {marker}", "\n\n{marker}"] {
            let done = run_against_stub(content).await;
            assert_eq!(done.text, None, "content {content:?}");
            assert_eq!(done.notice.as_deref(), Some(AGENT_EMPTY_NOTICE));
        }
    }

    /// A reply that never reached its marker ends mid-thought — and the
    /// notice must say *that*, not "unavailable": the agent answered, the
    /// answer just did not arrive whole.
    #[tokio::test]
    async fn a_truncated_reply_types_nothing_and_names_the_truncation() {
        let done = run_against_stub("Half an ans").await;
        assert_eq!(done.text, None, "half an answer must not reach the document");
        assert_eq!(done.notice.as_deref(), Some(AGENT_TRUNCATED_NOTICE));
        assert_ne!(done.notice.as_deref(), Some(AGENT_UNAVAILABLE_NOTICE));
    }

    // --- the selection lane -------------------------------------------------

    macro_rules! ctx {
        ($http:expr, $key:expr, $s:expr, $sel:expr) => {
            RouteCtx {
                chord: crate::routes::ChordKind::Agent,
                http: &$http,
                api_key: &$key,
                settings: &$s,
                target_language: &$s.translation.target_language,
                agent_name: &$s.agent.name,
                selection: $sel,
            }
        };
    }

    fn a_key() -> crate::sarvam::SharedKey {
        Arc::new(RwLock::new(Some("k".into())))
    }

    /// Nothing to edit, so the route is the one it has always been: the
    /// command goes to the model and the answer is typed at the cursor.
    #[test]
    fn a_capture_with_nothing_to_overwrite_runs_the_ordinary_agent_route() {
        let http = reqwest::Client::new();
        let key = a_key();
        let s = crate::settings::Settings::default();
        for capture in [
            None,
            Some(selection::Capture::Nothing),
            Some(selection::Capture::Unreadable(selection::Why::NoWindow)),
        ] {
            let ctx = ctx!(http, key, s, capture.as_ref());
            assert!(
                matches!(
                    apply(COMMAND.into(), "butterfly delete that", &ctx),
                    RouteOutcome::Deferred(_)
                ),
                "{capture:?} must not stop the command from running"
            );
        }
    }

    /// A capture that did not settle whether a selection exists must not
    /// reach the model at all — `Ready`, not `Deferred`, is the assertion that
    /// no request is made: a reply generated against a selection the app
    /// could not read has nowhere safe to land, and typing it at the cursor
    /// would paste over the selection it was about.
    #[test]
    fn a_capture_failure_types_nothing_and_never_calls_the_model() {
        let http = reqwest::Client::new();
        let key = a_key();
        let s = crate::settings::Settings::default();
        for (capture, want) in [
            (
                selection::Capture::Oversized {
                    chars: selection::MAX_SELECTION_CODE_POINTS + 1,
                },
                selection::SELECTION_TOO_LARGE_NOTICE,
            ),
            (selection::Capture::FocusMoved, selection::SELECTION_MOVED_NOTICE),
            (
                selection::Capture::Unreadable(selection::Why::ClipboardFailed),
                selection::SELECTION_UNREADABLE_NOTICE,
            ),
            (
                selection::Capture::Unreadable(selection::Why::TimedOut),
                selection::SELECTION_UNREADABLE_NOTICE,
            ),
        ] {
            let ctx = ctx!(http, key, s, Some(&capture));
            let RouteOutcome::Ready { text, notice } =
                apply(COMMAND.into(), "butterfly delete that", &ctx)
            else {
                panic!("{capture:?} must not reach the network");
            };
            assert_eq!(text, None, "{capture:?}");
            assert_eq!(notice.as_deref(), Some(want), "{capture:?}");
        }
    }

    /// A missing key is the more actionable diagnosis and is decided first:
    /// with no key nothing would run whatever the selection said.
    #[test]
    fn a_missing_key_is_reported_before_a_capture_failure() {
        let http = reqwest::Client::new();
        let key: crate::sarvam::SharedKey = Arc::new(RwLock::new(None));
        let s = crate::settings::Settings::default();
        let capture = selection::Capture::Oversized { chars: 9000 };
        let ctx = ctx!(http, key, s, Some(&capture));
        let RouteOutcome::Ready { text, notice } =
            apply(COMMAND.into(), "butterfly delete that", &ctx)
        else {
            panic!("with no key there is nothing to defer to");
        };
        assert_eq!(text, None);
        assert_eq!(notice.as_deref(), Some(AGENT_UNAVAILABLE_NOTICE));
    }

    /// A captured selection at the `apply` seam: it does not stop the
    /// command, it informs it.
    #[test]
    fn a_selected_capture_defers_like_any_other_agent_command() {
        let http = reqwest::Client::new();
        let key = a_key();
        let s = crate::settings::Settings::default();
        let capture = selection::test_selection("the quick brown fox");
        let ctx = ctx!(http, key, s, Some(&capture));
        assert!(matches!(
            apply(COMMAND.into(), "butterfly fix that", &ctx),
            RouteOutcome::Deferred(_)
        ));
    }

    /// THE WHOLE LANE, end to end with a stubbed capture and a stubbed
    /// replace: the selection reaches the model on the JSON boundary, the
    /// reply is verified-and-pasted rather than handed back to be typed at the
    /// caret, and what the caller gets is a row for History plus `pasted`.
    #[tokio::test]
    async fn a_selected_capture_replaces_the_selection_instead_of_typing() {
        let pasted = Pasted::default();
        let (done, body) = run_against_stub_with(
            "Shipped two things this week.{marker}",
            Some(an_edit(
                "shipped 2 things this wk",
                pasted.stub(selection::Replacement::Replaced),
            )),
        )
        .await;
        assert_eq!(
            pasted.seen().as_deref(),
            Some("Shipped two things this week."),
            "the model's reply is what goes into the document"
        );
        assert!(done.pasted, "the route pasted it itself");
        assert_eq!(
            done.text.as_deref(),
            Some("Shipped two things this week."),
            "and hands it back only so History can file what was produced"
        );
        assert_eq!(done.notice, None);
        assert!(
            body.contains("shipped 2 things this wk"),
            "the selection must reach the prompt"
        );
        assert!(
            body.contains("spoken_request"),
            "and it must arrive on the JSON boundary, not spliced into the command"
        );
    }

    /// ...and without one, nothing about selections is said to the model. A
    /// plain agent command must not start explaining a `selection` field
    /// that is not there — and its answer comes back to be typed, not pasted.
    #[tokio::test]
    async fn an_ordinary_agent_command_carries_no_selection_scaffolding() {
        let (done, body) = run_against_stub_with("An answer.{marker}", None).await;
        assert!(!body.contains("spoken_request"));
        assert!(!body.contains(r#"\"selection\""#));
        assert!(!done.pasted, "nothing but the selection lane pastes");
        assert_eq!(done.text.as_deref(), Some("An answer."));
    }

    /// FAILS CLOSED. Every way the verify-then-replace can decline ends the
    /// same way: no text, the replace step's own sentence on the pill, and
    /// nothing marked as pasted — so the controller files a declined row
    /// rather than a completed one, and never injects the replacement as a
    /// consolation prize.
    #[tokio::test]
    async fn a_declined_replace_types_nothing_and_passes_its_reason_through() {
        for notice in [
            selection::SELECTION_CHANGED_NOTICE,
            selection::SELECTION_EXPIRED_NOTICE,
            selection::SELECTION_MOVED_NOTICE,
            selection::SELECTION_UNREADABLE_NOTICE,
            selection::SELECTION_PASTE_FAILED_NOTICE,
        ] {
            let (done, _) = run_against_stub_with(
                "Shipped two things this week.{marker}",
                Some(an_edit(
                    "shipped 2 things this wk",
                    Box::new(move |_, _| selection::Replacement::Declined(notice)),
                )),
            )
            .await;
            assert_eq!(done.text, None, "{notice}");
            assert!(!done.pasted, "{notice}");
            assert_eq!(done.notice.as_deref(), Some(notice));
        }
    }

    /// THE EMPTY-EDIT GUARD, at the seam that matters: a whitespace-only reply
    /// never reaches the document at all. Not "replace the paragraph with a
    /// space" — the replace step is never called, so the user's selection is
    /// still theirs and the verification is not even spent on it.
    #[tokio::test]
    async fn an_empty_edit_never_reaches_the_document() {
        for content in ["{marker}", "   {marker}", "\n\n{marker}", " \t \n {marker}"] {
            let pasted = Pasted::default();
            let (done, _) = run_against_stub_with(
                content,
                Some(an_edit(
                    "the user's paragraph",
                    pasted.stub(selection::Replacement::Replaced),
                )),
            )
            .await;
            assert_eq!(pasted.seen(), None, "content {content:?} must not be pasted");
            assert_eq!(done.text, None, "content {content:?}");
            assert!(!done.pasted);
            assert_eq!(done.notice.as_deref(), Some(AGENT_EMPTY_NOTICE));
        }
    }

    /// THE UNTRIMMED-PASTE PROPERTY, asserted through the whole job rather
    /// than on `replacement_of` alone: this route trims nothing. The selection
    /// being replaced had its own indentation and the prompt told the model to
    /// preserve it, so trimming here would quietly overrule both.
    ///
    /// LEADING whitespace only, and the asymmetry is not this route's doing —
    /// see `only_the_leading_whitespace_can_survive_the_marker_strip` below
    /// for where the trailing end goes and why.
    #[tokio::test]
    async fn the_pasted_replacement_is_the_untrimmed_reply() {
        for reply in ["  indented replacement", "\tone tab in front", "\n\na blank line first"] {
            let pasted = Pasted::default();
            let (done, _) = run_against_stub_with(
                format!("{reply}{{marker}}"),
                Some(an_edit(
                    "the user's paragraph",
                    pasted.stub(selection::Replacement::Replaced),
                )),
            )
            .await;
            assert_eq!(
                pasted.seen().as_deref(),
                Some(reply),
                "the reply must reach the document unedited"
            );
            assert_eq!(done.text.as_deref(), Some(reply), "and be filed the same way");
        }
    }

    /// THE TRAILING-WHITESPACE TRADE, pinned. Slicing the marker off by
    /// length and trimming nothing would keep a replacement's trailing
    /// newline, at the price of a model that writes a newline *before* the
    /// marker failing the check and losing the whole command as "truncated".
    ///
    /// This app makes the other trade, on measurements:
    /// sarvam-105b ends a third of live replies with the marker punctuated as
    /// the last word of a sentence, so `ChatReply::strip_end_marker`
    /// tolerates a short noise tail and `trim_end()`s what is left. The
    /// information needed to tell "the model's own trailing newline" from "the
    /// newline it put before our sentinel" is destroyed by that tolerance, and
    /// it is not recoverable here.
    ///
    /// So an edit to the selection cannot end in whitespace. The cost is a
    /// replacement that should have kept a trailing newline losing it; the cost
    /// of the other trade is one edit in three silently refusing to run.
    #[tokio::test]
    async fn only_the_leading_whitespace_can_survive_the_marker_strip() {
        let pasted = Pasted::default();
        let (done, _) = run_against_stub_with(
            "\n  both ends  \n{marker}",
            Some(an_edit(
                "the user's paragraph",
                pasted.stub(selection::Replacement::Replaced),
            )),
        )
        .await;
        assert_eq!(
            pasted.seen().as_deref(),
            Some("\n  both ends"),
            "leading whitespace survives; the trailing end went with the marker"
        );
        assert_eq!(done.text.as_deref(), Some("\n  both ends"));
    }

    /// ...and the type-at-the-cursor path still trims, which is the contrast that
    /// makes the rule above a rule rather than an oversight. A reply typed at
    /// a caret carries Sarvam's habitual leading newline into the document as
    /// a blank line nobody asked for.
    #[tokio::test]
    async fn a_reply_typed_at_the_caret_is_still_trimmed() {
        let done = run_against_stub("\n  Shipped two things this week.  \n{marker}").await;
        assert_eq!(done.text.as_deref(), Some("Shipped two things this week."));
    }

    /// The property under the two tests above, on its own: the emptiness test
    /// is on the trimmed reply and the value kept is the untrimmed one.
    #[test]
    fn replacement_of_guards_on_the_trimmed_form_and_keeps_the_untrimmed_one() {
        for empty in ["", " ", "\n", "\t\r\n  ", "\u{a0}"] {
            assert_eq!(replacement_of(empty), None, "{empty:?} is not an edit");
        }
        for kept in ["  x", "x\n", "\n x \n", "x"] {
            assert_eq!(
                replacement_of(kept),
                Some(kept),
                "a real reply survives whole"
            );
        }
    }

    /// MARKER TRUNCATION on the selection path. A reply that never reached its
    /// end marker is half a replacement, and pasting it would delete
    /// the rest of the user's paragraph to put half a sentence in its place —
    /// so the replace step is never even called.
    #[tokio::test]
    async fn a_truncated_edit_to_the_selection_never_reaches_the_document() {
        let pasted = Pasted::default();
        let (done, _) = run_against_stub_with(
            "Shipped two thi",
            Some(an_edit(
                "the user's paragraph",
                pasted.stub(selection::Replacement::Replaced),
            )),
        )
        .await;
        assert_eq!(pasted.seen(), None, "half an answer must not be pasted");
        assert_eq!(done.text, None);
        assert!(!done.pasted);
        assert_eq!(done.notice.as_deref(), Some(AGENT_TRUNCATED_NOTICE));
    }

    /// An unreachable model on the selection path declines like any other
    /// agent failure — and, critically, without touching the selection.
    #[tokio::test]
    async fn a_dead_endpoint_leaves_the_selection_alone() {
        // Bound but never listening, so a connect is refused; held to the end
        // of the test so the port cannot go to another test's listener.
        let socket = tokio::net::TcpSocket::new_v4().expect("create socket");
        socket
            .bind("127.0.0.1:0".parse().expect("loopback address"))
            .expect("bind loopback socket");
        let addr = socket.local_addr().expect("local addr");

        let pasted = Pasted::default();
        let mut backend = Backend::sarvam("k", "sarvam-105b");
        backend.base_url = format!("http://{addr}");
        let done = job(
            reqwest::Client::new(),
            backend,
            "Butterfly".into(),
            Vec::new(),
            COMMAND.into(),
            Some(an_edit(
                "the user's paragraph",
                pasted.stub(selection::Replacement::Replaced),
            )),
            None,
            None,
        )
        .into_future()
        .await;
        assert_eq!(pasted.seen(), None);
        assert_eq!(done.text, None);
        assert!(!done.pasted);
        assert_eq!(done.notice.as_deref(), Some(AGENT_UNAVAILABLE_NOTICE));
    }

    /// The three declines are three different sentences. A user who is told
    /// "unavailable" when the model actually answered with nothing has been
    /// pointed at the wrong fix.
    #[test]
    fn every_decline_says_something_different() {
        let notices = [
            AGENT_UNAVAILABLE_NOTICE,
            AGENT_EMPTY_NOTICE,
            AGENT_TRUNCATED_NOTICE,
        ];
        for (i, a) in notices.iter().enumerate() {
            assert!(!a.is_empty());
            for b in &notices[i + 1..] {
                assert_ne!(a, b);
            }
        }
    }
}
