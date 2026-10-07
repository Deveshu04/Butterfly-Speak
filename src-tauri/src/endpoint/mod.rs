//! The one custom OpenAI-compatible endpoint slot.
//!
//! Butterfly Speak ships Sarvam. This module is the seam that lets a user
//! point the chat half of the app somewhere else — a self-hosted llama.cpp /
//! Ollama / vLLM server, or any host that speaks `/v1/chat/completions`.
//!
//! Three rules carry most of the weight:
//!
//! - [`trim_pasted_route`] reduces a pasted URL to its base: a trailing
//!   API route and trailing slashes come off, and a `?query` or `#fragment`
//!   stays at the end of every route built from it, because gateway URLs
//!   carry settings such as `?api-version=` there.
//! - [`is_local_network_host`] decides whether plain `http://` is allowed. It
//!   parses IP literals and judges the parsed address, and it accepts names
//!   only from the special-use local suffixes, so a public name that merely
//!   looks like a private address (`192-168-1-5.example.net`) never qualifies.
//! - [`resolve_base`] **fails closed**. An empty or unusable URL is an error;
//!   the request is never sent to any other host instead.
//!
//! This module is deliberately **not** referenced from `eval/`, `format/`,
//! `cleanup/` or `sarvam/chat.rs`: those are the trees `src/bin/fmtbench.rs`
//! compiles into itself via `#[path]`, and every module they name has to be
//! reachable from the benchmark binary too. Keeping the seam on this side of
//! that line is what lets `fmtbench` keep building without a shim for
//! `settings`, the credential store, or anything else the slot reads.

pub mod probe;

use crate::format::backend::Backend;
use crate::sarvam::key::{self, KeySlot};
use crate::sarvam::{Lane, Transport};
use crate::settings::{CustomEndpointSettings, Settings};
use std::net::{Ipv4Addr, Ipv6Addr};
use std::sync::{OnceLock, RwLock};

// ---------------------------------------------------------------------------
// URL normalization
// ---------------------------------------------------------------------------

/// Routes a user may paste on the end of their base URL, each written as it
/// follows the version segment. Taken from OpenAI's API reference and limited
/// to what someone setting up this slot copies out of a provider's docs or a
/// curl example:
///
/// - `/chat/completions`, `/models` and `/audio/transcriptions` are the three
///   routes this app calls.
/// - `/completions` is the text-completion route that llama.cpp, vLLM and
///   LM Studio show in their own quick-start examples.
/// - `/responses` is the route OpenAI's current quick-start examples use.
///
/// The order does not matter: [`strip_trailing_routes`] removes the longest
/// match, so `/chat/completions` is never mistaken for `/completions`.
const PASTED_ROUTES: [&str; 5] = [
    "/chat/completions",
    "/completions",
    "/responses",
    "/models",
    "/audio/transcriptions",
];

/// Splits `url` where its `?query` or `#fragment` starts. Routes are added
/// to the first half; the second half goes back on the end unchanged.
///
/// The first `?` or `#` is the right cut either way: a `?` after the `#` is
/// part of the fragment, and a `#` after the `?` ends the query.
fn split_off_tail(url: &str) -> (&str, &str) {
    match url.find(['?', '#']) {
        Some(at) => url.split_at(at),
        None => (url, ""),
    }
}

/// Splits a URL without its tail into `scheme://authority` and the path.
/// Without a `://` the first segment stands in for the authority, so a host
/// such as `models` or `v1` is never taken for part of the path.
fn split_off_path(url: &str) -> (&str, &str) {
    let authority_start = url.find("://").map_or(0, |at| at + 3);
    match url[authority_start..].find('/') {
        Some(at) => url.split_at(authority_start + at),
        None => (url, ""),
    }
}

/// Whether `path` ends in `suffix` (which starts with `/`), ignoring ASCII
/// case. The comparison is on bytes, and a match always begins at an ASCII
/// `/`, so slicing at `path.len() - suffix.len()` afterwards is safe.
fn ends_with_segments(path: &str, suffix: &str) -> bool {
    let (p, s) = (path.as_bytes(), suffix.as_bytes());
    p.len() >= s.len() && p[p.len() - s.len()..].eq_ignore_ascii_case(s)
}

/// Removes pasted routes from the end of `path`, then trailing slashes.
///
/// Stripping repeats until no route is left at the end, which makes
/// normalising an already normalised base a no-op. The route builders
/// normalise again whatever they are given, so a base that
/// [`resolve_base`] returned has to come back from them unchanged.
///
/// When a route was removed and a `v1` segment is left at the end, that
/// segment is written in lower case: it came from the same pasted route.
fn strip_trailing_routes(path: &str) -> String {
    let mut rest = path.trim_end_matches('/');
    let mut stripped = false;
    while let Some(route) = PASTED_ROUTES
        .iter()
        .filter(|route| ends_with_segments(rest, route))
        .max_by_key(|route| route.len())
    {
        rest = rest[..rest.len() - route.len()].trim_end_matches('/');
        stripped = true;
    }
    if stripped && ends_with_segments(rest, "/v1") {
        format!("{}/v1", &rest[..rest.len() - 3])
    } else {
        rest.to_string()
    }
}

/// The base URL a user meant, from whatever they pasted: surrounding
/// whitespace, a trailing OpenAI-style route and trailing slashes removed,
/// with any `?query#fragment` kept at the end.
///
/// Blank input gives an empty string. Nothing here supplies a host.
pub fn trim_pasted_route(value: &str) -> String {
    let value = value.trim();
    if value.is_empty() {
        return String::new();
    }
    let (url, tail) = split_off_tail(value);
    let (origin, path) = split_off_path(url);
    format!("{origin}{}{tail}", strip_trailing_routes(path))
}

/// The normalised base with a `/v1` segment on the end, added only when the
/// path does not already end in one.
///
/// `/V1` in any case counts as present and is left as the user wrote it:
/// adding a second segment would only produce `/V1/v1`, which no server
/// serves.
pub fn with_version_segment(base: &str) -> String {
    let normalized = trim_pasted_route(base);
    let (url, tail) = split_off_tail(&normalized);
    if url.is_empty() || ends_with_segments(split_off_path(url).1, "/v1") {
        return normalized;
    }
    format!("{url}/v1{tail}")
}

/// `route` appended to `base`'s path, in front of any `?query#fragment`.
fn append_route(base: &str, route: &str) -> String {
    let (url, tail) = split_off_tail(base);
    format!("{url}{route}{tail}")
}

/// The chat route the polish, agent, transform and note calls post to:
/// `{base}/v1/chat/completions`, with `/v1` added only when missing.
pub fn chat_completions_url(base: &str) -> String {
    append_route(&with_version_segment(base), "/chat/completions")
}

/// The transcription route: `{base}/audio/transcriptions` on the base
/// exactly as the user gave it. No version segment is ever added here,
/// because transcription servers differ on whether they serve one, and
/// the base the user typed is the only evidence of which kind this is.
pub fn transcriptions_url(base: &str) -> String {
    append_route(&trim_pasted_route(base), "/audio/transcriptions")
}

/// The model-listing route for a base URL.
///
/// Deliberately the *same* `/v1` treatment [`chat_completions_url`] gives the
/// chat route, rather than trying sibling bases and adopting whichever one
/// answers, which would rewrite the saved setting behind the user's back.
/// Discovery and the "Test connection" button both exist to answer "will polish
/// work?", and a probe that succeeds against a base inference will never use
/// answers the wrong question — worse than not answering it. One base, the real
/// one.
pub fn models_url(base: &str) -> String {
    append_route(&with_version_segment(base), "/models")
}

// ---------------------------------------------------------------------------
// When plain HTTP is allowed
// ---------------------------------------------------------------------------

/// Name suffixes that only resolve on the user's own machine or network.
/// Each matches on a label boundary: the host must end in `.{suffix}` with
/// at least one label in front.
///
/// - `localhost`: RFC 6761 section 6.3, loopback names.
/// - `local`: RFC 6762, multicast DNS names on the local link.
/// - `home.arpa`: RFC 8375, names inside a residential home network.
///
/// All three are in the IANA Special-Use Domain Names registry.
const LOCAL_NAME_SUFFIXES: [&str; 3] = ["localhost", "local", "home.arpa"];

/// IPv4 addresses a connection can only reach inside the user's own machine
/// or network, from the IANA IPv4 Special-Purpose Address Registry:
///
/// - `127.0.0.0/8` loopback (RFC 1122).
/// - `10.0.0.0/8`, `172.16.0.0/12`, `192.168.0.0/16` private-use (RFC 1918).
/// - `169.254.0.0/16` link-local (RFC 3927).
///
/// `100.64.0.0/10` (RFC 6598) is also not globally reachable, but it is the
/// carrier's shared address space, so a host in it can be another customer
/// of the same carrier. It stays out.
fn is_local_ipv4(ip: Ipv4Addr) -> bool {
    ip.is_loopback() || ip.is_private() || ip.is_link_local()
}

/// The IPv6 counterpart of [`is_local_ipv4`], from the IANA IPv6
/// Special-Purpose Address Registry:
///
/// - `::1` loopback (RFC 4291 section 2.5.3).
/// - `fc00::/7` unique local (RFC 4193).
/// - `fe80::/10` link-local unicast (RFC 4291 section 2.5.6).
/// - `::ffff:0:0/96` IPv4-mapped (RFC 4291 section 2.5.5.2), judged by the
///   IPv4 address inside it, because that is where the connection goes.
fn is_local_ipv6(ip: Ipv6Addr) -> bool {
    if let Some(v4) = ip.to_ipv4_mapped() {
        return is_local_ipv4(v4);
    }
    ip.is_loopback() || ip.is_unique_local() || ip.is_unicast_link_local()
}

