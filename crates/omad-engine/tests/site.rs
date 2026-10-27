//! Site extraction is delegated: a fake `yt-dlp` on PATH proves the wiring (a real one
//! would need the network and a page that still parses).

use std::os::unix::fs::PermissionsExt;
use std::sync::Arc;
use std::time::Duration;

use omad_core::TaskId;
use omad_core::ssrf::UrlPolicy;
use omad_engine::packs::{PackContext, PackError, run_site};
use omad_engine::{Outcome, Progress, StoreSink};

#[derive(Default)]
struct Recorder {
    last: std::sync::Mutex<u64>,
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
        *self.last.lock().unwrap() = progress.received;
    }
    fn on_commit(&self, _: TaskId, _: u64, _: Option<u64>, _: u64) {}
    fn on_finished(&self, _: TaskId, _: Outcome, _: u64, _: Option<u64>) {}
    fn on_failed(&self, _: TaskId, _: &str, _: bool) {}
}

// Both halves set `OMAD_YTDLP`, which is process-wide, so they run in one test rather than
// racing each other.
#[tokio::test]
async fn the_extractor_is_used_and_a_missing_one_says_what_to_install() {
    let dir = tempfile::tempdir().unwrap();
    // A stand-in for yt-dlp: it writes the output file the arguments ask for.
    let fake = dir.path().join("fake-yt-dlp");
    std::fs::write(
        &fake,
        "#!/bin/sh\n# last two args are <path> <url>\nwhile [ $# -gt 2 ]; do shift; done\nprintf 'extracted-bytes' > \"$1\"\n",
    )
    .unwrap();
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
    // SAFETY: this test sets the variable for itself only, and runs in its own process.
    unsafe { std::env::set_var("OMAD_YTDLP", &fake) };

    let target = dir.path().join("clip.mp4");
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
    let url = url::Url::parse("https://www.youtube.com/watch?v=abc").unwrap();

    let written = tokio::time::timeout(Duration::from_secs(20), run_site(&context, &url))
        .await
        .expect("the extractor finished")
        .expect("the extractor succeeded");

    assert_eq!(std::fs::read(&target).unwrap(), b"extracted-bytes");
    assert_eq!(written, 15);

    // Now the other half: a program that is not there.
    let dir = tempfile::tempdir().unwrap();
    // SAFETY: this test sets the variable for itself only.
    unsafe { std::env::set_var("OMAD_YTDLP", dir.path().join("does-not-exist")) };
    let client = reqwest::Client::new();
    let policy = UrlPolicy::permissive();
    let recorder = Arc::new(Recorder::default());
    let (_stop_tx, stop_rx) = tokio::sync::watch::channel(false);
    let target = dir.path().join("clip.mp4");
    let context = PackContext {
        id: TaskId(2),
        target: &target,
        sink: recorder.as_ref(),
        stop: stop_rx,
        client: &client,
        policy: &policy,
    };
    let url = url::Url::parse("https://youtu.be/abc").unwrap();
    match run_site(&context, &url).await {
        Err(PackError::Missing(message)) => {
            assert!(
                message.contains("yt-dlp"),
                "the message names the program: {message}"
            );
        }
        other => panic!("expected a Missing error, got {other:?}"),
    }
}
