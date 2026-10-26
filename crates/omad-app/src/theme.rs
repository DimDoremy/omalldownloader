//! Following the desktop's theme on Linux.
//!
//! ODK reads a theme from `$XDG_STATE_HOME/omarchy/current/theme/{colors.toml,shell.toml}`
//! and refuses a `colors.toml` without a `mode` key. Omarchy's own themes on this machine
//! have no `mode`: the light or dark choice lives in the wallpaper and the bar, not in the
//! palette. So pointing ODK straight at the session would leave every window on the
//! emergency palette — which is what happened before this module existed.
//!
//! This module is the adapter: it copies the session's theme into
//! `$XDG_STATE_HOME_ORIGINAL/omalldownloader/theme/omarchy/current/`, adding the `mode`
//! that Omarchy leaves implicit and inferring it from the background's luminance, and
//! points `XDG_STATE_HOME` at that copy **before** ODK starts. A watcher copies again when
//! the session's theme changes, which ODK's own watcher then sees, so a theme switch in
//! Omarchy switches the app within a second.
//!
//! Nothing here changes ODK or Omarchy: the adapter only says what Omarchy's palette means.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// Where Omarchy keeps the current theme, honouring a caller's `XDG_STATE_HOME`.
fn session_current(state_home: Option<&Path>) -> PathBuf {
    state_home
        .map(|p| p.to_path_buf())
        .or_else(|| std::env::var_os("XDG_STATE_HOME").map(PathBuf::from))
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".local/state")
        })
        .join("omarchy/current")
}

/// The state directory this process hands to ODK.
pub fn adapted_state_home() -> PathBuf {
    let base = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".local/state")
        });
    base.join("omalldownloader/theme")
}

/// Prepares the copy and points `XDG_STATE_HOME` at it.
///
/// Call this once, before `odk::ui::run`. It is safe to call when there is no Omarchy at
/// all: the copy is then empty and ODK falls back to its emergency palette, as designed.
pub fn install() -> Adapter {
    let original_state = std::env::var_os("XDG_STATE_HOME").map(PathBuf::from);
    let session = session_current(original_state.as_deref());
    let adapted = adapted_state_home();

    let mut adapter = Adapter {
        session,
        adapted: adapted.clone(),
        stamp: None,
    };
    adapter.sync();

    // SAFETY: called before the runtime starts, on the main thread, and nothing else reads
    // the environment during it.
    unsafe { std::env::set_var("XDG_STATE_HOME", &adapted) };
    adapter
}

/// Keeps the copy in step with the session.
pub struct Adapter {
    session: PathBuf,
    adapted: PathBuf,
    stamp: Option<(SystemTime, u64)>,
}

impl Adapter {
    /// Copies the theme when it changed. Returns whether anything was written.
    pub fn sync(&mut self) -> bool {
        let colors = self.session.join("theme/colors.toml");
        let Ok(meta) = std::fs::metadata(&colors) else {
            return false;
        };
        let stamp = (
            meta.modified().unwrap_or(SystemTime::UNIX_EPOCH),
            meta.len(),
        );
        if self.stamp == Some(stamp) {
            return false;
        }

        // ODK reads `$XDG_STATE_HOME/omarchy/current/theme/`, the same layout Omarchy
        // writes, so the copy has to sit at that depth.
        let target = self.adapted.join("omarchy/current/theme");
        if let Err(error) = copy_theme(&self.session, &target) {
            tracing::warn!(%error, "could not prepare the theme for ODK");
            self.stamp = Some(stamp);
            return false;
        }
        self.stamp = Some(stamp);
        true
    }

    /// Starts a thread that syncs every second or so. The thread lasts as long as the
    /// process; ODK's watcher on the copy does the rest.
    pub fn watch(self, on_change: impl Fn() + Send + 'static) -> std::thread::JoinHandle<()> {
        std::thread::Builder::new()
            .name("omad-theme".into())
            .spawn(move || {
                let mut this = self;
                loop {
                    std::thread::sleep(std::time::Duration::from_millis(1200));
                    if this.sync() {
                        on_change();
                    }
                }
            })
            .expect("the theme thread starts")
    }
}