/// Whether `hostname` can only be reached from the user's own machine or
/// local network. It judges the text alone and never does a DNS lookup.
///
/// - An IPv6 literal, bracketed or bare, is parsed and judged by address.
///   Text that does not parse is not local. That includes a literal with
///   a zone identifier (`fe80::1%25eth0`), which `Ipv6Addr` does not accept.
/// - A host that parses as a canonical dotted-decimal IPv4 address is judged
///   by address. `Ipv4Addr` rejects leading zeros, hex, octal and shortened
///   forms, so those are names, and a name is never judged by address even
///   when it spells one out (`192-168-1-5.example.net`).
/// - A name is local when it is `localhost` or ends in one of
///   [`LOCAL_NAME_SUFFIXES`] on a label boundary, ignoring case. A name with
///   an empty label, including a single trailing dot (`localhost.`), is not
///   local: the trailing-dot form skips the resolver's search rules, and
///   treating it as not local costs nothing but a slash.
pub fn is_local_network_host(hostname: &str) -> bool {
    if let Some(inner) = hostname.strip_prefix('[') {
        return inner
            .strip_suffix(']')
            .and_then(|literal| literal.parse::<Ipv6Addr>().ok())
            .is_some_and(is_local_ipv6);
    }
    if hostname.contains(':') {
        return hostname.parse::<Ipv6Addr>().is_ok_and(is_local_ipv6);
    }
    if let Ok(ip) = hostname.parse::<Ipv4Addr>() {
        return is_local_ipv4(ip);
    }
    let name = hostname.to_ascii_lowercase();
    if name.is_empty() || name.split('.').any(str::is_empty) {
        return false;
    }
    // No label is empty, so a front ending in `.` has a label before it.
    name == "localhost"
        || LOCAL_NAME_SUFFIXES.iter().any(|suffix| {
            name.strip_suffix(suffix)
                .is_some_and(|front| front.ends_with('.'))
        })
}

/// Scheme + `host[:port]` out of a URL, without pulling in a URL crate for a
/// few lines of parsing. Userinfo is dropped here and never returned, so no
/// caller can accidentally carry a credential out of a URL. `None` for
/// anything that isn't `scheme://host…`.
///
/// The authority ends at a backslash as well as at `/`, `?` and `#`, because
/// that is where the URL parser behind every request ends it for http and
/// https. Reading on past it would judge `http://evil.example\@localhost` by
/// `localhost` while the request went to `evil.example`.
fn scheme_and_authority(url: &str) -> Option<(String, &str)> {
    let (scheme, rest) = url.split_once("://")?;
    if scheme.is_empty()
        || !scheme
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'-' | b'.'))
    {
        return None;
    }
    let authority = rest.split(['/', '\\', '?', '#']).next().unwrap_or("");
    // Strip userinfo: the host is what comes after the last '@'.
    let host_port = match authority.rsplit_once('@') {
        Some((_, hp)) => hp,
        None => authority,
    };
    if host_port.is_empty() {
        return None;
    }
    Some((scheme.to_ascii_lowercase(), host_port))
}

/// Scheme + host, port removed — the shape [`is_local_network_host`] classifies.
fn scheme_and_host(url: &str) -> Option<(String, String)> {
    let (scheme, host_port) = scheme_and_authority(url)?;
    let host = if let Some(end) = host_port.find(']') {
        // Bracketed IPv6 literal; `is_local_network_host` strips the brackets.
        &host_port[..=end]
    } else if let Some(i) = host_port.rfind(':') {
        &host_port[..i]
    } else {
        host_port
    };
    if host.is_empty() {
        return None;
    }
    Some((scheme, host.to_string()))
}

/// Whether a request to `url` never sends the user's text, audio and key
/// unencrypted across the open internet: `https` to any host, `http` only to
/// a host [`is_local_network_host`] accepts. Any other scheme, or anything
/// without a scheme and a host, is refused.
pub fn is_safe_transport(url: &str) -> bool {
    match scheme_and_host(url) {
        Some((scheme, host)) => match scheme.as_str() {
            "https" => true,
            "http" => is_local_network_host(&host),
            _ => false,
        },
        None => false,
    }
}

// ---------------------------------------------------------------------------
// Validation — fail closed, never fall back
// ---------------------------------------------------------------------------

/// Why a configured custom endpoint cannot be used. Each variant carries the
/// sentence the UI shows, and each one stops the request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Invalid {
    /// No URL entered. The slot is simply off — not an error to show.
    NotConfigured,
    /// Something was entered, but it isn't a URL with a protocol.
    NotAUrl,
    /// Plain `http://` to a host that isn't on the local network.
    InsecureHttp,
    /// A usable URL, but no model named. Never returned by [`resolve_base`],
    /// which only judges URLs — it is the other half of "configured" that
    /// [`resolve`] has to check before it can build a request, since a body
    /// with an empty `model` is a 400 from every host.
    NoModel,
}

impl Invalid {
    pub fn message(self) -> &'static str {
        match self {
            Invalid::NotConfigured => "No custom endpoint URL yet — add one in Settings.",
            Invalid::NotAUrl => {
                "Custom endpoint URL is incomplete — start it with http:// or https:// and give the whole base address."
            }
            Invalid::InsecureHttp => {
                "Custom endpoint needs HTTPS — plain HTTP only works for a server on this computer or your local network."
            }
            Invalid::NoModel => "Custom endpoint has no model name — add it in Settings.",
        }
    }
}

/// What a dictation says when the custom endpoint owed it something and
/// could not be reached — the formatting pass, or the transcription itself.
///
/// The second half is the promise: the words are not lost. Whatever the rule
/// pipeline produced still goes into the document, exactly as spoken, and the
/// notice says so rather than leaving the user to wonder what happened to the
/// polish they configured. Lives here, not in `sarvam::ws`, because
/// `asr::custom` says the same sentence on the same failure and two copies
/// would drift.
pub const MSG_CUSTOM_UNAVAILABLE: &str = "Custom endpoint unavailable — pasted as dictated";

/// Why no chat backend could be produced at all.
///
/// This type exists so a call site can tell "use Sarvam" from "there is nothing
/// to use", which answering every question with *some* backend would hide. Both
/// variants below are a refusal the caller must report — never a silent skip,
/// and never a quiet substitution.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Unavailable {
    /// The custom endpoint owns this call and cannot be used, and degrading
    /// to Sarvam would send the text to a host this dictation had otherwise
    /// never touched (see [`resolve`]).
    CustomEndpoint(Invalid),
    /// Nothing is configured: the custom slot is off for chat, and there is
    /// no Sarvam key. The only variant whose fix is "add your Sarvam key",
    /// which is why the wording lives at the call sites — each of them knows
    /// which feature the user was reaching for.
    NoSarvamKey,
}

/// The gate every custom-endpoint request passes: trim the pasted route,
/// then require HTTPS unless the host is on the local network.
///
/// An empty base means the slot is off, and an unusable one is an error the
/// caller reports. Neither is ever swapped for another host's address.
pub fn resolve_base(configured: &str) -> Result<String, Invalid> {
    if configured.trim().is_empty() {
        return Err(Invalid::NotConfigured);
    }
    let normalized = trim_pasted_route(configured);
    if normalized.is_empty() {
        return Err(Invalid::NotAUrl);
    }
    if !is_safe_transport(&normalized) {
        // `scheme_and_host` also fails on "localhost:11434" (no protocol),
        // which reads better as "you didn't give me a URL" than as a
        // transport-security complaint.
        if scheme_and_host(&normalized).is_none() {
            return Err(Invalid::NotAUrl);
        }
        return Err(Invalid::InsecureHttp);
    }
    Ok(normalized)
}

// ---------------------------------------------------------------------------
// The slot itself
// ---------------------------------------------------------------------------

/// A URL rendered safe to log or print: scheme and host, nothing else.
///
/// The two places a credential hides in a pasted URL are userinfo
/// (`https://user:pw@host/v1`) and the query string (`?api_key=…`), and
/// neither survives this. Both are ordinary ways for a provider's docs to
/// hand someone a URL, so the raw string is never the thing that gets logged.
/// The port is kept: it is not a secret, and on a local endpoint it is most
/// of the diagnosis (11434 is Ollama, 1234 is LM Studio, 8080 is llama-server).
fn loggable_origin(url: &str) -> String {
    match scheme_and_authority(url) {
        Some((scheme, host_port)) => format!("{scheme}://{host_port}"),
        None => "(not a URL)".into(),
    }
}

/// The live custom endpoint: the settings half plus the secret half, which
/// never share a home on disk.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct CustomSlot {
    /// Exactly as the user typed it. Normalization happens at use time so a
    /// half-typed URL in Settings is never rewritten under the cursor.
    pub base_url: String,
    pub model: String,
    /// The transcription half's model id — see
    /// `settings::CustomEndpointSettings::stt_model` for why it is not the
    /// same string as `model`.
    pub stt_model: String,
    pub use_for_polish: bool,
    pub use_for_stt: bool,
    /// From the Windows credential store, never from `settings.json`.
    /// `None` is legitimate: plenty of self-hosted servers take no auth.
    pub api_key: Option<String>,
}

/// Hand-written for the same reason `Backend`'s is: this struct holds a
/// credential, nothing formats it today, and "nothing formats it today" is
/// the moment to make a `{:?}` leak impossible rather than the moment after.
impl std::fmt::Debug for CustomSlot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CustomSlot")
            .field("base_url", &loggable_origin(&self.base_url))
            .field("model", &self.model)
            .field("stt_model", &self.stt_model)
            .field("use_for_polish", &self.use_for_polish)
            .field("use_for_stt", &self.use_for_stt)
            .field(
                "api_key",
                &if self.api_key.is_some() {
                    "(redacted)"
                } else {
                    "(none)"
                },
            )
            .finish()
    }
}

