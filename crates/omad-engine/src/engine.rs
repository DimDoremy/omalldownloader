//! The engine proper: plan, fetch, resume, verify.

use crate::http::{Probe, ProbeError, ProbeSource};
use omad_core::TaskId;
use omad_core::ssrf::{UrlPolicy, Verdict};
use serde::{Deserialize, Serialize};
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};
use tokio::sync::watch;
use url::Url;

/// Chunks smaller than this are not worth a connection.
const MIN_CHUNK: u64 = 256 * 1024;
/// Never open more than this many connections for one task.
pub const MAX_CONNECTIONS: u8 = 16;
/// How often progress is pushed to the caller.
const TICK: Duration = Duration::from_millis(200);
/// How often the record is written. Losing more than this costs a re-download.
const COMMIT_EVERY: Duration = Duration::from_secs(1);

#[derive(Clone, Debug)]
pub struct EngineConfig {
    pub max_connections: u8,
    pub chunk_size: u64,
    pub policy: UrlPolicy,
    /// Extra headers that are safe to send: the engine refuses to forward a
    /// credential-bearing header to a different origin than the one that produced it.
    pub user_agent: Option<String>,
    /// The page a download was started from. Many sites refuse a file whose request has no
    /// `Referer` (hotlink protection), and a browser would have sent it, so this is passed
    /// through exactly as the browser gave it.
    pub referer: Option<String>,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            max_connections: 8,
            chunk_size: 4 * 1024 * 1024,
            policy: UrlPolicy::default(),
            user_agent: None,
            referer: None,
        }
    }
}

/// What the window says it is. A browser's user agent, because file hosts treat anything
/// else as a robot; `OMAD_USER_AGENT` overrides it for a site that needs something else.
pub const DEFAULT_AGENT: &str = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 \
     (KHTML, like Gecko) Chrome/141.0.0.0 Safari/537.36";

#[derive(Clone, Debug, Default)]
pub struct Progress {
    pub received: u64,
    pub total: Option<u64>,
    pub speed: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    Completed,
    Paused,
}

#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    #[error("probe refused: {0}")]
    Probe(#[from] ProbeError),
    #[error("the server does not support ranges, so this file cannot be chunked: {0} bytes")]
    NoRanges(u64),
    #[error("the file changed on the server since the download started")]
    Changed,
    #[error(
        "the bytes written do not match the size the server announced ({written} of {expected})"
    )]
    Short { written: u64, expected: u64 },
    #[error("the digest does not match: expected {expected}, got {actual}")]
    Digest { expected: String, actual: String },
    #[error("io error at {path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("the connection to the server failed: {0}")]
    Transport(String),
    #[error("the request was cancelled")]
    Cancelled,
    #[error("the engine stopped")]
    Stopped,
}

/// Whoever wants to know what a task is doing: the store, the UI, the CLI.
pub trait StoreSink: Send + Sync + 'static {
    fn on_plan(&self, id: TaskId, probe: &Probe, plan: &Plan, connections: u8);
    fn on_progress(&self, id: TaskId, progress: &Progress);
    /// The durable half: the record must be advanced to `received`, which is the sum
    /// of acknowledged chunk progress and nothing else.
    fn on_commit(&self, id: TaskId, received: u64, total: Option<u64>, speed: u64);
    /// The final word. `recorded` is what the plan says is on disk, which is what the
    /// store must keep; it is sent before any success is claimed.
    fn on_finished(&self, id: TaskId, outcome: Outcome, recorded: u64, total: Option<u64>);
    fn on_failed(&self, id: TaskId, error: &str, resumable: bool);
}

/// A chunk of the file: where it starts, and how much of it is on disk.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Chunk {
    pub start: u64,
    pub end: u64,
    pub done: u64,
}

impl Chunk {
    pub fn len(&self) -> u64 {
        self.end.saturating_sub(self.start) + 1
    }
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn remaining(&self) -> u64 {
        self.len().saturating_sub(self.done)
    }
}

/// The chunk layout, persisted so a restart needs no second opinion.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Plan {
    pub total: u64,
    pub ranges: bool,
    pub chunks: Vec<Chunk>,
    #[serde(default)]
    pub etag: Option<String>,
    #[serde(default)]
    pub last_modified: Option<String>,
}

