//! OmaLLDownloader core: the types every layer shares, and the security primitives that
//! guard the three entry points of the application (URLs, file names, destinations).
//!
//! Nothing here has a user interface, spawns a thread or opens a socket. The
//! architecture test in `omad-engine/tests/architecture.rs` fails the build if a GUI or
//! windowing crate ever reaches this layer.

pub mod category;
pub mod filename;
pub mod i18n;
pub mod id;
pub mod path;
pub mod ssrf;
pub mod task;

pub use id::{TaskId, TaskStatus};
pub use task::{AddTask, TaskSnapshot};