fn copy_theme(session: &Path, target: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(target)?;

    // The palette, completed: `mode` plus the keys a minimal Omarchy theme leaves out.
    let colors = std::fs::read_to_string(session.join("theme/colors.toml"))?;
    let patched = complete(&colors);
    std::fs::write(target.join("colors.toml"), patched)?;

    // Everything else the theme carries, verbatim: `shell.toml` holds the text size and the
    // user's reduced-motion request, and `theme.name` is the label the app shows.
    for file in ["shell.toml", "theme.name"] {
        let from = session.join("theme").join(file);
        if from.is_file() {
            let _ = std::fs::copy(&from, target.join(file));
        }
    }
    let name = session.join("theme.name");
    if name.is_file() {
        let _ = std::fs::copy(&name, target.join("theme.name"));
    }
    Ok(())
}

/// Adds `mode` when the palette has none, inferred from the background's luminance.
///
/// The threshold is relative luminance, not the raw channel: a mid-grey background is
/// "dark" when white text reads better on it, which is what the foreground tells us.
pub fn with_mode(colors: &str) -> String {
    if has_key(colors, "mode") {
        return colors.to_string();
    }
    let mode = infer_mode(colors);
    let mut out = String::with_capacity(colors.len() + 16);
    out.push_str(&format!("mode = \"{mode}\"\n"));
    out.push_str(colors);
    out
}

/// Completes a palette into the shape ODK reads.
///
/// Omarchy's stock themes carry `mode`, `muted`, `dark_background`, `lighter_background` and
/// named ANSI colours. A theme generated elsewhere often carries only
/// `accent/selection/background/foreground` and `color0`…`color15`. ODK refuses a palette that
/// lacks the rest, so this fills each missing key from what the theme does say: the ANSI
/// table for the colours, and a blend of the background and foreground for the surfaces and
/// the muted text. A theme that already has everything passes through untouched.
pub fn complete(colors: &str) -> String {
    let mut out = with_mode(colors);
    let background = value_of(&out, "background").unwrap_or_else(|| "#000000".into());
    let foreground = value_of(&out, "foreground").unwrap_or_else(|| "#ffffff".into());

    let derived: [(&str, String); 11] = [
        (
            "muted",
            mix(&foreground, &background, 0.45).unwrap_or_else(|| "#808080".into()),
        ),
        (
            "dark_background",
            mix(&background, "#000000", 0.35).unwrap_or_else(|| background.clone()),
        ),
        (
            "darker_background",
            mix(&background, "#000000", 0.65).unwrap_or_else(|| background.clone()),
        ),
        (
            "lighter_background",
            mix(&background, &foreground, 0.10).unwrap_or_else(|| background.clone()),
        ),
        (
            "dark_foreground",
            mix(&foreground, &background, 0.45).unwrap_or_else(|| foreground.clone()),
        ),
        (
            "light_foreground",
            mix(&foreground, "#ffffff", 0.18).unwrap_or_else(|| foreground.clone()),
        ),
        // The ANSI table, which is how a minimal theme names its colours.
        (
            "red",
            ansi(&out, "color1").unwrap_or_else(|| "#e65c5c".into()),
        ),
        (
            "yellow",
            ansi(&out, "color3").unwrap_or_else(|| "#ffcc66".into()),
        ),
        (
            "green",
            ansi(&out, "color2").unwrap_or_else(|| "#66cc66".into()),
        ),
        (
            "blue",
            ansi(&out, "color4").unwrap_or_else(|| "#6699ff".into()),
        ),
        (
            "magenta",
            ansi(&out, "color5").unwrap_or_else(|| "#cc66cc".into()),
        ),
    ];

    for (key, value) in derived {
        if !has_key(&out, key) {
            // The palette may not end in a newline; without one the new key would be glued
            // to the last line and the whole file would fail to parse.
            if !out.ends_with('\n') {
                out.push('\n');
            }
            out.push_str(&format!("{key} = \"{value}\"\n"));
        }
    }
    out
}

/// A colour from the ANSI table, when the theme has it.
fn ansi(text: &str, key: &str) -> Option<String> {
    value_of(text, key)
}

