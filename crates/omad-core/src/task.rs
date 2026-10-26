//! The task model: what a request to add a download may contain, and what the rest
//! of the application is allowed to know about a task.

use crate::filename;
use crate::id::{TaskId, TaskStatus};
use serde::{Deserialize, Serialize};

/// Everything a caller must say to add a download.
///
/// Note what is **absent**, on purpose: there is no path, no header list and no output
/// directory. `dest` is a logical folder name the user configured; the file name is
/// derived from the server or supplied as a bare name and sanitised again. This is the
/// structural fix for R2 — the wire format cannot express "write to /etc".
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AddTask {
    pub url: String,
    /// A logical destination such as `downloads`. Never a path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dest: Option<String>,
    /// A suggested bare file name. Sanitised; directories are stripped.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filename: Option<String>,
    /// The page the link came from, for the Referer and for audit. Not a write target.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub referer: Option<String>,
    /// Who asked: `cli`, `ui`, or an extension origin such as
    /// `chrome-extension://abc`. Recorded, shown to the user, never used as authority.
    #[serde(default)]
    pub origin: Origin,
    /// The user has explicitly agreed to reach this host even though it resolves to a
    /// loopback or private address.
    #[serde(default)]
    pub allow_private_network: bool,
    /// Connection count for this task; clamped by the engine to its own maximum.
    #[serde(default)]
    pub connections: Option<u8>,
}

impl AddTask {
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            dest: None,
            filename: None,
            referer: None,
            origin: Origin::Ui,
            allow_private_network: false,
            connections: None,
        }
    }

    pub fn with_filename(mut self, name: impl Into<String>) -> Self {
        self.filename = Some(name.into());
        self
    }

    pub fn with_dest(mut self, dest: impl Into<String>) -> Self {
        self.dest = Some(dest.into());
        self
    }

    pub fn from_origin(mut self, origin: Origin) -> Self {
        self.origin = origin;
        self
    }

    /// The name to try first: the caller's suggestion, else the URL's last segment.
    pub fn proposed_filename(&self, url: &url::Url) -> String {
        match &self.filename {
            Some(name) if !name.trim().is_empty() => filename::sanitize(name),
            _ => filename::from_url(url),
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Origin {
    Cli,
    #[default]
    Ui,
    Extension {
        /// `chrome-extension://<id>` or `moz-extension://<uuid>`.
        id: String,
    },
}

impl Origin {
    pub fn label(&self) -> String {
        match self {
            Origin::Cli => "cli".into(),
            Origin::Ui => "ui".into(),
            Origin::Extension { id } => id.clone(),
        }
    }
}

/// A task as the UI, the CLI and the extension see it. Cheap to serialise, no
/// handle into the engine.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TaskSnapshot {
    pub id: TaskId,
    pub url: String,
    pub filename: String,
    pub dest: String,
    pub status: TaskStatus,
    pub total_bytes: Option<u64>,
    pub received_bytes: u64,
    /// Bytes per second, smoothed. Zero when not running.
    pub speed: u64,
    pub connections: u8,
    /// The server's validator, kept so a resume can tell "changed" from "same".
    pub etag: Option<String>,
    pub error: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

impl TaskSnapshot {
    pub fn progress(&self) -> f32 {
        match self.total_bytes {
            Some(total) if total > 0 => (self.received_bytes as f32 / total as f32).clamp(0.0, 1.0),
            _ => 0.0,
        }
    }

    pub fn is_indeterminate(&self) -> bool {
        self.total_bytes.is_none()
    }
}
