//! BitTorrent, end to end: one session seeds a torrent, another downloads it.
//!
//! No tracker and no DHT are involved: the downloader is given the seeder's address as an
//! initial peer, which is exactly the path a real client takes once it has found a peer.
//! This is the test that was missing — it moves bytes between two peers and checks that
//! what landed is what was seeded.

use std::sync::Arc;
use std::time::Duration;

use librqbit::{AddTorrent, CreateTorrentOptions, Session, SessionOptions};
use omad_core::TaskId;
use omad_core::ssrf::UrlPolicy;
use omad_engine::packs::{PackContext, PackError, run_bt_with_peers};
use omad_engine::{Outcome, Progress, StoreSink};

#[derive(Default)]
struct Recorder {
    last: std::sync::Mutex<(u64, Option<u64>)>,
    commits: std::sync::atomic::AtomicU64,
    finished: std::sync::Mutex<Option<Outcome>>,
}

impl StoreSink for Recorder {
    fn on_plan(
        &self,
        _: TaskId,
        _: &omad_engine::http::Probe,
        _: &omad_engine::engine::Plan,
        _: u8,
    ) {
    }
    fn on_progress(&self, _: TaskId, progress: &Progress) {
        *self.last.lock().unwrap() = (progress.received, progress.total);
    }
    fn on_commit(&self, _: TaskId, _: u64, _: Option<u64>, _: u64) {
        self.commits
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
    fn on_finished(&self, _: TaskId, outcome: Outcome, _: u64, _: Option<u64>) {
        *self.finished.lock().unwrap() = Some(outcome);
    }
    fn on_failed(&self, _: TaskId, _: &str, _: bool) {}
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_torrent_seeded_on_this_machine_is_downloaded_byte_for_byte() {
    let seed_dir = tempfile::tempdir().unwrap();
    let content: Vec<u8> = (0..256 * 1024u32).map(|i| (i % 251) as u8).collect();
    std::fs::write(seed_dir.path().join("blob.bin"), &content).unwrap();

    let spawner = librqbit::spawn_utils::BlockingSpawner::new(2);
    let created = librqbit::create_torrent(
        seed_dir.path(),
        CreateTorrentOptions {
            name: Some("blob.bin"),
            ..Default::default()
        },
        &spawner,
    )
    .await
    .unwrap();
    let torrent_bytes = created.as_bytes().unwrap();
    let info_hash = created.info_hash();

    // The seeder keeps the file it already has, and listens on a random loopback port.
    let seeder = Session::new_with_opts(
        seed_dir.path().to_path_buf(),
        SessionOptions {
            dht: None,
            disable_trackers: true,
            listen: Some(librqbit::ListenerOptions {
                listen_addr: "127.0.0.1:0".parse().unwrap(),
                ipv4_only: true,
                ..Default::default()
            }),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    // The seeder already has the file, so it must be allowed to write over it (which is
    // how it verifies the pieces it is about to serve).
    seeder
        .add_torrent(
            AddTorrent::from_bytes(torrent_bytes.clone()),
            Some(librqbit::AddTorrentOptions {
                overwrite: true,
                ..Default::default()
            }),
        )
        .await
        .unwrap();
    let address = seeder.listen_addr().expect("the seeder listens");

    // A magnet built from the same infohash, so the downloader resolves metadata from the
    // peer just as it would from a swarm.
    let magnet = url::Url::parse(&format!(
        "magnet:?xt=urn:btih:{}&dn=blob.bin",
        info_hash.as_string()
    ))
    .unwrap();

    let download_dir = tempfile::tempdir().unwrap();
    let target = download_dir.path().join("blob.bin");
    let client = reqwest::Client::new();
    let policy = UrlPolicy::permissive();
    let recorder = Arc::new(Recorder::default());
    let (_stop_tx, stop_rx) = tokio::sync::watch::channel(false);
    let context = PackContext {
        id: TaskId(1),
        target: &target,
        sink: recorder.as_ref(),
        stop: stop_rx,
        client: &client,
        policy: &policy,
    };

    let downloaded = tokio::time::timeout(
        Duration::from_secs(90),
        run_bt_with_peers(&context, &magnet, Some(vec![address])),
    )
    .await
    .expect("the transfer finished in time")
    .expect("the transfer succeeded");

    let written = std::fs::read(download_dir.path().join("blob.bin")).unwrap();
    assert_eq!(written.len(), content.len(), "the whole file arrived");
    assert_eq!(written, content, "byte for byte");
    assert_eq!(downloaded, content.len() as u64);
    assert!(recorder.commits.load(std::sync::atomic::Ordering::Relaxed) > 0);

    seeder.stop().await;
    let _ = PackError::Paused; // the error type is part of the signature under test
}
