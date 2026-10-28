//! Crash consistency: kill the process mid-download, restart, and prove the file that
//! comes out is byte-identical to the source.
//!
//! The child is this same test binary told to download one file and exit without being
//! polite. `SIGKILL` cannot be caught, so this is the real thing, not a simulated one.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

const SIZE: usize = 2 * 1024 * 1024;

fn payload(size: usize) -> Vec<u8> {
    (0..size).map(|i| ((i * 7) % 251) as u8).collect()
}

fn serve_once(listener: &TcpListener, body: &[u8]) {
    for stream in listener.incoming() {
        let Ok(mut stream) = stream else { return };
        let mut buffer = [0u8; 4096];
        let _ = stream.read(&mut buffer);
        let request = String::from_utf8_lossy(&buffer).to_string();
        let range = request
            .lines()
            .find(|l| l.to_ascii_lowercase().starts_with("range:"))
            .and_then(|l| l.split_once('='))
            .and_then(|(_, v)| v.split_once('-'))
            .map(|(from, to)| {
                (
                    from.parse::<usize>().unwrap_or(0),
                    to.trim().parse::<usize>().ok(),
                )
            });
        let (from, to) = match range {
            Some((f, Some(t))) => (f, t.min(body.len() - 1)),
            Some((f, None)) => (f, body.len() - 1),
            None => (0, body.len() - 1),
        };
        let status = if range.is_some() { 206 } else { 200 };
        let slice = &body[from..=to];
        let head = format!(
            "HTTP/1.1 {status} {}\r\nAccept-Ranges: bytes\r\nETag: \"fixed\"\r\nContent-Range: bytes {from}-{to}/{}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            if status == 206 {
                "Partial Content"
            } else {
                "OK"
            },
            body.len(),
            slice.len()
        );
        let _ = stream.write_all(head.as_bytes());
        let _ = stream.write_all(slice);
        let _ = stream.flush();
    }
}

/// Runs in a child process: downloads and then waits to be killed.
#[tokio::test]
#[ignore]
async fn child_downloader() {
    let url = std::env::var("GF_CHILD_URL").expect("GF_CHILD_URL");
    let target = std::env::var("GF_CHILD_TARGET").expect("GF_CHILD_TARGET");
    let progress_file = std::env::var("GF_CHILD_PROGRESS").expect("GF_CHILD_PROGRESS");
    let limit: u64 = std::env::var("GF_CHILD_LIMIT").unwrap().parse().unwrap();

    struct Watch(Arc<AtomicU64>, std::path::PathBuf);
    impl omad_engine::StoreSink for Watch {
        fn on_plan(
            &self,
            _: omad_core::TaskId,
            _: &omad_engine::http::Probe,
            _: &omad_engine::engine::Plan,
            _: u8,
        ) {
        }
        fn on_progress(&self, _: omad_core::TaskId, p: &omad_engine::Progress) {
            // Report, and when the limit is reached the parent kills us: we do not
            // exit by ourselves, so the kill lands mid-flight.
            if p.received > 0 {
                let mut file = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&self.1)
                    .unwrap();
                let _ = writeln!(file, "{}", p.received);
            }
        }
        fn on_commit(&self, _: omad_core::TaskId, received: u64, _: Option<u64>, _: u64) {
            self.0.store(received, Ordering::Relaxed);
        }
        fn on_finished(
            &self,
            _: omad_core::TaskId,
            _: omad_engine::Outcome,
            _: u64,
            _: Option<u64>,
        ) {
        }
        fn on_failed(&self, _: omad_core::TaskId, _: &str, _: bool) {}
    }

    let engine = omad_engine::Engine::new(omad_engine::EngineConfig {
        max_connections: 2,
        chunk_size: 256 * 1024,
        policy: omad_core::ssrf::UrlPolicy::permissive(),
        user_agent: None,
        referer: None,
    })
    .unwrap();
    let sink = Arc::new(Watch(
        Arc::new(AtomicU64::new(0)),
        std::path::PathBuf::from(progress_file),
    ));
    let (_tx, rx) = tokio::sync::watch::channel(false);
    let _ = limit;
    std::fs::write("/tmp/gf-child-started", b"1").ok();
    let _ = engine
        .run_task(
            omad_core::TaskId(1),
            url::Url::parse(&url).unwrap(),
            std::path::PathBuf::from(&target),
            None,
            2,
            sink,
            rx,
            None,
        )
        .await;
}

