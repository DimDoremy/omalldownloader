//! A compositor that cannot be asked anything.
//!
//! Hyprland's IPC is a Unix socket, so on Windows there is no compositor to follow and no
//! animation setting to read: reduced motion then comes only from the theme's own
//! `shell.toml`. This module gives [`Hyprland`] and [`ReloadWatcher`] the same shape as the
//! Unix implementation so nothing above them needs a platform branch. (Local patch to ODK:
//! the Unix module was unconditional and did not compile on Windows.)

use std::ffi::OsString;
use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hyprland {
    runtime_dir: PathBuf,
    signature: Option<OsString>,
}

impl Hyprland {
    /// No compositor to look for.
    pub fn from_environment() -> Option<Self> {
        None
    }

    /// Kept for symmetry with the Unix implementation, so a test can construct one.
    pub fn at(runtime_dir: impl Into<PathBuf>, signature: Option<OsString>) -> Self {
        Self {
            runtime_dir: runtime_dir.into(),
            signature,
        }
    }

    /// Nothing to ask.
    pub fn animations_enabled(&self) -> Option<bool> {
        let _ = (&self.runtime_dir, &self.signature);
        None
    }

    /// Nothing to follow.
    pub fn watch_reloads(&self, _on_reload: impl Fn() + Send + 'static) -> Option<ReloadWatcher> {
        None
    }
}

#[derive(Debug)]
pub struct ReloadWatcher;