/// Blends `a` towards `b` by `amount` (0 keeps `a`, 1 gives `b`).
fn mix(a: &str, b: &str, amount: f64) -> Option<String> {
    let (ar, ag, ab) = parse_hex(a)?;
    let (br, bg, bb) = parse_hex(b)?;
    let amount = amount.clamp(0.0, 1.0);
    let channel = |x: f64, y: f64| {
        ((x * (1.0 - amount) + y * amount) * 255.0)
            .round()
            .clamp(0.0, 255.0) as u8
    };
    Some(format!(
        "#{:02x}{:02x}{:02x}",
        channel(ar, br),
        channel(ag, bg),
        channel(ab, bb)
    ))
}

fn has_key(text: &str, key: &str) -> bool {
    value_of(text, key).is_some()
}

/// One key's value, unquoted.
///
/// The naive "strip from `#`" that a TOML comment invites breaks here, because a colour
/// *starts* with `#`. So a comment is only stripped outside a quoted value.
fn value_of(text: &str, key: &str) -> Option<String> {
    for line in text.lines() {
        let trimmed = line.trim_start();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let Some((name, rest)) = trimmed.split_once('=') else {
            continue;
        };
        if name.trim() != key {
            continue;
        }
        return Some(unquote(rest.trim()));
    }
    None
}

/// The value alone: a quoted string up to its closing quote, else up to a comment.
fn unquote(rest: &str) -> String {
    let rest = rest.trim();
    if let Some(body) = rest.strip_prefix('"') {
        return body.split('"').next().unwrap_or("").to_string();
    }
    if let Some(body) = rest.strip_prefix('\'') {
        return body.split('\'').next().unwrap_or("").to_string();
    }
    rest.split('#').next().unwrap_or("").trim().to_string()
}

/// `dark` unless the background is clearly light.
pub fn infer_mode(colors: &str) -> &'static str {
    let background = value_of(colors, "background")
        .or_else(|| value_of(colors, "color0"))
        .unwrap_or_default();
    match parse_hex(&background) {
        Some((r, g, b)) => {
            let luminance = 0.2126 * srgb_to_linear(r)
                + 0.7152 * srgb_to_linear(g)
                + 0.0722 * srgb_to_linear(b);
            if luminance > 0.35 { "light" } else { "dark" }
        }
        None => "dark",
    }
}

fn srgb_to_linear(channel: f64) -> f64 {
    if channel <= 0.04045 {
        channel / 12.92
    } else {
        ((channel + 0.055) / 1.055).powf(2.4)
    }
}

fn parse_hex(text: &str) -> Option<(f64, f64, f64)> {
    let hex = text.trim().trim_start_matches('#');
    let expanded = match hex.len() {
        3 => hex.chars().flat_map(|c| [c, c]).collect::<String>(),
        6 => hex.to_string(),
        _ => return None,
    };
    let r = u8::from_str_radix(&expanded[0..2], 16).ok()? as f64 / 255.0;
    let g = u8::from_str_radix(&expanded[2..4], 16).ok()? as f64 / 255.0;
    let b = u8::from_str_radix(&expanded[4..6], 16).ok()? as f64 / 255.0;
    Some((r, g, b))
}

#[cfg(test)]
mod tests {
    use super::*;

    const DARK: &str = r##"
accent = "#6699ff"
background = "#000000"
foreground = "#e6e6e6"
color0 = "#000000"
"##;

    const LIGHT: &str = r##"
accent = "#3b6ea5"
background = "#fdf6e3"
foreground = "#1c1c1f"
color0 = "#fdf6e3"
"##;

    #[test]
    fn a_dark_session_palette_gets_mode_dark() {
        assert_eq!(infer_mode(DARK), "dark");
        let patched = with_mode(DARK);
        assert!(patched.starts_with("mode = \"dark\""));
        assert!(patched.contains("background = \"#000000\""));
    }

    #[test]
    fn a_light_session_palette_gets_mode_light() {
        assert_eq!(infer_mode(LIGHT), "light");
        assert!(with_mode(LIGHT).starts_with("mode = \"light\""));
    }

