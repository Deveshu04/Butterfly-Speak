//! The window's own fill colour, chosen before the webview has painted.
//!
//! # The frame this closes
//!
//! The pre-paint script in `src/app.html` keeps the first *painted* frame
//! dark for a dark-theme user. What it cannot reach is the frame before that:
//! between `main.show()` and Chromium producing anything, the window is filled
//! by whoever registered the fill. `tao` registers a null `hbrBackground` and
//! `wry` passes `None` to `create_controller`, so without this module
//! WebView2 falls back to its own default, white, and a dark-theme user sees
//! a white flash on every launch.
//!
//! `tauri.conf.json`'s `backgroundColor` cannot fix it: it is a single static
//! `Color` baked at config parse (`tauri-utils-2.9.3/src/config.rs:1693`),
//! so any literal there matches one theme and misses the other.
//!
//! # Why a sidecar file
//!
//! The theme preference lives in `localStorage`, and that is not an accident
//! to be undone — it is what makes the pre-paint stamp possible at all, since
//! settings arrive over an async IPC round-trip after the window is already on
//! screen (see the header of `src/lib/theme.svelte.ts`). But Rust cannot read
//! WebView2's `localStorage`, and `setup()` runs before any webview exists.
//!
//! So the frontend mirrors its *resolved* theme — never `auto` — into a
//! one-word file beside `settings.json`, and `setup()` reads that. The file is
//! a hint, not a source of truth: it is written after the fact by the window
//! it describes, so on a first launch it does not exist, and after the user
//! changes theme it describes the previous launch until the frontend catches
//! up (which it does on mount, before the change can matter to a later boot).
//! Every failure mode falls back to [`WebviewWindow::theme`], which on Windows
//! is `AppsUseLightTheme` — the same thing `prefers-color-scheme` resolves
//! `auto` against, so the fallback agrees with the frontend for every user who
//! has not overridden it.
//!
//! Deliberately not in `settings.rs`: this is not a setting. Nothing reads it
//! but the fill below, it never round-trips through `Settings`, and putting it
//! there would put a second source of truth for the theme into the exported
//! settings JSON. It is the same shape as `lib.rs`'s `background-notice-shown`
//! marker, and for the same stated reason — a file beside the settings keeps a
//! one-way fact out of the settings the UI round-trips.

use std::path::PathBuf;
use tauri::window::Color;
use tauri::{Theme, WebviewWindow};

/// The `--bg` token from `src/styles/tokens.css`, light theme: `#f4f3f0`,
/// written there as a hex literal.
const LIGHT_FILL: Color = Color(0xf4, 0xf3, 0xf0, 0xff);

/// The `--bg` token from `src/styles/tokens.css`, dark theme. The stylesheet
/// writes it as `oklch(0.215 0.006 91)`; this is that colour converted the way
/// CSS Color 4 specifies (OKLab to LMS, to XYZ D65, to linear sRGB, then the
/// sRGB transfer curve) and rounded to 8 bits: 26.4, 25.4, 22.3.
const DARK_FILL: Color = Color(0x1a, 0x19, 0x16, 0xff);

/// File name of the sidecar, beside `settings.json` in `%APPDATA%`.
const SIDECAR: &str = "theme";

fn sidecar_path() -> PathBuf {
    crate::settings::config_dir().join(SIDECAR)
}

/// The two words the sidecar may contain. Anything else is not a theme this
/// app has, so it is refused at both ends rather than coerced: refused on
/// write so the file cannot hold junk, refused on read so a file edited by
/// hand cannot pick a fill no stylesheet will match.
fn parse(raw: &str) -> Option<Theme> {
    match raw.trim() {
        "light" => Some(Theme::Light),
        "dark" => Some(Theme::Dark),
        _ => None,
    }
}

fn word(theme: Theme) -> &'static str {
    match theme {
        Theme::Dark => "dark",
        // `Theme` is `#[non_exhaustive]`; anything that is not Dark gets the
        // light fill, which is also the historical WebView2 default and so the
        // safe side of an unknown.
        _ => "light",
    }
}