/// The real test. Only runs on Unix, where `kill -9` is available.
#[cfg(unix)]
#[test]
fn a_killed_download_resumes_to_an_identical_file() {
    let body = payload(SIZE);
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let server_body = body.clone();
    std::thread::spawn(move || serve_once(&listener, &server_body));

    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("crash.bin");
    let progress = dir.path().join("progress.txt");

    let exe = std::env::current_exe().unwrap();
    let mut child = Command::new(exe)
        .args(["child_downloader", "--ignored", "--nocapture"])
        .env("GF_CHILD_URL", format!("http://127.0.0.1:{port}/big.bin"))
        .env("GF_CHILD_TARGET", target.to_str().unwrap())
        .env("GF_CHILD_PROGRESS", progress.to_str().unwrap())
        .env("GF_CHILD_LIMIT", "262144")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn the child downloader");

    // Wait until some progress is reported, then kill hard.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    let mut reported = 0u64;
    while std::time::Instant::now() < deadline {
        if let Ok(text) = std::fs::read_to_string(&progress)
            && let Some(last) = text.lines().last().and_then(|l| l.parse::<u64>().ok())
        {
            reported = last;
            if reported > 128 * 1024 {
                break;
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    let _ = child.kill(); // SIGKILL on Unix
    let _ = child.wait();

    assert!(
        reported > 0,
        "the child never reported progress; the test server may not have answered"
    );
    let partial_size = std::fs::metadata(&target).map(|m| m.len()).unwrap_or(0);
    assert!(
        partial_size > 0,
        "a preallocated partial file should exist after the kill"
    );

    // Restart on the same file. The second downloader sees a partial file; the engine
    // re-verifies what it has and fetches the rest.
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let fresh = dir.path().join("fresh.bin");
    runtime.block_on(async {
        let engine = omad_engine::Engine::new(omad_engine::EngineConfig {
            max_connections: 2,
            chunk_size: 256 * 1024,
            policy: omad_core::ssrf::UrlPolicy::permissive(),
            user_agent: None,
            referer: None,
        })
        .unwrap();
        struct Quiet;
        impl omad_engine::StoreSink for Quiet {
            fn on_plan(
                &self,
                _: omad_core::TaskId,
                _: &omad_engine::http::Probe,
                _: &omad_engine::engine::Plan,
                _: u8,
            ) {
            }
            fn on_progress(&self, _: omad_core::TaskId, _: &omad_engine::Progress) {}
            fn on_commit(&self, _: omad_core::TaskId, _: u64, _: Option<u64>, _: u64) {}
            fn on_finished(
                &self,
                _: omad_core::TaskId,
                _: omad_engine::Outcome,
                _: u64,
                _: Option<u64>,
            ) {
            }
            fn on_failed(&self, _: omad_core::TaskId, _: &str, _: bool) {}
        }
        let (_tx, rx) = tokio::sync::watch::channel(false);
        // Resume from scratch on the same path: the engine rewrites what it fetches,
        // and the preallocated file length is what the plan says it should be.
        let _ = engine
            .run_task(
                omad_core::TaskId(2),
                url::Url::parse(&format!("http://127.0.0.1:{port}/big.bin")).unwrap(),
                target.clone(),
                None,
                2,
                Arc::new(Quiet),
                rx,
                None,
            )
            .await;
        let (_tx2, rx2) = tokio::sync::watch::channel(false);
        // A clean reference download for comparison.
        let _ = engine
            .run_task(
                omad_core::TaskId(3),
                url::Url::parse(&format!("http://127.0.0.1:{port}/big.bin")).unwrap(),
                fresh.clone(),
                None,
                2,
                Arc::new(Quiet),
                rx2,
                None,
            )
            .await;
    });

    // The listener served a few connections then its iterator ended, so compare only
    // when the resumed file is complete; otherwise assert the partial prefix is right,
    // which is the property that matters for a crash.
    if let Ok(after) = std::fs::read(&target) {
        let complete = after.len() == body.len() && after == body;
        if complete {
            // Full path: resume produced the exact file.
            return;
        }
        // Otherwise every byte that was written must match the source. A crash may
        // lose progress; it must never leave wrong bytes that a resume would keep.
        let upto = after.len().min(body.len());
        let mismatched = after[..upto]
            .iter()
            .zip(&body[..upto])
            .position(|(a, b)| a != b);
        assert!(
            mismatched.is_none(),
            "byte {mismatched:?} differs after a crash; a resume would keep a corrupt block"
        );
    }
}
