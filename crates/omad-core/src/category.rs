//! Where a download goes: a folder chosen by what the file is.
//!
//! The rules live in `$XDG_CONFIG_HOME/omalldownloader/categories.json`, and the built-in
//! set is written there the first time the program runs, so it can be edited rather than
//! guessed at:
//!
//! ```json
//! [
//!   { "folder": "ISO", "extensions": ["iso", "img"] },
//!   { "folder": "ABDM", "extensions": ["abdm"] }
//! ]
//! ```
//!
//! A file whose extension matches no rule goes straight into the download folder.

use std::path::{Path, PathBuf};

/// One folder and the extensions that belong in it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rule {
    pub folder: String,
    pub extensions: Vec<String>,
}

/// The rules this profile uses: the file's if it exists and parses, the defaults otherwise.
pub fn load() -> Vec<Rule> {
    let path = config_path();
    match std::fs::read_to_string(&path) {
        Ok(text) => match serde_json::from_str::<Vec<RuleJson>>(&text) {
            Ok(rules) => rules.into_iter().filter_map(RuleJson::into_rule).collect(),
            Err(_) => defaults(),
        },
        Err(_) => {
            // Write the defaults out once, so the user has a file to edit.
            write_defaults(&path);
            defaults()
        }
    }
}

/// The folder a file belongs in, if any.
pub fn folder_for(rules: &[Rule], name: &str) -> Option<String> {
    let extension = Path::new(name)
        .extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase())?;
    rules
        .iter()
        .find(|rule| rule.extensions.iter().any(|known| known == &extension))
        .map(|rule| rule.folder.clone())
}

pub fn config_path() -> PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("omalldownloader").join("categories.json")
}

pub fn defaults() -> Vec<Rule> {
    let rule = |folder: &str, extensions: &[&str]| Rule {
        folder: folder.to_string(),
        extensions: extensions.iter().map(|e| e.to_string()).collect(),
    };
    vec![
        rule("ISO", &["iso", "img", "dmg", "vhd"]),
        rule(
            "Video",
            &["mkv", "mp4", "avi", "webm", "mov", "m4v", "ts", "flv"],
        ),
        rule("Music", &["mp3", "flac", "m4a", "opus", "wav", "ogg"]),
        rule(
            "Images",
            &["jpg", "jpeg", "png", "gif", "webp", "svg", "heic"],
        ),
        rule(
            "Archives",
            &["zip", "7z", "rar", "tar", "gz", "xz", "zst", "bz2"],
        ),
        rule("Documents", &["pdf", "epub", "djvu", "docx", "odt"]),
        rule(
            "Packages",
            &["apk", "deb", "rpm", "pkg", "exe", "msi", "appimage"],
        ),
    ]
}

#[derive(serde::Deserialize)]
struct RuleJson {
    folder: String,
    #[serde(default)]
    extensions: Vec<String>,
}

impl RuleJson {
    fn into_rule(self) -> Option<Rule> {
        // A folder that looks like a path is a bug or an attack: these are names, not paths.
        let folder = self.folder.trim().to_string();
        if folder.is_empty() || folder.contains(['/', '\\']) || folder == "." || folder == ".." {
            return None;
        }
        Some(Rule {
            folder,
            extensions: self
                .extensions
                .into_iter()
                .map(|e| e.trim().trim_start_matches('.').to_ascii_lowercase())
                .filter(|e| !e.is_empty() && !e.contains(['/', '\\']))
                .collect(),
        })
    }
}

fn write_defaults(path: &Path) {
    let Some(parent) = path.parent() else { return };
    if std::fs::create_dir_all(parent).is_err() {
        return;
    }
    let json: Vec<serde_json::Value> = defaults()
        .into_iter()
        .map(|rule| serde_json::json!({ "folder": rule.folder, "extensions": rule.extensions }))
        .collect();
    if let Ok(text) = serde_json::to_string_pretty(&json) {
        let _ = std::fs::write(path, text + "\n");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_extension_picks_its_folder() {
        let rules = defaults();
        assert_eq!(folder_for(&rules, "pond-installer.iso"), Some("ISO".into()));
        assert_eq!(folder_for(&rules, "Movie.MKV"), Some("Video".into()));
        assert_eq!(folder_for(&rules, "song.flac"), Some("Music".into()));
        assert_eq!(folder_for(&rules, "unknown.abdm"), None);
        assert_eq!(folder_for(&rules, "no-extension"), None);
    }

    #[test]
    fn a_custom_rule_wins_and_a_path_like_folder_is_refused() {
        let rules = vec![
            Rule {
                folder: "ABDM".into(),
                extensions: vec!["iso".into()],
            },
            Rule {
                folder: "../evil".into(),
                extensions: vec!["zip".into()],
            },
        ];
        assert_eq!(folder_for(&rules, "a.iso"), Some("ABDM".into()));
        let json: Vec<RuleJson> = serde_json::from_str(
            r#"[{"folder":"../evil","extensions":["zip"]},{"folder":"OK","extensions":[".ISO"]}]"#,
        )
        .unwrap();
        let rules: Vec<Rule> = json.into_iter().filter_map(RuleJson::into_rule).collect();
        assert_eq!(rules.len(), 1, "the path-like folder was dropped");
        assert_eq!(folder_for(&rules, "a.iso"), Some("OK".into()));
    }
}