/// The fill to hand [`WebviewWindow::set_background_color`].
fn fill(theme: Theme) -> Color {
    match theme {
        Theme::Dark => DARK_FILL,
        _ => LIGHT_FILL,
    }
}

/// Mirror a resolved theme into the sidecar. Called only by the
/// `persist_resolved_theme` command.
///
/// Rejects anything but `light`/`dark` — in particular `auto`, which is a
/// preference rather than a theme and has no fill of its own.
pub(crate) fn store(resolved: &str) -> anyhow::Result<()> {
    let theme = parse(resolved).ok_or_else(|| {
        // The rejected word is NOT echoed: this comes from the webview, and
        // the log rule does not carve out "but it is short".
        anyhow::anyhow!("not a resolved theme")
    })?;
    let path = sidecar_path();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    // Written whole and small; no temp-and-rename like `settings::save`,
    // because a torn write here costs one launch of the wrong fill and the
    // next `#apply` rewrites it. Paying for atomicity would be paying for a
    // guarantee this file does not need.
    std::fs::write(&path, word(theme))?;
    Ok(())
}

/// The theme the last run resolved to, if the sidecar says so.
fn from_sidecar() -> Option<Theme> {
    let raw = std::fs::read_to_string(sidecar_path()).ok()?;
    parse(&raw)
}