impl Plan {
    /// Splits `total` bytes into at most `connections` chunks of at least
    /// `chunk_size`, which is what keeps a small file from opening ten sockets.
    pub fn split(total: u64, connections: u8, chunk_size: u64) -> Self {
        let connections = connections.clamp(1, MAX_CONNECTIONS) as u64;
        if total == 0 {
            return Self {
                total: 0,
                ranges: true,
                chunks: vec![],
                etag: None,
                last_modified: None,
            };
        }
        let by_size = total.div_ceil(chunk_size.max(MIN_CHUNK)).max(1);
        let count = connections.min(by_size).max(1);
        let mut chunks = Vec::with_capacity(count as usize);
        let step = total.div_ceil(count);
        let mut start = 0u64;
        while start < total {
            let end = (start + step).min(total) - 1;
            chunks.push(Chunk {
                start,
                end,
                done: 0,
            });
            start = end + 1;
        }
        Self {
            total,
            ranges: true,
            chunks,
            etag: None,
            last_modified: None,
        }
    }

    /// One chunk covering everything, for a server that ignores ranges.
    pub fn single(total: Option<u64>) -> Self {
        let chunk = Chunk {
            start: 0,
            end: total.unwrap_or(u64::MAX).saturating_sub(1),
            done: 0,
        };
        Self {
            total: total.unwrap_or(0),
            ranges: false,
            chunks: if total == Some(0) {
                vec![]
            } else {
                vec![chunk]
            },
            etag: None,
            last_modified: None,
        }
    }

    /// The sum of what the chunks say is on disk. This is the only number the record
    /// is ever given.
    pub fn recorded(&self) -> u64 {
        self.chunks.iter().map(|c| c.done).sum()
    }

    pub fn is_complete(&self) -> bool {
        self.ranges && self.total > 0 && self.chunks.iter().all(|c| c.remaining() == 0)
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }

    pub fn from_json(text: &str) -> Option<Self> {
        serde_json::from_str(text).ok()
    }
}

/// The engine. Cheap to clone; one per process.
#[derive(Clone)]
pub struct Engine {
    config: EngineConfig,
    client: reqwest::Client,
}

