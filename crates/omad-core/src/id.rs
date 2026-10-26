//! Task identity and lifecycle.

use serde::{Deserialize, Serialize};

/// A task's stable identity. Created once, never reused, and the primary key of every
/// record the store keeps about it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TaskId(pub u64);

impl std::fmt::Display for TaskId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "tsk_{}", self.0)
    }
}

impl TaskId {
    /// Parses `tsk_12` or `12`. The CLI accepts both so a user can paste either.
    pub fn parse(text: &str) -> Option<Self> {
        text.trim()
            .strip_prefix("tsk_")
            .unwrap_or(text)
            .parse()
            .ok()
            .map(TaskId)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TaskStatus {
    /// Added, waiting for a slot.
    Queued,
    /// Bytes are moving.
    Running,
    /// Stopped on purpose; every byte stays on disk.
    Paused,
    /// Every chunk is written and the record says the file is complete.
    Completed,
    /// Stopped by an error; resumable unless [`TaskSnapshot::error`] says otherwise.
    Failed,
}

impl TaskStatus {
    pub fn is_terminal(self) -> bool {
        matches!(self, TaskStatus::Completed)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            TaskStatus::Queued => "queued",
            TaskStatus::Running => "running",
            TaskStatus::Paused => "paused",
            TaskStatus::Completed => "completed",
            TaskStatus::Failed => "failed",
        }
    }

    /// Parses the stored form. Named `from_storage` rather than `from_str` so that it is
    /// not mistaken for `std::str::FromStr`, which would have to be infallible-safe.
    pub fn from_storage(text: &str) -> Option<Self> {
        Some(match text {
            "queued" => TaskStatus::Queued,
            "running" => TaskStatus::Running,
            "paused" => TaskStatus::Paused,
            "completed" => TaskStatus::Completed,
            "failed" => TaskStatus::Failed,
            _ => return None,
        })
    }
}

impl std::str::FromStr for TaskStatus {
    type Err = ();
    fn from_str(text: &str) -> Result<Self, Self::Err> {
        TaskStatus::from_storage(text).ok_or(())
    }
}
