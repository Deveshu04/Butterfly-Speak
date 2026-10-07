//! The opt-in Markdown mirror: one `.md` file per note in a directory the
//! user chose, kept current on every committed write.
//!
//! ## Layout
//!
//! A note lands at `<root>/<folder directory>/<id> <title>.md`. The folder
//! directory is the folder's name made safe for Windows by
//! [`sanitize_component`]; a note in no folder goes to [`UNFILED_DIR`]. The id
//! keeps every name unique and is how a later write finds an old copy after a
//! title change, a move or a folder rename; the title is there for people.
//!
//! A file holds a YAML front-matter header (the note's id, title, folder and
//! its created and updated instants), then the note's text exactly as
//! [`super::body_source`] gives it.
//!
//! ## One way only
//!
//! The app never reads note data back out of a mirror file. It reads the first
//! few bytes of a file only to tell its own copies from the user's files, and
//! it replaces or deletes a file only when the name fits the scheme for that
//! note and the header carries that note's id. A file of the user's own at a
//! copy's name stays as it is, and the write counts as failed.
//!
//! ## Ordering
//!
//! [`commit_then_mirror`] is how every note write reaches the mirror: the
//! database write runs first, and the file work runs only if it succeeded, on
//! the same DB thread, before the command returns. A new copy is written
//! before any old copy is removed, so a failed write leaves the previous file
//! in place.
//!
//! ## Failures and privacy
//!
//! A mirror failure never fails the save. [`sync`] reports [`Counts`], and a
//! pass with failures logs one `warn` holding counts only: never a path, a
//! title or a body, since each of those is the user's own text.

use super::Note;
use rusqlite::{params, Connection, OptionalExtension};
use std::path::{Path, PathBuf};

/// The directory for notes in no folder, named with the word the Notes page
/// uses for them.
pub const UNFILED_DIR: &str = "Unfiled";

/// The file-name stand-in for a title with nothing usable in it.
pub const UNTITLED: &str = "Untitled";

/// Cap, in characters, on each name built from user text: a folder's
/// directory and the title part of a file name. NTFS allows 255 UTF-16 units
/// per component; the longest file name is 19 id digits + 1 space + (80 + 1
/// for a device-name `_`) characters at 2 units each + 3 for `.md` = 185
/// units. A mirror folder path of about 60 characters + an 81-character
/// directory + 1 separator + a 104-character file name = 246, inside the 260
/// of `MAX_PATH`.
pub const MAX_COMPONENT_CHARS: usize = 80;

/// Characters a mirror name may not hold; [`sanitize_component`] turns each
/// into `-`.
fn is_illegal(c: char) -> bool {
    matches!(
        c,
        // Reserved by Windows in every file and directory name.
        '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*'
        // Markdown editors that link notes by file name read these in a
        // name: `#` starts a heading reference, `^` a block reference, and
        // `[[` and `]]` wrap the link itself.
        | '#'
        | '^'
        | '[' | ']'
    )
    // Windows reserves the C0 controls; DEL and the C1 controls go too,
    // since an invisible character is never meant as part of a name.
    || c.is_control()
}