impl CustomSlot {
    fn apply(&mut self, cfg: &CustomEndpointSettings) {
        self.base_url = cfg.base_url.clone();
        self.model = cfg.model.clone();
        self.stt_model = cfg.stt_model.clone();
        self.use_for_polish = cfg.use_for_polish;
        self.use_for_stt = cfg.use_for_stt;
    }
}

/// One slot per install, so one cell per process.
///
/// This is the same shape as `sarvam::SharedKey` — a small piece of live
/// configuration that every route needs and none of them owns — with the
/// difference that it is reached by name rather than threaded through
/// `RouteCtx`, `TransformJob` and the websocket dispatcher. That is a
/// deliberate trade: the alternative adds a parameter to ten construction sites
/// to model a thing the product only ever has one of. Every decision that reads
/// it is factored into [`resolve`], which is pure and takes the slot as an
/// argument, so nothing about the behaviour is testable only through the
/// global.
static SLOT: OnceLock<RwLock<CustomSlot>> = OnceLock::new();

fn cell() -> &'static RwLock<CustomSlot> {
    SLOT.get_or_init(|| RwLock::new(CustomSlot::default()))
}

/// A snapshot of the live slot.
pub fn slot() -> CustomSlot {
    cell().read().expect("custom endpoint lock").clone()
}

/// Load the slot at startup: the settings half from `settings.json`, the key
/// half from the credential store.
pub fn init(settings: &Settings) {
    let mut next = CustomSlot {
        api_key: key::load(KeySlot::CustomEndpoint),
        ..CustomSlot::default()
    };
    next.apply(&settings.custom_endpoint);
    *cell().write().expect("custom endpoint lock") = next;
}

/// Refresh the settings half after a settings write, keeping the key.
pub fn set_config(cfg: &CustomEndpointSettings) {
    cell().write().expect("custom endpoint lock").apply(cfg);
}

/// Refresh the key half after the user enters or clears one.
pub fn set_key(api_key: Option<String>) {
    cell().write().expect("custom endpoint lock").api_key = api_key;
}

/// The stored key, for a probe of `url`: only when `url` has the saved
/// base's scheme, host and port.
///
/// The Settings screen probes whatever is in its URL field, and the page is
/// what names it, so the key saved for one endpoint is never sent to another
/// address on a page's say-so. Leaving the field saves it, so in ordinary use
/// the address being tested is the saved one and the key goes with it.
pub fn key_for_probe(slot: &CustomSlot, url: &str) -> Option<String> {
    let origin = |u: &str| {
        let base = resolve_base(u).ok()?;
        let (scheme, host_port) = scheme_and_authority(&base)?;
        Some((scheme, host_port.to_ascii_lowercase()))
    };
    let saved = origin(&slot.base_url)?;
    if origin(url)? == saved {
        slot.api_key.clone()
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// The resolver the production call sites use
// ---------------------------------------------------------------------------

/// A stored Sarvam key, or `None` — a blank one is not a key. The same
/// `trim().is_empty()` rule `Controller::key_present` applies, in one place
/// both can use, so "is there a key" cannot mean two things.
fn usable_sarvam_key(sarvam_key: Option<&str>) -> Option<&str> {
    sarvam_key.map(str::trim).filter(|k| !k.is_empty())
}

/// Which chat backend a polish / agent / transform call should use, given a
/// slot. Pure: no globals, no I/O, no credential store.
///
/// ## When an unusable custom endpoint may degrade to Sarvam, and when it
/// may not
///
/// An enabled-but-invalid slot degrading to `Backend::sarvam` is safe on one
/// premise — **Sarvam is already hearing this dictation**, so falling back
/// there cannot send the user's words somewhere new — and custom STT is
/// exactly what makes it false. A user running their own transcription shim
/// with no Sarvam account never handed Sarvam a syllable; silently posting
/// the transcript there because their polish URL cannot be used would be a
/// privacy surprise. So the fallback holds only while **both** of these are
/// true:
///
/// * the custom endpoint is not also doing STT (`use_for_stt` off), and
/// * a Sarvam key exists, i.e. Sarvam is a host this install already talks to.
///
/// Otherwise this returns [`Unavailable::CustomEndpoint`] and the caller
/// reports it. On the dictation path that means the rule-cleaned text still
/// pastes, with a notice — the words are never lost, only left unpolished.
pub(crate) fn resolve(
    slot: &CustomSlot,
    sarvam_key: Option<&str>,
    sarvam_model: &str,
) -> Result<Backend, Unavailable> {
    let home = usable_sarvam_key(sarvam_key).map(|key| Backend::sarvam(key, sarvam_model));
    resolve_against(slot, home)
}

/// The decision [`resolve`] describes, with "the host this dictation is
/// already on" passed in rather than reconstructed.
///
/// `home` is that host's backend: Sarvam's when a key is stored, the relay's
/// in Cloud mode, and `None` when there is neither — which is the only case
/// that can answer [`Unavailable::NoSarvamKey`].
fn resolve_against(slot: &CustomSlot, home: Option<Backend>) -> Result<Backend, Unavailable> {
    match pick(slot, home.is_some()) {
        Pick::Home(degraded) => {
            let backend = home.ok_or(Unavailable::NoSarvamKey)?;
            if let Some(why) = degraded {
                // Scheme and host only. The URL is configuration rather than
                // content, but a pasted one can carry a credential in its
                // userinfo or its query string, so what makes the failure
                // diagnosable — which host was attempted — is all that is
                // logged.
                tracing::warn!(
                    reason = ?why,
                    attempted = %loggable_origin(&slot.base_url),
                    // Which host it fell back to, now that there are two it
                    // could be. A kind, never a URL and never a credential.
                    using = ?backend.kind,
                    "custom endpoint rejected; using this dictation's own host"
                );
            }
            Ok(backend)
        }
        Pick::Custom(base) => Ok(Backend::custom(
            &chat_completions_url(&base),
            slot.api_key.clone(),
            slot.model.trim(),
        )),
        Pick::Nothing(Unavailable::CustomEndpoint(why)) => {
            tracing::warn!(
                reason = ?why,
                attempted = %loggable_origin(&slot.base_url),
                stt_is_custom = slot.use_for_stt,
                "custom endpoint rejected and Sarvam is not this dictation's host; failing instead"
            );
            Err(Unavailable::CustomEndpoint(why))
        }
        Pick::Nothing(why) => Err(why),
    }
}

/// Which host [`resolve_against`] is going to answer with, decided from the
/// slot and one bit: whether this dictation *has* a host of its own.
///
/// Split out because that bit is all the decision needs, and a credential is
/// not part of it. The chord-down gates ask exactly this question
/// (`controller::capable`, `routes::agent::apply`) and must answer it
/// synchronously — on the Cloud lane the credential is an `await` away, and a
/// hotkey press cannot wait for a token refresh to find out whether the agent
/// is available. One function so the gate and the resolution can never
/// disagree about the same install.
enum Pick {
    /// This dictation's own host — Sarvam's chat route, or the relay's.
    /// `Some(why)` when the custom slot was asked for and could not answer,
    /// which is the degrade the log line above describes.
    Home(Option<Invalid>),
    /// The custom endpoint, at this resolved base.
    Custom(String),
    /// Neither: there is nothing to make a chat call with.
    Nothing(Unavailable),
}

fn pick(slot: &CustomSlot, home: bool) -> Pick {
    if !slot.use_for_polish {
        return if home {
            Pick::Home(None)
        } else {
            Pick::Nothing(Unavailable::NoSarvamKey)
        };
    }
    let configured = if slot.model.trim().is_empty() {
        Err(Invalid::NoModel)
    } else {
        resolve_base(&slot.base_url)
    };
    match configured {
        Ok(base) => Pick::Custom(base),
        // The fallback's two conditions: a host of this dictation's own to fall
        // back *to*, and a dictation Sarvam (or the relay) is actually hearing
        // — see [`resolve`]'s doc comment.
        Err(why) if home && !slot.use_for_stt => Pick::Home(Some(why)),
        Err(why) => Pick::Nothing(Unavailable::CustomEndpoint(why)),
    }
}

/// Whether the custom endpoint answers the polish itself, whatever host the
/// dictation is on. The dictation's own credential then plays no part in the
/// call.
pub(crate) fn polishes_itself(slot: &CustomSlot) -> bool {
    matches!(pick(slot, false), Pick::Custom(_))
}

/// [`resolve`] against the live slot. This is the call the production sites
/// make in place of `Backend::sarvam`.
pub fn resolve_polish_backend(
    sarvam_key: Option<&str>,
    sarvam_model: &str,
) -> Result<Backend, Unavailable> {
    resolve(&slot(), sarvam_key, sarvam_model)
}

/// The same decision as [`resolve`], for a dictation whose host is already
/// known — which is what a running session has in hand
/// (`sarvam::ws::run_session` resolves its [`Transport`] once, before the
/// socket opens).
///
/// The rule is the same; only "the host this dictation is already on"
/// differs. On the Cloud lane that host is the relay, so an unusable custom
/// endpoint degrades *there* rather than to Sarvam — the same argument
/// [`resolve`]'s doc comment makes, applied to the other lane: the relay is
/// already hearing this dictation, and the app has no Sarvam key to degrade
/// to anyway.
pub(crate) fn resolve_for(
    slot: &CustomSlot,
    transport: &Transport,
    model: &str,
) -> Result<Backend, Unavailable> {
    let home = match transport {
        Transport::Sarvam { key } => usable_sarvam_key(Some(key)).map(|k| Backend::sarvam(k, model)),
        Transport::Relay { base, bearer } => Some(Backend::relay(base, bearer, model)),
    };
    resolve_against(slot, home)
}

/// [`resolve_for`] against the live slot.
pub fn resolve_polish_backend_for(
    transport: &Transport,
    model: &str,
) -> Result<Backend, Unavailable> {
    resolve_for(&slot(), transport, model)
}

/// The transcription model id sent when the slot names none.
///
/// OpenAI's API reference for `POST /v1/audio/transcriptions` lists the ids
/// its endpoint accepts, and `whisper-1` is the one that stands for the open
/// Whisper model. A server built to answer like OpenAI's endpoint while
/// hosting one Whisper model is most likely to accept that id, or to ignore
/// the field. Sent as is, with no check.
pub const DEFAULT_STT_MODEL: &str = "whisper-1";

/// Where one transcription request goes, and what it authenticates with.
#[derive(Clone, PartialEq, Eq)]
pub struct SttTarget {
    /// The full `{base}/audio/transcriptions` route.
    pub url: String,
    pub model: String,
    /// `None` for a server that takes no auth — the common self-hosted case,
    /// and the reason this is an `Option` rather than an empty string.
    pub api_key: Option<String>,
}

/// Redacting, for the same reason `Backend`'s and `CustomSlot`'s are: this
/// type holds a credential and a user-pasted URL, and the moment to make a
/// `{:?}` leak impossible is before something formats it.
impl std::fmt::Debug for SttTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SttTarget")
            .field("url", &loggable_origin(&self.url))
            .field("model", &self.model)
            .field(
                "api_key",
                &if self.api_key.is_some() {
                    "(redacted)"
                } else {
                    "(none)"
                },
            )
            .finish()
    }
}

