//! Note actions: saved prompts a person runs over a note from the Enhance
//! menu.
//!
//! A run sends the note's body to the model under [`ACTION_PREAMBLE`] and the
//! action's own prompt, and stores the reply in the note's `polished_body`
//! column, beside the text the person wrote and never over it. This module
//! owns the `note_actions` rows, the instruction a run sends, and the write
//! that stores a result.
//!
//! ## Shipped actions
//!
//! [`BUILTINS`] is the set the app ships. A shipped action is identified by
//! its `shipped_key`, never by its label, and is inserted the first time the
//! list is read. Seeding only inserts: a shipped row that already exists is
//! left exactly as it is, so a person's edit to it survives every launch and
//! every upgrade.
//!
//! When a release stops shipping a key, the same pass retires the rows that
//! still carry it: an unedited row is removed, and an edited one becomes an
//! ordinary user action with its text intact. [`ensure_builtins`] has the
//! rule.
//!
//! Shipped actions can be edited but not deleted.
//!
//! Nothing here logs a note's text or an action's prompt. Log lines carry
//! counts, ids and errors.

use crate::format::backend::Backend;
use rusqlite::types::Value;
use rusqlite::{params, params_from_iter, Connection, OptionalExtension, Row};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// The shared HTTP client for the notes lane.
///
/// One pool, minted once: `reqwest::Client` is `Arc`-backed and carries the
/// TLS session cache, so a fresh client per action would pay a full handshake
/// every time. The dictation hot path keeps its own in `controller`, and the
/// two deliberately do not share — a note action must not be able to queue
/// behind, or stall, the connection a live dictation is finalising on.
pub fn http() -> &'static reqwest::Client {
    static HTTP: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    HTTP.get_or_init(reqwest::Client::new)
}

/// The ceiling on how much of a note one action may rewrite.
///
/// **Deliberately not `transforms::MAX_WORDS`.** The two are different acts.
/// A Transform rewrites a live selection in somebody else's document, where
/// 1,000 words is a sane bound on what a keystroke should be allowed to
/// replace. An action rewrites a *stored note* the user explicitly pressed a
/// button on, and the notes lane's own import path stores whole transcripts:
/// at ~150 wpm, a 1,000-word ceiling would refuse everything past about seven
/// minutes of speech, while the shipped action is meant for long transcripts
/// too.
///
/// 3,000 words is roughly twenty minutes of speech. The output side is sized
/// where it belongs: `chat::note_transform` carries `NOTE_MAX_OUTPUT_TOKENS`,
/// which holds this cap coming back. `actions_cap_longer_than_transforms`
/// pins the decoupling.
pub const MAX_ACTION_WORDS: usize = 3000;

// ---------------------------------------------------------------------------
// The instruction a run sends.
// ---------------------------------------------------------------------------

/// What every run tells the model before the action's own prompt.
///
/// It is placed inside `chat::build_transform_system`'s rewrite envelope,
/// which already asks for the rewritten text alone and appends the
/// end-marker rule after it. So this text describes the input and the
/// shape of the result, and never says that nothing may follow the result:
/// the marker has to.
pub const ACTION_PREAMBLE: &str = "The user's message is a note they typed or dictated. Dictated notes can contain speech-recognition slips, filler words and run-on sentences; read through them to what the person meant. Carry out the task below on that note, following these rules:
- Keep every fact, number, name, date and commitment the note contains, and add nothing that it does not contain.
- Write in the same language and script as the note unless the task names another language. An English note stays in English. A note in an Indian language stays in that language and in its own script: Devanagari stays Devanagari, Tamil stays in Tamil script, and a note typed in Latin letters stays in Latin letters. A note that mixes languages keeps the same mix.
- The reply is shown as raw text in the app and saved as a Markdown file, so asterisks and other markup appear exactly as typed. Format with nothing but headings on a line of their own that start with \"## \", bullet points that start with \"- \", numbered lists and blank lines. Write labels and emphasis as plain words, without asterisks or underscores, and use no tables, links, HTML or code blocks.
- The app already shows the note's title above your reply, so the reply's first line is never a heading. Begin with the note's opening context or its first point, and put headings only in front of the topics after that.
- Reply with the finished note itself: no greeting, no introduction, and no comment on what you changed.";

/// The instruction for one run: [`ACTION_PREAMBLE`], then the action's
/// prompt unchanged inside a `<task>` block. The block gives the prompt a
/// clear end, so a one-word prompt and one of several paragraphs both read
/// as the task, and the envelope's own sentences after it do not run into
/// it.
pub fn action_instruction(action_prompt: &str) -> String {
    format!("{ACTION_PREAMBLE}\n\nThe task:\n<task>\n{action_prompt}\n</task>")
}

// ---------------------------------------------------------------------------
// The built-in set.
// ---------------------------------------------------------------------------

/// The glyph an action gets when it names none, matching the `glyph`
/// column's default: a page with lines on it, the plainest picture of a note.
pub const DEFAULT_GLYPH: &str = "note";

/// An action the app ships, as it is first written to `note_actions`. The
/// fields follow the table's columns, in the table's order.
pub struct Builtin {
    /// Stored as `shipped_key`, and the action's lasting identity. Seeding
    /// and retiring match on it, never on the label, so renaming a shipped
    /// action never seeds a second copy.
    pub key: &'static str,
    /// Where it starts in the Enhance menu.
    pub position: i64,
    /// The name shown in the Enhance menu.
    pub label: &'static str,
    /// The line under the label.
    pub summary: &'static str,
    /// What the model is asked to do, after [`ACTION_PREAMBLE`].
    pub instruction: &'static str,
    /// An `Icon` name the webview can draw.
    pub glyph: &'static str,
}

/// The shipped set: one general tidy-up that works on a short dictated note
/// and on a long imported transcript alike. Its glyph is a bulleted list,
/// because sorting a note into headed lists is most of what it does.
pub const BUILTINS: &[Builtin] = &[Builtin {
    key: "tidy_up",
    position: 0,
    label: "Tidy up",
    summary: "Cleans up a rough note or a long transcript and puts it in order.",
    instruction: "Turn this note into a clean, well-organised version of itself.
- Correct speech-recognition slips, spelling, capitalisation, punctuation and grammar, and remove filler words, false starts and repeated phrases.
- Break run-on sentences into clear ones and tighten passages that ramble, without dropping any point that was made.
- Gather related points together. If the note covers several topics, as a long transcript usually does, give each topic a short heading with its points listed under it. Opening context, such as when and where a meeting took place, stays a plain line at the top with no heading of its own. A short note about one thing needs no headings.
- Put lists, steps, decisions and action items on their own bullet lines, each with the person responsible and the date when the note says who or when.
- Where the wording is already clear, keep the person's own words and tone.",
    glyph: "bullets",
}];