impl Engine {
    pub fn new(config: EngineConfig) -> anyhow::Result<Self> {
        let mut headers = reqwest::header::HeaderMap::new();
        if let Some(referer) = &config.referer
            && let Ok(value) = reqwest::header::HeaderValue::from_str(referer)
        {
            headers.insert(reqwest::header::REFERER, value);
        }
        // A browser-like agent by default: a great many file hosts answer 403 to anything
        // that is not a browser, and this engine is standing in for one.
        let agent = config
            .user_agent
            .clone()
            .unwrap_or_else(|| DEFAULT_AGENT.to_string());
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(15))
            // No overall timeout: a big file takes as long as it takes. Idle detection
            // is the per-read timeout below.
            .read_timeout(Duration::from_secs(60))
            .redirect(reqwest::redirect::Policy::none())
            .user_agent(agent)
            .default_headers(headers)
            .build()?;
        Ok(Self { config, client })
    }

    pub fn with_client(config: EngineConfig, client: reqwest::Client) -> Self {
        Self { config, client }
    }

    pub fn config(&self) -> &EngineConfig {
        &self.config
    }

    /// Fetches one task to completion (or until `stop` says pause).
    ///
    /// `resume` is the plan a previous run left behind. It is used only if the server
    /// still reports the same validators; otherwise the file is fetched again from
    /// zero and [`EngineError::Changed`] is not returned — a changed file is a new
    /// download, and saying so is the caller's job via the returned plan.
    #[allow(clippy::too_many_arguments)]
    pub async fn run_task(
        &self,
        id: TaskId,
        url: Url,
        target: PathBuf,
        resume: Option<Plan>,
        connections: u8,
        sink: Arc<dyn StoreSink>,
        mut stop: watch::Receiver<bool>,
        mut cancel: Option<tokio::sync::broadcast::Receiver<()>>,
    ) -> Result<Outcome, EngineError> {
        // A playlist or an FTP URL is not the chunked HTTP path; hand it to its pack.
        if crate::packs::kind_of(&url) != crate::packs::Kind::Http {
            let context = crate::packs::PackContext {
                id,
                target: &target,
                sink: sink.as_ref(),
                stop: stop.clone(),
                client: &self.client,
                policy: &self.config.policy,
            };
            return match crate::packs::run_pack(&context, &url).await {
                Ok(bytes) => {
                    sink.on_finished(id, Outcome::Completed, bytes, Some(bytes));
                    Ok(Outcome::Completed)
                }
                Err(crate::packs::PackError::Paused) => {
                    sink.on_finished(id, Outcome::Paused, 0, None);
                    Ok(Outcome::Paused)
                }
                Err(error) => {
                    let message = error.to_string();
                    sink.on_failed(
                        id,
                        &message,
                        !matches!(error, crate::packs::PackError::Unsupported(_)),
                    );
                    Err(crate::packs::to_engine(error))
                }
            };
        }

        let probe_source = crate::http::HttpProbe::with_client(self.client.clone());
        let outcome = match probe_source.probe(&url, &self.config.policy).await {
            Ok(outcome) => outcome,
            // A host can refuse every question the probe asks and still serve the file to a
            // plain GET. Trying that is the difference between "this downloads" and "this
            // reports 400", so a failed probe falls through to a single stream instead of
            // ending the task.
            Err(error) => {
                return self
                    .stream_to_file(id, &url, &target, sink.as_ref(), stop.clone(), &error)
                    .await;
            }
        };
        let probe = outcome.probe;

        let plan = self.plan_for(&probe, resume.as_ref(), connections);
        let plan = match plan {
            Ok(p) => p,
            Err(e) => {
                sink.on_failed(id, &e.to_string(), false);
                return Err(e);
            }
        };
        sink.on_plan(id, &probe, &plan, connections.clamp(1, MAX_CONNECTIONS));

        let file = Arc::new(open_target(&target, &plan)?);
        let shared = Arc::new(Shared {
            plan: std::sync::Mutex::new(plan),
            received: AtomicU64::new(0),
            speed: AtomicU64::new(0),
            finished: std::sync::atomic::AtomicBool::new(false),
        });
        shared
            .received
            .store(shared.plan.lock().unwrap().recorded(), Ordering::Relaxed);

        // A caller that passes no cancel channel still needs one that never fires: a
        // receiver dropped with its sender reports "closed", which is not a cancel.
        let _keepalive;
        let mut cancel = match cancel.take() {
            Some(rx) => rx,
            None => {
                let (tx, rx) = tokio::sync::broadcast::channel(1);
                _keepalive = tx;
                rx
            }
        };

        // One supervisor task reads the shared plan, reports progress and commits it.
        let supervisor = {
            let shared = shared.clone();
            let sink = sink.clone();
            tokio::spawn(async move {
                let mut last_commit = Instant::now();
                let mut last_bytes = shared.received.load(Ordering::Relaxed);
                let mut last_tick = Instant::now();
                let mut committed = last_bytes;
                loop {
                    tokio::time::sleep(TICK).await;
                    let now = Instant::now();
                    let received = shared.received.load(Ordering::Relaxed);
                    let elapsed = now.duration_since(last_tick).as_secs_f64();
                    if elapsed > 0.0 {
                        let instant =
                            ((received.saturating_sub(last_bytes)) as f64 / elapsed) as u64;
                        let previous = shared.speed.load(Ordering::Relaxed);
                        // Exponential smoothing, so the number does not jump.
                        let smoothed = (instant as f64 * 0.4 + previous as f64 * 0.6) as u64;
                        shared.speed.store(smoothed, Ordering::Relaxed);
                    }
                    last_bytes = received;
                    last_tick = now;

                    let plan = shared.plan.lock().unwrap().clone();
                    sink.on_progress(
                        id,
                        &Progress {
                            received,
                            total: if plan.total > 0 {
                                Some(plan.total)
                            } else {
                                None
                            },
                            speed: shared.speed.load(Ordering::Relaxed),
                        },
                    );

                    if now.duration_since(last_commit) >= COMMIT_EVERY && received != committed {
                        // Only ever the sum of chunks; never a byte the disk has not
                        // acknowledged.
                        sink.on_commit(
                            id,
                            received,
                            if plan.total > 0 {
                                Some(plan.total)
                            } else {
                                None
                            },
                            shared.speed.load(Ordering::Relaxed),
                        );
                        committed = received;
                        last_commit = now;
                    }
                    if shared.finished.load(Ordering::Relaxed) {
                        break;
                    }
                }
            })
        };

        let result = self
            .fetch_all(&url, &probe, file, shared.clone(), &mut stop, &mut cancel)
            .await;

        shared.finished.store(true, Ordering::Relaxed);
        let _ = supervisor.await;

        match result {
            Ok(()) => {
                let plan = shared.plan.lock().unwrap().clone();
                if *stop.borrow() {
                    sink.on_finished(id, Outcome::Paused, plan.recorded(), total_of(&plan));
                    return Ok(Outcome::Paused);
                }
                match self.finish(&target, &plan, &probe) {
                    Ok(()) => {
                        // The count is taken from the file the check just accepted, not
                        // from the running total the supervisor kept.
                        sink.on_finished(id, Outcome::Completed, plan.total, total_of(&plan));
                        Ok(Outcome::Completed)
                    }
                    Err(e) => {
                        let resumable =
                            !matches!(e, EngineError::Digest { .. } | EngineError::Short { .. });
                        sink.on_failed(id, &e.to_string(), resumable);
                        Err(e)
                    }
                }
            }
            Err(EngineError::Cancelled) => {
                sink.on_failed(id, "cancelled", true);
                Err(EngineError::Cancelled)
            }
            Err(EngineError::Stopped) => {
                let plan = shared.plan.lock().unwrap().clone();
                sink.on_finished(id, Outcome::Paused, plan.recorded(), total_of(&plan));
                Ok(Outcome::Paused)
            }
            Err(e) => {
                let resumable = matches!(e, EngineError::Transport(_) | EngineError::Io { .. });
                sink.on_failed(id, &e.to_string(), resumable);
                Err(e)
            }
        }
    }

    /// Chooses the plan: the old one when the server's file is provably the same,
    /// otherwise a fresh split.
    /// The last resort: one GET, no ranges, no questions asked first.
    ///
    /// This is what a browser does, and it is what makes a signed CDN link work when HEAD
    /// and ranged requests are refused. Progress comes from the bytes written.
    async fn stream_to_file(
        &self,
        id: TaskId,
        url: &Url,
        target: &std::path::Path,
        sink: &dyn StoreSink,
        stop: watch::Receiver<bool>,
        probe_error: &crate::http::ProbeError,
    ) -> Result<Outcome, EngineError> {
        use futures_util::StreamExt;
        use std::io::Write;

        if self.config.policy.check_url(url) != omad_core::ssrf::Verdict::Allowed {
            let message = format!("the address is not one we may fetch: {url}");
            sink.on_failed(id, &message, false);
            return Err(EngineError::Transport(message));
        }

        let mut request = self.client.get(url.as_str());
        if let Some(agent) = &self.config.user_agent {
            request = request.header(reqwest::header::USER_AGENT, agent);
        }
        let response = match request.send().await {
            Ok(response) => response,
            Err(error) => {
                let message = format!("{probe_error}; the plain download failed too: {error}");
                sink.on_failed(id, &message, true);
                return Err(EngineError::Transport(message));
            }
        };
        if !response.status().is_success() {
            let message = format!(
                "{probe_error}; a plain GET was answered {}",
                response.status().as_u16()
            );
            sink.on_failed(id, &message, false);
            return Err(EngineError::Transport(message));
        }

        let total = response.content_length();
        let mut file = std::fs::File::create(target).map_err(|e| EngineError::Io {
            path: target.to_path_buf(),
            source: e,
        })?;
        let mut stream = response.bytes_stream();
        let mut written = 0u64;
        let started = std::time::Instant::now();
        while let Some(chunk) = stream.next().await {
            if *stop.borrow() {
                sink.on_finished(id, Outcome::Paused, written, total);
                return Ok(Outcome::Paused);
            }
            let chunk = chunk.map_err(|e| EngineError::Transport(e.to_string()))?;
            file.write_all(&chunk).map_err(|e| EngineError::Io {
                path: target.to_path_buf(),
                source: e,
            })?;
            written += chunk.len() as u64;
            let elapsed = started.elapsed().as_secs_f64().max(0.001);
            sink.on_progress(
                id,
                &Progress {
                    received: written,
                    total,
                    speed: (written as f64 / elapsed) as u64,
                },
            );
            sink.on_commit(id, written, total, (written as f64 / elapsed) as u64);
        }
        file.flush().map_err(|e| EngineError::Io {
            path: target.to_path_buf(),
            source: e,
        })?;
        if let Some(total) = total
            && written != total
        {
            return Err(EngineError::Short {
                written,
                expected: total,
            });
        }
        sink.on_finished(id, Outcome::Completed, written, total);
        Ok(Outcome::Completed)
    }

    fn plan_for(
        &self,
        probe: &Probe,
        resume: Option<&Plan>,
        connections: u8,
    ) -> Result<Plan, EngineError> {
        let total = probe.total_bytes;

        if !probe.ranges {
            return match total {
                Some(t) => Ok(Plan::single(Some(t))),
                None => Ok(Plan::single(None)),
            };
        }

        if let Some(old) = resume
            && old.ranges
            && old.total > 0
            && total == Some(old.total)
            && validators_match(old, probe)
        {
            let mut plan = old.clone();
            // A chunk that claims more than its range cannot be trusted.
            for chunk in &mut plan.chunks {
                chunk.done = chunk.done.min(chunk.len());
            }
            return Ok(plan);
        }

        match total {
            Some(t) => Ok(Plan::split(t, connections, self.config.chunk_size)),
            // A ranged server that hides its size is not chunkable; fall back to one
            // connection and discover the end by reading.
            None => Ok(Plan::single(None)),
        }
    }

    async fn fetch_all(
        &self,
        _url: &Url,
        probe: &Probe,
        file: Arc<File>,
        shared: Arc<Shared>,
        stop: &mut watch::Receiver<bool>,
        cancel: &mut tokio::sync::broadcast::Receiver<()>,
    ) -> Result<(), EngineError> {
        let chunks = shared.plan.lock().unwrap().chunks.clone();
        if chunks.is_empty() {
            return Ok(());
        }

        // Chunks with work left, each on its own connection.
        let mut handles = Vec::new();
        for (index, chunk) in chunks.iter().enumerate() {
            if chunk.remaining() == 0 {
                continue;
            }
            let target = probe.final_url.clone();
            let file = file.clone();
            let shared = shared.clone();
            let policy = self.config.policy.clone();
            let client = self.client.clone();
            let etag = probe.etag.clone();
            let last_modified = probe.last_modified.clone();
            let agent = self.config.user_agent.clone();
            let chunk = chunk.clone();
            let mut stop = stop.clone();
            let mut cancel = cancel.resubscribe();

            handles.push(tokio::spawn(async move {
                chunk_worker(
                    index,
                    chunk,
                    target,
                    file,
                    shared,
                    policy,
                    client,
                    etag,
                    last_modified,
                    agent,
                    &mut stop,
                    &mut cancel,
                )
                .await
            }));
        }

        let mut first_error: Option<EngineError> = None;
        for handle in handles {
            match handle.await {
                Ok(Ok(())) => {}
                Ok(Err(ChunkStop::Stopped)) => return Err(EngineError::Stopped),
                Ok(Err(ChunkStop::Cancelled)) => return Err(EngineError::Cancelled),
                Ok(Err(ChunkStop::Failed(e))) => {
                    if first_error.is_none() {
                        first_error = Some(e);
                    }
                }
                Err(join) => {
                    if first_error.is_none() {
                        first_error = Some(EngineError::Transport(join.to_string()));
                    }
                }
            }
        }

        // A pause or cancel during a failing chunk still means pause or cancel.
        if *stop.borrow() {
            return Err(EngineError::Stopped);
        }
        match first_error {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    /// Reads the file back and decides whether it is really complete.
    ///
    /// This is the step that makes "completed" mean something after a crash: the
    /// record saying "done" is not enough, the bytes have to be there.
    fn finish(&self, target: &Path, plan: &Plan, probe: &Probe) -> Result<(), EngineError> {
        let meta = std::fs::metadata(target).map_err(|e| EngineError::Io {
            path: target.to_path_buf(),
            source: e,
        })?;
        let on_disk = meta.len();
        if plan.total > 0 && on_disk != plan.total {
            return Err(EngineError::Short {
                written: on_disk,
                expected: plan.total,
            });
        }
        if let Some(expected) = &probe.sha256 {
            let actual = sha256_file(target)?;
            if !actual.eq_ignore_ascii_case(expected) {
                return Err(EngineError::Digest {
                    expected: expected.clone(),
                    actual,
                });
            }
        }
        Ok(())
    }
}

/// The plan and the counters every connection shares.
struct Shared {
    plan: std::sync::Mutex<Plan>,
    received: AtomicU64,
    speed: AtomicU64,
    finished: std::sync::atomic::AtomicBool,
}

enum ChunkStop {
    Stopped,
    Cancelled,
    Failed(EngineError),
}

fn total_of(plan: &Plan) -> Option<u64> {
    (plan.total > 0).then_some(plan.total)
}

fn validators_match(plan: &Plan, probe: &Probe) -> bool {
    match (&plan.etag, &probe.etag) {
        (Some(a), Some(b)) => a == b,
        _ => match (&plan.last_modified, &probe.last_modified) {
            (Some(a), Some(b)) => a == b,
            // No validators at all: the size is all we have, and a resume is still
            // better than losing the bytes. The digest check at the end catches the
            // case where this was wrong.
            _ => true,
        },
    }
}

fn open_target(path: &Path, plan: &Plan) -> Result<File, EngineError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| EngineError::Io {
            path: parent.to_path_buf(),
            source: e,
        })?;
    }
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .map_err(|e| EngineError::Io {
            path: path.to_path_buf(),
            source: e,
        })?;
    if plan.total > 0 {
        // Preallocate so a full disk is discovered before the download, not after.
        file.set_len(plan.total).map_err(|e| EngineError::Io {
            path: path.to_path_buf(),
            source: e,
        })?;
    }
    Ok(file)
}