/// Fill the window with the theme's own background before it is shown.
///
/// Must be called from `setup()`, on the main thread, **before**
/// `main.show()`. Both halves matter:
///
/// * *Main thread* is what makes it synchronous rather than queued —
///   `tauri-runtime-wry-2.11.4/src/lib.rs:239` handles the message inline when
///   `current_thread().id() == context.main_thread_id` and only posts it to the
///   event loop otherwise. Queued, it would land after the frame it exists to
///   colour.
/// * *Before `show()`* because this sets the fill, not a repaint. Afterwards
///   the white frame has already been presented.
///
/// It sets both halves of the problem: `WebviewWindow::set_background_color`
/// forwards to the window (tao's erase fill) and the webview (WebView2's
/// `DefaultBackgroundColor`) — `tauri-2.11.5/src/webview/webview_window.rs:2284`.
/// It needs no capability: the ACL gates IPC from the webview, and this is
/// Rust calling the runtime directly.
///
/// **Never call this for the overlay.** That window is `transparent: true` and
/// draws a floating pill; giving it an opaque fill would put a grey rectangle
/// on the user's screen.
pub(crate) fn paint_before_show(main: &WebviewWindow) {
    let theme = match from_sidecar() {
        Some(theme) => theme,
        None => {
            // Only the parse outcome is logged, never the file's contents.
            // First launch reaches this too, which is why it is not a warning.
            tracing::debug!("theme sidecar absent or unreadable; using the OS theme");
            main.theme().unwrap_or(Theme::Light)
        }
    };
    if let Err(e) = main.set_background_color(Some(fill(theme))) {
        // Non-fatal by construction: the cost of failing here is the white
        // frame this exists to remove, which is what shipped until now.
        tracing::warn!("could not set the window fill: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // What is NOT covered here, deliberately: a round trip through the real
    // file. `sidecar_path` resolves to the user's own `%APPDATA%`, and a test
    // that wrote there would clobber the running app's sidecar and race every
    // other test in the process. What can actually drift is the encoding —
    // `word` and `parse` are written apart and read a launch apart — so that
    // is what is pinned. The I/O either side of it is one `fs::write` and one
    // `fs::read_to_string`, and `paint_before_show` treats every failure of
    // both as "fall back to the OS theme".

    /// `auto` is the one input that looks plausible and must not be stored: it
    /// is a preference, and the frontend is required to resolve it before it
    /// gets here. Storing it would mean a launch with no fill to pick.
    #[test]
    fn auto_is_not_a_resolved_theme() {
        assert!(parse("auto").is_none());
        assert!(parse("").is_none());
        assert!(parse("Dark").is_none(), "the frontend writes lowercase");
        assert!(store("auto").is_err());
    }

    /// The sidecar is written by one process and read by the next, so the
    /// word and the parser have to be exact inverses — a mismatch would fail
    /// silently into the OS-theme fallback and look like the bug still being
    /// there.
    #[test]
    fn every_word_written_reads_back_as_the_theme_that_wrote_it() {
        for theme in [Theme::Light, Theme::Dark] {
            assert_eq!(parse(word(theme)), Some(theme));
        }
        // A trailing newline is what any hand edit or text editor will leave.
        assert_eq!(parse("dark\n"), Some(Theme::Dark));
        assert_eq!(parse(" light \r\n"), Some(Theme::Light));
    }

    /// Reads `--bg` out of one block of the stylesheet, as written.
    fn bg_token(css: &str, block_start: &str) -> String {
        let block = &css[css.find(block_start).expect("theme block")..];
        let block = &block[..block.find('}').expect("block end")];
        let line = block
            .lines()
            .map(str::trim)
            .find(|l| l.starts_with("--bg:"))
            .expect("--bg in the block");
        line["--bg:".len()..].trim().trim_end_matches(';').trim().to_string()
    }

    /// `oklch(L C H)` to 8-bit sRGB with the CSS Color 4 matrices, the same
    /// arithmetic the browser uses to paint the token.
    fn oklch_to_rgb8(token: &str) -> [u8; 3] {
        let inner = token.strip_prefix("oklch(").and_then(|t| t.strip_suffix(')')).expect("oklch()");
        let v: Vec<f64> = inner.split_whitespace().map(|n| n.parse().expect("number")).collect();
        let (l, c, h) = (v[0], v[1], v[2].to_radians());
        let (a, b) = (c * h.cos(), c * h.sin());
        let lms = [
            (l + 0.3963377773761749 * a + 0.2158037573099136 * b).powi(3),
            (l - 0.1055613458156586 * a - 0.0638541728258133 * b).powi(3),
            (l - 0.0894841775298119 * a - 1.2914855480194092 * b).powi(3),
        ];
        let xyz = [
            1.2268798758459243 * lms[0] - 0.5578149944602171 * lms[1] + 0.2813910456659647 * lms[2],
            -0.0405757452148008 * lms[0] + 1.1122868032803170 * lms[1] - 0.0717110580655164 * lms[2],
            -0.0763729366746601 * lms[0] - 0.4214933324022432 * lms[1] + 1.5869240198367816 * lms[2],
        ];
        let lin = [
            3.2409699419045226 * xyz[0] - 1.537383177570094 * xyz[1] - 0.4986107602930034 * xyz[2],
            -0.9692436362808796 * xyz[0] + 1.8759675015077202 * xyz[1] + 0.04155505740717559 * xyz[2],
            0.05563007969699366 * xyz[0] - 0.20397695888897652 * xyz[1] + 1.0569715142428786 * xyz[2],
        ];
        lin.map(|x| {
            assert!((0.0..=1.0).contains(&x), "{token} is outside sRGB");
            let e = if x > 0.0031308 { 1.055 * x.powf(1.0 / 2.4) - 0.055 } else { 12.92 * x };
            (e * 255.0).round() as u8
        })
    }

    /// The fill is what shows for the frame before the page paints, and the
    /// page then paints `--bg`. If the two differ by one step, a seam shows at
    /// launch, so each fill is checked against its theme's token as written in
    /// the stylesheet.
    #[test]
    fn the_fills_are_the_bg_tokens_of_each_theme() {
        let css = include_str!("../../src/styles/tokens.css");

        assert_eq!(bg_token(css, ":root {"), "#f4f3f0");
        assert_eq!(fill(Theme::Light), Color(0xf4, 0xf3, 0xf0, 0xff));

        let [r, g, b] = oklch_to_rgb8(&bg_token(css, ":root.dark {"));
        assert_eq!(fill(Theme::Dark), Color(r, g, b, 0xff));

        for theme in [Theme::Light, Theme::Dark] {
            assert_eq!(fill(theme).3, 0xff, "a see-through fill shows the desktop");
        }
    }
}
