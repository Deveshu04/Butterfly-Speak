//! The sign-in round trip, and the two tokens it leaves behind.
//!
//! Three things happen here and nowhere else:
//!
//! 1. **The browser trip.** [`begin_sign_in`] mints a PKCE verifier, keeps it
//!    in this process, and opens the *system* browser at GoTrue's
//!    `/authorize`. No webview login page, no Google SDK — the user's Google
//!    password is typed into Google's own page in their own browser, which is
//!    also the only place a password manager and a passkey will offer to help.
//! 2. **The hand-back.** Google returns to Supabase, Supabase redirects to
//!    `butterflylabs://auth/callback?code=…`, Windows launches (or re-uses)
//!    this app with that URL, and [`code_from_callback`] recognises it.
//!    Anything else on that scheme is ignored: the deep link is a public
//!    doorbell, and the only thing it is allowed to carry is a code that is
//!    worthless without the verifier held above.
//! 3. **The tokens.** The access token lives in memory for its hour and dies
//!    with the process. The refresh token goes to Windows Credential Manager
//!    through the same [`crate::sarvam::key`] path the Sarvam key uses — never
//!    settings.json, never a log line, never the webview.
//!
//! Log rule for this whole module: a line may say which step happened and
//! what HTTP status — or which class of transport fault — came back, and
//! nothing else. Not the code, not either token, not the verifier, not the
//! signed-in address. In particular no error is ever rendered whole: both
//! `reqwest::Error` and the opener's error put the URL they were handed into
//! their `Display`, and those URLs are the token endpoint and the authorize
//! URL.

use super::pkce;
use crate::sarvam::key::{self, KeySlot};
use serde::Deserialize;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};
use tauri::{AppHandle, Runtime};
use tauri_plugin_opener::OpenerExt;

/// The Butterfly Labs Supabase project. Public by design: the anon key is a
/// signed claim of the `anon` role and opens nothing RLS has not already
/// opened. The relay, not this app, holds anything that is actually secret.
pub const SUPABASE_URL: &str = "https://iassqjnfvdffocyxptis.supabase.co";
pub const SUPABASE_ANON_KEY: &str = "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJpc3MiOiJzdXBhYmFzZSIsInJlZiI6Imlhc3Nxam5mdmRmZm9jeXhwdGlzIiwicm9sZSI6ImFub24iLCJpYXQiOjE3ODk3MTY5MjQsImV4cCI6MjEwNTI5MjkyNH0.HTef2ALn-SmsyFll1vT9r8ZuRFnUiKNbHPW-3b9tkH4";

/// Where Supabase sends the browser when Google is done. Registered as an
/// exact string in the project's redirect allowlist (no glob), declared as a
/// scheme in `tauri.conf.json`, and claimed in the registry by the installer.
pub const DEEP_LINK_CALLBACK: &str = "butterflylabs://auth/callback";

/// How much life an access token must have left to be handed out as-is.
///
/// Five minutes, because the caller is about to open a dictation socket that
/// may stay up for a while: a token that is valid at the handshake and expires
/// mid-sentence is the failure this margin exists to prevent.
const REFRESH_MARGIN: Duration = Duration::from_secs(5 * 60);

/// The access token for this run, or nothing. Never persisted: an hour-long
/// bearer on disk buys nothing the refresh token does not already buy, and
/// costs a second place to leak from.
static SESSION: Mutex<Option<AuthSession>> = Mutex::new(None);

/// The PKCE verifier for a sign-in that is out at the browser, cleared once a
/// sign-in with it succeeds or the user signs out. In memory only and
/// deliberately so — the whole point of PKCE is that this never touches disk,
/// so a deep link forged by another program on the machine has nothing to
/// pair its code with.
///
/// A callback whose code the token endpoint refuses leaves it in place: the
/// deep link is a doorbell anyone can ring, and a stray or forged code must
/// not spend the verifier the real callback still needs.
static PENDING: Mutex<Option<String>> = Mutex::new(None);

/// Moves on when a browser trip starts, when a sign-in is taken and when the
/// user signs out: the moments that change which credential this machine
/// should hold. A token call notes the count before it goes out, and acts on
/// its answer (stores a token, or drops a refused one) only if the count has
/// not moved, checked under this lock, which every move also holds. So an
/// answer that lands after a sign-out cannot sign the user back in, and one
/// that lands after a newer sign-in cannot replace or delete it.
///
/// Lock order: this one first, then [`SESSION`] or [`PENDING`].
static GENERATION: Mutex<u64> = Mutex::new(0);

/// The current value of [`GENERATION`].
fn generation() -> u64 {
    *GENERATION.lock().expect("sign-in generation lock")
}

/// Run `act` under [`GENERATION`]'s lock if the count is still `started`,
/// and say whether it ran.
fn if_current(started: u64, act: impl FnOnce()) -> bool {
    let current = GENERATION.lock().expect("sign-in generation lock");
    let unchanged = *current == started;
    if unchanged {
        act();
    }
    unchanged
}

/// Serialises refreshes. Supabase rotates the refresh token on every use, so
/// two concurrent refreshes race to spend the same one and the loser is left
/// holding a token the server has already retired.
static REFRESHING: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// How long a call to the token endpoint may take before it counts as a
/// network failure.
///
/// There has to be one. `reqwest::Client::new()` has no default timeout at
/// all, and [`access_token`] holds [`REFRESHING`] across the refresh: a host
/// that accepts the connection and then says nothing would park every later
/// caller — the relay's per-connection bearer and the Cloud card's
/// `cloud_status` included — for as long as the socket stayed open. Fifteen
/// seconds is long enough to survive a slow handover between networks and
/// short enough that the card answers rather than spins.
const TOKEN_TIMEOUT: Duration = Duration::from_secs(15);

fn http() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .timeout(TOKEN_TIMEOUT)
            .build()
            .unwrap_or_else(|e| {
                // Same policy as `format::backend`'s client: a builder that
                // will not build is not a reason to refuse to sign in — it
                // is a reason to sign in without the deadline.
                tracing::warn!("auth http client builder failed ({e}); using defaults");
                reqwest::Client::new()
            })
    })
}

/// Where the system browser is sent to start a sign-in.
///
/// `code_challenge` is left unescaped on purpose: base64url's alphabet is a
/// subset of RFC 3986's `unreserved` set, so percent-encoding it would only
/// produce a string GoTrue has to decode back to the same bytes — and a
/// mismatch there is invisible until the exchange fails.
pub fn authorize_url(verifier: &str) -> String {
    let redirect = percent_encoding::utf8_percent_encode(
        DEEP_LINK_CALLBACK,
        percent_encoding::NON_ALPHANUMERIC,
    );
    format!(
        "{SUPABASE_URL}/auth/v1/authorize?provider=google&redirect_to={redirect}\
         &code_challenge={}&code_challenge_method=s256",
        pkce::challenge(verifier)
    )
}

/// The authorization code in a `butterflylabs://auth/callback?code=…` URL, or
/// `None` for anything else.
///
/// Every other URL is refused rather than parsed for a `code` parameter. The
/// app is the registered handler for the whole `butterflylabs://` scheme, so
/// it will be handed URLs this flow never asked for — a future
/// `butterflylabs://note/42`, a typo, a link on a web page someone clicked —
/// and only one shape of them is a sign-in.
pub fn code_from_callback(url: &str) -> Option<String> {
    let raw = param(callback_query(url)?, "code")?;
    Some(
        percent_encoding::percent_decode_str(raw)
            .decode_utf8_lossy()
            .into_owned(),
    )
    .filter(|code| !code.is_empty())
}

/// Whether `url` is this app's callback coming back *without* a code — the
/// user closed Google's consent screen, or the provider refused.
///
/// Only the presence of an `error` parameter is read. What it says is the
/// browser's business: the one thing the app does with this is stop waiting,
/// and a reason copied into a log line is a reason that can carry an address.
pub fn callback_declined(url: &str) -> bool {
    callback_query(url).is_some_and(|query| param(query, "error").is_some())
}

/// The query string of a `butterflylabs://auth/callback?…` URL, or `None` for
/// anything else on the scheme.
fn callback_query(url: &str) -> Option<&str> {
    let (base, query) = url.split_once('?')?;
    // A trailing slash survives some round-trips through `url::Url` and means
    // the same path; a different scheme, host or path does not.
    if !base
        .trim_end_matches('/')
        .eq_ignore_ascii_case(DEEP_LINK_CALLBACK)
    {
        return None;
    }
    Some(query)
}

/// One query parameter, still percent-encoded. A bare `?error` with no `=`
/// counts as present with an empty value, which is what a browser means by it.
fn param<'a>(query: &'a str, name: &str) -> Option<&'a str> {
    query
        .split('&')
        .map(|pair| pair.split_once('=').unwrap_or((pair, "")))
        .find(|(key, _)| *key == name)
        .map(|(_, value)| value)
}

/// GoTrue's token response. `token_type` is deliberately absent: serde ignores
/// what it is not asked for, and a field nothing reads is a field that can go
/// stale without anyone noticing.
#[derive(Deserialize)]
pub struct TokenResponse {
    pub access_token: String,
    pub expires_in: u64,
    pub refresh_token: String,
    pub user: TokenUser,
}

#[derive(Deserialize)]
pub struct TokenUser {
    pub email: Option<String>,
}

/// What this process knows about the signed-in user right now.
#[derive(Clone)]
pub struct AuthSession {
    pub access_token: String,
    pub expires_at: Instant,
    pub user_email: Option<String>,
}

impl AuthSession {
    pub fn from_response(response: &TokenResponse, now: Instant) -> Self {
        Self {
            access_token: response.access_token.clone(),
            expires_at: now + Duration::from_secs(response.expires_in),
            user_email: response.user.email.clone(),
        }
    }

    /// Whether this token is too close to its expiry to hand out.
    ///
    /// `saturating_duration_since`, not a subtraction: an already-expired
    /// token gives zero rather than panicking, and zero is correctly "needs
    /// refresh".
    pub fn needs_refresh(&self, now: Instant) -> bool {
        self.expires_at.saturating_duration_since(now) < REFRESH_MARGIN
    }
}

/// What the Cloud card shows.
#[derive(Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CloudStatus {
    pub signed_in: bool,
    /// Only known once a token has been exchanged or refreshed in this run —
    /// the address is not stored anywhere, so a fresh launch has the refresh
    /// token and no name to go with it until it spends one.
    pub email: Option<String>,
}