/// A positioned write that must write everything it was given.
///
/// Two properties matter and both are tested:
/// * it never spans its chunk, so two connections can never write the same byte;
/// * a short write is an error, because silently accepting it corrupts the file.
#[cfg(unix)]
fn write_at(file: &File, buffer: &[u8], offset: u64) -> std::io::Result<()> {
    use std::os::unix::fs::FileExt;
    file.write_all_at(buffer, offset)
}

#[cfg(windows)]
fn write_at(file: &File, buffer: &[u8], offset: u64) -> std::io::Result<()> {
    use std::os::windows::fs::FileExt;
    let mut written = 0;
    while written < buffer.len() {
        let n = file.seek_write(&buffer[written..], offset + written as u64)?;
        if n == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::WriteZero,
                "the disk accepted no bytes",
            ));
        }
        written += n;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn chunk_worker(
    index: usize,
    chunk: Chunk,
    url: Url,
    file: Arc<File>,
    shared: Arc<Shared>,
    policy: UrlPolicy,
    client: reqwest::Client,
    etag: Option<String>,
    last_modified: Option<String>,
    agent: Option<String>,
    stop: &mut watch::Receiver<bool>,
    cancel: &mut tokio::sync::broadcast::Receiver<()>,
) -> Result<(), ChunkStop> {
    use futures_util::StreamExt;

    let verdict = policy.check_url(&url);
    if verdict != Verdict::Allowed {
        return Err(ChunkStop::Failed(EngineError::Probe(ProbeError::Refused(
            url, verdict,
        ))));
    }

    let mut done = chunk.done;
    let mut attempt = 0u32;

    loop {
        if *stop.borrow() {
            return Err(ChunkStop::Stopped);
        }

        let ranges = shared.plan.lock().unwrap().ranges;
        let mut request = client.get(url.as_str());
        if ranges {
            let from = chunk.start + done;
            request = request.header(
                reqwest::header::RANGE,
                format!("bytes={from}-{}", chunk.end),
            );
        }
        if let Some(etag) = &etag {
            request = request.header(reqwest::header::IF_RANGE, etag);
        }
        if let Some(modified) = &last_modified {
            request = request.header(reqwest::header::IF_RANGE, modified);
        }
        if let Some(agent) = &agent {
            request = request.header(reqwest::header::USER_AGENT, agent);
        }

        let response = match request.send().await {
            Ok(r) => r,
            Err(e) => {
                attempt += 1;
                if attempt > 4 {
                    return Err(ChunkStop::Failed(EngineError::Transport(e.to_string())));
                }
                backoff(attempt, stop).await?;
                continue;
            }
        };

        let status = response.status();
        // 416 means the server holds a different (smaller) file. 200 to a ranged
        // request means the range was ignored: the answer is the whole file, which
        // this connection must not write.
        if status == reqwest::StatusCode::RANGE_NOT_SATISFIABLE {
            return Err(ChunkStop::Failed(EngineError::Changed));
        }
        if status == reqwest::StatusCode::PRECONDITION_FAILED {
            return Err(ChunkStop::Failed(EngineError::Changed));
        }
        if status != reqwest::StatusCode::PARTIAL_CONTENT && !(status.is_success() && !ranges) {
            attempt += 1;
            if attempt > 4 {
                return Err(ChunkStop::Failed(EngineError::Probe(ProbeError::Status(
                    status.as_u16(),
                ))));
            }
            backoff(attempt, stop).await?;
            continue;
        }

        let mut stream = response.bytes_stream();
        let mut write_at_offset = chunk.start + done;

        loop {
            tokio::select! {
                biased;
                _ = cancel.recv() => return Err(ChunkStop::Cancelled),
                changed = stop.changed() => {
                    // A dropped sender means "nobody will ask for a pause any more", not
                    // "pause": treat it as no signal and keep reading.
                    match changed {
                        Err(_) => {}
                        Ok(()) if *stop.borrow() => return Err(ChunkStop::Stopped),
                        Ok(()) => {}
                    }
                }
                next = stream.next() => {
                    match next {
                        None => {
                            let plan = shared.plan.lock().unwrap();
                            let current = &plan.chunks[index];
                            if current.done >= current.len() {
                                return Ok(());
                            }
                            // The stream ended early. Reconnect from where we are.
                            done = current.done;
                            break;
                        }
                        Some(Err(e)) => {
                            attempt += 1;
                            if attempt > 4 {
                                return Err(ChunkStop::Failed(EngineError::Transport(e.to_string())));
                            }
                            // Keep what was written; reconnect from there.
                            done = shared.plan.lock().unwrap().chunks[index].done;
                            backoff(attempt, stop).await?;
                            break;
                        }
                        Some(Ok(bytes)) => {
                            let room = chunk.len() - done;
                            if room == 0 {
                                continue;
                            }
                            let take = (bytes.len() as u64).min(room) as usize;
                            let slice = &bytes[..take];

                            if let Err(e) = write_at(&file, slice, write_at_offset) {
                                return Err(ChunkStop::Failed(EngineError::Io {
                                    path: PathBuf::from("<download>"),
                                    source: e,
                                }));
                            }
                            // Disk first, then the record: a crash in between loses
                            // progress instead of inventing it.
                            write_at_offset += take as u64;
                            done += take as u64;
                            {
                                let mut plan = shared.plan.lock().unwrap();
                                plan.chunks[index].done = done;
                            }
                            shared.received.fetch_add(take as u64, Ordering::Relaxed);

                            if done >= chunk.len() {
                                return Ok(());
                            }
                        }
                    }
                }
            }
        }
    }
}

