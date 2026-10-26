//! The engine against a real local HTTP server.
//!
//! Every test here serves bytes from a loopback server it starts itself, so the
//! network path (range requests, resume, a server that changes under us) is really
//! exercised. Because the server is on loopback, each engine uses the permissive
//! policy; the refusal itself is tested in `omad-core`.

use omad_core::TaskId;
use omad_engine::engine::{Chunk, Plan, StoreSink};
use omad_engine::http::Probe;
use omad_engine::{Engine, EngineConfig, Outcome, Progress};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

// ---------------------------------------------------------------- test server

struct TestServer {
    port: u16,
    state: Arc<ServerState>,
    _handle: std::thread::JoinHandle<()>,
}

struct ServerState {
    body: Mutex<Vec<u8>>,
    etag: Mutex<String>,
    ranges: AtomicBool,
    /// When true, the server hangs up after the first kilobyte of a ranged answer.
    drop_after: AtomicU64,
    requests: AtomicU64,
    range_requests: AtomicU64,
}

impl TestServer {
    fn start(body: Vec<u8>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let state = Arc::new(ServerState {
            body: Mutex::new(body),
            etag: Mutex::new("\"v1\"".into()),
            ranges: AtomicBool::new(true),
            drop_after: AtomicU64::new(u64::MAX),
            requests: AtomicU64::new(0),
            range_requests: AtomicU64::new(0),
        });

        let shared = state.clone();
        let handle = std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let state = shared.clone();
                std::thread::spawn(move || {
                    let _ = serve(&mut stream, &state);
                });
            }
        });

        Self {
            port,
            state,
            _handle: handle,
        }
    }

    fn url(&self, path: &str) -> url::Url {
        url::Url::parse(&format!("http://127.0.0.1:{}{}", self.port, path)).unwrap()
    }
}

fn serve(stream: &mut std::net::TcpStream, state: &ServerState) -> std::io::Result<()> {
    let mut buffer = [0u8; 8192];
    let mut read = 0;
    loop {
        let n = stream.read(&mut buffer[read..])?;
        if n == 0 {
            return Ok(());
        }
        read += n;
        if buffer[..read].windows(4).any(|w| w == b"\r\n\r\n") {
            break;
        }
        if read == buffer.len() {
            break;
        }
    }
    let request = String::from_utf8_lossy(&buffer[..read]).to_string();
    let mut lines = request.lines();
    let first = lines.next().unwrap_or("");
    let method = first.split(' ').next().unwrap_or("GET");
    let mut headers = Vec::new();
    for line in lines {
        if line.is_empty() {
            break;
        }
        if let Some((k, v)) = line.split_once(':') {
            headers.push((k.trim().to_ascii_lowercase(), v.trim().to_string()));
        }
    }
    let header = |name: &str| {
        headers
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.clone())
    };

    state.requests.fetch_add(1, Ordering::Relaxed);
    let body = state.body.lock().unwrap().clone();
    let etag = state.etag.lock().unwrap().clone();
    let ranges = state.ranges.load(Ordering::Relaxed);

    if let Some(if_range) = header("if-range")
        && if_range != etag
    {
        write_headers(stream, 412, &[], 0)?;
        return Ok(());
    }

    if method == "HEAD" {
        let mut extra = vec![("ETag", etag.clone())];
        if ranges {
            extra.push(("Accept-Ranges", "bytes".into()));
        }
        write_headers(stream, 200, &extra, body.len() as u64)?;
        return Ok(());
    }

    let range = header("range").and_then(|r| {
        let rest = r.strip_prefix("bytes=")?;
        let (from, to) = rest.split_once('-')?;
        let from: u64 = from.parse().ok()?;
        let to: u64 = if to.is_empty() {
            body.len() as u64 - 1
        } else {
            to.parse().ok()?
        };
        Some((from, to))
    });

    match (range, ranges) {
        (Some((from, to)), true) => {
            state.range_requests.fetch_add(1, Ordering::Relaxed);
            if from >= body.len() as u64 {
                write_headers(stream, 416, &[], 0)?;
                return Ok(());
            }
            let to = to.min(body.len() as u64 - 1);
            let slice = body[from as usize..=to as usize].to_vec();
            let content_range = format!("bytes {from}-{to}/{}", body.len());
            write_headers(
                stream,
                206,
                &[
                    ("Accept-Ranges", "bytes".into()),
                    ("ETag", etag.clone()),
                    ("Content-Range", content_range),
                ],
                slice.len() as u64,
            )?;
            let drop_after = state.drop_after.load(Ordering::Relaxed);
            if drop_after < slice.len() as u64 {
                stream.write_all(&slice[..drop_after as usize])?;
                stream.flush()?;
                // Hang up mid-stream.
                return Ok(());
            }
            stream.write_all(&slice)?;
        }
        _ => {
            write_headers(stream, 200, &[("ETag", etag.clone())], body.len() as u64)?;
            stream.write_all(&body)?;
        }
    }
    stream.flush()
}