/// Why a token call did not produce a token.
///
/// The distinction is the whole reason this is not one opaque error: a
/// *rejected* refresh means the stored credential is dead and the app should
/// stop claiming to be signed in, while an *unreachable* one means the user is
/// on a train and everything is still fine.
#[derive(Debug, PartialEq, Eq)]
enum Failure {
    /// Nothing is stored to refresh: this machine has never signed in, or has
    /// signed out. No request is sent and there is nothing to delete.
    SignedOut,
    /// The status that came with it is logged where it happens and not
    /// carried here: an HTTP number is not something to show a user, and the
    /// only decision this drives is "is the stored credential dead".
    Rejected,
    Unreachable,
}

impl Failure {
    fn sentence(&self, what: &str) -> String {
        match self {
            Failure::SignedOut => "You're not signed in to Butterfly Labs".into(),
            Failure::Rejected => format!("{what} — sign in again"),
            Failure::Unreachable => {
                "Couldn't reach Butterfly Labs — check your internet connection".into()
            }
        }
    }
}

/// Which kind of transport fault, in a few words.
///
/// Never the `reqwest::Error` itself. Its `Display` appends `for url (…)`, so
/// `{e}` would put the token endpoint and the grant being attempted into a log
/// line this module's own rule says may carry only the step and the status.
fn transport_class(e: &reqwest::Error) -> &'static str {
    if e.is_timeout() {
        "timed out"
    } else if e.is_connect() {
        "could not connect"
    } else if e.is_request() {
        "the request could not be sent"
    } else {
        "no response"
    }
}

/// What a non-2xx from the token endpoint means for the stored credential.
///
/// Only four statuses are GoTrue saying "this grant is no good": 400 (an
/// invalid or malformed refresh token — measured), 401 and 403 (the
/// session is gone), and 404, which is what a spent or unknown PKCE flow
/// answers with `flow_state_not_found` — also measured, and the reason nothing
/// here keys on 400 alone.
///
/// Everything else is the service having a bad day, and the difference is not
/// academic: [`refresh_at`] deletes the stored token on a rejection, so
/// treating a 429 or a 5xx as one would turn a single bad hour at Supabase
/// into a fresh Google consent screen for every user who happened to refresh
/// during it, with nothing to tell them why. Anything unrecognised is
/// therefore transport too — the cost of guessing wrong that way is one failed
/// call, and of guessing wrong the other way is a sign-in.
fn classify(status: reqwest::StatusCode) -> Failure {
    match status.as_u16() {
        400 | 401 | 403 | 404 => Failure::Rejected,
        _ => Failure::Unreachable,
    }
}

/// The body GoTrue's PKCE grant reads.
///
/// Measured against the live endpoint: the field is `auth_code`,
/// not `code`, and `code_verifier` sits beside it. A rename on either side
/// comes back as a 404 `flow_state_not_found`, which reads exactly like an
/// expired flow — which is why the test pins this function and not a literal
/// written out a second time beside it.
fn pkce_grant_body(code: &str, verifier: &str) -> serde_json::Value {
    serde_json::json!({ "auth_code": code, "code_verifier": verifier })
}

/// The stored half of a sign-in: one refresh token, in one credential slot.
///
/// A trait with exactly one implementation in the app, and it earns its keep
/// in the tests. The real slot is Windows Credential Manager, where on any
/// machine this is built the entry is the developer's *own* sign-in — a test
/// that wrote to it would sign them out.
///
/// `Send + Sync` because a `&dyn RefreshStore` is held across the await in
/// [`refresh_at`], and that future is awaited inside a `#[tauri::command]`,
/// which Tauri requires to be `Send`.
trait RefreshStore: Send + Sync {
    fn load(&self) -> Option<String>;
    fn save(&self, token: &str) -> anyhow::Result<()>;
    fn forget(&self) -> anyhow::Result<()>;
}

/// The real one: [`KeySlot::CloudRefresh`], through the same path the Sarvam
/// key uses.
struct CredentialManager;

impl RefreshStore for CredentialManager {
    fn load(&self) -> Option<String> {
        key::load(KeySlot::CloudRefresh)
    }
    fn save(&self, token: &str) -> anyhow::Result<()> {
        key::store(KeySlot::CloudRefresh, token)
    }
    fn forget(&self) -> anyhow::Result<()> {
        key::delete(KeySlot::CloudRefresh)
    }
}

/// `POST {base}/auth/v1/token?grant_type=…`. The only place either grant is
/// spoken.
///
/// `base` is a parameter rather than [`SUPABASE_URL`] for one reason: it is
/// the seam the tests point at a loopback listener, so what this does with a
/// 4xx, a 2xx and a dead socket is covered without the network.
async fn exchange_at(
    base: &str,
    grant_type: &str,
    body: serde_json::Value,
) -> Result<TokenResponse, Failure> {
    let response = http()
        .post(format!("{base}/auth/v1/token?grant_type={grant_type}"))
        .header("apikey", SUPABASE_ANON_KEY)
        .json(&body)
        .send()
        .await
        .map_err(|e| {
            tracing::debug!("cloud sign-in failed ({})", transport_class(&e));
            Failure::Unreachable
        })?;

    let status = response.status();
    if !status.is_success() {
        tracing::debug!("cloud sign-in failed (status {})", status.as_u16());
        return Err(classify(status));
    }

    response.json::<TokenResponse>().await.map_err(|_| {
        // A 2xx whose body will not parse is not something retrying fixes, but
        // it is also not proof the credential is dead — treat it as transport.
        tracing::debug!("cloud sign-in failed (unreadable token response)");
        Failure::Unreachable
    })
}

/// One turn at the refresh grant: spend what `store` holds, and leave `store`
/// holding whatever the answer means it should.
///
/// Both bindings are parameters so the three behaviours worth guarding can be
/// tested without a network or the machine's real credential slot — a refused
/// refresh deletes the stored token, an unreachable one keeps it, and a
/// successful one replaces it with the rotated token GoTrue just issued.
/// Deliberately does not touch [`SESSION`]: that is process state, and this is
/// the part that is about the credential.
///
/// `started` is [`GENERATION`] as the caller read it before this went out.
/// An answer that lands after it moved changes nothing in `store`, and a
/// token is answered as [`Failure::SignedOut`]: the sign-in it would renew
/// has ended here.
async fn refresh_at(
    base: &str,
    store: &dyn RefreshStore,
    started: u64,
) -> Result<TokenResponse, Failure> {
    let stored = store.load().ok_or(Failure::SignedOut)?;
    match exchange_at(
        base,
        "refresh_token",
        serde_json::json!({ "refresh_token": stored }),
    )
    .await
    {
        Ok(token) => {
            // GoTrue rotates: the token just spent is already retired
            // server-side, so the store must hold the new one before this
            // returns or the next launch presents a dead one.
            if !if_current(started, || persist(store, &token.refresh_token)) {
                tracing::debug!("cloud sign-in refresh dropped (signed out while it was out)");
                return Err(Failure::SignedOut);
            }
            Ok(token)
        }
        Err(Failure::Rejected) => {
            // The server has retired this token — it will never work again,
            // so keeping it only makes the app claim a sign-in it does not
            // have. Drop it and let the card say "Sign in".
            if_current(started, || {
                if let Err(e) = store.forget() {
                    tracing::warn!("couldn't remove the sign-in from the credential store: {e:#}");
                }
            });
            Err(Failure::Rejected)
        }
        Err(other) => Err(other),
    }
}

/// Put a refresh token in the store, or say why the sign-in will not survive.
///
/// A failure must not brick the run — the access token in memory still works
/// until it expires. It does cost the user a sign-in, and in the rotation case
/// it costs them one *silently*: the store is left holding a token the server
/// has already retired, which GoTrue's reuse detection refuses on the next
/// launch. So the line says so.
fn persist(store: &dyn RefreshStore, refresh_token: &str) {
    if let Err(e) = store.save(refresh_token) {
        tracing::warn!(
            "couldn't persist the sign-in to the credential store: {e:#}; \
             this sign-in will not survive a restart and will have to be repeated"
        );
    }
}

/// Take the tokens: refresh token to the credential store, the rest in memory.
fn adopt(store: &dyn RefreshStore, token: &TokenResponse) {
    persist(store, &token.refresh_token);
    remember(token);
}

/// Keep the access token for this run. Never persisted — see [`SESSION`].
fn remember(token: &TokenResponse) {
    *SESSION.lock().expect("session lock") =
        Some(AuthSession::from_response(token, Instant::now()));
}

/// Open the system browser at Google's consent screen and remember the
/// verifier the hand-back will have to be paired with.
pub fn begin_sign_in<R: Runtime>(app: &AppHandle<R>) -> Result<(), String> {
    let verifier = pkce::verifier();
    let url = authorize_url(&verifier);
    // Stored before the browser opens, not after: on a fast machine the
    // callback can arrive while `open_url` is still returning. A new trip
    // also moves the count on, so a token call from before it cannot land
    // on top of the sign-in it starts.
    {
        let mut generation = GENERATION.lock().expect("sign-in generation lock");
        *generation += 1;
        *PENDING.lock().expect("pkce verifier lock") = Some(verifier);
    }
    app.opener().open_url(url, None::<&str>).map_err(|_| {
        PENDING.lock().expect("pkce verifier lock").take();
        // The error is deliberately not rendered: the opener's `Display`
        // quotes the URL it was handed, which is the authorize URL with the
        // code challenge and the redirect in it. There is exactly one useful
        // fact here and the line carries it.
        tracing::debug!("cloud sign-in failed (browser did not open)");
        "Couldn't open your browser to sign in".to_string()
    })?;
    tracing::debug!("cloud sign-in started");
    Ok(())
}

/// Spend the code the deep link carried.
pub async fn complete_sign_in(code: &str) -> Result<(), String> {
    complete_sign_in_at(SUPABASE_URL, &CredentialManager, code).await
}