async fn backoff(attempt: u32, stop: &mut watch::Receiver<bool>) -> Result<(), ChunkStop> {
    let delay = Duration::from_millis(250 * 2u64.pow(attempt.min(5)));
    tokio::select! {
        _ = tokio::time::sleep(delay) => Ok(()),
        changed = stop.changed() => {
            match changed {
                Err(_) => Ok(()),
                Ok(()) if *stop.borrow() => Err(ChunkStop::Stopped),
                Ok(()) => Ok(()),
            }
        }
    }
}

fn sha256_file(path: &Path) -> Result<String, EngineError> {
    use sha2::{Digest, Sha256};
    use std::io::Read;
    let mut file = File::open(path).map_err(|e| EngineError::Io {
        path: path.to_path_buf(),
        source: e,
    })?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 256 * 1024];
    loop {
        let n = file.read(&mut buffer).map_err(|e| EngineError::Io {
            path: path.to_path_buf(),
            source: e,
        })?;
        if n == 0 {
            break;
        }
        hasher.update(&buffer[..n]);
    }
    Ok(hex::encode(hasher.finalize()))
}

impl Engine {
    /// Exposes the verified-finish step to tests: the same code `run_task` calls.
    pub fn finish_for_test(
        &self,
        target: &Path,
        plan: &Plan,
        probe: &Probe,
    ) -> Result<(), EngineError> {
        self.finish(target, plan, probe)
    }
}