    #[test]
    fn a_palette_that_already_has_a_mode_is_left_alone() {
        let text = "mode = \"light\"\nbackground = \"#ffffff\"\n";
        assert_eq!(with_mode(text), text);
        // A commented-out mode is not a mode.
        let commented = "# mode = \"light\"\nbackground = \"#ffffff\"\n";
        let patched = with_mode(commented);
        assert!(patched.starts_with("mode = \"light\"\n"), "{patched}");
    }

    #[test]
    fn a_missing_background_falls_back_to_dark() {
        assert_eq!(infer_mode("accent = \"#123456\"\n"), "dark");
        assert_eq!(infer_mode(""), "dark");
    }

    #[test]
    fn short_hex_is_understood() {
        assert_eq!(infer_mode("background = \"#fff\""), "light");
        assert_eq!(infer_mode("background = \"#000\""), "dark");
    }

    #[test]
    fn a_minimal_palette_is_completed_into_the_shape_odk_reads() {
        let minimal = r##"
accent = "#6699ff"
background = "#000000"
foreground = "#e6e6e6"
# a comment with a # in it
color1 = "#e65c5c"
color2 = "#66cc66"
color3 = "#ffcc66"
color4 = "#6699ff"
color5 = "#cc66cc"
"##;
        let done = complete(minimal);
        for key in [
            "mode",
            "accent",
            "muted",
            "background",
            "dark_background",
            "lighter_background",
            "foreground",
            "red",
            "yellow",
            "green",
            "blue",
            "magenta",
        ] {
            assert!(has_key(&done, key), "{key} should be present in:\n{done}");
        }
        // The colours the theme did give are kept, not overwritten.
        assert_eq!(value_of(&done, "accent").as_deref(), Some("#6699ff"));
        assert_eq!(value_of(&done, "red").as_deref(), Some("#e65c5c"));
        // A derived surface is never lighter than the background it came from.
        let dark = value_of(&done, "dark_background").unwrap();
        assert!(dark <= value_of(&done, "background").unwrap(), "{dark}");

        // On a light palette the derived surfaces still go the right way.
        let light =
            complete("background = \"#fdf6e3\"\nforeground = \"#1c1c1f\"\ncolormode = \"light\"");
        let darker = value_of(&light, "dark_background").unwrap();
        assert!(darker < value_of(&light, "background").unwrap(), "{darker}");
    }

    #[test]
    fn a_palette_without_a_trailing_newline_still_gains_its_keys() {
        let minimal = r##"accent = "#6699ff"
background = "#000000"
foreground = "#e6e6e6""##;
        let done = complete(minimal);
        assert!(done.ends_with('\n'), "{done:?}");
        // Every injected line is a line of its own.
        for key in ["mode", "muted", "dark_background"] {
            assert!(
                done.lines()
                    .any(|line| line.starts_with(&format!("{key} = "))),
                "{key} should start its own line in:\n{done}"
            );
        }
    }

    #[test]
    fn a_full_palette_is_not_touched() {
        let full = r##"
mode = "dark"
accent = "#7aa2f7"
muted = "#414868"
background = "#1a1b26"
dark_background = "#13141c"
darker_background = "#0e0e14"
lighter_background = "#24283b"
foreground = "#a9b1d6"
dark_foreground = "#565f89"
light_foreground = "#b4bee6"
red = "#f7768e"
yellow = "#e0af68"
green = "#9ece6a"
blue = "#7aa2f7"
magenta = "#bb9af7"
"##;
        assert_eq!(complete(full), full);
    }

    #[test]
    fn blending_stays_inside_the_colour_space() {
        assert_eq!(mix("#000000", "#ffffff", 0.5).unwrap(), "#808080");
        assert_eq!(mix("#ff0000", "#00ff00", 0.0).unwrap(), "#ff0000");
        assert_eq!(mix("#ff0000", "#00ff00", 1.0).unwrap(), "#00ff00");
        assert_eq!(mix("#ff0000", "#00ff00", 5.0).unwrap(), "#00ff00");
        assert!(mix("nonsense", "#000000", 0.5).is_none());
    }

    #[test]
    fn hex_parsing_refuses_rubbish() {
        assert!(parse_hex("#12").is_none());
        assert!(parse_hex("rgb(1,2,3)").is_none());
        assert!(parse_hex("#ff0000").is_some());
    }
}