/// Seed the shipped actions, and retire built-in rows this build no longer
/// ships.
///
/// Seeding is `INSERT OR IGNORE` against the unique `shipped_key`, so a row
/// that already exists is never touched. A seeded row takes both of its
/// timestamps from a single clock read, which is what lets the retirement
/// below tell a row nobody edited from one somebody did.
///
/// Retirement covers every row with a shipped key that is not in
/// [`BUILTINS`]:
///
/// * `updated_at` still equals `created_at`: nobody edited it, so it is
///   deleted.
/// * Anything else: it becomes a user action. The key is cleared, and every
///   other column keeps its value, `updated_at` included.
///
/// A second run finds nothing left to retire and nothing missing to insert.
pub fn ensure_builtins(conn: &Connection) -> rusqlite::Result<()> {
    let shipped: Vec<&str> = BUILTINS.iter().map(|b| b.key).collect();
    let slots = vec!["?"; shipped.len()].join(", ");
    let retired = format!("shipped_key NOT IN ({slots})");

    conn.execute(
        &format!("DELETE FROM note_actions WHERE {retired} AND updated_at = created_at"),
        params_from_iter(shipped.iter().copied()),
    )?;
    conn.execute(
        &format!("UPDATE note_actions SET shipped_key = NULL WHERE {retired}"),
        params_from_iter(shipped.iter().copied()),
    )?;

    for b in BUILTINS {
        conn.execute(
            "INSERT OR IGNORE INTO note_actions \
                 (shipped_key, position, label, summary, instruction, glyph, \
                  created_at, updated_at) \
             SELECT ?1, ?2, ?3, ?4, ?5, ?6, stamp, stamp \
             FROM (SELECT CAST(unixepoch('now','subsec') * 1000 AS INTEGER) AS stamp)",
            params![b.key, b.position, b.label, b.summary, b.instruction, b.glyph],
        )?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Rows.
// ---------------------------------------------------------------------------

/// A stored action, as the webview receives it. Every field is named after
/// its column, so the JSON reads like the row; `shipped` is derived, and
/// says whether `shipped_key` is set.
///
/// The JSON lives only on the IPC hop between this build and its own
/// webview. No settings file, export or mirror stores it, so there are no
/// older field names to keep reading.
#[derive(Clone, Debug, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct NoteAction {
    pub id: i64,
    /// `None` for a user's action; for a shipped one, the key seeding matches
    /// on, so it is never inserted twice.
    pub shipped_key: Option<String>,
    /// Shipped with the app. Editable, never deletable.
    pub shipped: bool,
    pub position: i64,
    pub label: String,
    pub summary: String,
    pub instruction: String,
    pub glyph: String,
    /// Epoch milliseconds, as everywhere else in `notes`.
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct NewAction {
    pub label: String,
    /// Stored empty when absent.
    pub summary: Option<String>,
    pub instruction: String,
    /// [`DEFAULT_GLYPH`] when absent or blank.
    pub glyph: Option<String>,
}

/// What an edit from the webview can change: an action's position, label,
/// summary, instruction and glyph.
///
/// The struct has no other fields, so an edit has no way to name the id, the
/// timestamps or the shipped key, and there is no runtime filter that a later
/// change could loosen. [`super::NoteUpdate`] is built the same way.
///
/// A field left out keeps its value. None of these columns takes NULL, so no
/// field needs the `double_option` treatment.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ActionUpdate {
    pub position: Option<i64>,
    pub label: Option<String>,
    pub summary: Option<String>,
    pub instruction: Option<String>,
    pub glyph: Option<String>,
}

const COLUMNS: &str = "id, shipped_key, position, label, summary, instruction, glyph, \
                       created_at, updated_at";

fn row_to_action(row: &Row) -> rusqlite::Result<NoteAction> {
    let shipped_key: Option<String> = row.get("shipped_key")?;
    Ok(NoteAction {
        id: row.get("id")?,
        shipped: shipped_key.is_some(),
        shipped_key,
        position: row.get("position")?,
        label: row.get("label")?,
        summary: row.get("summary")?,
        instruction: row.get("instruction")?,
        glyph: row.get("glyph")?,
        created_at: row.get("created_at")?,
        updated_at: row.get("updated_at")?,
    })
}

/// `notes`' own writer-side `updated_at` bump — the same literal, for the
/// same reason: writers set it, nothing triggers it (`schema.rs`).
/// `note_actions` has no FTS shadow to corrupt, but one convention beats two.
const TOUCH: &str = "updated_at = CAST(unixepoch('now','subsec') * 1000 AS INTEGER)";

// ---------------------------------------------------------------------------
// CRUD.
// ---------------------------------------------------------------------------

/// Every action, shipped and user-made, in the order the menu draws them.
///
/// Seeds the built-ins first, so a fresh install's first look at the menu is
/// what fills the table.
pub fn list_actions(conn: &Connection) -> rusqlite::Result<Vec<NoteAction>> {
    ensure_builtins(conn)?;
    let mut stmt = conn.prepare(&format!(
        "SELECT {COLUMNS} FROM note_actions ORDER BY position ASC, id ASC"
    ))?;
    let rows = stmt.query_map([], row_to_action)?;
    rows.collect()
}

/// One action by id, or `None`. Seeds the built-ins for the reason
/// [`list_actions`] does: a run started from anywhere but the menu must not
/// depend on the menu having been opened first.
pub fn get_action(conn: &Connection, id: i64) -> rusqlite::Result<Option<NoteAction>> {
    ensure_builtins(conn)?;
    conn.query_row(
        &format!("SELECT {COLUMNS} FROM note_actions WHERE id = ?1"),
        params![id],
        row_to_action,
    )
    .optional()
}

/// Store a new user action and return the row as stored.
///
/// The label and the instruction are trimmed and each must still say
/// something. The summary is trimmed and stored empty when absent; a missing
/// or blank glyph becomes [`DEFAULT_GLYPH`]. The action goes after every
/// existing one in the menu, and it is never a built-in.
///
/// The shipped set is seeded first, so on a table nobody has read yet the new
/// action still lands after the built-ins rather than tying with them.
pub fn create_action(conn: &Connection, new: &NewAction) -> anyhow::Result<NoteAction> {
    let (label, instruction) = (new.label.trim(), new.instruction.trim());
    match (label.is_empty(), instruction.is_empty()) {
        (true, _) => anyhow::bail!("Name the action so you can find it in the Enhance menu."),
        (_, true) => anyhow::bail!("Add a prompt that tells the model what the action does."),
        _ => {}
    }
    let summary = match &new.summary {
        Some(text) => text.trim(),
        None => "",
    };
    let glyph = match new.glyph.as_deref().map(str::trim) {
        Some(chosen) if !chosen.is_empty() => chosen,
        _ => DEFAULT_GLYPH,
    };

    ensure_builtins(conn)?;
    let next_position: i64 = conn.query_row(
        "SELECT COALESCE(MAX(position) + 1, 0) FROM note_actions",
        [],
        |r| r.get(0),
    )?;
    conn.execute(
        "INSERT INTO note_actions (position, label, summary, instruction, glyph) \
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![next_position, label, summary, instruction, glyph],
    )?;
    let id = conn.last_insert_rowid();
    match get_action(conn, id)? {
        Some(stored) => Ok(stored),
        None => anyhow::bail!("The new action was saved but could not be read back."),
    }
}

/// Write whichever fields `update` names, and return whether a row changed.
///
/// **No check for a shipped action, deliberately.** Built-ins are editable,
/// which is what makes shipping an opinionated default prompt safe: a person
/// who disagrees with it rewrites it instead of working around it.
///
/// An update naming no fields is `Ok(false)`, not an error, and does not move
/// `updated_at`.
///
/// A `label` or `instruction` that is named still has to say something after
/// trimming, the same rule [`create_action`] enforces: without it a webview
/// call could store `""` and ship an action with nothing to ask. An absent
/// field still means "leave it alone".
pub fn update_action(conn: &Connection, id: i64, update: &ActionUpdate) -> anyhow::Result<bool> {
    if update.label.as_deref().is_some_and(|s| s.trim().is_empty()) {
        anyhow::bail!("Give the action a name");
    }
    if update.instruction.as_deref().is_some_and(|s| s.trim().is_empty()) {
        anyhow::bail!("Write what the action should do");
    }

    let mut fields: Vec<&str> = Vec::new();
    let mut values: Vec<Value> = Vec::new();

    if let Some(position) = update.position {
        fields.push("position = ?");
        values.push(Value::Integer(position));
    }
    for (field, v) in [
        ("label = ?", &update.label),
        ("summary = ?", &update.summary),
        ("instruction = ?", &update.instruction),
        ("glyph = ?", &update.glyph),
    ] {
        if let Some(s) = v {
            fields.push(field);
            values.push(Value::Text(s.trim().to_string()));
        }
    }

    if fields.is_empty() {
        return Ok(false);
    }
    fields.push(TOUCH);
    values.push(Value::Integer(id));
    let sql = format!("UPDATE note_actions SET {} WHERE id = ?", fields.join(", "));
    Ok(conn.execute(&sql, params_from_iter(values))? > 0)
}

/// Delete a user action. Returns `true` once the row is gone.
///
/// A built-in is refused here as well as in the webview, which never draws
/// its delete control, so a direct command call cannot remove one either.
pub fn delete_action(conn: &Connection, id: i64) -> anyhow::Result<bool> {
    match get_action(conn, id)?.map(|a| a.shipped) {
        None => anyhow::bail!("That action isn't there any more."),
        Some(true) => {
            anyhow::bail!("Built-in actions can't be deleted, but you can edit them to suit you.")
        }
        Some(false) => {
            let removed = conn.execute("DELETE FROM note_actions WHERE id = ?1", params![id])?;
            Ok(removed == 1)
        }
    }
}

// ---------------------------------------------------------------------------
// Running one.
// ---------------------------------------------------------------------------

/// A fingerprint of the text an action's result was produced from, stored in
/// `notes.polished_from_hash` so the UI can tell someone the result is out of
/// date.
///
/// SHA-256 hex over the whole text, which the crate already depends on for
/// download verification. A real digest matters: a shortcut such as the
/// length plus the opening characters misses any edit that keeps both, and
/// the enhancement would then report itself current.
///
/// The input is exactly the text handed to the model — `content`, nothing
/// appended — so the write here and any later staleness check agree by
/// construction, rather than by two call sites each remembering to build the
/// same string.
pub fn content_hash(content: &str) -> String {
    format!("{:x}", Sha256::digest(content.as_bytes()))
}

/// What [`enhance`] refuses before it opens a connection.
///
/// Split out so the boundary is testable without a network: a caller that
/// gets `Err` here has sent nothing, spent nothing, and has a sentence to
/// show.
pub fn check_input(content: &str) -> anyhow::Result<()> {
    if content.trim().is_empty() {
        anyhow::bail!("There's nothing in this note to work on yet.");
    }
    if content.split_whitespace().count() > MAX_ACTION_WORDS {
        anyhow::bail!("This note is too long to enhance — actions work on up to 3,000 words.");
    }
    Ok(())
}

/// Rewrite `content` under `action_prompt`, and return the model's reply.
///
/// The network half, split from the database half so it can be pointed at a
/// loopback server in a test — the seam `routes::agent` and `sarvam::chat`'s
/// own round-trip tests use.
///
/// Runs over [`crate::sarvam::chat::note_transform`], which mints a
/// per-request end marker and rejects a reply that does not carry it.
/// That matters more here than for a Transform: this reply is *stored*, not
/// pasted, so a half-written enhancement would sit in the note looking
/// finished.
///
/// **Every `Err` from here is safe to show.** The transport's own error is
/// not: `reqwest` renders the URL it was handed, and for the custom endpoint
/// that is a string somebody pasted, which can carry a credential in its
/// userinfo or its query. So a failed request is logged with its whole source
/// chain and returned as one fixed sentence, while the failures this module
/// diagnoses itself keep their own wording.
pub async fn enhance(
    http: &reqwest::Client,
    backend: &Backend,
    action_prompt: &str,
    content: &str,
) -> anyhow::Result<String> {
    check_input(content)?;
    let instruction = action_instruction(action_prompt);
    // `note_transform`, not `transform`: the same request shaper and the same
    // end-marker check, against the notes lane's own deadline and
    // output ceiling rather than the dictation path's — see that function, and
    // [`MAX_ACTION_WORDS`] for why the two paths do not share a size. It
    // takes no prompt override, because a note action is not level-based
    // cleanup and has no user prompt-rules: `instruction` is already the
    // action's own prompt, so there is no shipped scaffold to override.
    let enhanced = crate::sarvam::chat::note_transform(http, backend, &instruction, content)
        .await
        .map_err(|e| {
            // `{e:#}` — the whole anyhow source chain, never `{e}`: a timeout's
            // top-level Display is the same generic sentence a connection
            // failure has, and the difference lives only in the sources.
            // Counts and statuses only; never the note, never the prompt — and
            // through `redact_urls` first, because reqwest's Display embeds the
            // request URL, which for the custom slot is a pasted string that
            // can carry a credential (`https://user:pw@host`, `?api_key=`).
            let reason = crate::format::backend::redact_urls(&format!("{e:#}"));
            tracing::warn!("note action call failed: {reason}");
            anyhow::anyhow!(crate::sarvam::chat::failure_sentence(&e)
                .unwrap_or("Couldn't finish enhancing this note. Try again."))
        })?;
    if enhanced.trim().is_empty() {
        anyhow::bail!("The model returned nothing. Try again.");
    }
    Ok(enhanced)
}

/// Store an enhancement: the text, the prompt that produced it, and the
/// fingerprint of what it was produced from. The three columns describe one
/// result, so they are always written together in one update.
///
/// Goes through [`super::update_note`] rather than its own `UPDATE`, so the
/// note allow-list stays the single place a note's columns are written and
/// `updated_at` moves the way every other note write moves it.
pub fn record_run(
    conn: &Connection,
    note_id: i64,
    action_prompt: &str,
    enhanced: &str,
    source: &str,
) -> rusqlite::Result<bool> {
    super::update_note(
        conn,
        note_id,
        &super::NoteUpdate {
            polished_body: Some(Some(enhanced.to_string())),
            polish_prompt: Some(Some(action_prompt.to_string())),
            polished_from_hash: Some(Some(content_hash(source))),
            ..Default::default()
        },
    )
}

/// A one-connection loopback chat-completions server for round-trip tests.
///
/// Raw sockets rather than a mock-server crate — the construction
/// `sarvam::chat` and `format::backend` already use for their own round trips:
/// no new dependency, and the assertions land on the bytes that actually went
/// out, which is the only way to prove a prompt *ships* rather than assert it.
/// Shared with [`super::title`]'s tests so `notes` carries one copy, not two.
#[cfg(test)]
pub(crate) mod stub {
    /// Answer with the request's own end marker appended — what a
    /// cooperative model does.
    pub(crate) const WITH_MARKER: bool = true;
    /// Answer without it. The API still says "stop" and reports a token count
    /// well under the ceiling, which is the shape a model that simply skipped
    /// the marker produces, and the transform path keeps it rather than
    /// rejecting it. Use [`chat_once_cut_off`] for a reply
    /// that was genuinely truncated.
    pub(crate) const NO_MARKER: bool = false;

    pub(crate) struct Stub {
        /// What to point `Backend::base_url` at.
        pub(crate) url: String,
        seen: tokio::sync::oneshot::Receiver<String>,
    }

    impl Stub {
        /// The `host:port` this stub listens on — what a leak check looks for
        /// in a message that is about to be shown to somebody.
        pub(crate) fn host(&self) -> String {
            self.url.trim_start_matches("http://").to_string()
        }

        /// The request body the stub saw, parsed.
        pub(crate) async fn request(self) -> serde_json::Value {
            let body = self.seen.await.expect("the stub captured no request");
            serde_json::from_str(&body).expect("the request body is JSON")
        }
    }

    /// Bind a loopback listener that answers exactly one chat-completions
    /// request with `content`.
    ///
    /// The end marker is minted inside `chat::transform`, so the stub
    /// cannot know it in advance — it reads it back off the wire the way a
    /// cooperative model would.
    pub(crate) async fn chat_once(content: &str, with_marker: bool) -> Stub {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind loopback listener");
        let url = format!("http://{}", listener.local_addr().expect("local addr"));
        let (tx, seen) = tokio::sync::oneshot::channel();
        let content = content.to_string();

        tokio::spawn(async move {
            if let Ok((socket, _)) = listener.accept().await {
                let body = read_request_body(&socket).await;
                let reply = if with_marker {
                    format!("{content}{}", extract_marker(&body))
                } else {
                    content
                };
                let _ = tx.send(body);
                write_json_response(&socket, &canned_chat_response(&reply)).await;
            }
        });
        Stub { url, seen }
    }

    fn extract_marker(body: &str) -> String {
        // Mirrors format::backend::mint_end_marker's shape.
        regex::Regex::new(r"<<[A-Z2-9]{4}>>")
            .expect("marker pattern")
            .find(body)
            .expect("the request carried no end marker")
            .as_str()
            .to_string()
    }

    /// A reply the API itself admits it cut off: `finish_reason: "length"`,
    /// no marker. The one shape that must still be refused — see
    /// `chat::transform_with_budget` for why a marker-less `"stop"` is not.
    pub(crate) async fn chat_once_cut_off(content: &str) -> Stub {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind loopback listener");
        let url = format!("http://{}", listener.local_addr().expect("local addr"));
        let (tx, seen) = tokio::sync::oneshot::channel();
        let content = content.to_string();
        tokio::spawn(async move {
            if let Ok((socket, _)) = listener.accept().await {
                let body = read_request_body(&socket).await;
                let _ = tx.send(body);
                write_json_response(&socket, &canned_response(&content, "length")).await;
            }
        });
        Stub { url, seen }
    }

    fn canned_chat_response(content: &str) -> String {
        canned_response(content, "stop")
    }

    fn canned_response(content: &str, finish_reason: &str) -> String {
        serde_json::json!({
            "choices": [{"finish_reason": finish_reason, "message": {"content": content}}],
            "usage": {"prompt_tokens": 5, "completion_tokens": 3},
        })
        .to_string()
    }

    /// Reads until `Content-Length` bytes of body have arrived. Windows
    /// answers a close with unread inbound data by sending a RST rather than
    /// a graceful FIN, which hyper surfaces as "connection aborted" — so the
    /// request is drained fully before anything is written back.
    async fn read_request_body(socket: &tokio::net::TcpStream) -> String {
        let mut buf: Vec<u8> = Vec::new();
        let mut chunk = [0u8; 8192];
        loop {
            if let Some(header_end) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                let headers = String::from_utf8_lossy(&buf[..header_end]).to_string();
                let content_length: usize = headers
                    .lines()
                    .find_map(|l| {
                        l.to_lowercase()
                            .starts_with("content-length:")
                            .then(|| l.splitn(2, ':').nth(1).unwrap_or("0").trim().to_string())
                    })
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(0);
                let body_start = header_end + 4;
                if buf.len() >= body_start + content_length {
                    return String::from_utf8_lossy(&buf[body_start..body_start + content_length])
                        .to_string();
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

    async fn write_json_response(socket: &tokio::net::TcpStream, body: &str) {
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::notes::{self, schema, NewNote};

    fn db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(schema::SCHEMA).unwrap();
        conn
    }

    fn builtin(conn: &Connection) -> NoteAction {
        list_actions(conn)
            .unwrap()
            .into_iter()
            .find(|a| a.shipped)
            .expect("a built-in was seeded")
    }

    fn user_action(conn: &Connection, label: &str) -> NoteAction {
        create_action(
            conn,
            &NewAction {
                label: label.into(),
                instruction: "Do the thing".into(),
                ..Default::default()
            },
        )
        .unwrap()
    }

    // -----------------------------------------------------------------------
    // The instruction.
    // -----------------------------------------------------------------------

    /// The preamble leads, and the action's prompt follows it unchanged,
    /// whether the prompt is one word or several lines.
    #[test]
    fn the_action_prompt_follows_the_base_text() {
        for prompt in [
            "Summarise",
            "Pull out every date the note mentions.\n\nList them oldest first,\n  one per line.",
        ] {
            let composed = action_instruction(prompt);
            assert!(composed.starts_with(ACTION_PREAMBLE), "the preamble leads: {composed}");
            assert!(
                composed[ACTION_PREAMBLE.len()..].contains(prompt),
                "the prompt follows the preamble, unchanged: {composed}"
            );
        }
    }

    #[test]
    fn the_base_text_asks_to_keep_the_notes_language() {
        assert!(
            ACTION_PREAMBLE.contains("the same language and script as the note"),
            "the preamble lost its language rule"
        );
    }

    /// The end-marker rule is appended after this text, so the text
    /// must never claim that nothing may follow the result.
    #[test]
    fn the_base_text_leaves_room_for_the_marker() {
        let lower = ACTION_PREAMBLE.to_lowercase();
        for phrase in ["nothing else", "nothing after", "nothing more"] {
            assert!(!lower.contains(phrase), "the preamble says {phrase:?}");
        }
    }

    /// **The two caps are decoupled on purpose, and this is the reason.**
    ///
    /// Held equal, they make the notes lane refuse its own imports: `import`
    /// stores a whole transcript, and 1,000 words is about seven minutes of
    /// speech, so running the shipped action on anything longer would answer
    /// "This note is too long to enhance".
    ///
    /// A Transform bounds what one keystroke may replace inside a live
    /// selection in somebody's document; an action bounds a stored note the
    /// user pressed a button on. Different acts, different ceilings. So this
    /// asserts the *relationship* — an action may never be the stricter of the
    /// two — rather than an equality, and says why here so a future tidy-up
    /// that unifies them fails with the argument attached.
    #[test]
    fn actions_cap_longer_than_transforms() {
        let transforms = include_str!("../transforms.rs");
        let cap = transforms
            .lines()
            .find_map(|l| l.trim().strip_prefix("const MAX_WORDS: usize = "))
            .and_then(|rest| rest.trim_end_matches(';').parse::<usize>().ok())
            .expect("transforms::MAX_WORDS is a plain integer constant");
        assert!(
            MAX_ACTION_WORDS > cap,
            "a note action must stay the roomier of the two ({MAX_ACTION_WORDS} vs {cap}); \
             see MAX_ACTION_WORDS for why they are not one number"
        );
        // ...and long enough to be worth the split: a twenty-minute recording
        // at ~150 wpm is the case that forced it.
        assert!(MAX_ACTION_WORDS >= 3000, "{MAX_ACTION_WORDS} is under twenty minutes of speech");
    }

    // -----------------------------------------------------------------------
    // Seeding and retiring.
    // -----------------------------------------------------------------------

    /// A shipped row under a key this build does not ship, stamped as an
    /// earlier release would have left it.
    fn insert_retired_builtin(conn: &Connection, key: &str, created_at: i64, updated_at: i64) -> i64 {
        conn.execute(
            "INSERT INTO note_actions \
                 (shipped_key, position, label, summary, instruction, glyph, \
                  created_at, updated_at) \
             VALUES (?1, 0, 'Old default', 'Shipped by an earlier release', \
                     'Do the old thing', 'book', ?2, ?3)",
            params![key, created_at, updated_at],
        )
        .unwrap();
        conn.last_insert_rowid()
    }

    #[test]
    fn every_shipped_action_is_seeded_on_first_read() {
        let conn = db();
        let before: i64 = conn
            .query_row("SELECT COUNT(*) FROM note_actions", [], |r| r.get(0))
            .unwrap();
        assert_eq!(before, 0, "nothing is seeded before the first read");

        let rows = list_actions(&conn).unwrap();
        assert_eq!(rows.len(), BUILTINS.len());
        for b in BUILTINS {
            let row = rows
                .iter()
                .find(|r| r.shipped_key.as_deref() == Some(b.key))
                .unwrap_or_else(|| panic!("{} was not seeded", b.key));
            assert!(row.shipped);
            assert_eq!(row.position, b.position);
            assert_eq!(row.label, b.label);
            assert_eq!(row.summary, b.summary);
            assert_eq!(row.instruction, b.instruction);
            assert_eq!(row.glyph, b.glyph);
            assert_eq!(
                row.created_at, row.updated_at,
                "a fresh seed must read as never edited, or retiring it later would keep it"
            );
        }
    }

    #[test]
    fn an_unedited_retired_builtin_is_removed() {
        let conn = db();
        let old = insert_retired_builtin(&conn, "retired_example", 1_000, 1_000);

        let rows = list_actions(&conn).unwrap();
        assert!(rows.iter().all(|r| r.id != old), "the untouched old row should be gone");
        assert_eq!(rows.len(), BUILTINS.len(), "only the current set remains");
    }

    /// A row whose timestamps came from the column defaults has both stamped
    /// in one statement, so it reads as unedited and is removed.
    #[test]
    fn a_retired_builtin_stamped_by_the_column_defaults_is_removed() {
        let conn = db();
        conn.execute(
            "INSERT INTO note_actions (label, instruction, shipped_key) \
             VALUES ('Old default', 'Do the old thing', 'retired_example')",
            [],
        )
        .unwrap();
        let old = conn.last_insert_rowid();

        let rows = list_actions(&conn).unwrap();
        assert!(rows.iter().all(|r| r.id != old), "the untouched old row should be gone");
    }

    #[test]
    fn an_edited_retired_builtin_becomes_a_user_action() {
        let conn = db();
        let old = insert_retired_builtin(&conn, "retired_example", 1_000, 2_000);

        let row = get_action(&conn, old)
            .unwrap()
            .expect("an edited old row is kept");
        assert!(!row.shipped);
        assert!(row.shipped_key.is_none());
        assert_eq!(row.position, 0);
        assert_eq!(row.label, "Old default");
        assert_eq!(row.summary, "Shipped by an earlier release");
        assert_eq!(row.instruction, "Do the old thing");
        assert_eq!(row.glyph, "book");
        assert_eq!((row.created_at, row.updated_at), (1_000, 2_000));

        assert!(delete_action(&conn, old).unwrap(), "it can now be deleted like any user action");
    }

    /// Moving a row counts as editing it: an old built-in whose only change
    /// is its menu position is kept too.
    #[test]
    fn a_retired_builtin_that_was_only_reordered_is_kept() {
        let conn = db();
        let old = insert_retired_builtin(&conn, "retired_example", 1_000, 1_000);
        update_action(
            &conn,
            old,
            &ActionUpdate {
                position: Some(7),
                ..Default::default()
            },
        )
        .unwrap();

        let row = get_action(&conn, old).unwrap().expect("a reordered old row is kept");
        assert!(!row.shipped && row.shipped_key.is_none());
        assert_eq!(row.position, 7);
    }

    #[test]
    fn retiring_is_idempotent() {
        let conn = db();
        let kept = insert_retired_builtin(&conn, "retired_edited", 1_000, 2_000);
        let gone = insert_retired_builtin(&conn, "retired_untouched", 1_000, 1_000);

        let first = list_actions(&conn).unwrap();
        ensure_builtins(&conn).unwrap();
        ensure_builtins(&conn).unwrap();
        let again = list_actions(&conn).unwrap();

        assert_eq!(first, again, "a later pass changes nothing");
        assert!(again.iter().any(|r| r.id == kept && !r.shipped));
        assert!(again.iter().all(|r| r.id != gone));
    }

    /// Both rows start at the same menu position, so the older row lists
    /// first by id.
    #[test]
    fn the_new_builtin_is_seeded_beside_a_retired_one() {
        let conn = db();
        let old = insert_retired_builtin(&conn, "retired_example", 1_000, 2_000);

        let rows = list_actions(&conn).unwrap();
        assert_eq!(rows.len(), BUILTINS.len() + 1);
        assert_eq!(rows[0].id, old);
        assert!(!rows[0].shipped);
        for b in BUILTINS {
            assert!(
                rows.iter().any(|r| r.shipped && r.shipped_key.as_deref() == Some(b.key)),
                "{} is missing",
                b.key
            );
        }
    }

    #[test]
    fn seeding_repeatedly_creates_nothing_after_the_first_time() {
        let conn = db();
        ensure_builtins(&conn).unwrap();
        ensure_builtins(&conn).unwrap();
        ensure_builtins(&conn).unwrap();
        assert_eq!(list_actions(&conn).unwrap().len(), 1);
    }

    /// Seeding can only insert. A re-seed that rewrote a shipped row back to
    /// the constant would silently destroy an edit to a prompt the app
    /// invites people to edit.
    #[test]
    fn re_seeding_never_overwrites_an_edited_builtin() {
        let conn = db();
        let b = builtin(&conn);
        update_action(
            &conn,
            b.id,
            &ActionUpdate {
                label: Some("My notes".into()),
                instruction: Some("Rewrite it as a haiku.".into()),
                ..Default::default()
            },
        )
        .unwrap();

        ensure_builtins(&conn).unwrap();

        let after = list_actions(&conn).unwrap();
        assert_eq!(after.len(), 1, "a second copy was seeded");
        assert_eq!(after[0].label, "My notes");
        assert_eq!(after[0].instruction, "Rewrite it as a haiku.");
        assert!(after[0].shipped, "it is still the built-in row");
    }

    /// Seeded by `shipped_key`, not by label: renaming a built-in must not
    /// look like a missing one on the next launch.
    #[test]
    fn identity_is_the_key_not_the_label() {
        let conn = db();
        let b = builtin(&conn);
        conn.execute(
            "UPDATE note_actions SET label = 'Something else' WHERE id = ?1",
            params![b.id],
        )
        .unwrap();
        ensure_builtins(&conn).unwrap();
        assert_eq!(list_actions(&conn).unwrap().len(), 1);
    }

    /// A user-created action never carries a key, so it can never collide
    /// with a built-in — SQLite's `UNIQUE` counts NULLs as distinct, which is
    /// exactly what is wanted here.
    #[test]
    fn user_actions_carry_no_shipped_key() {
        let conn = db();
        let a = user_action(&conn, "Bulletise");
        let b = user_action(&conn, "Shorten");
        assert!(a.shipped_key.is_none() && b.shipped_key.is_none());
        assert!(!a.shipped && !b.shipped);
        assert_eq!(list_actions(&conn).unwrap().len(), 3);
    }

    // -----------------------------------------------------------------------
    // CRUD.
    // -----------------------------------------------------------------------

    /// No action, shipped or new, wears the Enhance button's own glyph, and
    /// the constant agrees with the column default a bare INSERT gets.
    #[test]
    fn the_default_glyph_is_the_columns_default() {
        assert_ne!(DEFAULT_GLYPH, "sparkles");
        assert!(BUILTINS.iter().all(|b| b.glyph != "sparkles"));
        assert!(
            schema::SCHEMA.contains(&format!("glyph       TEXT    NOT NULL DEFAULT '{DEFAULT_GLYPH}'")),
            "the glyph column's default and DEFAULT_GLYPH disagree"
        );
    }

    /// `Icon.svelte` draws an unknown name as an empty box, and the webview
    /// only offers the names in `glyphs.ts`, so every glyph this module hands
    /// out has to be in both, every offered glyph has to be drawable, and the
    /// webview's new-action glyph has to be [`DEFAULT_GLYPH`]. Read with
    /// patterns that ignore spacing and line breaks, so reformatting either
    /// file leaves the check intact.
    #[test]
    fn every_glyph_handed_out_is_one_the_webview_draws_and_offers() {
        let icons = include_str!("../../../src/lib/components/Icon.svelte");
        let shared = include_str!("../../../src/lib/notes/glyphs.ts");
        let pattern = |p: &str| regex::Regex::new(p).expect("a valid pattern");

        // `Icon.svelte` keys each path set as `name: [` or `"name": [`.
        let drawn = |glyph: &str| {
            pattern(&format!(r#"(?m)^\s*"?{}"?\s*:\s*\["#, regex::escape(glyph))).is_match(icons)
        };
        let list = pattern(r"(?s)export\s+const\s+GLYPHS\b[^=]*=\s*\[(.*?)\]")
            .captures(shared)
            .and_then(|c| c.get(1))
            .expect("glyphs.ts exports a GLYPHS array")
            .as_str();
        let offered: Vec<&str> = pattern(r#""([^"]+)""#)
            .captures_iter(list)
            .filter_map(|c| c.get(1).map(|m| m.as_str()))
            .collect();
        let new_action = pattern(r#"export\s+const\s+NEW_ACTION_GLYPH\b[^=]*=\s*"([^"]+)""#)
            .captures(shared)
            .and_then(|c| c.get(1))
            .expect("glyphs.ts exports NEW_ACTION_GLYPH")
            .as_str();

        assert!(!offered.is_empty(), "no glyphs read from glyphs.ts");
        assert_eq!(new_action, DEFAULT_GLYPH, "the webview's new-action glyph");
        for glyph in BUILTINS.iter().map(|b| b.glyph).chain([DEFAULT_GLYPH]) {
            assert!(offered.contains(&glyph), "glyphs.ts does not offer {glyph:?}: {offered:?}");
        }
        for glyph in &offered {
            assert!(drawn(glyph), "Icon.svelte has no path for {glyph:?}");
        }
    }

    /// The IPC JSON carries the column names both ways: what the webview
    /// receives, and what it sends to create or edit an action.
    #[test]
    fn the_ipc_json_uses_the_column_names() {
        let conn = db();
        let sent = serde_json::to_value(builtin(&conn)).unwrap();
        let mut keys: Vec<&str> = sent.as_object().unwrap().keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            [
                "createdAt",
                "glyph",
                "id",
                "instruction",
                "label",
                "position",
                "shipped",
                "shippedKey",
                "summary",
                "updatedAt",
            ]
        );

        let new: NewAction = serde_json::from_value(serde_json::json!({
            "label": "Kharif plan",
            "summary": "Sowing dates by field",
            "instruction": "List each field with its sowing date.",
            "glyph": "clock",
        }))
        .unwrap();
        assert_eq!(
            (new.label.as_str(), new.summary.as_deref(), new.glyph.as_deref()),
            ("Kharif plan", Some("Sowing dates by field"), Some("clock"))
        );
        assert_eq!(new.instruction, "List each field with its sowing date.");

        let edit: ActionUpdate =
            serde_json::from_value(serde_json::json!({ "position": 4, "glyph": "book" })).unwrap();
        assert_eq!((edit.position, edit.glyph.as_deref()), (Some(4), Some("book")));
        assert!(edit.label.is_none() && edit.summary.is_none() && edit.instruction.is_none());
    }

    #[test]
    fn create_trims_defaults_the_glyph_and_appends_at_the_end() {
        let conn = db();
        let shipped = list_actions(&conn).unwrap();

        let a = create_action(
            &conn,
            &NewAction {
                label: "  Bulletise  ".into(),
                summary: Some("  Every point as a bullet \n".into()),
                instruction: "\n  Turn it into bullets.  ".into(),
                glyph: None,
            },
        )
        .unwrap();
        assert_eq!(a.label, "Bulletise");
        assert_eq!(a.summary, "Every point as a bullet");
        assert_eq!(a.instruction, "Turn it into bullets.");
        assert_eq!(a.glyph, DEFAULT_GLYPH);
        assert!(!a.shipped && a.shipped_key.is_none());
        assert!(
            shipped.iter().all(|b| b.position < a.position),
            "a new action sorts after every built-in"
        );

        let b = create_action(
            &conn,
            &NewAction {
                label: "Shorten".into(),
                instruction: "Make it shorter.".into(),
                glyph: Some("   ".into()),
                summary: None,
            },
        )
        .unwrap();
        assert_eq!(b.glyph, DEFAULT_GLYPH, "a blank glyph falls back");
        assert_eq!(b.summary, "", "an absent summary is stored empty");
        assert!(b.position > a.position);

        let c = create_action(
            &conn,
            &NewAction {
                label: "Dates".into(),
                instruction: "List the dates.".into(),
                glyph: Some("  clock ".into()),
                summary: None,
            },
        )
        .unwrap();
        assert_eq!(c.glyph, "clock");

        let order: Vec<i64> = list_actions(&conn).unwrap().iter().map(|r| r.id).collect();
        assert_eq!(&order[order.len() - 3..], &[a.id, b.id, c.id]);
    }

    /// Created before anything has read the list, a new action still lands
    /// after the built-ins that the first read seeds.
    #[test]
    fn a_first_action_on_a_fresh_table_still_sorts_last() {
        let conn = db();
        let first = user_action(&conn, "First");
        let rows = list_actions(&conn).unwrap();
        assert_eq!(rows.len(), BUILTINS.len() + 1);
        assert_eq!(rows.last().map(|r| r.id), Some(first.id));
    }

    #[test]
    fn create_rejects_an_empty_label_or_instruction() {
        let conn = db();
        assert!(create_action(
            &conn,
            &NewAction {
                label: "   ".into(),
                instruction: "Do it".into(),
                ..Default::default()
            }
        )
        .is_err());
        assert!(create_action(
            &conn,
            &NewAction {
                label: "Named".into(),
                instruction: "  ".into(),
                ..Default::default()
            }
        )
        .is_err());
    }

    /// Enforcement point one. (Point two is `ActionManager.svelte` not
    /// drawing the button.)
    #[test]
    fn a_builtin_cannot_be_deleted() {
        let conn = db();
        let b = builtin(&conn);
        let err = delete_action(&conn, b.id).unwrap_err().to_string();
        assert!(err.contains("Built-in"), "unhelpful message: {err}");
        assert_eq!(list_actions(&conn).unwrap().len(), 1, "it is still there");
    }

    #[test]
    fn a_user_action_can_be_deleted_and_a_missing_one_says_so() {
        let conn = db();
        let a = user_action(&conn, "Bulletise");
        assert!(delete_action(&conn, a.id).unwrap());
        assert!(delete_action(&conn, a.id).is_err(), "gone means gone");
        assert!(delete_action(&conn, 404).is_err());
    }

    /// Built-ins are editable — the other half of "editable, not deletable".
    #[test]
    fn a_builtin_is_editable() {
        let conn = db();
        let b = builtin(&conn);
        assert!(update_action(
            &conn,
            b.id,
            &ActionUpdate {
                instruction: Some("Rewrite it as a haiku.".into()),
                ..Default::default()
            }
        )
        .unwrap());
        assert_eq!(builtin(&conn).instruction, "Rewrite it as a haiku.");
    }

    /// `update_action` enforces the same non-empty rule `create_action` does:
    /// a `label` or `instruction` that trims to nothing is refused, so a
    /// webview call cannot blank out what an action asks for. An untouched
    /// field is left exactly as it was.
    #[test]
    fn update_rejects_an_emptied_label_or_instruction() {
        let conn = db();
        let a = user_action(&conn, "Bulletise");
        assert!(update_action(
            &conn,
            a.id,
            &ActionUpdate {
                label: Some("   ".into()),
                ..Default::default()
            }
        )
        .is_err());
        assert!(update_action(
            &conn,
            a.id,
            &ActionUpdate {
                instruction: Some("  ".into()),
                ..Default::default()
            }
        )
        .is_err());
        let after = get_action(&conn, a.id).unwrap().unwrap();
        assert_eq!(after.label, "Bulletise", "a refused update changed nothing");
        assert_eq!(after.instruction, "Do the thing");
    }

    /// `ActionUpdate` has no field for the built-in flag or the key, so a
    /// payload that names them anyway changes neither.
    #[test]
    fn an_update_cannot_reach_the_shipped_flag_or_the_key() {
        let conn = db();
        let b = builtin(&conn);
        let update: ActionUpdate = serde_json::from_value(serde_json::json!({
            "label": "Renamed",
            "shipped": false,
            "shippedKey": "something_else",
        }))
        .unwrap();

        assert!(update_action(&conn, b.id, &update).unwrap());

        let after = get_action(&conn, b.id).unwrap().unwrap();
        assert_eq!(after.label, "Renamed");
        assert!(after.shipped);
        assert_eq!(after.shipped_key.as_deref(), Some(BUILTINS[0].key));
    }

    #[test]
    fn an_update_naming_nothing_changes_nothing() {
        let conn = db();
        let b = builtin(&conn);
        assert!(!update_action(&conn, b.id, &ActionUpdate::default()).unwrap());
        assert_eq!(
            get_action(&conn, b.id).unwrap().unwrap().updated_at,
            b.updated_at
        );
    }

    #[test]
    fn an_update_moves_updated_at_forward() {
        let conn = db();
        let b = builtin(&conn);
        conn.execute(
            "UPDATE note_actions SET updated_at = 0 WHERE id = ?1",
            params![b.id],
        )
        .unwrap();
        update_action(
            &conn,
            b.id,
            &ActionUpdate {
                label: Some("Renamed".into()),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(get_action(&conn, b.id).unwrap().unwrap().updated_at > 0);
    }

    #[test]
    fn the_menu_order_is_position_then_id() {
        let conn = db();
        let b = builtin(&conn);
        let first = user_action(&conn, "First");
        let second = user_action(&conn, "Second");
        // Second moves ahead of everything; First ties with the built-in and
        // falls behind it on id.
        for (id, position) in [(second.id, b.position - 1), (first.id, b.position)] {
            update_action(
                &conn,
                id,
                &ActionUpdate {
                    position: Some(position),
                    ..Default::default()
                },
            )
            .unwrap();
        }

        let labels: Vec<String> =
            list_actions(&conn).unwrap().into_iter().map(|a| a.label).collect();
        assert_eq!(labels, ["Second", BUILTINS[0].label, "First"]);
    }

    // -----------------------------------------------------------------------
    // The pre-flight.
    // -----------------------------------------------------------------------

    #[test]
    fn an_empty_note_is_refused_before_any_request() {
        let err = check_input("   \n  ").unwrap_err().to_string();
        assert!(err.contains("nothing in this note"), "unhelpful: {err}");
    }

    #[test]
    fn the_word_cap_is_inclusive_and_says_the_number() {
        assert!(check_input(&"word ".repeat(MAX_ACTION_WORDS)).is_ok());
        let err = check_input(&"word ".repeat(MAX_ACTION_WORDS + 1))
            .unwrap_err()
            .to_string();
        assert!(err.contains("3,000 words"), "unhelpful: {err}");
    }

    // -----------------------------------------------------------------------
    // The hash and the write.
    // -----------------------------------------------------------------------

    /// An edit that keeps the length and the first fifty characters must
    /// still change the fingerprint, or a stale enhancement would report
    /// itself current.
    #[test]
    fn the_hash_notices_a_length_preserving_edit() {
        let head = "x".repeat(60);
        let a = format!("{head}alpha");
        let b = format!("{head}omega");
        assert_eq!(a.len(), b.len());
        assert_eq!(&a[..50], &b[..50]);
        assert_ne!(content_hash(&a), content_hash(&b));
        assert_eq!(content_hash(&a), content_hash(&a.clone()));
    }

    /// A digest over bytes, so two Devanagari strings that differ only in a
    /// matra are different — the pair a consonant-skeleton comparison misses.
    #[test]
    fn the_hash_separates_indic_minimal_pairs() {
        assert_ne!(content_hash("दिन"), content_hash("दान"));
        assert_ne!(content_hash("किताब"), content_hash("कुतुब"));
    }

    #[test]
    fn recording_a_run_writes_the_three_columns_together() {
        let conn = db();
        let id = notes::create_note(
            &conn,
            &NewNote {
                content: Some("raw thoughts".into()),
                ..Default::default()
            },
        )
        .unwrap();
        let b = builtin(&conn);

        assert!(record_run(&conn, id, &b.instruction, "# Clean\n\n- one", "raw thoughts").unwrap());

        let note = notes::get_note(&conn, id).unwrap().unwrap();
        assert_eq!(note.polished_body.as_deref(), Some("# Clean\n\n- one"));
        assert_eq!(note.polish_prompt.as_deref(), Some(b.instruction.as_str()));
        assert_eq!(
            note.polished_from_hash.as_deref(),
            Some(content_hash("raw thoughts").as_str())
        );
        assert_eq!(note.content, "raw thoughts", "the raw body is untouched");
    }

    /// The stored fingerprint is of the text the model saw, so a later edit
    /// to the body is detectable by re-hashing the body and comparing — no
    /// second concatenation for two call sites to disagree about.
    #[test]
    fn a_later_edit_no_longer_matches_the_stored_fingerprint() {
        let conn = db();
        let id = notes::create_note(
            &conn,
            &NewNote {
                content: Some("raw thoughts".into()),
                ..Default::default()
            },
        )
        .unwrap();
        record_run(&conn, id, "p", "enhanced", "raw thoughts").unwrap();
        notes::update_note(
            &conn,
            id,
            &notes::NoteUpdate {
                content: Some("raw thoughts, revised".into()),
                ..Default::default()
            },
        )
        .unwrap();
        let note = notes::get_note(&conn, id).unwrap().unwrap();
        assert_ne!(
            note.polished_from_hash.as_deref(),
            Some(content_hash(&note.content).as_str()),
            "the enhancement should read as stale"
        );
    }

    /// A run recorded against a note that no longer exists is `false`, not an
    /// error: someone deleted it while the model was answering, which is a
    /// result rather than a fault.
    #[test]
    fn recording_against_a_deleted_note_is_false() {
        let conn = db();
        assert!(!record_run(&conn, 404, "p", "enhanced", "source").unwrap());
    }

    // -----------------------------------------------------------------------
    // The call, against the loopback stub.
    // -----------------------------------------------------------------------

    /// The system message carries the preamble and then the action's prompt,
    /// the user message is the note exactly, and the reply comes back without
    /// its end marker.
    #[tokio::test]
    async fn the_base_text_and_the_action_prompt_reach_the_model() {
        let stub = stub::chat_once("- Buy milk\n- Collect the dry cleaning", stub::WITH_MARKER).await;
        let mut backend = Backend::sarvam("k", "sarvam-105b");
        backend.base_url = stub.url.clone();
        let prompt = "List every errand as a bullet.";
        let note = "buy milk and uh collect the dry cleaning before six";

        let out = enhance(&reqwest::Client::new(), &backend, prompt, note)
            .await
            .expect("the stub answers with its marker");
        assert_eq!(out, "- Buy milk\n- Collect the dry cleaning");

        let sent = stub.request().await;
        let system = sent["messages"][0]["content"].as_str().unwrap();
        let preamble_at = system.find(ACTION_PREAMBLE).expect("the preamble ships");
        assert!(
            system[preamble_at + ACTION_PREAMBLE.len()..].contains(prompt),
            "the action prompt ships after the preamble: {system}"
        );
        assert_eq!(sent["messages"][1]["content"].as_str().unwrap(), note);
    }

    /// A reply the API admits it cut off must be an `Err`, because the
    /// alternative is storing half an enhancement that looks finished — and
    /// the sentence it fails with must not carry the endpoint's address,
    /// which on the custom slot is a pasted string that can hold a credential.
    #[tokio::test]
    async fn an_enhancement_the_api_cut_off_is_refused() {
        let stub = stub::chat_once_cut_off("# Half a not").await;
        let mut backend = Backend::sarvam("k", "sarvam-105b");
        backend.base_url = stub.url.clone();

        let err = enhance(
            &reqwest::Client::new(),
            &backend,
            BUILTINS[0].instruction,
            "some rough thoughts",
        )
        .await
        .expect_err("a marker-less reply must not be stored")
        .to_string();
        assert!(err.contains("Couldn't finish enhancing"), "got: {err}");
        assert!(
            !err.contains(&stub.host()),
            "the endpoint leaked into a user-facing message: {err}"
        );
    }

    /// The other half of the rule above. A live note action can come back
    /// with `api_finish_reason=stop reply_chars=2716 completion_tokens=571`
    /// against an 8,192 ceiling — complete by the API's own account, 7,600
    /// tokens short of being cut off, and missing only the marker. A
    /// marker-less reply that the API says finished, well short of its
    /// ceiling, is kept rather than refused as truncated.
    #[tokio::test]
    async fn an_enhancement_that_finished_without_its_marker_is_kept() {
        let stub = stub::chat_once("# A whole note", stub::NO_MARKER).await;
        let mut backend = Backend::sarvam("k", "sarvam-105b");
        backend.base_url = stub.url.clone();

        let out = enhance(
            &reqwest::Client::new(),
            &backend,
            BUILTINS[0].instruction,
            "some rough thoughts",
        )
        .await
        .expect("a complete reply must survive a missing marker");
        assert_eq!(out, "# A whole note");
    }

    /// An empty reply is not an enhancement, and must not overwrite whatever
    /// the note already had.
    #[tokio::test]
    async fn an_empty_reply_is_refused() {
        let stub = stub::chat_once("", stub::WITH_MARKER).await;
        let mut backend = Backend::sarvam("k", "sarvam-105b");
        backend.base_url = stub.url.clone();

        let err = enhance(
            &reqwest::Client::new(),
            &backend,
            BUILTINS[0].instruction,
            "some rough thoughts",
        )
        .await
        .expect_err("an empty reply must not be stored")
        .to_string();
        assert!(err.contains("returned nothing"), "got: {err}");
    }
}
