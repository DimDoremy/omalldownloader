//! A host that refuses every question a probe asks, but serves the file to a plain GET.
//!
//! This is what a signed CDN link does — HEAD 400, ranged GET 400, plain GET 200 — and it
//! is why a task used to fail with "probe refused: the server answered 400" while a browser
//! downloaded the same file happily.

use std::sync::Arc;

use omad_core::TaskId;
use omad_core::ssrf::UrlPolicy;
use omad_engine::engine::{Engine, EngineConfig, Plan};
use omad_engine::http::Probe;
use omad_engine::{Outcome, Progress, StoreSink};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const BODY: &[u8] = b"signed-cdn-payload-0123456789";

#[derive(Default)]
struct Recorder {
    finished: std::sync::Mutex<Option<Outcome>>,
    failed: std::sync::Mutex<Option<String>>,
    received: std::sync::atomic::AtomicU64,
}

impl StoreSink for Recorder {
    fn on_plan(&self, _: TaskId, _: &Probe, _: &Plan, _: u8) {}
    fn on_progress(&self, _: TaskId, progress: &Progress) {
        self.received
            .store(progress.received, std::sync::atomic::Ordering::Relaxed);
    }
    fn on_commit(&self, _: TaskId, _: u64, _: Option<u64>, _: u64) {}
    fn on_finished(&self, _: TaskId, outcome: Outcome, _: u64, _: Option<u64>) {
        *self.finished.lock().unwrap() = Some(outcome);
    }
    fn on_failed(&self, _: TaskId, error: &str, _: bool) {
        *self.failed.lock().unwrap() = Some(error.to_string());
    }
}

async fn serve_once() -> std::net::SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                break;
            };
            let mut buffer = [0u8; 2048];
            let Ok(n) = socket.read(&mut buffer).await else {
                continue;
            };
            let request = String::from_utf8_lossy(&buffer[..n]).to_string();
            let head = request.lines().next().unwrap_or_default().to_string();
            let ranged = request.to_lowercase().contains("range:");
            let reply = if head.starts_with("HEAD") || ranged {
                // Exactly what the real host did.
                "HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    .to_string()
            } else {
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\n\
                     Content-Disposition: attachment; filename=\"signed.iso\"\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n{}",
                    BODY.len(),
                    String::from_utf8_lossy(BODY)
                )
            };
            let _ = socket.write_all(reply.as_bytes()).await;
            let _ = socket.shutdown().await;
        }
    });
    addr
}

#[tokio::test]
async fn a_host_that_refuses_head_and_ranges_still_downloads() {
    let addr = serve_once().await;
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("signed.iso");

    let engine = Engine::new(EngineConfig {
        max_connections: 4,
        chunk_size: 256 * 1024,
        policy: UrlPolicy::permissive(),
        user_agent: None,
        referer: None,
    })
    .unwrap();
    let recorder = Arc::new(Recorder::default());
    let (_stop_tx, stop_rx) = tokio::sync::watch::channel(false);
    let outcome = engine
        .run_task(
            TaskId(1),
            url::Url::parse(&format!("http://{addr}/get?id=7")).unwrap(),
            target.clone(),
            None,
            4,
            recorder.clone(),
            stop_rx,
            None,
        )
        .await
        .expect("the plain-GET fallback carried the download");

    assert_eq!(outcome, Outcome::Completed);
    assert_eq!(std::fs::read(&target).unwrap(), BODY);
    assert_eq!(*recorder.finished.lock().unwrap(), Some(Outcome::Completed));
    assert!(recorder.failed.lock().unwrap().is_none());
}

/// A chunk request must carry exactly one `If-Range`.
///
/// Cloudflare answers a request with two of them with 400 Bad Request, which is how a
/// 1.4 GB download sat at 0 B while `curl` with the same headers worked.
#[tokio::test]
async fn a_chunk_request_sends_one_if_range_header() {
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let count = Arc::new(AtomicUsize::new(0));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let recorded = seen.clone();
    let requests = count.clone();
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                break;
            };
            let mut buffer = [0u8; 4096];
            let Ok(n) = socket.read(&mut buffer).await else {
                continue;
            };
            let request = String::from_utf8_lossy(&buffer[..n]).to_string();
            requests.fetch_add(1, Ordering::Relaxed);
            recorded.lock().unwrap().push(request.clone());
            let reply = if request.starts_with("HEAD") {
                "HTTP/1.1 200 OK\r\nContent-Length: 1024\r\nAccept-Ranges: bytes\r\n\
                 ETag: \"v1\"\r\nLast-Modified: Wed, 01 Jan 2025 00:00:00 GMT\r\n\
                 Connection: close\r\n\r\n"
                    .to_string()
            } else {
                format!(
                    "HTTP/1.1 206 Partial Content\r\nContent-Length: {}\r\n\
                     Content-Range: bytes 0-1023/1024\r\nConnection: close\r\n\r\n{}",
                    BODY.len(),
                    String::from_utf8_lossy(BODY)
                )
            };
            let _ = socket.write_all(reply.as_bytes()).await;
            let _ = socket.shutdown().await;
        }
    });

    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::new(EngineConfig {
        max_connections: 2,
        chunk_size: 1024,
        policy: UrlPolicy::permissive(),
        user_agent: None,
        referer: None,
    })
    .unwrap();
    let recorder = Arc::new(Recorder::default());
    let (_stop, stop_rx) = tokio::sync::watch::channel(false);
    let _ = engine
        .run_task(
            TaskId(2),
            url::Url::parse(&format!("http://{addr}/f.bin")).unwrap(),
            dir.path().join("f.bin"),
            None,
            2,
            recorder,
            stop_rx,
            None,
        )
        .await;

    let requests = seen.lock().unwrap().clone();
    let ranged: Vec<&String> = requests.iter().filter(|r| r.starts_with("GET")).collect();
    assert!(!ranged.is_empty(), "a range request was made");
    for request in ranged {
        let count = request.to_lowercase().matches("if-range:").count();
        assert!(
            count <= 1,
            "a chunk request carried {count} If-Range headers:\n{request}"
        );
    }
}