/// [`complete_sign_in`] against a given host and store, so what a refused
/// code and a sign-out mid-exchange do is tested against a loopback listener
/// and an in-memory store.
async fn complete_sign_in_at(
    base: &str,
    store: &dyn RefreshStore,
    code: &str,
) -> Result<(), String> {
    let started = generation();
    // Read, not taken: the verifier is spent only by a sign-in that works
    // (see `PENDING`).
    let verifier = PENDING
        .lock()
        .expect("pkce verifier lock")
        .clone()
        .ok_or_else(|| {
            // Either a sign-in that was never started here, or a second
            // callback for one already spent. Both are the same answer, and
            // neither is worth a code in the log.
            tracing::debug!("cloud sign-in failed (no sign-in was in progress)");
            "That sign-in link wasn't for this app — start again from Settings".to_string()
        })?;

    let token = exchange_at(base, "pkce", pkce_grant_body(code, &verifier))
        .await
        .map_err(|f| f.sentence("Sign-in didn't complete"))?;

    // Taken only if nothing moved the count while the exchange was out: a
    // Cancel or a sign-out in that time means the user said stop.
    {
        let mut generation = GENERATION.lock().expect("sign-in generation lock");
        if *generation != started {
            tracing::debug!("cloud sign-in dropped (cancelled while it was out)");
            return Err("That sign-in was cancelled before it finished".to_string());
        }
        *generation += 1;
        adopt(store, &token);
        PENDING.lock().expect("pkce verifier lock").take();
    }
    tracing::debug!("cloud sign-in completed");
    Ok(())
}

/// A usable bearer for the relay, refreshing first when there are under five
/// minutes left — or when this process has only the stored refresh token to go
/// on, which is every launch after the first.
pub async fn access_token() -> Result<String, String> {
    access_token_at(SUPABASE_URL, &CredentialManager).await
}

/// [`access_token`] against a given host and store, the seam its callers'
/// tests use.
async fn access_token_at(base: &str, store: &dyn RefreshStore) -> Result<String, String> {
    bearer_at(base, store).await.map_err(expired_sentence)
}

/// [`access_token_at`] with the failure still classified, for the one caller
/// that decides something by its kind: [`sign_out_at`].
async fn bearer_at(base: &str, store: &dyn RefreshStore) -> Result<String, Failure> {
    if let Some(token) = live_token() {
        return Ok(token);
    }

    // One refresh at a time, and re-check after the wait: whoever held the
    // lock has very likely just done the work this call was about to repeat.
    let _turn = REFRESHING.lock().await;
    if let Some(token) = live_token() {
        return Ok(token);
    }

    let started = generation();
    adopt_refresh(refresh_at(base, store, started).await, started)
}

/// A *new* access token, whatever life the one in memory has left.
///
/// [`access_token`] hands back what it holds until the five-minute margin,
/// which is right for every caller but one: the relay has just refused a
/// connection with `401`, and presenting the very same bearer a second time
/// can only collect a second `401`. This is that retry's own call — it skips
/// the cache, spends the stored refresh token, and classifies the answer
/// exactly as `access_token` does.
///
/// Takes [`REFRESHING`] like every other refresh, because GoTrue rotates: two
/// dictations that both met a `401` must not race to spend the same token.
pub async fn force_refresh() -> Result<String, String> {
    force_refresh_at(SUPABASE_URL, &CredentialManager).await
}

/// [`force_refresh`] against a given host and store — the same seam
/// [`refresh_at`] exposes, and for the same reason: what this does with a
/// live token, a refusal and a dead socket is coverable without the network.
/// A sign-out whose bearer GoTrue refuses as `bad_jwt` spends it too, for the
/// same reason the relay's retry does (see [`end_session_at`]).
async fn force_refresh_at(base: &str, store: &dyn RefreshStore) -> Result<String, String> {
    let _turn = REFRESHING.lock().await;
    // Deliberately no `live_token()` re-check, and that is the whole
    // difference from [`access_token`]: the token in memory is the one that
    // was just refused, so finding it still inside its margin proves nothing.
    let started = generation();
    adopt_refresh(refresh_at(base, store, started).await, started).map_err(expired_sentence)
}

/// Take what a refresh produced, or say why it failed.
///
/// One function so the entry points cannot classify the same answer
/// differently: a rejection has already cost the stored credential inside
/// [`refresh_at`], and the in-memory half has to go with it or [`status`]
/// keeps reporting a signed-in session from an expired one. Both halves are
/// acted on only while [`GENERATION`] is still `started`, as in `refresh_at`.
fn adopt_refresh(
    outcome: Result<TokenResponse, Failure>,
    started: u64,
) -> Result<String, Failure> {
    match outcome {
        Ok(token) => {
            if !if_current(started, || remember(&token)) {
                return Err(Failure::SignedOut);
            }
            tracing::debug!("cloud sign-in refreshed");
            Ok(token.access_token)
        }
        Err(failure) => {
            if failure == Failure::Rejected {
                if_current(started, || {
                    SESSION.lock().expect("session lock").take();
                });
            }
            Err(failure)
        }
    }
}

/// The sentence for a bearer that could not be had.
fn expired_sentence(failure: Failure) -> String {
    failure.sentence("Your Butterfly Labs sign-in has expired")
}

/// The in-memory access token, if there is one with enough life left.
fn live_token() -> Option<String> {
    let now = Instant::now();
    SESSION
        .lock()
        .expect("session lock")
        .as_ref()
        .filter(|session| !session.needs_refresh(now))
        .map(|session| session.access_token.clone())
}

/// Drop the credentials this machine holds.
///
/// Deliberately leaves `PENDING` alone. That verifier belongs to a browser
/// trip, not to a credential, and the two can overlap: a stale refresh token
/// being rejected in the background while the user is *already* signing in
/// again must not quietly break the sign-in they are in the middle of.
fn forget(store: &dyn RefreshStore) {
    SESSION.lock().expect("session lock").take();
    if let Err(e) = store.forget() {
        tracing::warn!("couldn't remove the sign-in from the credential store: {e:#}");
    }
}

/// What a sign-out did on Supabase's side. The local half, both tokens and a
/// browser trip still out, happens in every case.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub enum SignOut {
    /// Supabase ended this sign-in, or said there was nothing left to end:
    /// the logout's session or user is already gone.
    Ended,
    /// Supabase did not end this sign-in, so it keeps its record of it:
    /// it was not reached, answered without acting (an outage, a rate
    /// limit), or refused this machine's credentials. A refused refresh is
    /// here, not under `Ended`: after `refresh_token_already_used` GoTrue
    /// revokes the session's refresh tokens and keeps the session row. Nothing
    /// on this machine can end that record later: both tokens are gone, and a
    /// later sign-out ends only its own session (`scope=local`, see
    /// [`logout_at`]). Deleting the Cloud account removes it with the account.
    HereOnly,
    /// Nothing was signed in: the call cancelled, at most, a browser trip.
    NotSignedIn,
}

/// Revoke this device's session server-side, then forget it here, and say
/// whether Supabase ended its side.
///
/// The local half happens whatever the network does. A sign-out that leaves a
/// working refresh token on the machine because the user was offline is not a
/// sign-out. What it cannot do offline is end Supabase's record of the
/// session, and the answer says so, so the card can tell the user.
pub async fn sign_out() -> SignOut {
    sign_out_at(SUPABASE_URL, &CredentialManager).await
}

/// [`sign_out`] against a given host and store, so what it reports for a
/// logout that is answered, refused or never arrives is tested against a
/// loopback listener and an in-memory store.
async fn sign_out_at(base: &str, store: &dyn RefreshStore) -> SignOut {
    let outcome = match bearer_at(base, store).await {
        Ok(access) => end_session_at(base, store, &access).await,
        Err(Failure::SignedOut) => SignOut::NotSignedIn,
        // A refused refresh says only that Supabase no longer honours this
        // refresh token, not that the sign-in is gone: after
        // `refresh_token_already_used` GoTrue revokes the session's refresh
        // tokens but keeps the session row, and with no token left there is
        // no bearer to end it with.
        Err(Failure::Rejected | Failure::Unreachable) => SignOut::HereOnly,
    };
    sign_out_here(store);
    outcome
}

/// Send the logout with `access`, and once more with a refreshed bearer if
/// GoTrue says `access` itself is no good.
///
/// `bad_jwt` means GoTrue refused the bearer before it looked for the
/// session: the token in memory had expired (a machine that slept through its
/// hour) or no longer verifies. A refresh gives a bearer for the same session,
/// so one more logout can still end it. One, not a loop: if that does not end
/// the sign-in, nothing more from here will.
async fn end_session_at(base: &str, store: &dyn RefreshStore, access: &str) -> SignOut {
    match logout_at(base, access).await {
        Logout::Ended => SignOut::Ended,
        Logout::Kept => SignOut::HereOnly,
        Logout::BadBearer => {
            let Ok(fresh) = force_refresh_at(base, store).await else {
                return SignOut::HereOnly;
            };
            match logout_at(base, &fresh).await {
                Logout::Ended => SignOut::Ended,
                Logout::BadBearer | Logout::Kept => SignOut::HereOnly,
            }
        }
    }
}

/// The local half of a sign-out: both tokens, and a browser trip still out.
///
/// The browser trip goes here, and only here: an explicit sign-out cancels
/// one, so a callback arriving after it cannot sign the user back in. The
/// count moves on too, so a sign-in exchange or a refresh still out when
/// this runs cannot store what it brings back (see [`GENERATION`]).
fn sign_out_here(store: &dyn RefreshStore) {
    let mut generation = GENERATION.lock().expect("sign-in generation lock");
    *generation += 1;
    forget(store);
    PENDING.lock().expect("pkce verifier lock").take();
}

/// The relay route that deletes the signed-in user's Cloud account.
const RELAY_ACCOUNT_PATH: &str = "/v1/account";

/// How long the delete may take. The relay's worst case is a counter write
/// already queued (up to 3 s when it is a rollover waiting on Supabase), the
/// carry record, then up to 10 s for GoTrue; this covers that with room for
/// the network.
const DELETE_TIMEOUT: Duration = Duration::from_secs(25);

/// Delete the signed-in user's Cloud account, then sign out here.
///
/// The relay empties its copy and deletes the user from Supabase; only its
/// `204` says both are done. GoTrue's logout is not called afterwards: the
/// user it would log out no longer exists.
pub async fn delete_account(relay_base: &str) -> Result<(), String> {
    delete_account_with(relay_base, SUPABASE_URL, &CredentialManager).await
}

/// [`delete_account`] with the token endpoint and the store as arguments. A
/// refresh refused on the way signs the user out here, as it does anywhere.
///
/// A `401` from the relay is what a bearer gone stale looks like (a machine
/// that slept through its hour), and asking again with the same one can only
/// be refused again. So the stored refresh token is spent for a new bearer
/// and the delete asked once more, as a dictation's `401` retry does.
async fn delete_account_with(
    relay_base: &str,
    auth_base: &str,
    store: &dyn RefreshStore,
) -> Result<(), String> {
    let bearer = access_token_at(auth_base, store).await?;
    let first = delete_account_at(relay_base, &bearer, store).await;
    if first != Err(DeleteFailure::Unauthorized) {
        return first.map_err(|failure| failure.sentence());
    }
    let fresh = force_refresh_at(auth_base, store).await?;
    delete_account_at(relay_base, &fresh, store)
        .await
        .map_err(|failure| failure.sentence())
}