/// What a transcription request against this slot would look like, or why it
/// cannot be made. Pure — no globals, no I/O.
///
/// Deliberately does **not** consult `use_for_stt`: whether the custom
/// endpoint owns this dictation was decided at chord-down and snapshotted
/// (`controller::SttPath`), so re-deciding it here would let a settings save
/// mid-utterance send the audio somewhere the dictation did not start out
/// bound for. This answers only "what would that request be".
///
/// Unlike [`resolve`] there is no model-shaped failure: an unnamed
/// transcription model has a documented default ([`DEFAULT_STT_MODEL`]),
/// where an unnamed chat model has none.
pub(crate) fn resolve_stt(slot: &CustomSlot) -> Result<SttTarget, Invalid> {
    let base = resolve_base(&slot.base_url)?;
    let model = match slot.stt_model.trim() {
        "" => DEFAULT_STT_MODEL,
        named => named,
    };
    Ok(SttTarget {
        url: transcriptions_url(&base),
        model: model.to_string(),
        api_key: slot.api_key.clone(),
    })
}

/// Whether any chat backend at all is reachable right now — the gate the
/// chord-time decisions use (`routes::resolve`'s agent arm, the selection
/// lane) so that "add your Sarvam key" is never the answer given to someone
/// whose install runs entirely on their own endpoint.
pub fn chat_backend_exists(sarvam_key: Option<&str>, sarvam_model: &str) -> bool {
    resolve_polish_backend(sarvam_key, sarvam_model).is_ok()
}

/// The same question on the Cloud lane, and the answer needs no credential:
/// a signed-in install always has a host for chat, because the relay proxies
/// `/v1/chat/completions` for anyone whose token it accepts.
///
/// Synchronous for the reason [`Pick`] exists — this runs at chord-down, and
/// the bearer is an `await` away. A signed-*out* install is not automatically
/// refused: a custom endpoint configured for polish still answers, exactly as
/// it does for an install with no Sarvam key.
pub fn cloud_chat_backend_exists(signed_in: bool) -> bool {
    chat_available(signed_in).is_ok()
}

/// Whether a chat call could be made at all, given only whether this
/// dictation's own host is available — and, when it could not, the same
/// [`Unavailable`] the resolvers return, so every existing call site's
/// wording applies unchanged.
pub fn chat_available(home: bool) -> Result<(), Unavailable> {
    match pick(&slot(), home) {
        Pick::Nothing(why) => Err(why),
        _ => Ok(()),
    }
}

/// Why a chat call has no backend, once the lane can have a credential of its
/// own to fail at.
#[derive(Debug)]
pub enum ChatUnavailable {
    /// No backend resolves at all. The caller words this one: each feature
    /// knows which thing the user was reaching for.
    Backend(Unavailable),
    /// There is a host, but this install cannot authenticate to it right now.
    /// Already a finished sentence — `auth::session` separates "sign in
    /// again" from "you are offline", and a user on a train must not be told
    /// to sign in.
    SignIn(String),
}

/// The chat backend for one call on `lane`, credential included.
///
/// The asynchronous counterpart to [`resolve_polish_backend`], and what the
/// three chat features that are not dictation — transform, voice agent, note
/// actions — resolve through, because the Cloud lane's bearer has to be
/// awaited.
///
/// Order is the whole design. The custom endpoint is asked first and needs
/// nothing from the lane, so an install whose polish runs on its own server
/// is never asked to sign in; only a call that is really going to Sarvam or
/// to the relay pays for a credential. And the Bring-your-own-key lane goes
/// through the same resolver, with the same `Unavailable` and the same
/// sentence at every call site.
pub async fn chat_backend_for(
    lane: &Lane,
    sarvam_key: Option<&str>,
    model: &str,
) -> Result<Backend, ChatUnavailable> {
    let base = match lane {
        Lane::Cloud { relay } => relay.as_str(),
        // Never polled: the Bring-your-own-key arm returns before the bearer
        // is awaited. `Transport::cloud` is an `async fn`, so building this
        // runs none of it.
        Lane::Byok => "",
    };
    chat_backend_on(&slot(), lane, sarvam_key, model, Transport::cloud(base)).await
}

