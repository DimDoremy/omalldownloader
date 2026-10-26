//! The wire protocol: one version number pair, one message enum per direction, and a
//! golden test that fails when either changes without the number changing.

use omad_core::{AddTask, TaskId, TaskStatus};
use serde::{Deserialize, Serialize};

/// Breaking changes bump this and the extension refuses to talk.
pub const PROTOCOL_MAJOR: u32 = 1;
/// Additions bump this; an older peer keeps working with the part it knows.
pub const PROTOCOL_MINOR: u32 = 0;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMessage {
    /// First message. `proof` is `HMAC-SHA256(secret, nonce)` in lowercase hex.
    Hello {
        major: u32,
        minor: u32,
        extension_version: String,
        proof: String,
    },
    /// Pairing, step one: the extension asks to be paired from `origin`, proving it
    /// knows the code the user read off the desktop. There is no other way in.
    Pair {
        origin: String,
        code_proof: String,
    },
    ListTasks,
    AddTask(AddTask),
    Pause {
        id: TaskId,
    },
    Resume {
        id: TaskId,
    },
    Remove {
        id: TaskId,
    },
    Cancel {
        id: TaskId,
    },
    /// Ask for fresh snapshots after every change. The server pushes unprompted after
    /// a mutation in any case; this makes a reconnecting client catch up.
    Subscribe,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMessage {
    /// The challenge. `nonce` is single-use and expires.
    Challenge {
        nonce: String,
        expires_in_ms: u64,
        /// What the client will gain: the intersection of what it asked for and what
        /// this build has. Never a promise the server cannot keep.
        capabilities: Vec<String>,
    },
    Welcome {
        major: u32,
        minor: u32,
        server_version: String,
    },
    Paired {
        /// The shared secret, base64. The **only** time it travels, and only over a
        /// channel whose `Origin` was checked and whose code proof was correct.
        secret: String,
    },
    Tasks {
        tasks: Vec<TaskView>,
    },
    Task {
        task: TaskView,
    },
    Removed {
        id: TaskId,
    },
    Error {
        code: ErrorCode,
        message: String,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TaskView {
    pub id: TaskId,
    pub url: String,
    pub filename: String,
    pub status: TaskStatus,
    pub received_bytes: u64,
    pub total_bytes: Option<u64>,
    pub speed: u64,
    pub error: Option<String>,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    Unauthenticated,
    ForbiddenOrigin,
    BadProof,
    Expired,
    BadRequest,
    NotFound,
    Refused,
    Internal,
}

impl ErrorCode {
    pub fn as_str(self) -> &'static str {
        match self {
            ErrorCode::Unauthenticated => "unauthenticated",
            ErrorCode::ForbiddenOrigin => "forbidden_origin",
            ErrorCode::BadProof => "bad_proof",
            ErrorCode::Expired => "expired",
            ErrorCode::BadRequest => "bad_request",
            ErrorCode::NotFound => "not_found",
            ErrorCode::Refused => "refused",
            ErrorCode::Internal => "internal",
        }
    }
}