/// Why the relay did not confirm a delete.
#[derive(Debug, PartialEq, Eq)]
enum DeleteFailure {
    /// The relay was not reached.
    Unreachable,
    /// The relay refused the bearer (`401`).
    Unauthorized,
    /// Any other answer but `204`.
    Unfinished,
}

impl DeleteFailure {
    fn sentence(&self) -> String {
        match self {
            DeleteFailure::Unreachable => {
                "Couldn't reach Butterfly Labs — check your internet connection"
            }
            DeleteFailure::Unauthorized => "Sign in again, then delete",
            DeleteFailure::Unfinished => "Couldn't finish deleting your account — try again",
        }
        .to_string()
    }
}

/// One delete request with `bearer`, and the local sign-out its `204`
/// calls for. The bearer and the store are arguments so it can be tested
/// against a loopback listener and an in-memory store.
async fn delete_account_at(
    base: &str,
    bearer: &str,
    store: &dyn RefreshStore,
) -> Result<(), DeleteFailure> {
    let response = http()
        .delete(format!("{base}{RELAY_ACCOUNT_PATH}"))
        .bearer_auth(bearer)
        .timeout(DELETE_TIMEOUT)
        .send()
        .await
        .map_err(|e| {
            tracing::debug!("cloud account delete failed ({})", transport_class(&e));
            DeleteFailure::Unreachable
        })?;
    let status = response.status().as_u16();
    if status == 204 {
        sign_out_here(store);
        tracing::debug!("cloud account deleted");
        return Ok(());
    }
    tracing::debug!("cloud account delete failed (status {status})");
    Err(if status == 401 {
        DeleteFailure::Unauthorized
    } else {
        DeleteFailure::Unfinished
    })
}

/// `POST {base}/auth/v1/logout?scope=local` with `access` as the bearer.
///
/// `scope=local` is the whole point: GoTrue's logout defaults to `global`,
/// which revokes every refresh token the user holds, so "Sign out" on one
/// machine would quietly sign them out of all the others too. `local` ends
/// the one session this bearer belongs to.
///
/// `base` is a parameter for the reason [`exchange_at`]'s is: the tests point
/// it at a loopback listener and read the request line it sends.
///
/// A 2xx ended the session. A refusal says what it means in its body, not
/// its status, because GoTrue answers `403` for opposite things:
/// `session_not_found` or `user_not_found` when the session or the user is
/// already gone, so nothing is left to end, and `bad_jwt` when it refused the
/// bearer without looking for the session (see [`end_session_at`]). Anything
/// else, and a request that never arrives, leaves Supabase's record where it
/// was.
async fn logout_at(base: &str, access: &str) -> Logout {
    let sent = http()
        .post(format!("{base}/auth/v1/logout?scope=local"))
        .header("apikey", SUPABASE_ANON_KEY)
        .bearer_auth(access)
        .send()
        .await;
    let response = match sent {
        Ok(response) => response,
        Err(e) => {
            tracing::debug!("cloud sign-out failed ({})", transport_class(&e));
            return Logout::Kept;
        }
    };
    let status = response.status();
    let outcome = if status.is_success() {
        Logout::Ended
    } else {
        match error_code(response).await.as_deref() {
            Some("session_not_found" | "user_not_found") => Logout::Ended,
            Some("bad_jwt") => Logout::BadBearer,
            _ => Logout::Kept,
        }
    };
    // Which way it went in this module's own words, never the body: the log
    // rule allows the step and the status.
    tracing::debug!(
        "cloud sign-out (status {}, {})",
        status.as_u16(),
        outcome.word()
    );
    outcome
}

/// What one logout request did to Supabase's record of the session.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Logout {
    /// Ended, or already gone.
    Ended,
    /// GoTrue refused the bearer itself (`bad_jwt`) and did not look for
    /// the session.
    BadBearer,
    /// Anything else: the record stays.
    Kept,
}

impl Logout {
    fn word(self) -> &'static str {
        match self {
            Logout::Ended => "ended",
            Logout::BadBearer => "bearer refused",
            Logout::Kept => "not ended",
        }
    }
}

/// The `error_code` in a GoTrue error body, if there is one.
///
/// GoTrue puts it there for a client that sends no `X-Supabase-Api-Version`
/// header, which this one never does: `{"code":403,"error_code":"bad_jwt",
/// "msg":"…"}`. A body that is empty, is not JSON or has no such field gives
/// `None`, which every caller treats as "not proven".
async fn error_code(response: reqwest::Response) -> Option<String> {
    #[derive(Deserialize)]
    struct ErrorBody {
        error_code: Option<String>,
    }
    response
        .json::<ErrorBody>()
        .await
        .ok()
        .and_then(|body| body.error_code)
}