/// [`chat_backend_for`] against a given slot and a given source of the
/// relay's credential — the two seams its tests need.
///
/// `slot` because `slot()` is process state written from Settings, and
/// `bearer` because the real one is
/// [`crate::auth::session::access_token`], which reads — and on a rejection
/// *deletes* — the developer's own Windows credential. A test that reached it
/// would spend or destroy a real Supabase session, so the credential is
/// handed in and every test hands in a stub. Laziness is the contract: the
/// future is built by the caller and only polled on the paths that genuinely
/// need a credential, which is what makes "was it asked?" a thing a test can
/// assert.
pub(crate) async fn chat_backend_on(
    slot: &CustomSlot,
    lane: &Lane,
    sarvam_key: Option<&str>,
    model: &str,
    bearer: impl std::future::Future<Output = Result<Transport, String>>,
) -> Result<Backend, ChatUnavailable> {
    if matches!(lane, Lane::Byok) {
        return resolve(slot, sarvam_key, model).map_err(ChatUnavailable::Backend);
    }
    // Two answers cost nothing and settle the call without a credential, and
    // both are the same expression — the slot asked with no home at all:
    //
    // * the custom endpoint answers on its own, so the relay is not involved;
    // * or the slot is unusable *and* doing this install's speech-to-text, in
    //   which case the fallback to the relay is the one `resolve`'s rule
    //   forbids — `resolve_for` would fetch a bearer and then throw it away,
    //   and fetching one can cost a fifteen-second token round trip.
    let settled = match pick(slot, false) {
        Pick::Custom(_) => true,
        Pick::Nothing(Unavailable::CustomEndpoint(_)) => slot.use_for_stt,
        _ => false,
    };
    if settled {
        return resolve_against(slot, None).map_err(ChatUnavailable::Backend);
    }
    let transport = bearer.await.map_err(ChatUnavailable::SignIn)?;
    resolve_for(slot, &transport, model).map_err(ChatUnavailable::Backend)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::backend::{BackendKind, SARVAM_CHAT_URL};

    // -- trim_pasted_route ---------------------------------------------------

    /// Each pasted route comes off whether or not `/v1` sits in front of it,
    /// and the three routes this app calls are always on the list.
    #[test]
    fn a_pasted_route_comes_off_with_or_without_a_version_segment() {
        for route in ["/chat/completions", "/models", "/audio/transcriptions"] {
            assert!(PASTED_ROUTES.contains(&route), "{route} is missing");
        }
        let origin = "https://gw.example.com:8443";
        for route in PASTED_ROUTES {
            assert_eq!(
                trim_pasted_route(&format!("{origin}/v1{route}")),
                format!("{origin}/v1"),
                "route {route} after /v1"
            );
            assert_eq!(
                trim_pasted_route(&format!("{origin}{route}")),
                origin,
                "route {route} on the bare origin"
            );
        }
    }

    /// Stripping repeats, so the result is a base no route builder will
    /// change again. A host named like a route is never stripped.
    #[test]
    fn trimming_twice_changes_nothing() {
        assert_eq!(
            trim_pasted_route("https://gw.example.com/v1/chat/completions/models"),
            "https://gw.example.com/v1"
        );
        for input in [
            "https://gw.example.com/v1/chat/completions/chat/completions",
            "https://gw.example.com/V1/Models/?x=1",
            "http://192.168.4.20:8080/api/v1/audio/transcriptions",
            "https://models",
            "https://h?x=1",
        ] {
            let once = trim_pasted_route(input);
            assert_eq!(trim_pasted_route(&once), once, "input {input}");
        }
        assert_eq!(trim_pasted_route("https://models"), "https://models");
    }

    /// A URL with a query and no path gets its routes in front of the query.
    #[test]
    fn a_query_with_no_path_stays_after_every_route() {
        assert_eq!(trim_pasted_route("https://h?x=1"), "https://h?x=1");
        assert_eq!(chat_completions_url("https://h?x=1"), "https://h/v1/chat/completions?x=1");
        assert_eq!(models_url("https://h#top"), "https://h/v1/models#top");
        assert_eq!(transcriptions_url("https://h?x=1#f"), "https://h/audio/transcriptions?x=1#f");
    }

    /// `/V1` counts as the version segment already there, so the chat route
    /// is not given a second one, and the user's spelling is kept.
    #[test]
    fn an_upper_case_v1_counts_as_present() {
        assert_eq!(with_version_segment("https://gw.example.com/V1"), "https://gw.example.com/V1");
        assert_eq!(
            chat_completions_url("https://gw.example.com/V1"),
            "https://gw.example.com/V1/chat/completions"
        );
        assert_eq!(models_url("https://gw.example.com/V1"), "https://gw.example.com/V1/models");
    }

    /// A base under another version segment still gets `/v1` on the chat
    /// route. Only a trailing `/v1` counts as the version already being
    /// there: reading `v1beta` as a stand-in for it would be a guess about
    /// the host's layout, and the route has to follow from what was typed.
    #[test]
    fn a_base_under_another_version_still_gets_v1() {
        assert_eq!(
            chat_completions_url("https://gw.example.com/v1beta/openai"),
            "https://gw.example.com/v1beta/openai/v1/chat/completions"
        );
    }

    /// A route typed in any case comes off, and the `V1` in front of it is
    /// lowered to `v1`. Extra slashes at the end go too, as do spaces around
    /// the paste.
    #[test]
    fn mixed_case_routes_and_extra_slashes_come_off() {
        assert_eq!(trim_pasted_route("https://h/V1/MODELS//"), "https://h/v1");
        assert_eq!(trim_pasted_route("https://h/v1///"), "https://h/v1");
        assert_eq!(trim_pasted_route("   https://h/v1  "), "https://h/v1");
    }

    /// Some hosts take a setting in the query string (an `api-version`, a
    /// deployment key), and a request without it gets a 404. Whatever follows
    /// `?` or `#` is kept as it was, after the route comes off.
    #[test]
    fn the_query_and_fragment_outlive_the_route() {
        assert_eq!(
            trim_pasted_route("https://h/v1/chat/completions?api-version=2025-01-01-preview"),
            "https://h/v1?api-version=2025-01-01-preview"
        );
        assert_eq!(
            trim_pasted_route("https://h/chat/completions?a=1#frag"),
            "https://h?a=1#frag"
        );
    }

    /// Nothing typed means no base, never a vendor's default address.
    #[test]
    fn blank_input_gives_no_base_at_all() {
        assert_eq!(trim_pasted_route(""), "");
        assert_eq!(trim_pasted_route("   "), "");
    }

    /// A path that merely *contains* a route name is not a suffix.
    #[test]
    fn only_a_trailing_route_is_stripped() {
        assert_eq!(
            trim_pasted_route("https://h/models/registry"),
            "https://h/models/registry"
        );
    }

    // -- /v1 and the chat route ---------------------------------------------

    #[test]
    fn v1_is_added_once_and_only_when_missing() {
        assert_eq!(with_version_segment("http://localhost:11434"), "http://localhost:11434/v1");
        assert_eq!(with_version_segment("http://localhost:11434/v1"), "http://localhost:11434/v1");
        assert_eq!(
            with_version_segment("https://h/v1/chat/completions"),
            "https://h/v1"
        );
    }

    /// The bare origin an LM Studio / Ollama user types has to reach the same
    /// route as the full URL they could have pasted instead.
    #[test]
    fn the_chat_route_is_the_same_wherever_the_user_started() {
        for input in [
            "http://localhost:11434",
            "http://localhost:11434/",
            "http://localhost:11434/v1",
            "http://localhost:11434/v1/chat/completions",
        ] {
            assert_eq!(
                chat_completions_url(input),
                "http://localhost:11434/v1/chat/completions",
                "input {input}"
            );
        }
    }

    /// Transcription servers differ on whether they serve `/v1`, so this
    /// route takes the base exactly as typed, where the chat route adds it.
    #[test]
    fn the_transcription_route_adds_no_version_segment() {
        assert_eq!(
            transcriptions_url("http://192.168.4.20:9000"),
            "http://192.168.4.20:9000/audio/transcriptions"
        );
        assert_eq!(
            transcriptions_url("http://192.168.4.20:9000/v1"),
            "http://192.168.4.20:9000/v1/audio/transcriptions"
        );
        assert_eq!(
            chat_completions_url("http://192.168.4.20:9000"),
            "http://192.168.4.20:9000/v1/chat/completions"
        );
    }

    /// Same "wherever the user started" guarantee the chat route has: the
    /// full URL out of a curl example and the bare base agree.
    #[test]
    fn the_transcription_route_is_the_same_wherever_the_user_started() {
        for input in [
            "https://h/v1",
            "https://h/v1/",
            "https://h/v1/audio/transcriptions",
            "https://h/v1/chat/completions",
        ] {
            assert_eq!(
                transcriptions_url(input),
                "https://h/v1/audio/transcriptions",
                "input {input}"
            );
        }
    }

    /// Discovery has to probe the host inference will actually call, or a
    /// green "Test connection" means nothing. Same base, same `/v1`, two
    /// routes.
    #[test]
    fn the_models_route_hangs_off_the_same_base_as_the_chat_route() {
        for input in [
            "http://localhost:11434",
            "http://localhost:11434/v1",
            "http://localhost:11434/v1/chat/completions",
            "http://localhost:11434/v1/models",
        ] {
            let chat = chat_completions_url(input);
            let models = models_url(input);
            assert_eq!(models, "http://localhost:11434/v1/models", "input {input}");
            assert_eq!(
                chat.strip_suffix("/chat/completions"),
                models.strip_suffix("/models"),
                "input {input}"
            );
        }
    }

    #[test]
    fn the_transcription_route_keeps_the_query_string_at_the_end() {
        assert_eq!(
            transcriptions_url("https://h/v1?api-version=2025-01-01-preview"),
            "https://h/v1/audio/transcriptions?api-version=2025-01-01-preview"
        );
    }

    #[test]
    fn the_models_route_keeps_the_query_string_at_the_end() {
        assert_eq!(
            models_url("https://h/v1?api-version=2025-01-01-preview"),
            "https://h/v1/models?api-version=2025-01-01-preview"
        );
    }

    #[test]
    fn the_chat_route_keeps_the_query_string_at_the_end() {
        assert_eq!(
            chat_completions_url("https://h/v1?api-version=2025-01-01-preview"),
            "https://h/v1/chat/completions?api-version=2025-01-01-preview"
        );
    }

    // -- is_local_network_host ----------------------------------------------

    /// Only a canonical dotted-decimal address is judged by address. Every
    /// other spelling is a name, and a numeric-looking name is not local.
    #[test]
    fn only_canonical_ipv4_is_judged_as_an_address() {
        // The standard parser rejects a leading-zero part on this toolchain.
        assert!("192.168.001.7".parse::<Ipv4Addr>().is_err());

        for host in ["10.20.30.40", "172.20.1.9", "192.168.50.2", "127.0.0.1"] {
            assert!(is_local_network_host(host), "{host}");
            assert!(is_safe_transport(&format!("http://{host}:8000/v1")), "{host}");
        }
        for host in [
            "192.168.001.7",
            "010.20.30.40",
            "0x0a.20.30.40",
            "10.0x14.30.40",
            "10.20.30",
            "10.20.30.40.50",
            "10..30.40",
            "10.20.30.256",
            "127.1",
            "10.20.30.40.example.com",
            "192.168.50.example.net",
        ] {
            assert!(!is_local_network_host(host), "{host}");
            assert!(!is_safe_transport(&format!("http://{host}/v1")), "{host}");
        }
    }

    const LOCAL_IPV4_RANGES: [(Ipv4Addr, Ipv4Addr); 5] = [
        (Ipv4Addr::new(127, 0, 0, 0), Ipv4Addr::new(127, 255, 255, 255)),
        (Ipv4Addr::new(10, 0, 0, 0), Ipv4Addr::new(10, 255, 255, 255)),
        (Ipv4Addr::new(172, 16, 0, 0), Ipv4Addr::new(172, 31, 255, 255)),
        (Ipv4Addr::new(192, 168, 0, 0), Ipv4Addr::new(192, 168, 255, 255)),
        (Ipv4Addr::new(169, 254, 0, 0), Ipv4Addr::new(169, 254, 255, 255)),
    ];

    /// One address inside each IPv6 rule.
    const LOCAL_IPV6_SAMPLES: [&str; 4] = ["::1", "fd12:3456:789a::5", "fe80::c0a8:1", "::ffff:192.168.7.7"];

    #[test]
    fn every_local_range_and_name_counts_as_local() {
        for (first, last) in LOCAL_IPV4_RANGES {
            assert!(is_local_network_host(&first.to_string()), "{first}");
            assert!(is_local_network_host(&last.to_string()), "{last}");
        }
        for ip in LOCAL_IPV6_SAMPLES {
            assert!(is_local_network_host(ip), "bare {ip}");
            assert!(is_local_network_host(&format!("[{ip}]")), "bracketed {ip}");
            assert!(is_safe_transport(&format!("http://[{ip}]:8080/v1")), "url {ip}");
        }
        for name in ["LocalHost", "Api.LOCALHOST", "Studio-PC.Local", "nas.Home.Arpa"] {
            assert!(is_local_network_host(name), "{name}");
        }
    }

    #[test]
    fn hosts_next_to_the_local_ones_are_public() {
        for (first, last) in LOCAL_IPV4_RANGES {
            let below = Ipv4Addr::from(u32::from(first) - 1);
            let above = Ipv4Addr::from(u32::from(last) + 1);
            assert!(!is_local_network_host(&below.to_string()), "{below}");
            assert!(!is_local_network_host(&above.to_string()), "{above}");
        }
        for host in [
            // Carrier shared address space, deliberately left out.
            "100.64.0.1",
            // Documentation addresses (RFC 5737, RFC 3849).
            "192.0.2.10",
            "198.51.100.10",
            "203.0.113.10",
            "2001:db8::10",
            "[2001:db8::10]",
            // Starts like a local IPv6 prefix but is not an address.
            "fd00:example",
            "[fe80::zz]",
            "fe80:not-an-address:local",
            // Suffix without a label boundary, a name that starts with an
            // allowed one, and an allowed suffix with nothing in front.
            "evillocal",
            "notlocalhost",
            "myhome.arpa",
            "localhost.example.com",
            "local.example.org",
            ".local",
            "home.arpa",
            "",
        ] {
            assert!(!is_local_network_host(host), "{host}");
        }
    }

    /// An IPv4-mapped IPv6 address is judged by the IPv4 address inside it.
    #[test]
    fn an_ipv4_mapped_address_follows_its_ipv4_address() {
        assert!(is_local_network_host("[::ffff:127.0.0.1]"));
        assert!(is_local_network_host("::ffff:10.1.2.3"));
        assert!(!is_local_network_host("[::ffff:198.51.100.10]"));
        assert!(!is_local_network_host("[::ffff:100.64.0.1]"));
    }

    /// A zone identifier does not parse as an `Ipv6Addr`, so a link-local
    /// literal carrying one is not local.
    #[test]
    fn an_ipv6_literal_with_a_zone_is_not_local() {
        assert!(!is_local_network_host("[fe80::1%25eth0]"));
        assert!(!is_local_network_host("fe80::1%eth0"));
        assert!(!is_safe_transport("http://[fe80::1%25eth0]:8080/v1"));
    }

    /// A single trailing dot is treated as not local.
    #[test]
    fn a_trailing_dot_name_is_not_local() {
        assert!(!is_local_network_host("localhost."));
        assert!(!is_local_network_host("studio.local."));
        assert!(!is_safe_transport("http://localhost.:11434/v1"));
    }

    /// Scheme case does not matter; an authority with a port and no host is
    /// not a URL; userinfo with `:` and `@` in it is never the host.
    #[test]
    fn scheme_case_empty_hosts_and_tangled_userinfo() {
        assert!(is_safe_transport("HTTP://localhost:1234/v1"));
        assert_eq!(resolve_base("HTTP://localhost:1234"), Ok("HTTP://localhost:1234".into()));
        assert_eq!(resolve_base("http://:8080"), Err(Invalid::NotAUrl));
        assert!(!is_safe_transport("http://a:b@localhost@gw.example.com/v1"));
        assert!(is_safe_transport("http://a@b:c@192.168.9.9:8080/v1"));
    }

    // -- is_safe_transport ---------------------------------------------------

    #[test]
    fn http_is_allowed_only_to_the_local_network() {
        assert!(is_safe_transport("https://llm.example.org/v1"));
        assert!(is_safe_transport("http://localhost:11434/v1"));
        assert!(is_safe_transport("http://192.168.4.20:1234/v1"));
        assert!(is_safe_transport("http://[::1]:11434/v1"));

        assert!(!is_safe_transport("http://llm.example.org/v1"));
        assert!(!is_safe_transport("http://192-168-1-5.example.net/v1"));
        assert!(!is_safe_transport("ftp://localhost/v1"));
        assert!(!is_safe_transport("llm.example.org/v1"));
        assert!(!is_safe_transport(""));
        assert!(!is_safe_transport("https://"));
    }

    /// Userinfo must not be mistaken for the host, or
    /// `http://localhost@evil.com` would read as local.
    #[test]
    fn userinfo_is_not_the_host() {
        assert!(!is_safe_transport("http://localhost@evil.com/v1"));
        assert!(is_safe_transport("http://user:pw@127.0.0.1:7342/v1"));
    }

    /// For http and https a backslash ends the authority, as a slash does:
    /// that is how the request is actually sent, so `evil.example` is the
    /// host here, and `@localhost:11434` is only the start of the path.
    #[test]
    fn a_backslash_ends_the_host_the_way_the_request_reads_it() {
        let url = "http://evil.example\\@localhost:11434";
        assert_eq!(
            reqwest::Url::parse(url).unwrap().host_str(),
            Some("evil.example"),
            "the parser the request goes through"
        );
        assert!(!is_safe_transport(url));
        assert_eq!(resolve_base(url), Err(Invalid::InsecureHttp));
        assert_eq!(loggable_origin(url), "http://evil.example");
        assert!(is_safe_transport("http://localhost:11434\\v1"));
    }

    /// The stored key goes only to the saved endpoint's address, whatever
    /// URL the page asks to probe.
    #[test]
    fn a_probe_sends_the_stored_key_only_to_the_saved_address() {
        let slot = CustomSlot {
            base_url: "https://gw.example.com/v1".into(),
            api_key: Some("sk-live".into()),
            ..CustomSlot::default()
        };
        for same in [
            "https://gw.example.com/v1",
            "https://GW.example.com",
            "https://gw.example.com/v1/models",
            "https://user:pw@gw.example.com/v1",
        ] {
            assert_eq!(key_for_probe(&slot, same).as_deref(), Some("sk-live"), "{same}");
        }
        for other in [
            "https://evil.example/v1",
            "https://gw.example.com:8443/v1",
            "https://gw.example.com.evil.example/v1",
            "http://localhost:11434",
            "not a url",
        ] {
            assert_eq!(key_for_probe(&slot, other), None, "{other}");
        }
        let unsaved = CustomSlot {
            base_url: String::new(),
            ..slot
        };
        assert_eq!(key_for_probe(&unsaved, "https://gw.example.com/v1"), None);
    }

    // -- resolve_base --------------------------------------------------------

    #[test]
    fn an_unconfigured_endpoint_is_off_not_a_fallback() {
        assert_eq!(resolve_base(""), Err(Invalid::NotConfigured));
        assert_eq!(resolve_base("   "), Err(Invalid::NotConfigured));
    }

    #[test]
    fn a_url_without_a_protocol_is_rejected_as_unusable() {
        assert_eq!(resolve_base("localhost:11434"), Err(Invalid::NotAUrl));
        assert_eq!(resolve_base("not a url"), Err(Invalid::NotAUrl));
    }

    #[test]
    fn cleartext_to_a_public_host_is_rejected() {
        assert_eq!(
            resolve_base("http://api.example.com/v1"),
            Err(Invalid::InsecureHttp)
        );
        assert_eq!(
            resolve_base("http://localhost:11434/v1/chat/completions"),
            Ok("http://localhost:11434/v1".into())
        );
    }

    // -- redaction -----------------------------------------------------------

    /// The slot holds the credential. Nothing formats it today, which is
    /// precisely why the guarantee belongs on the type.
    #[test]
    fn debugging_the_slot_never_prints_its_key() {
        let slot = CustomSlot {
            api_key: Some("sk-live-secret".into()),
            ..on_slot()
        };
        let shown = format!("{slot:?}");
        assert!(!shown.contains("sk-live-secret"), "{shown}");
        assert!(shown.contains("(redacted)"), "{shown}");
        assert!(shown.contains("qwen3:8b"), "the model is diagnosable");

        let keyless = format!("{:?}", on_slot());
        assert!(keyless.contains("(none)"), "{keyless}");
    }

    #[test]
    fn a_loggable_origin_drops_userinfo_and_the_query_string() {
        assert_eq!(
            loggable_origin("https://user:pw@h.example.com/v1?api_key=sk-live"),
            "https://h.example.com"
        );
        assert_eq!(
            loggable_origin("http://127.0.0.1:1234/v1"),
            "http://127.0.0.1:1234"
        );
        assert_eq!(loggable_origin("nonsense"), "(not a URL)");
    }

    #[test]
    fn debugging_the_slot_never_prints_a_url_secret() {
        let slot = CustomSlot {
            base_url: "https://user:pw@h/v1?api_key=sk-live".into(),
            ..on_slot()
        };
        let shown = format!("{slot:?}");
        assert!(!shown.contains("sk-live"), "{shown}");
        assert!(!shown.contains("pw"), "{shown}");
    }

    // -- the resolver --------------------------------------------------------

    fn on_slot() -> CustomSlot {
        CustomSlot {
            base_url: "http://localhost:11434/v1".into(),
            model: "qwen3:8b".into(),
            stt_model: String::new(),
            use_for_polish: true,
            use_for_stt: false,
            api_key: None,
        }
    }

    // -- the STT target ------------------------------------------------------

    /// The whole reason `stt_model` is its own field: the chat model id must
    /// never be what gets posted to the transcription route.
    #[test]
    fn the_stt_target_never_borrows_the_chat_model() {
        let target = resolve_stt(&on_slot()).expect("the URL is usable");
        // `/v1` because the user's own base ends in it — not because this
        // route added one (`the_transcription_route_adds_no_version_segment`).
        assert_eq!(target.url, "http://localhost:11434/v1/audio/transcriptions");
        assert_eq!(target.model, DEFAULT_STT_MODEL);
        assert_ne!(target.model, "qwen3:8b");
    }

    #[test]
    fn a_named_stt_model_is_passed_through_verbatim() {
        let slot = CustomSlot {
            stt_model: "  Systran/faster-whisper-large-v3  ".into(),
            ..on_slot()
        };
        assert_eq!(
            resolve_stt(&slot).expect("usable").model,
            "Systran/faster-whisper-large-v3"
        );
    }

    #[test]
    fn the_stt_target_carries_the_slots_key_and_refuses_an_unusable_url() {
        let slot = CustomSlot {
            api_key: Some("sk-abc".into()),
            ..on_slot()
        };
        assert_eq!(
            resolve_stt(&slot).expect("usable").api_key.as_deref(),
            Some("sk-abc")
        );
        let broken = CustomSlot {
            base_url: "http://api.example.com".into(),
            ..on_slot()
        };
        assert_eq!(resolve_stt(&broken), Err(Invalid::InsecureHttp));
        let unset = CustomSlot {
            base_url: String::new(),
            ..on_slot()
        };
        assert_eq!(resolve_stt(&unset), Err(Invalid::NotConfigured));
    }

    #[test]
    fn debugging_an_stt_target_never_prints_its_key() {
        let target = resolve_stt(&CustomSlot {
            api_key: Some("sk-live-secret".into()),
            ..on_slot()
        })
        .expect("usable");
        let shown = format!("{target:?}");
        assert!(!shown.contains("sk-live-secret"), "{shown}");
        assert!(shown.contains("(redacted)"), "{shown}");
    }

    /// The load-bearing guard for every Sarvam user: with the slot off, the
    /// resolver has to produce exactly `Backend::sarvam`.
    #[test]
    fn an_off_slot_resolves_to_the_plain_sarvam_backend() {
        let b = resolve(&CustomSlot::default(), Some("k"), "sarvam-105b").expect("a key is stored");
        assert_eq!(b.base_url, SARVAM_CHAT_URL);
        assert_eq!(b.api_key, "k");
        assert_eq!(b.model, "sarvam-105b");
        assert_eq!(b.kind, BackendKind::Sarvam);
    }

    /// A configured-but-not-enabled endpoint is still off. The toggle is the
    /// switch, not the presence of a URL.
    #[test]
    fn a_configured_slot_that_is_not_enabled_stays_on_sarvam() {
        let slot = CustomSlot {
            use_for_polish: false,
            ..on_slot()
        };
        let b = resolve(&slot, Some("k"), "sarvam-105b").expect("a key is stored");
        assert_eq!(b.base_url, SARVAM_CHAT_URL);
    }

    #[test]
    fn an_enabled_slot_resolves_to_its_own_chat_route_and_model() {
        let b = resolve(&on_slot(), Some("k"), "sarvam-105b").expect("the slot is usable");
        assert_eq!(b.base_url, "http://localhost:11434/v1/chat/completions");
        assert_eq!(b.model, "qwen3:8b");
        assert_eq!(b.kind, BackendKind::Custom);
        assert_eq!(b.api_key, "", "no key entered means no bearer token");
    }

    /// The custom endpoint is the whole point of a Sarvam-less install: with
    /// a usable slot, no Sarvam key is needed for chat at all.
    #[test]
    fn an_enabled_slot_needs_no_sarvam_key() {
        let b = resolve(&on_slot(), None, "sarvam-105b").expect("the slot is usable");
        assert_eq!(b.kind, BackendKind::Custom);
    }

    fn relay_transport() -> Transport {
        Transport::Relay {
            base: "https://relay.example.workers.dev".into(),
            bearer: "supabase-access-token".into(),
        }
    }

    /// Cloud mode's polish goes to the relay with the user's own token, and
    /// asks for no Sarvam key anywhere on the way.
    #[test]
    fn the_cloud_lane_polishes_through_the_relay() {
        let b = resolve_for(&CustomSlot::default(), &relay_transport(), "sarvam-105b")
            .expect("the relay is always usable when signed in");
        assert_eq!(b.kind, BackendKind::Relay);
        assert_eq!(
            b.base_url,
            "https://relay.example.workers.dev/v1/chat/completions"
        );
        assert_eq!(b.api_key, "supabase-access-token");
        assert_eq!(b.model, "sarvam-105b");
    }

    /// The Bring-your-own-key lane through the same door: still Sarvam's own
    /// backend.
    #[test]
    fn the_byok_lane_resolves_to_the_sarvam_backend() {
        let b = resolve_for(
            &CustomSlot::default(),
            &Transport::Sarvam { key: "k".into() },
            "sarvam-105b",
        )
        .expect("a key is stored");
        assert_eq!(b.kind, BackendKind::Sarvam);
        assert_eq!(b.base_url, SARVAM_CHAT_URL);
        assert_eq!(b.api_key, "k");
    }

    /// The custom slot is a toggle on top of the provider, not a fourth
    /// provider, so it keeps winning for polish on the Cloud lane exactly as
    /// it does on Bring-your-own-key.
    #[test]
    fn an_enabled_slot_still_wins_on_the_cloud_lane() {
        let b = resolve_for(&on_slot(), &relay_transport(), "sarvam-105b")
            .expect("the slot is usable");
        assert_eq!(b.kind, BackendKind::Custom);
    }

    /// And when that slot is unusable, the Cloud lane degrades to the relay
    /// — the host already hearing this dictation — rather than to Sarvam,
    /// which this install cannot authenticate against at all.
    #[test]
    fn an_unusable_slot_degrades_to_the_relay_not_to_sarvam() {
        let slot = CustomSlot {
            base_url: "not a url".into(),
            ..on_slot()
        };
        let b = resolve_for(&slot, &relay_transport(), "sarvam-105b")
            .expect("degrades to the relay");
        assert_eq!(b.kind, BackendKind::Relay);
    }

    fn cloud_lane() -> Lane {
        Lane::Cloud {
            relay: "https://relay.example.workers.dev".into(),
        }
    }

    /// The gate the chord-down decisions ask, which has to answer without a
    /// credential: a signed-in Cloud install can make a chat call, and a
    /// signed-out one with nothing else configured cannot.
    #[test]
    fn a_signed_in_cloud_install_has_a_chat_backend() {
        assert!(matches!(pick(&CustomSlot::default(), true), Pick::Home(None)));
        assert!(matches!(
            pick(&CustomSlot::default(), false),
            Pick::Nothing(Unavailable::NoSarvamKey)
        ));
    }

    /// And the gate agrees with the resolution it is gating, on every shape
    /// of slot: whenever `pick` says there is something, `resolve_against`
    /// produces a backend, and whenever it says there is nothing, that is an
    /// `Err`. They are one function precisely so this cannot drift.
    #[test]
    fn the_gate_and_the_resolution_never_disagree() {
        let slots = [
            CustomSlot::default(),
            on_slot(),
            CustomSlot {
                base_url: "not a url".into(),
                ..on_slot()
            },
            CustomSlot {
                model: String::new(),
                ..on_slot()
            },
            CustomSlot {
                use_for_stt: true,
                base_url: "not a url".into(),
                ..on_slot()
            },
        ];
        for slot in slots {
            for home in [true, false] {
                let gated = !matches!(pick(&slot, home), Pick::Nothing(_));
                let home_backend = home.then(|| Backend::sarvam("k", "sarvam-105b"));
                let resolved = resolve_against(&slot, home_backend).is_ok();
                assert_eq!(gated, resolved, "slot {slot:?}, home {home}");
            }
        }
    }

    /// The bearer source every test hands in, and a record of whether it was
    /// asked for.
    ///
    /// The real one is `auth::session::access_token`, which reads — and on a
    /// rejection *deletes* — the developer's own Windows credential entry. No
    /// test may reach it: a regression would spend or destroy a real Supabase
    /// session from a `cargo test` run. Handing it in also makes the
    /// interesting half assertable, because "this path never asked for a
    /// credential" is exactly what several of these tests are about.
    fn counted(
        calls: &std::cell::Cell<u32>,
        answer: Result<Transport, String>,
    ) -> impl std::future::Future<Output = Result<Transport, String>> + '_ {
        async move {
            calls.set(calls.get() + 1);
            answer
        }
    }

    fn a_bearer() -> Result<Transport, String> {
        Ok(relay_transport())
    }

    /// A Cloud install whose polish runs on its own endpoint is never asked
    /// to sign in: the custom endpoint answers on its own, and the relay's
    /// bearer is never fetched at all.
    #[tokio::test]
    async fn the_cloud_lane_needs_no_sign_in_when_the_custom_endpoint_answers() {
        let calls = std::cell::Cell::new(0);
        let b = chat_backend_on(
            &on_slot(),
            &cloud_lane(),
            None,
            "sarvam-105b",
            counted(&calls, a_bearer()),
        )
        .await
        .expect("the slot is usable on its own");
        assert_eq!(b.kind, BackendKind::Custom);
        assert_eq!(
            calls.get(),
            0,
            "a call that never goes to the relay must not ask for a sign-in"
        );
    }

    /// And when the call *is* going to the relay, the bearer that comes back
    /// is what the request will carry.
    #[tokio::test]
    async fn the_bearer_becomes_the_relay_backend() {
        let calls = std::cell::Cell::new(0);
        let b = chat_backend_on(
            &CustomSlot::default(),
            &cloud_lane(),
            None,
            "sarvam-105b",
            counted(&calls, a_bearer()),
        )
        .await
        .expect("a signed-in Cloud install has the relay");
        assert_eq!(b.kind, BackendKind::Relay);
        assert_eq!(
            b.base_url,
            "https://relay.example.workers.dev/v1/chat/completions"
        );
        assert_eq!(b.api_key, "supabase-access-token");
        assert_eq!(b.model, "sarvam-105b");
        assert_eq!(calls.get(), 1, "exactly one credential per call");
    }

    /// A sign-in that cannot be obtained is its own shape, carrying
    /// `auth::session`'s finished sentence — which is the one that knows
    /// whether this user must sign in again or is simply offline. It must
    /// never be reported as "add your Sarvam key".
    #[tokio::test]
    async fn a_credential_that_cannot_be_obtained_is_the_sign_in_shape() {
        let calls = std::cell::Cell::new(0);
        let outcome = chat_backend_on(
            &CustomSlot::default(),
            &cloud_lane(),
            None,
            "sarvam-105b",
            counted(&calls, Err("You're not signed in to Butterfly Labs".into())),
        )
        .await;
        match outcome {
            Err(ChatUnavailable::SignIn(sentence)) => {
                assert_eq!(sentence, "You're not signed in to Butterfly Labs");
            }
            other => panic!("expected the sign-in shape, got {other:?}"),
        }
        assert_eq!(calls.get(), 1);
    }

    /// The slot that refuses whatever the host is: unusable, and doing this
    /// install's speech-to-text as well, so the fallback to the relay is the
    /// one `resolve`'s rule forbids. Deciding it before the credential is
    /// fetched saves a user who cannot be helped a fifteen- second token round
    /// trip on every chord.
    #[tokio::test]
    async fn a_refusal_that_no_credential_could_change_never_asks_for_one() {
        let slot = CustomSlot {
            base_url: "not a url".into(),
            use_for_stt: true,
            ..on_slot()
        };
        let calls = std::cell::Cell::new(0);
        let outcome = chat_backend_on(
            &slot,
            &cloud_lane(),
            None,
            "sarvam-105b",
            counted(&calls, a_bearer()),
        )
        .await;
        assert!(
            matches!(
                outcome,
                Err(ChatUnavailable::Backend(Unavailable::CustomEndpoint(_)))
            ),
            "the slot's own reason is what the user can act on"
        );
        assert_eq!(
            calls.get(),
            0,
            "no bearer can rescue this call, so none is fetched"
        );
    }

    /// The Bring-your-own-key lane through the async door is the synchronous
    /// resolver — including the install with no Sarvam key at all and a
    /// custom endpoint that works.
    #[tokio::test]
    async fn the_async_door_resolves_bring_your_own_key_like_the_sync_one() {
        let calls = std::cell::Cell::new(0);
        let with_key = chat_backend_on(
            &CustomSlot::default(),
            &Lane::Byok,
            Some("k"),
            "sarvam-105b",
            counted(&calls, a_bearer()),
        )
        .await
        .expect("a key is stored");
        assert_eq!(with_key.kind, BackendKind::Sarvam);
        assert_eq!(with_key.base_url, SARVAM_CHAT_URL);

        let no_key_own_endpoint = chat_backend_on(
            &on_slot(),
            &Lane::Byok,
            None,
            "sarvam-105b",
            counted(&calls, a_bearer()),
        )
        .await
        .expect("an install with no Sarvam account still has a backend");
        assert_eq!(no_key_own_endpoint.kind, BackendKind::Custom);

        let nothing = chat_backend_on(
            &CustomSlot::default(),
            &Lane::Byok,
            None,
            "sarvam-105b",
            counted(&calls, a_bearer()),
        )
        .await;
        assert!(
            matches!(
                nothing,
                Err(ChatUnavailable::Backend(Unavailable::NoSarvamKey))
            ),
            "a missing key must stay the shape every call site already words"
        );
        assert_eq!(
            calls.get(),
            0,
            "the key lane has a credential already and must never fetch a bearer"
        );
    }

    #[test]
    fn the_slots_key_reaches_the_backend() {
        let slot = CustomSlot {
            api_key: Some("sk-abc".into()),
            ..on_slot()
        };
        let b = resolve(&slot, Some("k"), "sarvam-105b").expect("the slot is usable");
        assert_eq!(b.api_key, "sk-abc");
    }

    /// Fallback rule, branch one: Sarvam is already hearing this dictation (it
    /// is doing the STT) and has a key, so an unusable polish endpoint may
    /// still degrade there — nothing reaches a host this install wasn't using.
    #[test]
    fn an_invalid_slot_degrades_to_sarvam_while_sarvam_is_doing_the_stt() {
        for base in ["", "   ", "http://api.example.com/v1", "nonsense"] {
            let slot = CustomSlot {
                base_url: base.into(),
                ..on_slot()
            };
            let b = resolve(&slot, Some("k"), "sarvam-105b").expect("degrades to Sarvam");
            assert_eq!(b.base_url, SARVAM_CHAT_URL, "base {base:?}");
            assert_eq!(b.kind, BackendKind::Sarvam, "base {base:?}");
        }
    }

    /// Fallback rule, branch two: with the endpoint doing STT, Sarvam has heard
    /// nothing — degrading would hand it the transcript of a dictation it was
    /// never part of. Fail instead, and say so.
    #[test]
    fn an_invalid_slot_fails_instead_of_degrading_when_it_also_does_the_stt() {
        let slot = CustomSlot {
            base_url: "http://api.example.com/v1".into(),
            use_for_stt: true,
            ..on_slot()
        };
        assert_eq!(
            resolve(&slot, Some("k"), "sarvam-105b").expect_err("must not degrade"),
            Unavailable::CustomEndpoint(Invalid::InsecureHttp),
            "a stored Sarvam key is not permission to use it"
        );
    }

    /// Fallback rule, branch three: no Sarvam key at all. There is nothing to
    /// degrade *to*, and the request goes to no other host either.
    #[test]
    fn an_invalid_slot_fails_when_there_is_no_sarvam_key_to_degrade_to() {
        let slot = CustomSlot {
            base_url: "nonsense".into(),
            ..on_slot()
        };
        assert_eq!(
            resolve(&slot, None, "sarvam-105b").expect_err("nothing to degrade to"),
            Unavailable::CustomEndpoint(Invalid::NotAUrl)
        );
        assert_eq!(
            resolve(&slot, Some("   "), "sarvam-105b").expect_err("nothing to degrade to"),
            Unavailable::CustomEndpoint(Invalid::NotAUrl),
            "a blank key is not a key"
        );
    }

    /// An endpoint with no model named cannot be called at all — a request
    /// with an empty `model` is a 400 from every host — and it is reported
    /// with its own reason rather than as a URL problem.
    #[test]
    fn an_enabled_slot_with_no_model_is_reported_as_such() {
        let slot = CustomSlot {
            model: "  ".into(),
            use_for_stt: true,
            ..on_slot()
        };
        assert_eq!(
            resolve(&slot, Some("k"), "sarvam-105b").expect_err("must not degrade"),
            Unavailable::CustomEndpoint(Invalid::NoModel)
        );
        // ...and with Sarvam still in the loop, the same slot degrades.
        let degradable = CustomSlot {
            use_for_stt: false,
            ..slot
        };
        assert_eq!(
            resolve(&degradable, Some("k"), "sarvam-105b")
                .expect("degrades")
                .base_url,
            SARVAM_CHAT_URL
        );
    }

    /// With the slot off entirely and no Sarvam key, there is no backend —
    /// which the caller has to hear as "nothing configured", not as a
    /// Sarvam-shaped `Backend` with an empty key that 401s.
    #[test]
    fn no_slot_and_no_key_is_no_backend() {
        assert_eq!(
            resolve(&CustomSlot::default(), None, "sarvam-105b").expect_err("no backend"),
            Unavailable::NoSarvamKey
        );
        assert_eq!(
            resolve(&CustomSlot::default(), Some(""), "sarvam-105b").expect_err("no backend"),
            Unavailable::NoSarvamKey
        );
    }

    /// Every refusal has a sentence, and no two say the same thing — the
    /// notice is the whole product surface of this enum.
    #[test]
    fn every_invalid_reason_says_something_different() {
        let all = [
            Invalid::NotConfigured,
            Invalid::NotAUrl,
            Invalid::InsecureHttp,
            Invalid::NoModel,
        ];
        let messages: std::collections::HashSet<&str> = all.iter().map(|i| i.message()).collect();
        assert_eq!(messages.len(), all.len());
    }

    /// The Settings screen tells the user *why* an endpoint they switched on
    /// isn't being used ("Falling back to Sarvam: …"), and it derives that
    /// sentence from two checks of its own: an empty model, then
    /// `check_custom_endpoint` on the URL. This pins that those really are
    /// the only two ways an enabled slot degrades — a third reason added here
    /// without a matching line in `EndpointSection.svelte` would show the
    /// user a green panel over a Sarvam request.
    #[test]
    fn an_enabled_slot_degrades_only_for_a_missing_model_or_an_unusable_base() {
        let good = on_slot();
        assert_eq!(
            resolve(&good, Some("k"), "sarvam-105b")
                .expect("a usable slot resolves")
                .kind,
            BackendKind::Custom,
            "a slot with a model and a usable base must not degrade"
        );
        assert!(!good.model.trim().is_empty() && resolve_base(&good.base_url).is_ok());

        // Every degrading variant fails at least one of those same two
        // checks, which is what makes the UI's derivation faithful.
        for slot in [
            CustomSlot { model: "  ".into(), ..on_slot() },
            CustomSlot { base_url: String::new(), ..on_slot() },
            CustomSlot { base_url: "localhost:11434".into(), ..on_slot() },
            CustomSlot { base_url: "http://api.example.com".into(), ..on_slot() },
        ] {
            assert_eq!(
                resolve(&slot, Some("k"), "sarvam-105b")
                    .expect("Sarvam is still hearing this dictation, so it degrades")
                    .kind,
                BackendKind::Sarvam,
                "slot {slot:?}"
            );
            assert!(
                slot.model.trim().is_empty() || resolve_base(&slot.base_url).is_err(),
                "slot {slot:?} degraded for a reason the Settings screen cannot name"
            );
            // ...and the same slot with speech-to-text pointed here does not
            // degrade at all, which is the third thing that panel has to be
            // able to say. The two checks it derives from are still
            // the whole story — only the sentence changes.
            let stt_too = CustomSlot {
                use_for_stt: true,
                ..slot.clone()
            };
            assert!(
                matches!(
                    resolve(&stt_too, Some("k"), "sarvam-105b"),
                    Err(Unavailable::CustomEndpoint(_))
                ),
                "slot {stt_too:?} must not hand Sarvam a dictation it never heard"
            );
        }
    }
}