/// The DOS device names. Windows still resolves these as devices in any
/// directory and with any extension, so `CON.md` is not a file you can create.
/// Checked against the stem, case-insensitively.
const RESERVED: &[&str] = &[
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

/// Make `raw` safe to use as one Windows path component.
///
/// Illegal characters and control characters become `-`; leading and trailing
/// whitespace and trailing dots are removed (Windows silently strips them,
/// so a directory named `Notes.` is not the directory you asked for); a
/// reserved device name gains a `_`; the result is capped at `max_chars`
/// `char`s and falls back to `fallback` when nothing survives.
///
/// The cap is applied *before* the trailing-dot trim, so truncating
/// `Roadmap...v2` at the wrong place cannot leave a trailing dot behind.
pub fn sanitize_component(raw: &str, fallback: &str, max_chars: usize) -> String {
    let mapped: String = raw
        .chars()
        .map(|c| if is_illegal(c) { '-' } else { c })
        .collect();

    let capped: String = mapped.chars().take(max_chars).collect();

    let trimmed = capped
        .trim()
        .trim_end_matches(|c: char| c == '.' || c.is_whitespace())
        .trim_start();

    // Nothing the user meant survived if the result is empty *or* is only the
    // `-`s that illegal characters mapped to: `***` and `///` are `---`, not a
    // filename made of dashes. This is "all dashes -> fallback", not "strip
    // trailing dashes": `a?` keeps its real content and stays `a-`.
    if trimmed.is_empty() || trimmed.chars().all(|c| c == '-') {
        return fallback.to_string();
    }

    // The device check looks at the stem: `NUL`, `nul.md` and `Nul.txt` are
    // all the same device.
    let stem = trimmed.split('.').next().unwrap_or(trimmed);
    if RESERVED.iter().any(|r| r.eq_ignore_ascii_case(stem)) {
        return format!("{trimmed}_");
    }
    trimmed.to_string()
}

/// The mirror file name for note `id`: the id, a space, then the title made
/// safe for Windows. Leading with digits also means the name can never be a
/// device name or a hidden dot-file, whatever the title.
pub fn note_file_name(id: i64, title: &str) -> String {
    let title = sanitize_component(title, UNTITLED, MAX_COMPONENT_CHARS);
    format!("{id} {title}.md")
}

/// The directory a note in `folder_name` mirrors into; `None` is unfiled.
pub fn note_dir(root: &Path, folder_name: Option<&str>) -> PathBuf {
    let name = folder_name.unwrap_or(UNFILED_DIR);
    root.join(sanitize_component(name, UNFILED_DIR, MAX_COMPONENT_CHARS))
}

/// Whether `file_name` is what [`note_file_name`] gives note `id`, under any
/// title.
fn named_like_copy_of(file_name: &str, id: i64) -> bool {
    file_name
        .strip_prefix(id.to_string().as_str())
        .and_then(|rest| rest.strip_prefix(' '))
        .and_then(|rest| rest.strip_suffix(".md"))
        .is_some_and(|title| !title.is_empty())
}

// ---------------------------------------------------------------------------
// Frontmatter.
// ---------------------------------------------------------------------------

/// `value` as a YAML double-quoted scalar. Quoting every string means no
/// value can read back as a number, a boolean, a date, null or YAML syntax.
/// Inside the quotes, `"` and `\` are escaped, and so is every character YAML
/// does not allow raw or that a parser could take for a line break.
fn yaml_quoted(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for c in value.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            // Outside YAML's printable set (C0, DEL, C1 including NEL,
            // U+FFFE and U+FFFF), the two Unicode line and paragraph
            // separators, and the byte-order mark.
            '\u{0}'..='\u{1F}'
            | '\u{7F}'..='\u{9F}'
            | '\u{2028}'
            | '\u{2029}'
            | '\u{FEFF}'
            | '\u{FFFE}'
            | '\u{FFFF}' => out.push_str(&format!("\\u{:04X}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Epoch milliseconds as `YYYY-MM-DDTHH:MM:SSZ`.
///
/// The frontmatter dates are the half of the file a note-taking app reads, so
/// they are written as timestamps rather than as the integers this schema
/// stores. Days-from-civil (Howard Hinnant's algorithm) rather than a date
/// crate — this is the only place in the codebase that needs it, and
/// `iso8601_matches_sqlite` checks the arithmetic against SQLite's own
/// `strftime` rather than against numbers written by hand.
fn iso8601_utc(ms: i64) -> String {
    let secs = ms.div_euclid(1000);
    let days = secs.div_euclid(86_400);
    let tod = secs.rem_euclid(86_400);

    // Shift the era so March is month 0 and the leap day lands at the end.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };

    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        tod / 3600,
        (tod % 3600) / 60,
        tod % 60
    )
}

/// The header key that marks a file as the mirror's copy of a note. It is the
/// first line after the opening `---`, so recognising a copy needs only the
/// first few bytes of the file.
const ID_KEY: &str = "butterfly-speak-id";

/// How many bytes are read to recognise a copy: a BOM, the opener with CRLF,
/// the key, the widest id and a line break fit with room to spare.
const PROBE_BYTES: usize = 64;

/// The YAML front-matter header a mirrored note opens with. `folder` is the
/// folder's display name, as the Notes page shows it, not its directory name.
fn header(note: &Note, folder: Option<&str>) -> String {
    let mut out = format!(
        "---\n{ID_KEY}: {}\ntitle: {}\n",
        note.id,
        yaml_quoted(&note.title)
    );
    if let Some(name) = folder {
        out.push_str(&format!("folder: {}\n", yaml_quoted(name)));
    }
    out.push_str(&format!(
        "created: {}\nupdated: {}\n---\n",
        iso8601_utc(note.created_at),
        iso8601_utc(note.updated_at)
    ));
    out
}

/// Whether `head`, the opening bytes of a file, is the header of note `id`'s
/// copy. A UTF-8 byte-order mark and CRLF line endings are accepted, since
/// Windows editors can add both when they save a file.
fn opens_as_copy_of(head: &[u8], id: i64) -> bool {
    fn line_break(bytes: &[u8]) -> Option<&[u8]> {
        bytes.strip_prefix(b"\r\n").or_else(|| bytes.strip_prefix(b"\n"))
    }
    let head = head.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(head);
    let id_line = format!("{ID_KEY}: {id}");
    head.strip_prefix(b"---")
        .and_then(line_break)
        .and_then(|rest| rest.strip_prefix(id_line.as_bytes()))
        .and_then(line_break)
        .is_some()
}

/// Whether the file at `path` is the mirror's copy of note `id`: named like
/// one, and opening with the header that carries `id`.
fn is_copy_of(path: &Path, id: i64) -> bool {
    use std::io::Read;

    let named = path
        .file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| named_like_copy_of(n, id));
    if !named || !path.is_file() {
        return false;
    }
    let mut head = Vec::with_capacity(PROBE_BYTES);
    let read = std::fs::File::open(path)
        .and_then(|file| file.take(PROBE_BYTES as u64).read_to_end(&mut head));
    read.is_ok() && opens_as_copy_of(&head, id)
}

// ---------------------------------------------------------------------------
// What a mirror pass did.
// ---------------------------------------------------------------------------

/// The outcome of one [`sync`], in numbers only — see the module doc on why
/// this is not a list of paths.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Counts {
    pub written: u32,
    pub removed: u32,
    pub failed: u32,
}

impl Counts {
    /// One `warn` per pass that lost something, carrying counts and nothing
    /// else: no path, no title, no body. A user whose mirror directory has
    /// gone (an unplugged drive, a synced folder mid-conflict) gets one line
    /// per save saying how many files could not be written, which is enough to
    /// diagnose and does not turn the log into a second copy of their notes.
    fn log(self) {
        if self.failed > 0 {
            tracing::warn!(
                failed = self.failed,
                written = self.written,
                "notes mirror: {} file(s) could not be written or removed",
                self.failed
            );
        }
    }
}

// ---------------------------------------------------------------------------
// The write path.
// ---------------------------------------------------------------------------

/// Bring `note`'s mirror file up to date, then sweep its other copies. The
/// sweep runs only once the new copy is in place, so a failed write leaves
/// the old one where it was.
fn write_copy(root: &Path, note: &Note, folder: Option<&str>) -> Counts {
    let mut counts = Counts::default();
    let dir = note_dir(root, folder);
    if std::fs::create_dir_all(&dir).is_err() {
        counts.failed += 1;
        return counts;
    }
    let target = dir.join(note_file_name(note.id, &note.title));
    // Something already at that name that is not this note's copy is the
    // user's own (a vault can hold a `3 Budget.md` of its own), and only the
    // app's own copies are this module's to replace.
    if target.exists() && !is_copy_of(&target, note.id) {
        counts.failed += 1;
        return counts;
    }
    let text = header(note, folder) + super::body_source(note);
    if replace_file(&target, text.as_bytes()).is_err() {
        counts.failed += 1;
        return counts;
    }
    counts.written += 1;
    let swept = sweep_copies(root, note.id, Some(&target));
    counts.removed += swept.removed;
    counts.failed += swept.failed;
    counts
}

/// Write `bytes` to a sibling of `target`, then move it over `target`, so
/// `target` holds either all of its old contents or all of the new ones.
fn replace_file(target: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut staged = target.as_os_str().to_owned();
    staged.push(".partial");
    let staged = PathBuf::from(staged);
    let result = std::fs::write(&staged, bytes).and_then(|()| std::fs::rename(&staged, target));
    if result.is_err() {
        let _ = std::fs::remove_file(&staged);
    }
    result
}

/// Delete every copy of note `id` in the root's first-level directories,
/// except `keep`, and remove each directory that leaves empty. Only a file
/// [`is_copy_of`] accepts is deleted, a directory is only removed when empty,
/// and one failed unlink does not stop the others.
fn sweep_copies(root: &Path, id: i64, keep: Option<&Path>) -> Counts {
    // Paths are compared as the filesystem resolves them: on a
    // case-insensitive volume, `Work/1 Plan.md` written after a case-only
    // rename is the same file the listing calls `work/1 plan.md`.
    let resolve = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    let keep = keep.map(resolve);
    let mut counts = Counts::default();
    let Ok(entries) = std::fs::read_dir(root) else {
        counts.failed += 1;
        return counts;
    };
    for dir in entries.flatten().map(|e| e.path()).filter(|p| p.is_dir()) {
        let Ok(files) = std::fs::read_dir(&dir) else {
            counts.failed += 1;
            continue;
        };
        let mut deleted_here = false;
        for path in files.flatten().map(|e| e.path()) {
            if !is_copy_of(&path, id) || keep.as_deref() == Some(resolve(&path).as_path()) {
                continue;
            }
            match std::fs::remove_file(&path) {
                Ok(()) => {
                    counts.removed += 1;
                    deleted_here = true;
                }
                Err(_) => counts.failed += 1,
            }
        }
        if deleted_here {
            // `remove_dir` refuses a directory that still holds anything,
            // which is the only case where it should stay.
            let _ = std::fs::remove_dir(&dir);
        }
    }
    counts
}

/// What the mirror should do once a write has committed.
pub enum Job {
    /// Bring these notes' files up to date from their committed rows.
    Write(Vec<i64>),
    /// Their rows are gone: unlink their files, and drop `dir` if the delete
    /// left it empty.
    Remove { ids: Vec<i64>, dir: Option<String> },
}

/// Apply `job` against the committed database. Always returns counts; never
/// returns an error, because a mirror failure must not fail the save that
/// already succeeded.
pub fn sync(conn: &Connection, root: &Path, job: Job) -> Counts {
    let mut counts = Counts::default();
    match job {
        Job::Write(ids) => {
            for id in ids {
                match super::get_note(conn, id) {
                    // Gone between the commit and here (a delete raced this
                    // save): nothing to write, and nothing wrong either.
                    Ok(None) => {}
                    Ok(Some(note)) => {
                        let folder = note.folder_id.and_then(|f| folder_name(conn, f));
                        let one = write_copy(root, &note, folder.as_deref());
                        counts.written += one.written;
                        counts.removed += one.removed;
                        counts.failed += one.failed;
                    }
                    Err(_) => counts.failed += 1,
                }
            }
        }
        Job::Remove { ids, dir } => {
            for id in ids {
                let one = sweep_copies(root, id, None);
                counts.removed += one.removed;
                counts.failed += one.failed;
            }
            if let Some(name) = dir {
                // `remove_dir`, never `remove_dir_all`: the folder is gone
                // from the database, but anything else the user left in that
                // directory is theirs and is not this module's to delete.
                let _ = std::fs::remove_dir(note_dir(root, Some(&name)));
            }
        }
    }
    counts.log();
    counts
}

/// The folder's name, or `None` if it has none (unfiled) or the read failed.
///
/// A read failure answers "unfiled" rather than an error on purpose: this only
/// ever decides a directory name, and the note still has to be mirrored
/// somewhere.
pub fn folder_name(conn: &Connection, folder_id: i64) -> Option<String> {
    conn.query_row(
        "SELECT name FROM folders WHERE id = ?1",
        params![folder_id],
        |r| r.get::<_, String>(0),
    )
    .optional()
    .ok()
    .flatten()
}

/// **The ordering rule, as the only way to invoke it.**
///
/// Runs `write` against the connection; only if it succeeded, and only when
/// the mirror is on (`root` is `Some`), runs the file half — after the SQLite
/// write returned, on the same DB thread, before the caller's reply is sent.
///
/// `job` is handed the connection and the write's own output, so a caller can
/// mirror exactly what the write touched (the cascaded ids from a folder
/// delete, say) and can read anything else it needs from the committed state.
/// Data that only exists *before* the write — a folder's name, when the write
/// is what deletes it — is captured in the closure's environment instead.
///
/// A failed write mirrors nothing: the file on disk keeps matching the row
/// that is still in the table.
pub fn commit_then_mirror<T, E>(
    conn: &Connection,
    root: Option<&Path>,
    write: impl FnOnce(&Connection) -> Result<T, E>,
    job: impl FnOnce(&Connection, &T) -> Job,
) -> Result<T, E> {
    let out = write(conn)?;
    if let Some(root) = root {
        sync(conn, root, job(conn, &out));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::notes::{
        create_note, delete_folder, delete_note, schema, update_note, NewNote, NoteUpdate,
    };

    // -----------------------------------------------------------------------
    // Test scaffolding.
    // -----------------------------------------------------------------------

    /// A unique directory under the OS temp dir, removed on drop. The crate
    /// has no `tempfile` dependency, so the tests make their own.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(name: &str) -> Self {
            let mut path = std::env::temp_dir();
            path.push(format!(
                "bs-mirror-test-{name}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            std::fs::create_dir_all(&path).expect("create temp mirror dir");
            TempDir(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(schema::SCHEMA).unwrap();
        conn
    }

    fn note(title: &str, content: &str) -> NewNote {
        NewNote {
            title: Some(title.into()),
            content: Some(content.into()),
            ..Default::default()
        }
    }

    /// Every `.md` under the mirror root, as `folder/file` strings, sorted.
    fn tree(root: &Path) -> Vec<String> {
        let mut found = Vec::new();
        for dir in std::fs::read_dir(root).unwrap().flatten() {
            if !dir.path().is_dir() {
                continue;
            }
            let folder = dir.file_name().to_string_lossy().into_owned();
            for file in std::fs::read_dir(dir.path()).unwrap().flatten() {
                found.push(format!(
                    "{folder}/{}",
                    file.file_name().to_string_lossy()
                ));
            }
        }
        found.sort();
        found
    }

    /// Where note `id` titled `title` in `folder` sits, as [`tree`] lists it.
    fn listed(folder: Option<&str>, id: i64, title: &str) -> String {
        let dir = note_dir(Path::new(""), folder);
        format!("{}/{}", dir.display(), note_file_name(id, title))
    }

    /// The same place as a path under `root`.
    fn copy_path(root: &Path, folder: Option<&str>, id: i64, title: &str) -> PathBuf {
        note_dir(root, folder).join(note_file_name(id, title))
    }

    // -----------------------------------------------------------------------
    // The sanitiser table. Every row is a name that reaches the filesystem.
    // -----------------------------------------------------------------------

    #[test]
    fn the_sanitiser_table() {
        // (raw, sanitised-as-a-folder)
        let cases: &[(&str, &str)] = &[
            // Left alone.
            ("Work", "Work"),
            ("Q1 planning", "Q1 planning"),
            ("नोट्स", "नोट्स"),
            // The nine Windows-illegal characters, one at a time.
            ("Q1/Q2", "Q1-Q2"),
            (r"Q1\Q2", "Q1-Q2"),
            ("notes:2026", "notes-2026"),
            ("a<b", "a-b"),
            ("a>b", "a-b"),
            ("a\"b", "a-b"),
            ("a|b", "a-b"),
            ("a?b", "a-b"),
            ("a*b", "a-b"),
            // Link syntax that note-linking Markdown editors read in a name.
            ("#tag", "-tag"),
            ("a^b", "a-b"),
            ("[draft]", "-draft-"),
            // Characters with no special meaning to Windows or to that link
            // syntax stay.
            ("100%", "100%"),
            ("Budget $500 & more!", "Budget $500 & more!"),
            ("(v2) {final} 'quoted'", "(v2) {final} 'quoted'"),
            // DEL and the C1 controls, as well as the C0 ones below.
            ("a\u{7f}b", "a-b"),
            ("a\u{85}b", "a-b"),
            // ASCII control characters.
            ("line\nbreak", "line-break"),
            ("tab\there", "tab-here"),
            // Trailing dots and spaces: Windows strips them silently, so a
            // directory created under this name is not the one asked for.
            ("Notes.", "Notes"),
            ("Notes...", "Notes"),
            ("Notes ", "Notes"),
            (" Notes ", "Notes"),
            ("Notes. . ", "Notes"),
            // Nothing survives -> the fallback.
            ("", UNFILED_DIR),
            ("   ", UNFILED_DIR),
            ("...", UNFILED_DIR),
            ("///", UNFILED_DIR),
            // Reserved DOS device names, in every disguise.
            ("CON", "CON_"),
            ("con", "con_"),
            ("Con", "Con_"),
            ("NUL", "NUL_"),
            ("aux", "aux_"),
            ("PRN", "PRN_"),
            ("COM1", "COM1_"),
            ("com9", "com9_"),
            ("LPT1", "LPT1_"),
            ("lpt9", "lpt9_"),
            ("con.md", "con.md_"),
            // ...and the near-misses that are ordinary names.
            ("COM0", "COM0"),
            ("COM10", "COM10"),
            ("LPT0", "LPT0"),
            ("CONSOLE", "CONSOLE"),
            ("CON2", "CON2"),
        ];
        for (raw, want) in cases {
            assert_eq!(
                &sanitize_component(raw, UNFILED_DIR, MAX_COMPONENT_CHARS),
                want,
                "sanitising {raw:?}"
            );
        }
    }

    #[test]
    fn the_length_cap_counts_characters_and_never_leaves_a_trailing_dot() {
        let long = "a".repeat(MAX_COMPONENT_CHARS * 3);
        assert_eq!(
            sanitize_component(&long, UNFILED_DIR, MAX_COMPONENT_CHARS).chars().count(),
            MAX_COMPONENT_CHARS
        );

        // The cap counts characters: an emoji is one, although it is four
        // bytes and two UTF-16 units.
        let emoji = "🎉".repeat(MAX_COMPONENT_CHARS + 20);
        let capped = sanitize_component(&emoji, UNFILED_DIR, MAX_COMPONENT_CHARS);
        assert_eq!(capped.chars().count(), MAX_COMPONENT_CHARS);
        assert!(capped.chars().all(|c| c == '🎉'));

        // Truncation must not manufacture the trailing dot the trim exists to
        // remove: the cap runs first, the trim second.
        let dotted = format!("{}...tail", "b".repeat(MAX_COMPONENT_CHARS - 3));
        let capped = sanitize_component(&dotted, UNFILED_DIR, MAX_COMPONENT_CHARS);
        assert!(!capped.ends_with('.'), "{capped:?} ends with a dot");
    }

    #[test]
    fn a_file_name_holds_the_id_and_the_title_as_written() {
        assert_eq!(
            note_file_name(23, "Site visit, Whitefield"),
            "23 Site visit, Whitefield.md"
        );
        assert_eq!(note_file_name(24, "बैठक के नोट्स 🎉"), "24 बैठक के नोट्स 🎉.md");
    }

    #[test]
    fn a_title_with_nothing_usable_falls_back() {
        for title in ["", "   ", "...", "///", "***", " . . ", "\n\t"] {
            let name = note_file_name(12, title);
            assert!(name.contains(UNTITLED), "{title:?} gave {name:?}");
            assert!(named_like_copy_of(&name, 12), "{name:?}");
        }
    }

    #[test]
    fn every_file_name_is_one_windows_accepts() {
        let long = "word ".repeat(MAX_COMPONENT_CHARS);
        let emoji = "🎉".repeat(MAX_COMPONENT_CHARS * 2);
        let device_prefix = format!("con.{}", "x".repeat(MAX_COMPONENT_CHARS));
        let titles = [
            "CON",
            "nul.txt",
            "COM1",
            "lpt9",
            "Trailing dots...",
            "trailing space ",
            "a<b>c:d\"e/f\\g|h?i*j",
            "#tag ^ref [link]",
            "line\nbreak\ttab",
            ".hidden",
            &long,
            &emoji,
            &device_prefix,
        ];
        for title in titles {
            let name = note_file_name(3, title);
            assert_eq!(
                sanitize_component(&name, "fallback", usize::MAX),
                name,
                "{title:?} gave {name:?}"
            );
            assert!(!name.chars().any(is_illegal), "{name:?}");
            let stem = name.strip_suffix(".md").unwrap();
            assert!(!stem.ends_with(|c: char| c == '.' || c == ' '), "{name:?}");
        }
    }

    #[test]
    fn the_longest_file_name_fits_one_ntfs_component() {
        // Characters outside the Basic Multilingual Plane take two UTF-16
        // units each, so they make the longest names.
        let widest = "𝄞".repeat(MAX_COMPONENT_CHARS * 3);
        let device = format!("con.{}", "𝄞".repeat(MAX_COMPONENT_CHARS * 3));
        for title in [widest.as_str(), device.as_str()] {
            let name = note_file_name(i64::MAX, title);
            let units = name.encode_utf16().count();
            assert!(units <= 255, "{units} UTF-16 units");
        }
        let dir = sanitize_component(&widest, UNFILED_DIR, MAX_COMPONENT_CHARS);
        assert!(dir.encode_utf16().count() <= 255);
    }

    #[test]
    fn two_notes_with_one_title_get_different_names() {
        assert_ne!(note_file_name(1, "Standup"), note_file_name(2, "Standup"));
        assert_ne!(note_file_name(1, ""), note_file_name(2, ""));
        assert_ne!(note_file_name(1, "CON"), note_file_name(11, "CON"));
    }

    #[test]
    fn a_name_fits_the_scheme_for_its_own_id_only() {
        let name = note_file_name(12, "Plan");
        assert!(named_like_copy_of(&name, 12));
        assert!(named_like_copy_of(&note_file_name(12, "Another title"), 12));
        assert!(!named_like_copy_of(&name, 1));
        assert!(!named_like_copy_of(&name, 2));
        assert!(!named_like_copy_of(&name, 123));
        assert!(!named_like_copy_of("12 Plan.txt", 12));
        assert!(!named_like_copy_of("Plan.md", 12));
        assert!(!named_like_copy_of("12.md", 12));
    }

    #[test]
    fn the_unfiled_directory_is_its_own_sanitised_name() {
        assert_eq!(
            sanitize_component(UNFILED_DIR, UNFILED_DIR, MAX_COMPONENT_CHARS),
            UNFILED_DIR
        );
        assert_eq!(note_dir(Path::new("root"), None), Path::new("root").join(UNFILED_DIR));
        assert_eq!(
            note_dir(Path::new("root"), Some("Q1/Q2")),
            Path::new("root").join("Q1-Q2")
        );
        assert_eq!(
            note_dir(Path::new("root"), Some("   ")),
            Path::new("root").join(UNFILED_DIR)
        );
    }

    // -----------------------------------------------------------------------
    // Frontmatter.
    // -----------------------------------------------------------------------

    /// The next `n` characters as a hex code point.
    fn hex_char(chars: &mut std::str::Chars, n: usize) -> char {
        let digits: String = chars.by_ref().take(n).collect();
        assert_eq!(digits.len(), n, "short hex escape");
        char::from_u32(u32::from_str_radix(&digits, 16).unwrap()).expect("a scalar value")
    }

    /// Characters YAML 1.2 allows raw in a stream (`c-printable`, 5.1).
    fn yaml_printable(c: char) -> bool {
        matches!(c as u32,
            0x09 | 0x0A | 0x0D | 0x20..=0x7E | 0x85 | 0xA0..=0xD7FF | 0xE000..=0xFFFD
            | 0x10000..=0x10FFFF)
    }

    /// Read a one-line YAML 1.2 double-quoted scalar (7.3.1) back to its value,
    /// with the escapes of 5.7.
    fn read_yaml(scalar: &str) -> String {
        let inner = scalar
            .strip_prefix('"')
            .and_then(|s| s.strip_suffix('"'))
            .unwrap_or_else(|| panic!("{scalar:?} is not double-quoted"));
        assert!(
            !inner.contains(|c: char| c == '\n' || c == '\r'),
            "a raw line break would be folded: {scalar:?}"
        );
        let mut out = String::new();
        let mut chars = inner.chars();
        while let Some(c) = chars.next() {
            match c {
                '"' => panic!("an unescaped quote ends the scalar early: {scalar:?}"),
                '\\' => {
                    let escape = chars.next().expect("an escape needs a character");
                    out.push(match escape {
                        '0' => '\0',
                        'a' => '\u{7}',
                        'b' => '\u{8}',
                        't' | '\t' => '\t',
                        'n' => '\n',
                        'v' => '\u{B}',
                        'f' => '\u{C}',
                        'r' => '\r',
                        'e' => '\u{1B}',
                        ' ' => ' ',
                        '"' => '"',
                        '/' => '/',
                        '\\' => '\\',
                        'N' => '\u{85}',
                        '_' => '\u{A0}',
                        'L' => '\u{2028}',
                        'P' => '\u{2029}',
                        'x' => hex_char(&mut chars, 2),
                        'u' => hex_char(&mut chars, 4),
                        'U' => hex_char(&mut chars, 8),
                        other => panic!("\\{other} is not a YAML escape"),
                    });
                }
                c => {
                    assert!(yaml_printable(c), "{c:?} may not appear raw");
                    out.push(c);
                }
            }
        }
        out
    }

    /// Every string value is double-quoted, so nothing can be read as another
    /// type or as YAML syntax. Expected forms follow the YAML 1.2 spec.
    #[test]
    fn header_strings_read_back_exactly_under_yaml() {
        let cases: &[(&str, &str)] = &[
            ("", r#""""#),
            ("  padded  ", r#""  padded  ""#),
            ("key: value # comment", r#""key: value # comment""#),
            ("- dash", r#""- dash""#),
            ("? question", r#""? question""#),
            ("@at", r#""@at""#),
            ("`tick", r#""`tick""#),
            ("!tag", r#""!tag""#),
            ("&anchor", r#""&anchor""#),
            ("*alias", r#""*alias""#),
            ("[flow", r#""[flow""#),
            ("{flow", r#""{flow""#),
            (r#"say "hi""#, r#""say \"hi\"""#),
            ("it's", r#""it's""#),
            (r"C:\notes", r#""C:\\notes""#),
            ("two\nlines", r#""two\nlines""#),
            ("carriage\rreturn", r#""carriage\rreturn""#),
            ("tab\there", r#""tab\there""#),
            ("true", r#""true""#),
            ("null", r#""null""#),
            ("~", r#""~""#),
            ("123", r#""123""#),
            ("1e3", r#""1e3""#),
            ("2026-01-01", r#""2026-01-01""#),
            ("नमस्ते दुनिया", "\"नमस्ते दुनिया\""),
            ("🎉 party", "\"🎉 party\""),
            ("bell\u{7}", r#""bell\u0007""#),
            ("del\u{7F}", r#""del\u007F""#),
            ("\u{85}next line", r#""\u0085next line""#),
            ("line\u{2028}separator", r#""line\u2028separator""#),
            ("\u{FEFF}bom", r#""\uFEFFbom""#),
        ];
        for (value, want) in cases {
            let quoted = yaml_quoted(value);
            assert_eq!(&quoted, want, "quoting {value:?}");
            assert_eq!(&read_yaml(&quoted), value, "reading back {quoted:?}");
        }
    }

    /// The date arithmetic, checked against SQLite's calendar rather than
    /// against numbers typed into this file: leap day, century non-leap,
    /// 400-year leap, the epoch, and a pre-epoch instant.
    #[test]
    fn iso8601_matches_sqlite() {
        let conn = Connection::open_in_memory().unwrap();
        for ms in [
            0_i64,
            1_000,
            951_782_400_000,   // 2000-02-29, a 400-year leap day
            1_078_012_800_000, // 2004-02-29
            4_107_542_400_000, // 2100-03-01 (2100 is not a leap year)
            1_757_116_800_123, // an ordinary 2025 instant, with sub-second junk
            -86_400_000,       // 1969-12-31
        ] {
            let want: String = conn
                .query_row(
                    "SELECT strftime('%Y-%m-%dT%H:%M:%SZ', ?1 / 1000, 'unixepoch')",
                    params![ms],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(iso8601_utc(ms), want, "for {ms} ms");
        }
    }

    /// The lines between a file's opening and closing `---`.
    fn header_lines(text: &str) -> Vec<&str> {
        let rest = text.strip_prefix("---\n").expect("the file opens with ---");
        let end = rest.find("\n---\n").expect("the header closes with ---");
        rest[..end].lines().collect()
    }

    /// The raw value of `key` in a file's header, if the key is there.
    fn header_value<'a>(text: &'a str, key: &str) -> Option<&'a str> {
        header_lines(text)
            .into_iter()
            .find_map(|line| line.strip_prefix(key)?.strip_prefix(": "))
    }

    #[test]
    fn the_header_carries_the_id_the_title_the_folder_and_both_instants() {
        let dir = TempDir::new("header");
        let conn = db();
        let folder = crate::notes::create_folder(&conn, "Q1/Q2").unwrap();
        let title = "Plan: \"v2\" # final";
        let id = create_note(
            &conn,
            &NewNote {
                folder_id: Some(folder.id),
                title: Some(title.into()),
                content: Some("body text".into()),
                created_at: Some(1_757_116_800_000),
                updated_at: Some(1_757_120_400_000),
                ..Default::default()
            },
        )
        .unwrap();
        sync(&conn, dir.path(), Job::Write(vec![id]));

        let bytes = std::fs::read(copy_path(dir.path(), Some("Q1/Q2"), id, title)).unwrap();
        assert!(!bytes.starts_with(b"\xEF\xBB\xBF"), "no byte-order mark");
        let text = String::from_utf8(bytes).expect("UTF-8");
        assert!(text.ends_with("\n---\nbody text"), "{text}");
        assert_eq!(header_lines(&text)[0], format!("{ID_KEY}: {id}"), "the id comes first");
        assert_eq!(read_yaml(header_value(&text, "title").unwrap()), title);
        // The folder is named as the Notes page shows it, not as the
        // directory the file sits in.
        assert_eq!(read_yaml(header_value(&text, "folder").unwrap()), "Q1/Q2");
        assert_eq!(header_value(&text, "created"), Some("2025-09-06T00:00:00Z"));
        assert_eq!(header_value(&text, "updated"), Some("2025-09-06T01:00:00Z"));
    }

    #[test]
    fn an_unfiled_note_names_no_folder() {
        let dir = TempDir::new("header-unfiled");
        let conn = db();
        let id = create_note(&conn, &note("Loose", "body")).unwrap();
        sync(&conn, dir.path(), Job::Write(vec![id]));
        let text = std::fs::read_to_string(copy_path(dir.path(), None, id, "Loose")).unwrap();
        assert_eq!(header_value(&text, "folder"), None);
        assert_eq!(read_yaml(header_value(&text, "title").unwrap()), "Loose");
    }

    #[test]
    fn a_copy_is_known_by_its_name_and_its_header() {
        let dir = TempDir::new("recognise");
        let conn = db();
        let id = create_note(&conn, &note("Recognised", "body")).unwrap();
        sync(&conn, dir.path(), Job::Write(vec![id]));
        let path = copy_path(dir.path(), None, id, "Recognised");
        let written = std::fs::read(&path).unwrap();
        assert!(is_copy_of(&path, id), "a file the mirror wrote");

        // Windows editors may add a byte-order mark and CRLF line endings
        // when they save; the copy is still ours.
        let with_bom = |b: &[u8]| [b"\xEF\xBB\xBF".as_slice(), b].concat();
        let crlf = String::from_utf8(written.clone())
            .unwrap()
            .replace('\n', "\r\n")
            .into_bytes();
        let other = copy_path(dir.path(), None, id, "Variant");
        for (label, bytes) in [
            ("BOM", with_bom(&written)),
            ("CRLF", crlf.clone()),
            ("BOM and CRLF", with_bom(&crlf)),
        ] {
            std::fs::write(&other, bytes).unwrap();
            assert!(is_copy_of(&other, id), "{label}");
        }

        let for_another_note = String::from_utf8(written.clone())
            .unwrap()
            .replacen(&format!("{ID_KEY}: {id}\n"), &format!("{ID_KEY}: {}\n", id + 1), 1)
            .into_bytes();
        for (label, bytes) in [
            ("a user's Markdown file", b"# Notes\n\nmine\n".to_vec()),
            ("a user's own front matter", b"---\ntitle: mine\n---\nmine\n".to_vec()),
            ("an empty file", Vec::new()),
            ("the header of another note", for_another_note),
        ] {
            std::fs::write(&other, bytes).unwrap();
            assert!(!is_copy_of(&other, id), "{label}");
        }
        std::fs::remove_file(&other).unwrap();
        assert!(!is_copy_of(&other, id), "a missing file");

        // The right header under another note's name is not a copy of this one.
        let misnamed = copy_path(dir.path(), None, id + 1, "Recognised");
        std::fs::write(&misnamed, &written).unwrap();
        assert!(!is_copy_of(&misnamed, id));
    }

    #[test]
    fn a_filed_note_lands_in_its_folders_directory() {
        let dir = TempDir::new("filed");
        let conn = db();
        let farm = crate::notes::create_folder(&conn, "Khet").unwrap();
        let id = create_note(
            &conn,
            &NewNote {
                folder_id: Some(farm.id),
                title: Some("Rabi crop plan".into()),
                content: Some("# Sowing\n\nWheat by mid-November.".into()),
                ..Default::default()
            },
        )
        .unwrap();

        let counts = sync(&conn, dir.path(), Job::Write(vec![id]));
        assert_eq!(counts, Counts { written: 1, removed: 0, failed: 0 });
        assert_eq!(tree(dir.path()), vec![listed(Some("Khet"), id, "Rabi crop plan")]);
        let text =
            std::fs::read_to_string(copy_path(dir.path(), Some("Khet"), id, "Rabi crop plan"))
                .unwrap();
        assert!(text.ends_with("# Sowing\n\nWheat by mid-November."), "{text}");
    }

    #[test]
    fn renaming_a_folder_moves_its_notes_and_drops_the_old_directory() {
        let dir = TempDir::new("folder-rename");
        let conn = db();
        let work = crate::notes::create_folder(&conn, "Work").unwrap();
        let ids: Vec<i64> = ["One", "Two"]
            .iter()
            .map(|t| {
                create_note(
                    &conn,
                    &NewNote {
                        folder_id: Some(work.id),
                        title: Some((*t).into()),
                        ..Default::default()
                    },
                )
                .unwrap()
            })
            .collect();
        sync(&conn, dir.path(), Job::Write(ids.clone()));

        crate::notes::rename_folder(&conn, work.id, "Projects").unwrap();
        let counts = sync(&conn, dir.path(), Job::Write(ids.clone()));

        assert_eq!(counts, Counts { written: 2, removed: 2, failed: 0 });
        let mut want = vec![
            listed(Some("Projects"), ids[0], "One"),
            listed(Some("Projects"), ids[1], "Two"),
        ];
        want.sort();
        assert_eq!(tree(dir.path()), want);
        assert!(!note_dir(dir.path(), Some("Work")).exists());
    }

    /// Two folder names can sanitise to one directory. Deleting one folder
    /// takes only its own notes' copies, and the directory stays while the
    /// other folder's copy is in it.
    #[test]
    fn two_folders_sharing_a_directory_leave_each_other_alone() {
        let dir = TempDir::new("shared-dir");
        let conn = db();
        let slash = crate::notes::create_folder(&conn, "Q1/Q2").unwrap();
        let dash = crate::notes::create_folder(&conn, "Q1-Q2").unwrap();
        let a = create_note(
            &conn,
            &NewNote { folder_id: Some(slash.id), ..note("Same title", "a") },
        )
        .unwrap();
        let b = create_note(
            &conn,
            &NewNote { folder_id: Some(dash.id), ..note("Same title", "b") },
        )
        .unwrap();
        sync(&conn, dir.path(), Job::Write(vec![a, b]));
        assert_eq!(tree(dir.path()).len(), 2);

        let gone = crate::notes::delete_folder(&conn, slash.id).unwrap();
        let counts = sync(
            &conn,
            dir.path(),
            Job::Remove { ids: gone, dir: Some("Q1/Q2".into()) },
        );

        assert_eq!(counts, Counts { written: 0, removed: 1, failed: 0 });
        assert_eq!(tree(dir.path()), vec![listed(Some("Q1-Q2"), b, "Same title")]);
    }

    #[test]
    fn a_failed_write_leaves_the_old_copy_in_place() {
        let dir = TempDir::new("write-fails");
        let conn = db();
        let id = create_note(&conn, &note("First", "body")).unwrap();
        sync(&conn, dir.path(), Job::Write(vec![id]));

        update_note(
            &conn,
            id,
            &NoteUpdate { title: Some("Second".into()), ..Default::default() },
        )
        .unwrap();
        // A directory where the new file should go makes the write fail.
        std::fs::create_dir(copy_path(dir.path(), None, id, "Second")).unwrap();
        let counts = sync(&conn, dir.path(), Job::Write(vec![id]));

        assert_eq!(counts, Counts { written: 0, removed: 0, failed: 1 });
        assert!(copy_path(dir.path(), None, id, "First").is_file(), "the old copy stays");
    }

    #[test]
    fn an_unusable_root_counts_a_failure_and_does_not_panic() {
        let dir = TempDir::new("bad-root");
        let conn = db();
        let id = create_note(&conn, &note("Nowhere", "body")).unwrap();
        // A root that is a file cannot hold directories.
        let root = dir.path().join("not-a-directory");
        std::fs::write(&root, "x").unwrap();

        let counts = sync(&conn, &root, Job::Write(vec![id]));
        assert_eq!(counts, Counts { written: 0, removed: 0, failed: 1 });
        let counts = sync(&conn, &root, Job::Remove { ids: vec![id], dir: None });
        assert_eq!(counts.failed, 1);
        assert_eq!(counts.removed, 0);
    }

    /// Windows names are case-insensitive, so after a change of case alone
    /// the new copy and the old one can be the same file. It must survive
    /// its own sweep.
    #[test]
    fn a_change_of_case_alone_keeps_exactly_one_copy() {
        let dir = TempDir::new("case");
        let conn = db();
        let work = crate::notes::create_folder(&conn, "work").unwrap();
        let id = create_note(
            &conn,
            &NewNote { folder_id: Some(work.id), ..note("plan", "body") },
        )
        .unwrap();
        sync(&conn, dir.path(), Job::Write(vec![id]));

        crate::notes::rename_folder(&conn, work.id, "Work").unwrap();
        let counts = sync(&conn, dir.path(), Job::Write(vec![id]));
        assert_eq!((counts.written, counts.failed), (1, 0));
        assert_eq!(tree(dir.path()).len(), 1, "{:?}", tree(dir.path()));

        update_note(
            &conn,
            id,
            &NoteUpdate { title: Some("Plan".into()), ..Default::default() },
        )
        .unwrap();
        let counts = sync(&conn, dir.path(), Job::Write(vec![id]));
        assert_eq!((counts.written, counts.failed), (1, 0));
        let files = tree(dir.path());
        assert_eq!(files.len(), 1, "{files:?}");
        let text = std::fs::read_to_string(dir.path().join(&files[0])).unwrap();
        assert_eq!(read_yaml(header_value(&text, "title").unwrap()), "Plan");
    }

    /// A copy held open by another program cannot be deleted. That one unlink
    /// fails and is counted; the sweep still removes the other copies.
    #[cfg(windows)]
    #[test]
    fn a_locked_copy_is_counted_and_the_rest_are_still_removed() {
        use std::os::windows::fs::OpenOptionsExt;

        let dir = TempDir::new("locked");
        let conn = db();
        let id = create_note(&conn, &note("Held", "body")).unwrap();
        sync(&conn, dir.path(), Job::Write(vec![id]));
        let written = copy_path(dir.path(), None, id, "Held");
        for folder in ["Old one", "Old two"] {
            std::fs::create_dir(note_dir(dir.path(), Some(folder))).unwrap();
            std::fs::copy(&written, copy_path(dir.path(), Some(folder), id, "Held")).unwrap();
        }
        let held = copy_path(dir.path(), Some("Old one"), id, "Held");
        let lock = std::fs::OpenOptions::new()
            .read(true)
            // Other programs may read it, but not delete it.
            .share_mode(1)
            .open(&held)
            .unwrap();

        delete_note(&conn, id).unwrap();
        let counts = sync(&conn, dir.path(), Job::Remove { ids: vec![id], dir: None });
        assert_eq!(counts, Counts { written: 0, removed: 2, failed: 1 });
        assert!(held.exists());
        drop(lock);
        assert_eq!(tree(dir.path()), vec![listed(Some("Old one"), id, "Held")]);
    }

    /// A folder name Windows would refuse still gets a directory: its
    /// sanitised form.
    #[test]
    fn a_folder_name_windows_rejects_still_mirrors() {
        for (raw, expected_dir) in [
            ("Q1/Q2", "Q1-Q2"),
            ("CON", "CON_"),
            ("notes:2026", "notes-2026"),
            (r"back\slash", "back-slash"),
            ("trailing.", "trailing"),
        ] {
            let dir = TempDir::new("hostile");
            let conn = db();
            let folder = crate::notes::create_folder(&conn, raw).unwrap();
            let id = create_note(
                &conn,
                &NewNote {
                    folder_id: Some(folder.id),
                    title: Some("Note".into()),
                    ..Default::default()
                },
            )
            .unwrap();

            let counts = sync(&conn, dir.path(), Job::Write(vec![id]));
            assert_eq!(counts.failed, 0, "folder {raw:?} failed to mirror");
            assert_eq!(
                tree(dir.path()),
                vec![format!("{expected_dir}/{}", note_file_name(id, "Note"))]
            );
        }
    }

    #[test]
    fn an_unfiled_note_goes_to_the_fallback_directory() {
        let dir = TempDir::new("unfiled");
        let conn = db();
        let id = create_note(&conn, &note("Loose thought", "body")).unwrap();
        sync(&conn, dir.path(), Job::Write(vec![id]));
        assert_eq!(
            tree(dir.path()),
            vec![listed(None, id, "Loose thought")]
        );
    }

    #[test]
    fn the_mirrored_body_is_the_polished_version_when_there_is_one() {
        let dir = TempDir::new("enhanced");
        let conn = db();
        let id = create_note(&conn, &note("Draft", "raw dictation")).unwrap();
        update_note(
            &conn,
            id,
            &NoteUpdate {
                polished_body: Some(Some("# Polished\n\nprose".into())),
                ..Default::default()
            },
        )
        .unwrap();

        sync(&conn, dir.path(), Job::Write(vec![id]));
        let text = std::fs::read_to_string(copy_path(dir.path(), None, id, "Draft")).unwrap();
        assert!(text.ends_with("# Polished\n\nprose"), "{text}");
        assert!(!text.contains("raw dictation"));
    }

    /// A title change renames the copy: the new file is written and the old
    /// one swept, leaving one.
    #[test]
    fn renaming_a_note_leaves_exactly_one_file() {
        let dir = TempDir::new("rename");
        let conn = db();
        let id = create_note(&conn, &note("First title", "body")).unwrap();
        sync(&conn, dir.path(), Job::Write(vec![id]));

        update_note(
            &conn,
            id,
            &NoteUpdate {
                title: Some("Second title".into()),
                ..Default::default()
            },
        )
        .unwrap();
        let counts = sync(&conn, dir.path(), Job::Write(vec![id]));

        assert_eq!(counts, Counts { written: 1, removed: 1, failed: 0 });
        assert_eq!(
            tree(dir.path()),
            vec![listed(None, id, "Second title")]
        );
    }

    /// Moving a note between folders is the case the whole-tree scan exists
    /// for: the stale file is in a *different* directory, and the directory it
    /// leaves behind is pruned only because it is now empty.
    #[test]
    fn moving_a_note_between_folders_takes_its_file_with_it() {
        let dir = TempDir::new("move");
        let conn = db();
        let work = crate::notes::create_folder(&conn, "Work").unwrap();
        let personal = crate::notes::create_folder(&conn, "Personal life").unwrap();
        let id = create_note(
            &conn,
            &NewNote {
                folder_id: Some(work.id),
                title: Some("Idea".into()),
                ..Default::default()
            },
        )
        .unwrap();
        sync(&conn, dir.path(), Job::Write(vec![id]));
        assert_eq!(tree(dir.path()), vec![listed(Some("Work"), id, "Idea")]);

        update_note(
            &conn,
            id,
            &NoteUpdate {
                folder_id: Some(Some(personal.id)),
                ..Default::default()
            },
        )
        .unwrap();
        sync(&conn, dir.path(), Job::Write(vec![id]));

        assert_eq!(tree(dir.path()), vec![listed(Some("Personal life"), id, "Idea")]);
        assert!(
            !note_dir(dir.path(), Some("Work")).exists(),
            "the emptied directory should not linger"
        );
    }

    /// A file the user put in the mirror directory is not this module's to
    /// delete, even when its name fits the scheme for that note.
    #[test]
    fn the_sweep_leaves_a_file_that_is_not_ours_alone() {
        let dir = TempDir::new("foreign");
        let conn = db();
        let id = create_note(&conn, &note("Mine", "body")).unwrap();
        sync(&conn, dir.path(), Job::Write(vec![id]));

        let foreign = copy_path(dir.path(), None, id, "Hand written");
        std::fs::write(&foreign, "# Something the user wrote\n").unwrap();

        update_note(
            &conn,
            id,
            &NoteUpdate {
                title: Some("Mine, renamed".into()),
                ..Default::default()
            },
        )
        .unwrap();
        sync(&conn, dir.path(), Job::Write(vec![id]));

        assert!(foreign.exists(), "a file without the mirror header must survive");
        let mut want = vec![
            listed(None, id, "Hand written"),
            listed(None, id, "Mine, renamed"),
        ];
        want.sort();
        assert_eq!(tree(dir.path()), want);
    }

    /// A mirror pointed at an existing vault can meet a file of the user's
    /// own at the very name a note's copy would take. That file is not this
    /// module's to replace: the write counts as failed and the file keeps what
    /// the user wrote.
    #[test]
    fn a_users_own_file_at_the_copys_name_is_not_overwritten() {
        let dir = TempDir::new("occupied");
        let conn = db();
        let id = create_note(&conn, &note("Budget", "the note")).unwrap();
        let theirs = copy_path(dir.path(), None, id, "Budget");
        std::fs::create_dir_all(theirs.parent().unwrap()).unwrap();
        std::fs::write(&theirs, "# My own budget\n").unwrap();

        let counts = sync(&conn, dir.path(), Job::Write(vec![id]));

        assert_eq!(counts, Counts { written: 0, removed: 0, failed: 1 });
        assert_eq!(std::fs::read_to_string(&theirs).unwrap(), "# My own budget\n");
        assert_eq!(tree(dir.path()), vec![listed(None, id, "Budget")]);
    }

    #[test]
    fn deleting_a_note_unlinks_its_file_and_prunes_the_directory() {
        let dir = TempDir::new("delete");
        let conn = db();
        let id = create_note(&conn, &note("Doomed", "body")).unwrap();
        sync(&conn, dir.path(), Job::Write(vec![id]));

        delete_note(&conn, id).unwrap();
        let counts = sync(
            &conn,
            dir.path(),
            Job::Remove { ids: vec![id], dir: None },
        );

        assert_eq!(counts, Counts { written: 0, removed: 1, failed: 0 });
        assert!(tree(dir.path()).is_empty());
        assert!(!note_dir(dir.path(), None).exists());
    }

    #[test]
    fn deleting_a_folder_unlinks_every_note_in_it() {
        let dir = TempDir::new("folder-delete");
        let conn = db();
        let work = crate::notes::create_folder(&conn, "Work").unwrap();
        let ids: Vec<i64> = ["One", "Two", "Three"]
            .iter()
            .map(|t| {
                create_note(
                    &conn,
                    &NewNote {
                        folder_id: Some(work.id),
                        title: Some((*t).into()),
                        ..Default::default()
                    },
                )
                .unwrap()
            })
            .collect();
        sync(&conn, dir.path(), Job::Write(ids.clone()));
        assert_eq!(tree(dir.path()).len(), 3);

        let cascaded = delete_folder(&conn, work.id).unwrap();
        let counts = sync(
            &conn,
            dir.path(),
            Job::Remove {
                ids: cascaded,
                dir: Some("Work".into()),
            },
        );

        assert_eq!(counts.removed, 3);
        assert_eq!(counts.failed, 0);
        assert!(!note_dir(dir.path(), Some("Work")).exists());
    }

    /// A note deleted between the commit and the mirror pass is not a failure
    /// — there is simply nothing left to write.
    #[test]
    fn a_row_that_vanished_is_not_counted_as_a_failure() {
        let dir = TempDir::new("vanished");
        let conn = db();
        let counts = sync(&conn, dir.path(), Job::Write(vec![9999]));
        assert_eq!(counts, Counts::default());
        assert!(tree(dir.path()).is_empty());
    }

    // -----------------------------------------------------------------------
    // The ordering rule.
    // -----------------------------------------------------------------------

    /// The mirror step runs **after** the SQLite write, and reads the
    /// committed row: the file's body is the one the write put in the table,
    /// which it could only see if the write had already happened.
    ///
    /// The stub is the `write` closure — it records when it ran, and the
    /// `job`/mirror half is production code.
    #[test]
    fn the_mirror_runs_after_the_write_and_sees_the_committed_row() {
        let dir = TempDir::new("ordering");
        let conn = db();
        let mut order: Vec<&str> = Vec::new();

        let id = commit_then_mirror(
            &conn,
            Some(dir.path()),
            |c| {
                order.push("write");
                create_note(c, &note("Ordered", "committed body")).map_err(|e| e.to_string())
            },
            |c, id| {
                // Reading the row back through the same connection is the
                // assertion: an uncommitted write would not be here.
                assert!(
                    crate::notes::get_note(c, *id).unwrap().is_some(),
                    "the mirror step must see the committed row"
                );
                Job::Write(vec![*id])
            },
        )
        .unwrap();

        assert_eq!(order, vec!["write"]);
        let text = std::fs::read_to_string(copy_path(dir.path(), None, id, "Ordered")).unwrap();
        assert!(text.ends_with("committed body"), "{text}");
    }

    /// A write that failed mirrors nothing: the file on disk keeps matching
    /// the row still in the table.
    #[test]
    fn a_failed_write_mirrors_nothing() {
        let dir = TempDir::new("failed-write");
        let conn = db();
        let mut mirrored = false;

        let result: Result<i64, String> = commit_then_mirror(
            &conn,
            Some(dir.path()),
            |_| Err("the write failed".to_string()),
            |_, _| {
                mirrored = true;
                Job::Write(vec![])
            },
        );

        assert_eq!(result, Err("the write failed".into()));
        assert!(!mirrored, "the mirror must not run for a failed write");
        assert!(tree(dir.path()).is_empty());
    }

    /// The mirror is off: the write still happens, and not one byte is
    /// written outside the database.
    #[test]
    fn the_mirror_being_off_writes_no_file_at_all() {
        let dir = TempDir::new("off");
        let conn = db();
        let mut asked = false;

        let id = commit_then_mirror(
            &conn,
            None,
            |c| create_note(c, &note("Private", "body")).map_err(|e| e.to_string()),
            |_, id| {
                asked = true;
                Job::Write(vec![*id])
            },
        )
        .unwrap();

        assert!(crate::notes::get_note(&conn, id).unwrap().is_some());
        assert!(!asked, "an off mirror should not even be asked what to do");
        assert!(tree(dir.path()).is_empty());
    }

    /// **The action path.** `commands::run_note_action` stores an enhancement
    /// through `actions::record_run`, and the webview adopts the row it
    /// answers with rather than saving again — so that write is the only one
    /// that can carry the file. [`super::body_source`] selects
    /// `polished_body`, so a run that skipped the mirror left the `.md`
    /// holding the raw dictation while the app showed the action's result.
    ///
    /// The closures are the command's own, verbatim; only the `State<Backend>`
    /// it needs (and a unit test cannot build) is absent.
    #[test]
    fn an_action_rewrites_the_mirrored_body_to_the_enhanced_text() {
        let dir = TempDir::new("action");
        let conn = db();
        const RAW: &str = "um so yeah we shipped";

        let id: i64 = commit_then_mirror(
            &conn,
            Some(dir.path()),
            |c| create_note(c, &note("Standup", RAW)).map_err(|e| e.to_string()),
            |_, id| Job::Write(vec![*id]),
        )
        .unwrap();
        let path = copy_path(dir.path(), None, id, "Standup");
        assert!(std::fs::read_to_string(&path).unwrap().ends_with(RAW));

        commit_then_mirror(
            &conn,
            Some(dir.path()),
            |c| {
                crate::notes::actions::record_run(c, id, "Tidy it", "# Standup\n\nWe shipped.", RAW)
                    .map_err(|e| e.to_string())
            },
            |_, _| Job::Write(vec![id]),
        )
        .unwrap();

        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.ends_with("# Standup\n\nWe shipped."), "{text}");
        assert!(!text.contains(RAW), "the raw dictation must not survive in the file");
        assert_eq!(tree(dir.path()), vec![listed(None, id, "Standup")]);
    }

    /// **The auto-title path.** The title is in a mirrored file's name and in
    /// its header, so a new title has to rename the file and rewrite the
    /// header in the same write. The webview adopts the title the command
    /// returns without saving again, so this write is the only one that can.
    #[test]
    fn auto_title_renames_the_mirrored_file_and_rewrites_its_frontmatter() {
        let dir = TempDir::new("auto-title");
        let conn = db();

        let id: i64 = commit_then_mirror(
            &conn,
            Some(dir.path()),
            |c| {
                create_note(c, &note("Imported recording", "we agreed to ship on friday"))
                    .map_err(|e| e.to_string())
            },
            |_, id| Job::Write(vec![*id]),
        )
        .unwrap();
        assert_eq!(
            tree(dir.path()),
            vec![listed(None, id, "Imported recording")]
        );

        commit_then_mirror(
            &conn,
            Some(dir.path()),
            |c| {
                update_note(
                    c,
                    id,
                    &NoteUpdate {
                        title: Some("Ship date agreed".into()),
                        ..Default::default()
                    },
                )
                .map_err(|e| e.to_string())
            },
            |_, _| Job::Write(vec![id]),
        )
        .unwrap();

        assert_eq!(
            tree(dir.path()),
            vec![listed(None, id, "Ship date agreed")],
            "the file under the old title must not linger"
        );
        let text =
            std::fs::read_to_string(copy_path(dir.path(), None, id, "Ship date agreed")).unwrap();
        assert_eq!(
            header_value(&text, "title").map(read_yaml).as_deref(),
            Some("Ship date agreed"),
            "the header's title is the new one: {text}"
        );
    }

    /// End to end, through the real `history` DB thread: the mirror write runs
    /// on that thread, and the file exists by the time `with_connection`
    /// returns — i.e. before the reply reaches the webview.
    #[test]
    fn the_file_exists_before_the_caller_hears_back() {
        let dir = TempDir::new("db-thread");
        let mut db_path = std::env::temp_dir();
        db_path.push(format!(
            "bs-mirror-thread-{}-{}.db",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let recorder = crate::history::spawn(
            db_path.clone(),
            crate::history::RetentionCfg { enabled: true, keep_days: 0 },
        );

        let root = dir.path().to_path_buf();
        let (id, thread_name) = recorder
            .with_connection(move |conn| {
                let id: i64 = commit_then_mirror(
                    conn,
                    Some(&root),
                    |c| create_note(c, &note("On the tail", "body")).map_err(|e| e.to_string()),
                    |_, id| Job::Write(vec![*id]),
                )
                .unwrap();
                (
                    id,
                    std::thread::current().name().unwrap_or_default().to_string(),
                )
            })
            .expect("the DB thread answered");

        assert_eq!(thread_name, "history-db", "the mirror ran on the DB thread");
        assert!(
            copy_path(dir.path(), None, id, "On the tail").exists(),
            "the file must already be there when the command returns"
        );

        drop(recorder);
        let _ = std::fs::remove_file(&db_path);
    }
}