/// Who is signed in, as far as this process can tell without asking the
/// network. A stored refresh token counts: it is what a restart wakes up with.
pub fn status() -> CloudStatus {
    let session = SESSION
        .lock()
        .expect("session lock")
        .as_ref()
        .map(|session| session.user_email.clone());
    CloudStatus {
        signed_in: session.is_some() || CredentialManager.load().is_some(),
        email: session.flatten(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::pkce::challenge;

    #[test]
    fn the_authorize_url_carries_provider_redirect_and_challenge() {
        let u = authorize_url("ver");
        assert!(u.starts_with("https://iassqjnfvdffocyxptis.supabase.co/auth/v1/authorize?"));
        assert!(u.contains("provider=google"));
        assert!(u.contains("redirect_to=butterflylabs%3A%2F%2Fauth%2Fcallback"));
        assert!(u.contains("code_challenge_method=s256"));
        assert!(u.contains(&format!("code_challenge={}", challenge("ver"))));
    }

    #[test]
    fn a_callback_url_yields_its_code_and_rejects_other_urls() {
        assert_eq!(
            code_from_callback("butterflylabs://auth/callback?code=abc-123").as_deref(),
            Some("abc-123")
        );
        assert_eq!(code_from_callback("butterflylabs://other?code=x"), None);
        assert_eq!(code_from_callback("https://evil/?code=x"), None);
    }

    #[test]
    fn a_token_response_parses_and_needs_refresh_inside_five_minutes() {
        let t: TokenResponse = serde_json::from_str(
            r#"{"access_token":"a","token_type":"bearer","expires_in":3600,"refresh_token":"r","user":{"email":"x@y.z"}}"#,
        )
        .unwrap();
        let s = AuthSession::from_response(&t, Instant::now());
        assert!(!s.needs_refresh(Instant::now()));
        assert!(s.needs_refresh(Instant::now() + Duration::from_secs(3600 - 200)));
    }

    /// A stray `butterflylabs://` launch must not be mistaken for a sign-in,
    /// however much it looks like one. The scheme is a public doorbell: any
    /// page on the web can ring it.
    #[test]
    fn a_callback_shaped_url_on_the_wrong_path_or_scheme_is_refused() {
        // Right scheme, wrong path — a future in-app link, say.
        assert_eq!(code_from_callback("butterflylabs://auth/other?code=x"), None);
        // Right path, wrong scheme.
        assert_eq!(code_from_callback("butterflylab://auth/callback?code=x"), None);
        // The callback with no code at all (GoTrue sends `error=` here when
        // the user declines the consent screen).
        assert_eq!(
            code_from_callback("butterflylabs://auth/callback?error=access_denied"),
            None
        );
        assert_eq!(code_from_callback("butterflylabs://auth/callback?code="), None);
        assert_eq!(code_from_callback("butterflylabs://auth/callback"), None);
    }

    /// The exchange body is what the live endpoint was measured to
    /// accept — `auth_code`, not `code`, and `code_verifier` beside it. A
    /// rename on either side fails as a 404 `flow_state_not_found`, which
    /// reads exactly like an expired flow.
    #[test]
    fn the_pkce_grant_body_names_the_fields_gotrue_reads() {
        let body = pkce_grant_body("c", "v");
        assert_eq!(body["auth_code"], "c");
        assert_eq!(body["code_verifier"], "v");
        assert_eq!(
            serde_json::to_string(&body).unwrap(),
            r#"{"auth_code":"c","code_verifier":"v"}"#
        );
    }

    /// A user who closes Google's consent screen comes back on the same
    /// callback with `error=` and no code. That is not "a stray deep link" —
    /// it is this sign-in, ending — and the card waiting on it has to be told.
    #[test]
    fn a_declined_consent_is_recognised_as_this_callback_ending() {
        assert!(callback_declined(
            "butterflylabs://auth/callback?error=access_denied&error_description=x"
        ));
        assert!(callback_declined("butterflylabs://auth/callback/?error=server_error"));
        // A callback that worked is not a decline.
        assert!(!callback_declined("butterflylabs://auth/callback?code=abc"));
        // Nor is anything else on the scheme, however it is decorated.
        assert!(!callback_declined("butterflylabs://note/42?error=access_denied"));
        assert!(!callback_declined("https://evil/?error=access_denied"));
        assert!(!callback_declined("butterflylabs://auth/callback"));
    }

    // -- the token endpoint ---------------------------------------------------

    /// A one-shot loopback HTTP server, the same raw-socket approach
    /// `endpoint::probe`'s tests use and for the same reason: no new
    /// dependency, and `tokio`'s `io-util` feature is off in this crate.
    async fn stub(code: u16, body: &'static str) -> std::net::SocketAddr {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind loopback listener");
        let addr = listener.local_addr().expect("local addr");
        tokio::spawn(async move {
            let Ok((socket, _)) = listener.accept().await else {
                return;
            };
            // Drain the request first: closing a socket with unread inbound
            // data queued makes Windows send an RST instead of a FIN, which
            // hyper reports as "connection aborted" — masking the status the
            // test is about.
            let mut buf = [0u8; 8192];
            socket.readable().await.expect("socket readable");
            match socket.try_read(&mut buf) {
                Ok(_) | Err(_) => {}
            }
            reply(&socket, code, body).await;
        });
        addr
    }

    /// Write a whole response to `socket`.
    async fn reply(socket: &tokio::net::TcpStream, code: u16, body: &str) {
        let response = format!(
            "HTTP/1.1 {code} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let mut bytes = response.as_bytes();
        while !bytes.is_empty() {
            socket.writable().await.expect("socket writable");
            match socket.try_write(bytes) {
                Ok(n) => bytes = &bytes[n..],
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => continue,
                Err(e) => panic!("stub write failed: {e}"),
            }
        }
    }

    /// [`stub`] for a request with no body, which also hands back the
    /// request's head — for a test about what was asked rather than what the
    /// answer did. Reads until the blank line that ends the headers, so the
    /// whole request is drained before the reply (see `stub` for why that
    /// matters on Windows).
    async fn recording_stub(
        code: u16,
    ) -> (std::net::SocketAddr, tokio::sync::oneshot::Receiver<String>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind loopback listener");
        let addr = listener.local_addr().expect("local addr");
        let (seen, heard) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let Ok((socket, _)) = listener.accept().await else {
                return;
            };
            let mut buf = [0u8; 8192];
            let mut len = 0;
            while len < buf.len() && !buf[..len].windows(4).any(|w| w == b"\r\n\r\n") {
                socket.readable().await.expect("socket readable");
                match socket.try_read(&mut buf[len..]) {
                    Ok(0) => break,
                    Ok(n) => len += n,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => continue,
                    Err(_) => break,
                }
            }
            let _ = seen.send(String::from_utf8_lossy(&buf[..len]).into_owned());
            reply(&socket, code, "").await;
        });
        (addr, heard)
    }

    /// A loopback server that answers one request per connection with the
    /// next reply in `replies`, and hands back each request whole — head and
    /// body — in the order they came. For a test about a sequence of calls to
    /// the same host: a logout, the refresh it forces, and the logout again.
    ///
    /// Every reply closes its connection, so each request arrives on a new
    /// one. By the time the code under test returns, every request it made is
    /// already in the channel: the stub sends it before it replies.
    async fn scripted_stub(
        replies: Vec<(u16, &'static str)>,
    ) -> (
        std::net::SocketAddr,
        tokio::sync::mpsc::UnboundedReceiver<String>,
    ) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind loopback listener");
        let addr = listener.local_addr().expect("local addr");
        let (seen, heard) = tokio::sync::mpsc::unbounded_channel();
        tokio::spawn(async move {
            for (code, body) in replies {
                let Ok((socket, _)) = listener.accept().await else {
                    return;
                };
                let _ = seen.send(read_request(&socket).await);
                reply(&socket, code, body).await;
            }
        });
        (addr, heard)
    }

    /// One whole request off `socket`: the head, then as many body bytes as
    /// its `Content-Length` names, so nothing is left unread when the reply
    /// closes the socket (see [`stub`] for why that matters on Windows).
    async fn read_request(socket: &tokio::net::TcpStream) -> String {
        let mut buf = Vec::new();
        let mut chunk = [0u8; 4096];
        loop {
            if let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                let head = String::from_utf8_lossy(&buf[..end]).to_ascii_lowercase();
                let length = head
                    .lines()
                    .find_map(|line| line.strip_prefix("content-length:"))
                    .and_then(|value| value.trim().parse::<usize>().ok())
                    .unwrap_or(0);
                if buf.len() >= end + 4 + length {
                    break;
                }
            }
            socket.readable().await.expect("socket readable");
            match socket.try_read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => buf.extend_from_slice(&chunk[..n]),
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => continue,
                Err(_) => break,
            }
        }
        String::from_utf8_lossy(&buf).into_owned()
    }

    /// A loopback server that answers one request with `code` and `body`, but
    /// only once `release` fires; `arrived` fires when the whole request is
    /// in. For a test about what happens while a token call is still out.
    async fn held_stub(
        code: u16,
        body: &'static str,
    ) -> (
        std::net::SocketAddr,
        tokio::sync::oneshot::Receiver<()>,
        tokio::sync::oneshot::Sender<()>,
    ) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind loopback listener");
        let addr = listener.local_addr().expect("local addr");
        let (arrived_tx, arrived) = tokio::sync::oneshot::channel();
        let (release, released) = tokio::sync::oneshot::channel::<()>();
        tokio::spawn(async move {
            let Ok((socket, _)) = listener.accept().await else {
                return;
            };
            read_request(&socket).await;
            let _ = arrived_tx.send(());
            let _ = released.await;
            reply(&socket, code, body).await;
        });
        (addr, arrived, release)
    }

    /// Everything a [`scripted_stub`] has heard so far.
    fn requests(heard: &mut tokio::sync::mpsc::UnboundedReceiver<String>) -> Vec<String> {
        let mut all = Vec::new();
        while let Ok(request) = heard.try_recv() {
            all.push(request);
        }
        all
    }

    /// The first line of each request: what was asked, in order.
    fn request_lines(requests: &[String]) -> Vec<&str> {
        requests
            .iter()
            .map(|request| request.lines().next().unwrap_or_default())
            .collect()
    }

    /// A port with nothing behind it: the socket is bound but never listens,
    /// so a connect is refused — the offline case, without waiting out a
    /// timeout. Keep the socket for the whole test: a port released early
    /// could be handed to another test's listener, which would then answer.
    fn nothing_listening() -> (tokio::net::TcpSocket, std::net::SocketAddr) {
        let socket = tokio::net::TcpSocket::new_v4().expect("create socket");
        socket
            .bind("127.0.0.1:0".parse().expect("loopback address"))
            .expect("bind loopback socket");
        let addr = socket.local_addr().expect("local addr");
        (socket, addr)
    }

    /// The credential slot, in memory.
    ///
    /// The real slot is Windows Credential Manager, and on the machine these
    /// tests run on it holds the developer's own sign-in — a test that wrote
    /// to it would sign them out.
    #[derive(Default)]
    struct FakeStore(Mutex<Option<String>>);

    impl FakeStore {
        fn holding(token: &str) -> Self {
            Self(Mutex::new(Some(token.to_string())))
        }
        fn held(&self) -> Option<String> {
            self.0.lock().expect("fake store").clone()
        }
    }

    impl RefreshStore for FakeStore {
        fn load(&self) -> Option<String> {
            self.held()
        }
        fn save(&self, token: &str) -> anyhow::Result<()> {
            *self.0.lock().expect("fake store") = Some(token.to_string());
            Ok(())
        }
        fn forget(&self) -> anyhow::Result<()> {
            self.0.lock().expect("fake store").take();
            Ok(())
        }
    }

    /// A refresh the service *refuses* is a dead credential. Keeping it would
    /// make the Cloud card claim a sign-in that can never be honoured, and
    /// every later call would spend a round trip rediscovering that.
    #[tokio::test]
    async fn a_rejected_refresh_deletes_the_stored_token() {
        let _session = ScopedSession::empty();
        let addr = stub(400, r#"{"code":400,"error_code":"validation_failed"}"#).await;
        let store = FakeStore::holding("dead-refresh-token");
        let failure = refresh_at(&format!("http://{addr}"), &store, generation())
            .await
            .err()
            .expect("a 400 from the token endpoint is a failure");
        assert_eq!(failure, Failure::Rejected);
        assert_eq!(store.held(), None, "a refused credential must not be kept");
    }

    /// The four statuses that mean the grant itself is no good. Each one is
    /// worth its own round trip here, because getting any of them wrong costs
    /// a user their sign-in.
    #[tokio::test]
    async fn every_refusing_status_deletes_the_stored_token() {
        let _session = ScopedSession::empty();
        for status in [401, 403, 404] {
            let addr = stub(status, r#"{"error_code":"bad_jwt"}"#).await;
            let store = FakeStore::holding("dead-refresh-token");
            let failure = refresh_at(&format!("http://{addr}"), &store, generation())
                .await
                .err()
                .unwrap_or_else(|| panic!("a {status} from the token endpoint is a failure"));
            assert_eq!(failure, Failure::Rejected, "status {status}");
            assert_eq!(store.held(), None, "status {status} must not keep the token");
        }
    }

    /// A Supabase incident is not a revoked sign-in. Deleting the refresh
    /// token on a 5xx or a 429 would turn one bad hour at the service into a
    /// fresh Google consent screen for every user who happened to refresh
    /// during it — and they would have no way to tell why.
    #[tokio::test]
    async fn an_outage_or_a_rate_limit_keeps_the_stored_token() {
        for status in [429, 500, 502, 503] {
            let addr = stub(status, r#"{"msg":"service unavailable"}"#).await;
            let store = FakeStore::holding("still-good-token");
            let failure = refresh_at(&format!("http://{addr}"), &store, generation())
                .await
                .err()
                .unwrap_or_else(|| panic!("a {status} from the token endpoint is a failure"));
            assert_eq!(failure, Failure::Unreachable, "status {status}");
            assert_eq!(
                store.held().as_deref(),
                Some("still-good-token"),
                "status {status} says nothing about whether the credential is still good"
            );
        }
    }

    /// The other half of that split, and the one that matters on a train: a
    /// refresh that never reached the service says nothing about whether the
    /// credential is still good, so it has to survive.
    #[tokio::test]
    async fn an_unreachable_token_endpoint_keeps_the_stored_token() {
        let (_socket, addr) = nothing_listening();
        let store = FakeStore::holding("still-good-token");
        let failure = refresh_at(&format!("http://{addr}"), &store, generation())
            .await
            .err()
            .expect("a refused connection is a failure");
        assert_eq!(failure, Failure::Unreachable);
        assert_eq!(
            store.held().as_deref(),
            Some("still-good-token"),
            "offline is not signed out"
        );
    }

    /// GoTrue rotates on every refresh: the token just spent is retired
    /// server-side the moment it answers. Storing the *old* one back would
    /// trip its reuse detection on the next launch and sign the user out.
    #[tokio::test]
    async fn a_successful_refresh_stores_the_rotated_token() {
        let _session = ScopedSession::empty();
        let addr = stub(
            200,
            r#"{"access_token":"fresh","token_type":"bearer","expires_in":3600,"refresh_token":"rotated","user":{"email":"x@y.z"}}"#,
        )
        .await;
        let store = FakeStore::holding("spent-token");
        let token = refresh_at(&format!("http://{addr}"), &store, generation())
            .await
            .ok()
            .expect("a 2xx with a token body is a session");
        assert_eq!(token.access_token, "fresh");
        assert_eq!(
            store.held().as_deref(),
            Some("rotated"),
            "the rotated token has to replace the one it retired"
        );
    }

    // -- the forced refresh ---------------------------------------------------
    //
    // `force_refresh` is what the relay's `401` retry spends. All three tests
    // go through the `RefreshStore` seam and a loopback listener, so the
    // machine's real credential slot — which on any machine this is built on
    // holds the developer's own sign-in — is never touched.

    /// Serialises every test that reads or moves the process-wide sign-in
    /// state: [`SESSION`], [`PENDING`] and [`GENERATION`]. Cargo runs tests
    /// on several threads, and without this one test's sign-out is another's
    /// dropped refresh.
    static SESSION_TESTS: Mutex<()> = Mutex::new(());

    /// Put a session in memory and take it out again whatever the test did,
    /// so one of these cannot leave a token behind for the next.
    struct ScopedSession(#[allow(dead_code)] std::sync::MutexGuard<'static, ()>);

    impl ScopedSession {
        fn holding(access: &str) -> Self {
            // A panicking test must not take the rest of them down with a
            // poisoned lock: what this guards is reset by `Drop` either way.
            let turn = SESSION_TESTS.lock().unwrap_or_else(|e| e.into_inner());
            remember(&TokenResponse {
                access_token: access.to_string(),
                expires_in: 3600,
                refresh_token: "in-memory".into(),
                user: TokenUser {
                    email: Some("x@y.z".into()),
                },
            });
            ScopedSession(turn)
        }

        /// No session in memory, for a test that needs the refresh path.
        fn empty() -> Self {
            let turn = SESSION_TESTS.lock().unwrap_or_else(|e| e.into_inner());
            SESSION.lock().expect("session lock").take();
            ScopedSession(turn)
        }
    }

    impl Drop for ScopedSession {
        fn drop(&mut self) {
            SESSION.lock().expect("session lock").take();
        }
    }

    /// The whole reason this function exists. `access_token` would hand back
    /// the hour-long token it already holds — which, after a `401`, is the
    /// one the relay has just refused. A forced refresh has to spend the
    /// stored credential and come back with a different bearer.
    #[tokio::test]
    async fn a_forced_refresh_ignores_the_live_token_and_rotates() {
        let _session = ScopedSession::holding("the-refused-token");
        let addr = stub(
            200,
            r#"{"access_token":"fresh","token_type":"bearer","expires_in":3600,"refresh_token":"rotated","user":{"email":"x@y.z"}}"#,
        )
        .await;
        let store = FakeStore::holding("spent-token");
        let access = force_refresh_at(&format!("http://{addr}"), &store)
            .await
            .expect("a 2xx with a token body is a session");
        assert_eq!(
            access, "fresh",
            "a forced refresh must never hand back the token that was refused"
        );
        assert_eq!(
            store.held().as_deref(),
            Some("rotated"),
            "the rotated token has to replace the one it retired"
        );
        assert_eq!(
            live_token().as_deref(),
            Some("fresh"),
            "the new token is what the rest of this dictation authenticates with"
        );
    }

    /// Same classification as `access_token`: a refusal is a dead sign-in, so
    /// both halves go — the stored refresh token (inside `refresh_at`) and
    /// the session in memory, which would otherwise keep the Cloud card
    /// claiming a sign-in that can never be honoured.
    #[tokio::test]
    async fn a_forced_refresh_that_is_refused_clears_both_halves() {
        let _session = ScopedSession::holding("the-refused-token");
        let addr = stub(401, r#"{"error_code":"bad_jwt"}"#).await;
        let store = FakeStore::holding("dead-refresh-token");
        let sentence = force_refresh_at(&format!("http://{addr}"), &store)
            .await
            .expect_err("a 401 from the token endpoint is a failure");
        assert_eq!(
            sentence,
            "Your Butterfly Labs sign-in has expired — sign in again"
        );
        assert_eq!(store.held(), None, "a refused credential must not be kept");
        assert!(
            SESSION.lock().expect("session lock").is_none(),
            "the in-memory half has to go with the stored one"
        );
    }

    /// And the other half of that split: a refresh that never reached the
    /// service says nothing about the credential, so the sign-in survives and
    /// the sentence is about the network rather than about signing in again.
    #[tokio::test]
    async fn a_forced_refresh_that_cannot_reach_the_service_keeps_the_sign_in() {
        let _session = ScopedSession::holding("the-refused-token");
        let (_socket, addr) = nothing_listening();
        let store = FakeStore::holding("still-good-token");
        let sentence = force_refresh_at(&format!("http://{addr}"), &store)
            .await
            .expect_err("a refused connection is a failure");
        assert_eq!(
            sentence,
            "Couldn't reach Butterfly Labs — check your internet connection"
        );
        assert_eq!(
            store.held().as_deref(),
            Some("still-good-token"),
            "offline is not signed out"
        );
    }

    /// Nothing stored is its own answer, not a dead credential: no request is
    /// sent, and there is nothing to delete.
    #[tokio::test]
    async fn nothing_stored_is_signed_out_without_a_round_trip() {
        let store = FakeStore::default();
        let failure = refresh_at("http://127.0.0.1:1", &store, generation())
            .await
            .err()
            .expect("no stored token is a failure");
        assert_eq!(failure, Failure::SignedOut);
    }

    /// The reason there is a timeout at all. `access_token` holds
    /// [`REFRESHING`] across this call, so a host that accepts and then says
    /// nothing would park every later caller — the relay's per-connection
    /// bearer included — for as long as the socket stayed open.
    #[tokio::test]
    async fn a_host_that_never_answers_fails_instead_of_parking_the_refresh() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind loopback listener");
        let addr = listener.local_addr().expect("local addr");
        // Accept and hold: never answer, never close.
        tokio::spawn(async move {
            let held = listener.accept().await;
            std::future::pending::<()>().await;
            drop(held);
        });

        let outcome = tokio::time::timeout(
            TOKEN_TIMEOUT + Duration::from_secs(5),
            exchange_at(
                &format!("http://{addr}"),
                "refresh_token",
                serde_json::json!({ "refresh_token": "x" }),
            ),
        )
        .await;
        match outcome {
            Err(_) => panic!(
                "the token call never returned; the {TOKEN_TIMEOUT:?} budget is not being enforced"
            ),
            Ok(Ok(_)) => panic!("a host that says nothing cannot produce a token"),
            Ok(Err(Failure::Unreachable)) => {}
            Ok(Err(_)) => panic!("a timed-out call is a transport failure, never a rejection"),
        }
    }

    // -- sign-out -------------------------------------------------------------

    /// Signing out here signs out *here*. GoTrue's logout defaults to
    /// `scope=global`, which revokes every refresh token the user holds — a
    /// click on the laptop would silently end the sign-in on their desktop.
    #[tokio::test]
    async fn signing_out_revokes_only_this_devices_session() {
        let (addr, heard) = recording_stub(204).await;
        let outcome = logout_at(&format!("http://{addr}"), "this-devices-token").await;
        assert_eq!(outcome, Logout::Ended);
        let request = heard.await.expect("the logout reached the listener");
        let request_line = request.lines().next().unwrap_or_default();
        assert_eq!(
            request_line, "POST /auth/v1/logout?scope=local HTTP/1.1",
            "sign-out must name the local scope, or GoTrue revokes every device"
        );
        assert!(
            request
                .to_ascii_lowercase()
                .contains("authorization: bearer this-devices-token"),
            "the session revoked is the one this bearer belongs to"
        );
    }

    /// Put an access token in memory inside a test that already holds
    /// [`SESSION_TESTS`] through a [`ScopedSession`].
    fn hold_access(access: &str) {
        remember(&TokenResponse {
            access_token: access.to_string(),
            expires_in: 3600,
            refresh_token: "in-memory".into(),
            user: TokenUser { email: None },
        });
    }

    /// A sign-out Supabase answers ends the sign-in there, and this machine
    /// forgets both tokens and a browser trip still out.
    #[tokio::test]
    async fn a_sign_out_that_reaches_supabase_ends_the_sign_in_there() {
        let _session = ScopedSession::holding("this-devices-token");
        *PENDING.lock().expect("pkce verifier lock") = Some("a-verifier".into());
        let (addr, heard) = recording_stub(204).await;
        let store = FakeStore::holding("the-refresh-token");

        let outcome = sign_out_at(&format!("http://{addr}"), &store).await;

        assert_eq!(outcome, SignOut::Ended);
        let request = heard.await.expect("the logout reached the listener");
        assert_eq!(
            request.lines().next().unwrap_or_default(),
            "POST /auth/v1/logout?scope=local HTTP/1.1"
        );
        assert_eq!(store.held(), None, "the refresh token goes");
        assert!(
            SESSION.lock().expect("session lock").is_none(),
            "and the access token"
        );
        assert!(
            PENDING.lock().expect("pkce verifier lock").is_none(),
            "and a sign-in still out at the browser"
        );
    }

    /// Offline, the sign-out still happens on this computer, and it says that
    /// it happened only here: Supabase never heard it, so it keeps its record
    /// of this sign-in.
    #[tokio::test]
    async fn a_sign_out_that_cannot_reach_supabase_signs_out_here_only() {
        let _session = ScopedSession::holding("this-devices-token");
        let (_socket, addr) = nothing_listening();
        let store = FakeStore::holding("the-refresh-token");

        let outcome = sign_out_at(&format!("http://{addr}"), &store).await;

        assert_eq!(outcome, SignOut::HereOnly);
        assert_eq!(store.held(), None, "offline is still signed out here");
        assert!(SESSION.lock().expect("session lock").is_none());
    }

    /// The first sign-out after a launch has no access token in memory and
    /// has to refresh for one. Offline, that refresh cannot reach Supabase
    /// either, so the logout is never sent.
    #[tokio::test]
    async fn a_sign_out_whose_refresh_cannot_reach_supabase_signs_out_here_only() {
        let _session = ScopedSession::empty();
        let (_socket, addr) = nothing_listening();
        let store = FakeStore::holding("the-refresh-token");

        let outcome = sign_out_at(&format!("http://{addr}"), &store).await;

        assert_eq!(outcome, SignOut::HereOnly);
        assert_eq!(store.held(), None, "offline is still signed out here");
    }

    /// Supabase answering is not the same as Supabase acting: a rate limit or
    /// an outage leaves its record of the sign-in where it was.
    #[tokio::test]
    async fn a_logout_supabase_does_not_carry_out_signs_out_here_only() {
        let _session = ScopedSession::empty();
        for status in [429, 500, 503] {
            hold_access("this-devices-token");
            let (addr, _heard) = recording_stub(status).await;
            let store = FakeStore::holding("the-refresh-token");
            let outcome = sign_out_at(&format!("http://{addr}"), &store).await;
            assert_eq!(outcome, SignOut::HereOnly, "status {status}");
            assert_eq!(store.held(), None, "status {status}");
        }
    }

    /// GoTrue refuses a logout whose session or user is already gone
    /// (`403 session_not_found`, `user_not_found`): nothing is left for a
    /// sign-out to end, so there is nothing to warn about.
    #[tokio::test]
    async fn a_logout_refused_because_the_session_is_gone_has_nothing_left_to_end() {
        let _session = ScopedSession::empty();
        for body in [
            r#"{"code":403,"error_code":"session_not_found","msg":"Session from session_id claim in JWT does not exist"}"#,
            r#"{"code":403,"error_code":"user_not_found","msg":"User from sub claim in JWT does not exist"}"#,
        ] {
            hold_access("this-devices-token");
            let (addr, mut heard) = scripted_stub(vec![(403, body)]).await;
            let store = FakeStore::holding("the-refresh-token");
            let outcome = sign_out_at(&format!("http://{addr}"), &store).await;
            assert_eq!(outcome, SignOut::Ended, "{body}");
            assert_eq!(requests(&mut heard).len(), 1, "no retry for {body}");
            assert_eq!(store.held(), None, "{body}");
        }
    }

    /// A refusal is not proof the session is gone. Only the two error codes
    /// above say that; any other refusal, including one whose body has no
    /// `error_code` (an empty body, or a proxy's), leaves Supabase's record
    /// where it was.
    #[tokio::test]
    async fn a_logout_refused_for_any_other_reason_signs_out_here_only() {
        let _session = ScopedSession::empty();
        for (status, body) in [
            (
                401,
                r#"{"code":401,"error_code":"no_authorization","msg":"This endpoint requires a Bearer token"}"#,
            ),
            (
                400,
                r#"{"code":400,"error_code":"validation_failed","msg":"x"}"#,
            ),
            (403, ""),
            (404, r#"{"message":"no Route matched with those values"}"#),
        ] {
            hold_access("this-devices-token");
            let (addr, mut heard) = scripted_stub(vec![(status, body)]).await;
            let store = FakeStore::holding("the-refresh-token");
            let outcome = sign_out_at(&format!("http://{addr}"), &store).await;
            assert_eq!(outcome, SignOut::HereOnly, "{status} {body}");
            assert_eq!(
                requests(&mut heard).len(),
                1,
                "no retry for {status} {body}"
            );
            assert_eq!(store.held(), None, "{status} {body}");
        }
    }

    /// The token endpoint's answer to a spent access token or refresh token.
    const FRESH_TOKEN: &str = r#"{"access_token":"fresh","token_type":"bearer","expires_in":3600,"refresh_token":"rotated","user":{"email":"x@y.z"}}"#;

    /// GoTrue's `403` for a bearer that is expired or no longer verifies.
    const BAD_JWT: &str = r#"{"code":403,"error_code":"bad_jwt","msg":"invalid JWT: unable to parse or verify signature, token has invalid claims: token is expired"}"#;

    /// `bad_jwt` means GoTrue never looked for the session: the bearer in
    /// memory had expired (a machine that slept through its hour, say). A
    /// refreshed bearer belongs to the same session, so the logout is sent
    /// once more with it, and that one ends the sign-in.
    #[tokio::test]
    async fn a_logout_whose_bearer_has_expired_refreshes_and_ends_the_sign_in() {
        let _session = ScopedSession::holding("the-expired-token");
        *PENDING.lock().expect("pkce verifier lock") = Some("a-verifier".into());
        let (addr, mut heard) =
            scripted_stub(vec![(403, BAD_JWT), (200, FRESH_TOKEN), (204, "")]).await;
        let store = FakeStore::holding("the-refresh-token");

        let outcome = sign_out_at(&format!("http://{addr}"), &store).await;

        assert_eq!(outcome, SignOut::Ended);
        let requests = requests(&mut heard);
        assert_eq!(
            request_lines(&requests),
            [
                "POST /auth/v1/logout?scope=local HTTP/1.1",
                "POST /auth/v1/token?grant_type=refresh_token HTTP/1.1",
                "POST /auth/v1/logout?scope=local HTTP/1.1",
            ]
        );
        assert!(
            requests[0]
                .to_ascii_lowercase()
                .contains("authorization: bearer the-expired-token"),
            "{}",
            requests[0]
        );
        assert!(
            requests[1].contains("the-refresh-token"),
            "the refresh spends the stored token: {}",
            requests[1]
        );
        assert!(
            requests[2]
                .to_ascii_lowercase()
                .contains("authorization: bearer fresh"),
            "the retry carries the refreshed bearer: {}",
            requests[2]
        );
        assert_eq!(store.held(), None, "the rotated refresh token goes too");
        assert!(SESSION.lock().expect("session lock").is_none());
        assert!(PENDING.lock().expect("pkce verifier lock").is_none());
    }

    /// One retry, not a loop, and a retry that does not end the sign-in is
    /// a sign-out on this computer only: the second logout refused or
    /// unanswered, or the refresh for it refused or unanswered.
    #[tokio::test]
    async fn a_logout_whose_bearer_has_expired_and_whose_retry_fails_signs_out_here_only() {
        let _session = ScopedSession::empty();
        for script in [
            vec![(403, BAD_JWT), (200, FRESH_TOKEN), (403, BAD_JWT)],
            vec![(403, BAD_JWT), (200, FRESH_TOKEN), (503, "")],
            vec![
                (403, BAD_JWT),
                (
                    400,
                    r#"{"code":400,"error_code":"refresh_token_already_used","msg":"Invalid Refresh Token: Already Used"}"#,
                ),
            ],
            vec![(403, BAD_JWT), (503, "")],
        ] {
            let sent = script.len();
            hold_access("the-expired-token");
            *PENDING.lock().expect("pkce verifier lock") = Some("a-verifier".into());
            let (addr, mut heard) = scripted_stub(script).await;
            let store = FakeStore::holding("the-refresh-token");

            let outcome = sign_out_at(&format!("http://{addr}"), &store).await;

            assert_eq!(outcome, SignOut::HereOnly, "after {sent} requests");
            assert_eq!(requests(&mut heard).len(), sent, "one retry at most");
            assert_eq!(store.held(), None, "after {sent} requests");
            assert!(SESSION.lock().expect("session lock").is_none());
            assert!(PENDING.lock().expect("pkce verifier lock").is_none());
        }
    }

    /// A refused refresh is not an ended sign-in. After
    /// `refresh_token_already_used` GoTrue revokes every refresh token of the
    /// session but keeps the session row, and with no token left nothing on
    /// this machine can end it: the sign-out happens here only.
    #[tokio::test]
    async fn a_sign_out_whose_refresh_is_refused_signs_out_here_only() {
        let _session = ScopedSession::empty();
        *PENDING.lock().expect("pkce verifier lock") = Some("a-verifier".into());
        let addr = stub(
            400,
            r#"{"code":400,"error_code":"refresh_token_already_used","msg":"Invalid Refresh Token: Already Used"}"#,
        )
        .await;
        let store = FakeStore::holding("the-spent-refresh-token");

        let outcome = sign_out_at(&format!("http://{addr}"), &store).await;

        assert_eq!(outcome, SignOut::HereOnly);
        assert_eq!(store.held(), None, "the refused token is gone");
        assert!(SESSION.lock().expect("session lock").is_none());
        assert!(PENDING.lock().expect("pkce verifier lock").is_none());
    }

    /// Cancelling a browser trip is this same call with nothing stored: no
    /// request, nothing to warn about, and the trip is cancelled.
    #[tokio::test]
    async fn a_sign_out_with_nothing_stored_only_cancels_the_browser_trip() {
        let _session = ScopedSession::empty();
        *PENDING.lock().expect("pkce verifier lock") = Some("a-verifier".into());
        let store = FakeStore::default();

        let outcome = sign_out_at("http://127.0.0.1:1", &store).await;

        assert_eq!(outcome, SignOut::NotSignedIn);
        assert!(PENDING.lock().expect("pkce verifier lock").is_none());
    }

    /// The card's Cancel is a sign-out with nothing stored. If the browser has
    /// already handed back its code and the exchange is out, the tokens it
    /// brings back after the Cancel are not kept: the user said stop.
    #[tokio::test]
    async fn a_sign_in_that_lands_after_a_cancel_is_not_kept() {
        let _session = ScopedSession::empty();
        *PENDING.lock().expect("pkce verifier lock") = Some("a-verifier".into());
        let (addr, arrived, release) = held_stub(200, FRESH_TOKEN).await;
        let base = format!("http://{addr}");
        let store = FakeStore::default();

        let (signed_in, cancelled) = tokio::join!(
            complete_sign_in_at(&base, &store, "the-code"),
            async {
                arrived.await.expect("the exchange reached the listener");
                let outcome = sign_out_at(&base, &store).await;
                let _ = release.send(());
                outcome
            }
        );

        assert_eq!(cancelled, SignOut::NotSignedIn);
        assert!(signed_in.is_err(), "a cancelled sign-in is not a sign-in");
        assert_eq!(store.held(), None, "its refresh token is not kept");
        assert!(
            SESSION.lock().expect("session lock").is_none(),
            "nor its access token"
        );
    }

    /// A dictation's forced refresh can be out when the user signs out. The
    /// rotated token it brings back afterwards is not stored, or a refresh
    /// the user never saw would undo the sign-out.
    #[tokio::test]
    async fn a_refresh_that_lands_after_a_sign_out_is_not_kept() {
        let _session = ScopedSession::holding("the-refused-token");
        let (token_endpoint, arrived, release) = held_stub(200, FRESH_TOKEN).await;
        let token_endpoint = format!("http://{token_endpoint}");
        let (_socket, offline) = nothing_listening();
        let store = FakeStore::holding("the-refresh-token");

        let (refreshed, signed_out) = tokio::join!(
            force_refresh_at(&token_endpoint, &store),
            async {
                arrived.await.expect("the refresh reached the listener");
                let outcome = sign_out_at(&format!("http://{offline}"), &store).await;
                let _ = release.send(());
                outcome
            }
        );

        assert_eq!(signed_out, SignOut::HereOnly);
        assert!(refreshed.is_err(), "a refresh for an ended sign-in hands out no bearer");
        assert_eq!(store.held(), None, "the rotated token is not stored");
        assert!(SESSION.lock().expect("session lock").is_none());
    }

    /// The deep link is a doorbell anyone can ring. A callback whose code the
    /// token endpoint refuses (a stray link, or a forged one) leaves the
    /// verifier where it is, so the real callback can still finish.
    #[tokio::test]
    async fn a_refused_callback_leaves_the_verifier_for_the_real_one() {
        let _session = ScopedSession::empty();
        *PENDING.lock().expect("pkce verifier lock") = Some("the-verifier".into());
        let (addr, mut heard) = scripted_stub(vec![
            (
                404,
                r#"{"code":404,"error_code":"flow_state_not_found","msg":"invalid flow state, no valid flow state found"}"#,
            ),
            (200, FRESH_TOKEN),
        ])
        .await;
        let base = format!("http://{addr}");
        let store = FakeStore::default();

        complete_sign_in_at(&base, &store, "a-forged-code")
            .await
            .expect_err("the token endpoint refused that code");
        let pending = PENDING.lock().expect("pkce verifier lock").clone();
        assert_eq!(
            pending.as_deref(),
            Some("the-verifier"),
            "a refused code must not spend the verifier"
        );

        complete_sign_in_at(&base, &store, "the-real-code")
            .await
            .expect("the real code signs in");
        let requests = requests(&mut heard);
        assert!(
            requests[1].contains("the-real-code") && requests[1].contains("the-verifier"),
            "{}",
            requests[1]
        );
        assert_eq!(store.held().as_deref(), Some("rotated"));
        assert_eq!(live_token().as_deref(), Some("fresh"));
        let pending = PENDING.lock().expect("pkce verifier lock").clone();
        assert!(pending.is_none(), "a finished sign-in spends its verifier");
    }

    /// The webview reads these three words (`SignOutOutcome` in `api.ts`).
    #[test]
    fn a_sign_out_outcome_reaches_the_webview_in_camel_case() {
        for (outcome, word) in [
            (SignOut::Ended, "\"ended\""),
            (SignOut::HereOnly, "\"hereOnly\""),
            (SignOut::NotSignedIn, "\"notSignedIn\""),
        ] {
            assert_eq!(serde_json::to_string(&outcome).unwrap(), word);
        }
    }

    // -- deleting the account -------------------------------------------------

    /// Only the relay's `204` means the account is gone. Then this machine
    /// forgets the sign-in as a sign-out does — both tokens, and a browser
    /// trip still out — and the request carried the bearer and no other
    /// credential.
    #[tokio::test]
    async fn a_deleted_account_is_signed_out_here() {
        let _session = ScopedSession::holding("the-access-token");
        *PENDING.lock().expect("pkce verifier lock") = Some("a-verifier".into());
        let (addr, heard) = recording_stub(204).await;
        let store = FakeStore::holding("the-refresh-token");
        delete_account_at(&format!("http://{addr}"), "the-access-token", &store)
            .await
            .expect("a 204 is a deleted account");

        let request = heard.await.expect("the delete reached the listener");
        assert_eq!(
            request.lines().next().unwrap_or_default(),
            "DELETE /v1/account HTTP/1.1"
        );
        let head = request.to_ascii_lowercase();
        assert!(
            head.contains("authorization: bearer the-access-token"),
            "{request}"
        );
        assert!(
            !head.contains("apikey"),
            "Supabase's key has no business on a relay route: {request}"
        );
        assert!(!head.contains("api-subscription-key"), "{request}");

        assert_eq!(
            store.held(),
            None,
            "the refresh token goes with the account"
        );
        assert!(
            SESSION.lock().expect("session lock").is_none(),
            "and the access token"
        );
        assert!(
            PENDING.lock().expect("pkce verifier lock").is_none(),
            "and a sign-in still out at the browser"
        );
    }

    /// Anything but a `204` leaves the user signed in, so the button can be
    /// pressed again: after a `502` the relay's copy is already gone and a
    /// second call finishes the job. A `200` is not the relay's answer either,
    /// and a sign-out on the strength of one would claim a deletion nobody
    /// confirmed.
    #[tokio::test]
    async fn anything_but_204_keeps_the_sign_in() {
        let _session = ScopedSession::holding("the-access-token");
        for (status, expected) in [
            (502, "Couldn't finish deleting your account — try again"),
            (401, "Sign in again, then delete"),
            (429, "Couldn't finish deleting your account — try again"),
            (503, "Couldn't finish deleting your account — try again"),
            (200, "Couldn't finish deleting your account — try again"),
        ] {
            let (addr, _heard) = recording_stub(status).await;
            let store = FakeStore::holding("the-refresh-token");
            let sentence = delete_account_at(&format!("http://{addr}"), "the-access-token", &store)
                .await
                .expect_err("only a 204 is a deleted account")
                .sentence();
            assert_eq!(sentence, expected, "status {status}");
            assert_eq!(
                store.held().as_deref(),
                Some("the-refresh-token"),
                "status {status}"
            );
            assert_eq!(
                live_token().as_deref(),
                Some("the-access-token"),
                "status {status}"
            );
        }
    }

    /// The relay can take well over the 15 s a token call is given: a counter
    /// write already queued (up to 3 s when it is a rollover waiting on
    /// Supabase), the carry record, then up to 10 s for GoTrue. A delete that
    /// finishes in 16 s must not be reported as a network failure.
    #[tokio::test]
    async fn a_slow_relay_still_finishes_the_delete() {
        let _session = ScopedSession::holding("the-access-token");
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind loopback listener");
        let addr = listener.local_addr().expect("local addr");
        tokio::spawn(async move {
            let Ok((socket, _)) = listener.accept().await else {
                return;
            };
            let mut buf = [0u8; 8192];
            socket.readable().await.expect("socket readable");
            let _ = socket.try_read(&mut buf);
            tokio::time::sleep(Duration::from_secs(16)).await;
            reply(&socket, 204, "").await;
        });
        let store = FakeStore::holding("the-refresh-token");
        delete_account_at(&format!("http://{addr}"), "the-access-token", &store)
            .await
            .expect("a 204 after 16 s is still a deleted account");
        assert_eq!(store.held(), None);
    }

    /// A delete that needs a fresh bearer and finds the sign-in revoked (the
    /// refresh is refused) leaves this machine signed out without reaching
    /// the relay, which is why the command tells the card on this failure too.
    #[tokio::test]
    async fn a_delete_whose_refresh_is_refused_signs_out_here() {
        let _session = ScopedSession::empty();
        let token_endpoint = stub(400, r#"{"error_code":"refresh_token_not_found"}"#).await;
        let (_relay_socket, relay) = nothing_listening();
        let store = FakeStore::holding("dead-refresh-token");
        let sentence = delete_account_with(
            &format!("http://{relay}"),
            &format!("http://{token_endpoint}"),
            &store,
        )
        .await
        .expect_err("a refused refresh gives no bearer to delete with");
        assert_eq!(
            sentence,
            "Your Butterfly Labs sign-in has expired — sign in again"
        );
        assert_eq!(store.held(), None, "the refused credential is gone");
        assert!(SESSION.lock().expect("session lock").is_none());
    }

    /// A `401` from the relay is what a bearer gone stale looks like (a
    /// machine that slept through its hour). The delete refreshes once and
    /// asks again, as a dictation does, rather than sending the user to sign
    /// in again.
    #[tokio::test]
    async fn a_delete_refused_with_401_refreshes_once_and_asks_again() {
        let _session = ScopedSession::holding("the-stale-token");
        let (relay, mut heard) = scripted_stub(vec![(401, ""), (204, "")]).await;
        let token_endpoint = stub(200, FRESH_TOKEN).await;
        let store = FakeStore::holding("the-refresh-token");

        delete_account_with(
            &format!("http://{relay}"),
            &format!("http://{token_endpoint}"),
            &store,
        )
        .await
        .expect("the second ask, with a fresh bearer, deletes the account");

        let requests = requests(&mut heard);
        assert_eq!(
            request_lines(&requests),
            ["DELETE /v1/account HTTP/1.1", "DELETE /v1/account HTTP/1.1"]
        );
        assert!(
            requests[0]
                .to_ascii_lowercase()
                .contains("authorization: bearer the-stale-token"),
            "{}",
            requests[0]
        );
        assert!(
            requests[1]
                .to_ascii_lowercase()
                .contains("authorization: bearer fresh"),
            "the retry carries the refreshed bearer: {}",
            requests[1]
        );
        assert_eq!(store.held(), None, "the deleted account is signed out here");
    }

    /// One retry, not a loop: a relay that refuses the fresh bearer too gets
    /// the sign-in-again sentence, and the sign-in stays.
    #[tokio::test]
    async fn a_delete_refused_twice_asks_the_user_to_sign_in_again() {
        let _session = ScopedSession::holding("the-stale-token");
        let (relay, mut heard) = scripted_stub(vec![(401, ""), (401, "")]).await;
        let token_endpoint = stub(200, FRESH_TOKEN).await;
        let store = FakeStore::holding("the-refresh-token");

        let sentence = delete_account_with(
            &format!("http://{relay}"),
            &format!("http://{token_endpoint}"),
            &store,
        )
        .await
        .expect_err("the relay refused both bearers");

        assert_eq!(sentence, "Sign in again, then delete");
        assert_eq!(requests(&mut heard).len(), 2, "one retry at most");
        assert_eq!(store.held().as_deref(), Some("rotated"));
    }

    /// Offline is not deleted, and the sentence says which it was.
    #[tokio::test]
    async fn an_unreachable_relay_keeps_the_sign_in() {
        let _session = ScopedSession::holding("the-access-token");
        let (_socket, addr) = nothing_listening();
        let store = FakeStore::holding("the-refresh-token");
        let sentence = delete_account_at(&format!("http://{addr}"), "the-access-token", &store)
            .await
            .expect_err("nothing is listening")
            .sentence();
        assert_eq!(
            sentence,
            "Couldn't reach Butterfly Labs — check your internet connection"
        );
        assert_eq!(store.held().as_deref(), Some("the-refresh-token"));
        assert_eq!(live_token().as_deref(), Some("the-access-token"));
    }

    /// The anon key is public, but it still has to be the *right* project's —
    /// a key from another Supabase project would authenticate a user this
    /// app's relay has never heard of.
    #[test]
    fn the_anon_key_belongs_to_the_butterfly_labs_project() {
        let claims = SUPABASE_ANON_KEY.split('.').nth(1).expect("a JWT payload");
        let json = base64::Engine::decode(
            &base64::engine::general_purpose::URL_SAFE_NO_PAD,
            claims,
        )
        .expect("base64url claims");
        let claims: serde_json::Value = serde_json::from_slice(&json).expect("JSON claims");
        assert_eq!(claims["ref"], "iassqjnfvdffocyxptis");
        assert_eq!(claims["role"], "anon");
        assert!(SUPABASE_URL.contains(claims["ref"].as_str().unwrap()));
    }
}
