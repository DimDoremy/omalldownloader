//! Pausing a download that is moving.
//!
//! A pause has to be cooperative: the engine must stop at the next read, keep every byte it
//! already wrote, tell the store how far it got, and be resumable from exactly there. This
//! test drives that with a server that dribbles bytes so there is a real transfer to
//! interrupt, and then checks the file on disk against the source.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use omad_core::TaskId;
use omad_engine::engine::Plan;
use omad_engine::{Engine, EngineConfig, Outcome, Progress, StoreSink};

const SIZE: usize = 2 * 1024 * 1024;

fn payload(size: usize) -> Vec<u8> {
    (0..size).map(|i| ((i * 13) % 251) as u8).collect()
}

/// Serves the body in small pieces with a pause between them, so a test can stop mid-stream.
fn slow_server(body: Vec<u8>, chunk: usize, gap: Duration) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { return };
            let body = body.clone();
            std::thread::spawn(move || {
                let mut buffer = [0u8; 8192];
                let mut read = 0;
                loop {
                    let Ok(n) = stream.read(&mut buffer[read..]) else {
                        return;
                    };
                    if n == 0 {
                        return;
                    }
                    read += n;
                    if buffer[..read].windows(4).any(|w| w == b"\r\n\r\n") {
                        break;
                    }
                }
                let request = String::from_utf8_lossy(&buffer[..read]).to_string();
                let head_only = request.starts_with("HEAD");
                // Honour a Range request: the engine chunks with them, and a server that
                // ignored ranges while advertising them would be a different test.
                let range = request
                    .lines()
                    .find(|line| line.to_ascii_lowercase().starts_with("range:"))
                    .and_then(|line| line.split_once('='))
                    .and_then(|(_, value)| value.split_once('-'))
                    .map(|(from, to)| {
                        let from: usize = from.trim().parse().unwrap_or(0);
                        let to = to
                            .trim()
                            .parse::<usize>()
                            .ok()
                            .map(|t| t.min(body.len() - 1))
                            .unwrap_or(body.len() - 1);
                        (from, to)
                    });
                let (from, to, status) = match range {
                    Some((from, to)) => (from, to, 206u16),
                    None => (0, body.len() - 1, 200u16),
                };
                let slice = &body[from..=to];
                let head = format!(
                    "HTTP/1.1 {status} {}\r\nAccept-Ranges: bytes\r\nETag: \"pause\"\r\n\
                     Content-Disposition: attachment; filename=\"big.bin\"\r\n\
                     Content-Range: bytes {from}-{to}/{}\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n",
                    if status == 206 {
                        "Partial Content"
                    } else {
                        "OK"
                    },
                    body.len(),
                    slice.len()
                );
                let _ = stream.write_all(head.as_bytes());
                if head_only {
                    return;
                }
                for piece in slice.chunks(chunk) {
                    if stream.write_all(piece).is_err() {
                        return;
                    }
                    let _ = stream.flush();
                    std::thread::sleep(gap);
                }
            });
        }
    });
    port
}

#[derive(Default)]
struct Watcher {
    received: AtomicU64,
    finished: std::sync::Mutex<Option<Outcome>>,
    committed: AtomicU64,
    plan: std::sync::Mutex<Option<Plan>>,
}

