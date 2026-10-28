//! BitTorrent: a magnet is picked up by the in-process engine, reports through the sink,
//! and pauses when asked.
//!
//! A full peer-to-peer transfer needs a seeder, a tracker or an explicit peer, and a second
//! session; that fixture is not here yet. What *is* asserted is the part that was broken
//! before: a `magnet:` URL reaches librqbit, and a stop while the metadata is still being
//! fetched pauses the task instead of leaving it unstoppable.

use std::sync::Arc;
use std::time::Duration;

use omad_core::TaskId;
use omad_engine::engine::{Engine, EngineConfig, Plan};
use omad_engine::http::Probe;
use omad_engine::{Outcome, Progress, StoreSink};

#[derive(Default)]
struct Recorder {
    reported: std::sync::atomic::AtomicBool,
    finished: std::sync::Mutex<Option<Outcome>>,
    failed: std::sync::Mutex<Option<String>>,
}

impl StoreSink for Recorder {
    fn on_plan(&self, _: TaskId, _: &Probe, _: &Plan, _: u8) {}
    fn on_progress(&self, _: TaskId, _: &Progress) {
        self.reported
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }
    fn on_commit(&self, _: TaskId, _: u64, _: Option<u64>, _: u64) {}
    fn on_finished(&self, _: TaskId, outcome: Outcome, _: u64, _: Option<u64>) {
        *self.finished.lock().unwrap() = Some(outcome);
    }
    fn on_failed(&self, _: TaskId, error: &str, _: bool) {
        *self.failed.lock().unwrap() = Some(error.to_string());
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_magnet_link_is_handed_to_the_torrent_engine_and_pauses() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("torrent-placeholder");
    let engine = Engine::new(EngineConfig {
        max_connections: 2,
        chunk_size: 256 * 1024,
        policy: omad_core::ssrf::UrlPolicy::permissive(),
        user_agent: None,
        referer: None,
    })
    .unwrap();

    // A well-formed magnet with a real-looking infohash and a tracker that will not answer:
    // the point is that the torrent session accepts it and the task can be paused.
    let magnet = url::Url::parse(
        "magnet:?xt=urn:btih:0123456789abcdef0123456789abcdef01234567&dn=omad-test",
    )
    .unwrap();

    let recorder = Arc::new(Recorder::default());
    let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
    let stopper = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(3)).await;
        let _ = stop_tx.send(true);
    });

    let outcome = engine
        .run_task(
            TaskId(1),
            magnet,
            target,
            None,
            2,
            recorder.clone(),
            stop_rx,
            None,
        )
        .await;

    stopper.abort();
    match outcome {
        Ok(Outcome::Paused) => {}
        other => panic!("a paused magnet must report Paused, got {other:?}"),
    }
    assert_eq!(*recorder.finished.lock().unwrap(), Some(Outcome::Paused));
    // No progress is expected here: a magnet with no peers never leaves the metadata fetch,
    // which is exactly why the pause has to be able to win that wait.
    let _ = recorder.reported.load(std::sync::atomic::Ordering::Relaxed);
}