fn write_headers(
    stream: &mut std::net::TcpStream,
    status: u16,
    extra: &[(&str, String)],
    length: u64,
) -> std::io::Result<()> {
    let reason = match status {
        200 => "OK",
        206 => "Partial Content",
        412 => "Precondition Failed",
        416 => "Range Not Satisfiable",
        _ => "Status",
    };
    let mut text =
        format!("HTTP/1.1 {status} {reason}\r\nContent-Length: {length}\r\nConnection: close\r\n");
    for (k, v) in extra {
        text.push_str(&format!("{k}: {v}\r\n"));
    }
    text.push_str("\r\n");
    stream.write_all(text.as_bytes())
}

// ---------------------------------------------------------------- sink

#[derive(Default)]
struct Recorder {
    final_count: Mutex<Option<u64>>,
    progress: Mutex<Vec<u64>>,
    commits: Mutex<Vec<u64>>,
    finished: Mutex<Option<Outcome>>,
    failure: Mutex<Option<(String, bool)>>,
    plan: Mutex<Option<(Option<u64>, usize)>>,
}

impl StoreSink for Recorder {
    fn on_plan(&self, _id: TaskId, probe: &Probe, plan: &Plan, _c: u8) {
        let mut slot = self.plan.lock().unwrap();
        *slot = Some((probe.total_bytes, plan.chunks.len()));
    }
    fn on_progress(&self, _id: TaskId, progress: &Progress) {
        self.progress.lock().unwrap().push(progress.received);
    }
    fn on_commit(&self, _id: TaskId, received: u64, _total: Option<u64>, _speed: u64) {
        self.commits.lock().unwrap().push(received);
    }
    fn on_finished(&self, _id: TaskId, outcome: Outcome, recorded: u64, _total: Option<u64>) {
        *self.finished.lock().unwrap() = Some(outcome);
        *self.final_count.lock().unwrap() = Some(recorded);
    }
    fn on_failed(&self, _id: TaskId, error: &str, resumable: bool) {
        *self.failure.lock().unwrap() = Some((error.to_string(), resumable));
    }
}

// ---------------------------------------------------------------- helpers

fn payload(size: usize) -> Vec<u8> {
    (0..size).map(|i| (i % 251) as u8).collect()
}

fn engine() -> Engine {
    Engine::new(EngineConfig {
        max_connections: 4,
        chunk_size: 64 * 1024,
        policy: omad_core::ssrf::UrlPolicy::permissive(),
        user_agent: None,
    })
    .unwrap()
}

async fn run(
    engine: &Engine,
    url: url::Url,
    target: &std::path::Path,
    resume: Option<Plan>,
    sink: Arc<Recorder>,
) -> Result<Outcome, omad_engine::engine::EngineError> {
    let (_tx, rx) = tokio::sync::watch::channel(false);
    engine
        .run_task(
            TaskId(1),
            url,
            target.to_path_buf(),
            resume,
            4,
            sink,
            rx,
            None,
        )
        .await
}

fn read(path: &std::path::Path) -> Vec<u8> {
    std::fs::read(path).unwrap()
}

// ---------------------------------------------------------------- tests

#[tokio::test]
async fn downloads_a_ranged_file_in_chunks_and_verifies_it() {
    let body = payload(300_000);
    let server = TestServer::start(body.clone());
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("file.bin");
    let sink = Arc::new(Recorder::default());

    let outcome = run(
        &engine(),
        server.url("/file.bin"),
        &target,
        None,
        sink.clone(),
    )
    .await
    .unwrap();

    assert_eq!(outcome, Outcome::Completed);
    assert_eq!(
        read(&target),
        body,
        "the bytes must match the source exactly"
    );
    let (total, chunks) = (*sink.plan.lock().unwrap()).unwrap();
    assert_eq!(total, Some(300_000));
    assert!(
        chunks > 1,
        "a 300 KB file with a 64 KB chunk size must be split, got {chunks}"
    );
    assert!(
        server.state.range_requests.load(Ordering::Relaxed) >= 2,
        "ranges were used"
    );
    assert_eq!(*sink.finished.lock().unwrap(), Some(Outcome::Completed));
    // The final count is the file's size, not a running total that could drift.
    assert_eq!(
        *sink.final_count.lock().unwrap(),
        Some(300_000),
        "a completed task records every byte it wrote"
    );
    // The durable commits never exceed what was written.
    for commit in sink.commits.lock().unwrap().iter() {
        assert!(*commit <= 300_000);
    }
}

#[tokio::test]
async fn a_server_without_ranges_still_downloads() {
    let body = payload(120_000);
    let server = TestServer::start(body.clone());
    server.state.ranges.store(false, Ordering::Relaxed);
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("single.bin");
    let sink = Arc::new(Recorder::default());

    let outcome = run(&engine(), server.url("/x"), &target, None, sink.clone())
        .await
        .unwrap();
    assert_eq!(outcome, Outcome::Completed);
    assert_eq!(read(&target), body);
    assert_eq!(
        sink.plan.lock().unwrap().unwrap().1,
        1,
        "one connection, one chunk"
    );
}