impl StoreSink for Watcher {
    fn on_plan(&self, _: TaskId, _: &omad_engine::http::Probe, plan: &Plan, _: u8) {
        *self.plan.lock().unwrap() = Some(plan.clone());
    }
    fn on_progress(&self, _: TaskId, progress: &Progress) {
        self.received.store(progress.received, Ordering::Relaxed);
    }
    fn on_commit(&self, _: TaskId, received: u64, _: Option<u64>, _: u64) {
        self.committed.store(received, Ordering::Relaxed);
    }
    fn on_finished(&self, _: TaskId, outcome: Outcome, recorded: u64, _: Option<u64>) {
        self.committed.store(recorded, Ordering::Relaxed);
        *self.finished.lock().unwrap() = Some(outcome);
    }
    fn on_failed(&self, _: TaskId, _: &str, _: bool) {}
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pausing_stops_promptly_and_keeps_what_was_written() {
    let body = payload(SIZE);
    let port = slow_server(body.clone(), 64 * 1024, Duration::from_millis(60));
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("big.bin");

    let engine = Engine::new(EngineConfig {
        max_connections: 2,
        chunk_size: 256 * 1024,
        policy: omad_core::ssrf::UrlPolicy::permissive(),
        user_agent: None,
        referer: None,
    })
    .unwrap();

    let watcher = Arc::new(Watcher::default());
    let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
    let url = url::Url::parse(&format!("http://127.0.0.1:{port}/big.bin")).unwrap();

    // Ask for the pause once a third of the file is on disk.
    let stopper = {
        let watcher = watcher.clone();
        let stop_tx = stop_tx.clone();
        tokio::spawn(async move {
            let deadline = Instant::now() + Duration::from_secs(20);
            loop {
                if watcher.received.load(Ordering::Relaxed) >= SIZE as u64 / 3
                    || Instant::now() > deadline
                {
                    let _ = stop_tx.send(true);
                    return;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
    };

    let started = Instant::now();
    let outcome = engine
        .run_task(
            TaskId(1),
            url.clone(),
            target.clone(),
            None,
            2,
            watcher.clone(),
            stop_rx,
            None,
        )
        .await
        .expect("a pause is not an error");
    stopper.abort();

    assert_eq!(outcome, Outcome::Paused, "the engine must report a pause");
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "the pause took {:?}, which is not prompt",
        started.elapsed()
    );
    assert_eq!(
        *watcher.finished.lock().unwrap(),
        Some(Outcome::Paused),
        "the caller is told the task paused"
    );

    let recorded = watcher.committed.load(Ordering::Relaxed);
    assert!(recorded > 0, "some bytes were written before the pause");
    assert!(recorded < SIZE as u64, "the pause happened before the end");

    // The bytes that were written must be the bytes the server sent, at the offsets the
    // plan says they went to. Chunks are written in parallel, so this is checked per chunk
    // rather than over one prefix of the file.
    let plan = watcher
        .plan
        .lock()
        .unwrap()
        .clone()
        .expect("a plan was reported");
    let on_disk = std::fs::read(&target).unwrap();
    assert_eq!(
        on_disk.len(),
        SIZE,
        "a paused file is preallocated to its full size"
    );
    // Every byte that was written is correct, at its own offset. The rest of the file is
    // still the zeroes the preallocation left, which a resume overwrites.
    let mut nonzero = 0usize;
    for (offset, byte) in on_disk.iter().enumerate() {
        if *byte == 0 {
            continue;
        }
        nonzero += 1;
        assert!(
            *byte == body[offset],
            "byte {offset} is {} but the server sent {}",
            byte,
            body[offset]
        );
    }
    assert!(nonzero > 0, "a pause before any byte is a different test");
    assert!(
        nonzero < SIZE,
        "the pause happened before the end, so part of the file is still missing"
    );
    let planned: u64 = plan.chunks.iter().map(|c| c.end - c.start + 1).sum();
    assert_eq!(planned, SIZE as u64, "the plan covers the whole file");

    // Resuming from that same plan finishes the file, byte for byte.
    let (_resume_tx, resume_rx) = tokio::sync::watch::channel(false);
    let resumed = engine
        .run_task(
            TaskId(1),
            url,
            target.clone(),
            Some(plan),
            2,
            watcher.clone(),
            resume_rx,
            None,
        )
        .await
        .expect("the resume runs");
    assert_eq!(resumed, Outcome::Completed);
    assert!(
        std::fs::read(&target).unwrap() == body,
        "the resumed file is identical to the source"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pausing_before_the_first_byte_is_also_a_pause() {
    let body = payload(256 * 1024);
    let port = slow_server(body, 64 * 1024, Duration::from_millis(80));
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("early.bin");
    let engine = Engine::new(EngineConfig {
        max_connections: 1,
        chunk_size: 256 * 1024,
        policy: omad_core::ssrf::UrlPolicy::permissive(),
        user_agent: None,
        referer: None,
    })
    .unwrap();
    let watcher = Arc::new(Watcher::default());
    let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
    // Already paused when the engine starts.
    stop_tx.send(true).unwrap();
    let url = url::Url::parse(&format!("http://127.0.0.1:{port}/x")).unwrap();
    let outcome = engine
        .run_task(TaskId(2), url, target, None, 1, watcher, stop_rx, None)
        .await
        .expect("an immediate pause is not an error");
    assert_eq!(outcome, Outcome::Paused);
}
