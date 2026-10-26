//! The download engine.
//!
//! A task is:
//!
//! 1. **probe** — ask for the size, the validators and whether ranges work;
//! 2. **plan** — cut the file into chunks, or one chunk when ranges are refused;
//! 3. **fetch** — N connections at once, each writing into its own region of one
//!    preallocated file with positioned writes;
//! 4. **finish** — re-read what was written, count it, and only then mark the record
//!    complete.
//!
//! Three invariants the review asked for, and where they live:
//!
//! * A chunk's position on disk advances only after its bytes are written, and the
//!   record is only ever told the sum of the chunks. A crash loses progress, never
//!   invents it ([`engine::Plan::recorded`]).
//! * A positioned write that returns short is a **failure**, not a retry of the
//!   remainder ([`engine::write_at`]).
//! * Nothing is marked complete until the bytes are on disk and, when the server gave
//!   a digest, the digest matches ([`engine::Engine::finish`]).

pub mod engine;
pub mod http;

pub use engine::{Engine, EngineConfig, Outcome, Progress, StoreSink};
pub use http::{HttpProbe, Probe, ProbeOutcome, ProbeSource};

/// Asks a server what a URL is, without downloading it. The CLI and the application use
/// this before they create a task, so the file gets the name the server says it has.
pub async fn probe(
    url: &url::Url,
    policy: &omad_core::ssrf::UrlPolicy,
) -> anyhow::Result<ProbeOutcome> {
    let probe = HttpProbe::new()?;
    Ok(probe.probe(url, policy).await?)
}