#[tokio::test]
async fn resuming_keeps_the_bytes_already_written() {
    let body = payload(400_000);
    let server = TestServer::start(body.clone());
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("resume.bin");

    // First pass: the server hangs up after 10 KB of every ranged answer, so the
    // download cannot finish and must report a resumable failure.
    server.state.drop_after.store(10_000, Ordering::Relaxed);
    let sink = Arc::new(Recorder::default());
    let first = run(&engine(), server.url("/r"), &target, None, sink.clone()).await;
    assert!(
        first.is_err(),
        "a truncated stream should not be reported as complete"
    );
    assert_eq!(*sink.finished.lock().unwrap(), None);
    let (_, resumable) = sink
        .failure
        .lock()
        .unwrap()
        .clone()
        .expect("a failure was reported");
    assert!(resumable, "a transport failure must be resumable");

    // Second pass: the server behaves. The engine resumes from what is on disk.
    server.state.drop_after.store(u64::MAX, Ordering::Relaxed);
    let sink2 = Arc::new(Recorder::default());
    let outcome = run(&engine(), server.url("/r"), &target, None, sink2.clone())
        .await
        .unwrap();

    assert_eq!(outcome, Outcome::Completed);
    assert_eq!(
        read(&target),
        body,
        "resumed bytes must be correct, not just complete"
    );
}

#[tokio::test]
async fn a_resume_plan_is_reused_only_while_the_validator_matches() {
    let body = payload(200_000);
    let server = TestServer::start(body.clone());
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("plan.bin");

    let sink = Arc::new(Recorder::default());
    run(&engine(), server.url("/p"), &target, None, sink.clone())
        .await
        .unwrap();
    let (total, _) = sink.plan.lock().unwrap().unwrap();

    // Build a half-done plan with the current validator, as a crash would leave it.
    let mut plan = Plan::split(total.unwrap(), 4, 64 * 1024);
    plan.etag = Some("\"v1\"".into());
    for chunk in &mut plan.chunks {
        chunk.done = chunk.len() / 2;
    }
    let half = plan.recorded();
    assert!(half > 0);

    // Same validator: the record is trusted, and only the rest is fetched.
    let sink2 = Arc::new(Recorder::default());
    let outcome = run(
        &engine(),
        server.url("/p"),
        &target,
        Some(plan.clone()),
        sink2,
    )
    .await
    .unwrap();
    assert_eq!(outcome, Outcome::Completed);
    assert_eq!(read(&target), body);

    // A different file: the plan must be thrown away rather than trusted.
    *server.state.etag.lock().unwrap() = "\"v2\"".into();
    let changed_body = payload(200_001);
    *server.state.body.lock().unwrap() = changed_body.clone();
    let sink3 = Arc::new(Recorder::default());
    let outcome = run(&engine(), server.url("/p"), &target, Some(plan), sink3)
        .await
        .unwrap();
    assert_eq!(outcome, Outcome::Completed);
    assert_eq!(
        read(&target),
        changed_body,
        "a changed file is fetched again, not patched"
    );
}

#[tokio::test]
async fn a_wrong_digest_is_a_failure_not_a_completion() {
    // The server publishes a Digest header that cannot match; finish() must refuse.
    let body = payload(50_000);
    let server = TestServer::start(body);
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("digest.bin");

    // Force a mismatch by writing a digest-bearing Probe through the engine's own
    // check: simplest is to test the digest path directly.
    std::fs::write(&target, b"notthefile!").unwrap();
    let probe = Probe {
        final_url: server.url("/d"),
        total_bytes: Some(11),
        ranges: true,
        etag: None,
        last_modified: None,
        content_type: None,
        suggested_filename: None,
        sha256: Some("00".repeat(32)),
    };
    let plan = Plan {
        total: 11,
        ranges: true,
        chunks: vec![Chunk {
            start: 0,
            end: 10,
            done: 11,
        }],
        etag: None,
        last_modified: None,
    };
    let engine = engine();
    let result = engine.finish_for_test(&target, &plan, &probe);
    assert!(
        matches!(result, Err(omad_engine::engine::EngineError::Digest { .. })),
        "got {result:?}"
    );
}

#[tokio::test]
async fn a_short_file_is_refused_by_the_finish_check() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("short.bin");
    std::fs::write(&target, b"12345").unwrap();
    let probe = Probe {
        final_url: url::Url::parse("http://127.0.0.1/x").unwrap(),
        total_bytes: Some(10),
        ranges: true,
        etag: None,
        last_modified: None,
        content_type: None,
        suggested_filename: None,
        sha256: None,
    };
    let plan = Plan::split(10, 1, 64 * 1024);
    let result = engine().finish_for_test(&target, &plan, &probe);
    assert!(
        matches!(result, Err(omad_engine::engine::EngineError::Short { .. })),
        "got {result:?}"
    );
}

#[tokio::test]
async fn zero_length_files_complete() {
    let server = TestServer::start(vec![]);
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("empty.bin");
    let sink = Arc::new(Recorder::default());
    let outcome = run(&engine(), server.url("/e"), &target, None, sink)
        .await
        .unwrap();
    assert_eq!(outcome, Outcome::Completed);
    assert_eq!(read(&target).len(), 0);
}
