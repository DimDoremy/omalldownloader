//! The whole path a browser download takes: pair, add, watch, finish.
//!
//! This test starts the real bridge, a real desktop-side host (the store and the engine,
//! minus the window), and a real websocket client that behaves like the extension:
//! it reads the challenge from `Chrome`-style origin, proves the code, then authenticates
//! with HMAC on every later connection and adds a task.

use futures_util::{SinkExt, StreamExt};
use omad_core::{AddTask, TaskId, TaskStatus, ssrf::UrlPolicy};
use omad_engine::engine::Plan;
use omad_engine::{Engine, EngineConfig, Outcome, Progress, StoreSink};
use omad_rpc::protocol::{ClientMessage, ErrorCode, ServerMessage, TaskView};
use omad_rpc::server::{Bridge, BridgeConfig, BridgeHost, hmac_hex};
use omad_store::{NewTask, Store};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};
use tokio_tungstenite::tungstenite::Message;

const ORIGIN: &str = "chrome-extension://abcdefghijklmnop";

// ------------------------------------------------------------ a file to fetch

fn serve_file(body: Vec<u8>) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { return };
            let body = body.clone();
            // One thread per connection: the engine keeps several requests open at once.
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
                let (from, to, status) = match range {
                    Some((from, Some(to))) => (from, to.min(body.len() - 1), 206),
                    Some((from, None)) => (from, body.len() - 1, 206),
                    None => (0, body.len() - 1, 200),
                };
                let slice = &body[from..=to];
                let head = format!(
                    "HTTP/1.1 {status} {}\r\nAccept-Ranges: bytes\r\nETag: \"v1\"\r\nContent-Disposition: attachment; filename=\"from-extension.bin\"\r\nContent-Range: bytes {from}-{to}/{}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    if status == 206 {
                        "Partial Content"
                    } else {
                        "OK"
                    },
                    body.len(),
                    slice.len()
                );
                let _ = stream.write_all(head.as_bytes());
                if !head_only {
                    let _ = stream.write_all(slice);
                }
            });
        }
    });
    port
}

// ------------------------------------------------------------ the desktop

/// The application's host, with no window: it does exactly what `omad-app` does with a
/// task the extension sent.
struct Desktop {
    store: Store,
    root: omad_core::path::DownloadRoot,
    board: Arc<Mutex<Vec<TaskView>>>,
    runtime: tokio::runtime::Handle,
}

impl BridgeHost for Desktop {
    fn list(&self, _origin: &str) -> Vec<TaskView> {
        self.board.lock().unwrap().clone()
    }

    fn add(&self, _origin: &str, task: AddTask) -> Result<TaskView, String> {
        let runtime = self.runtime.clone();
        let store = self.store.clone();
        let root = self.root.clone();
        let board = self.board.clone();
        // The reply is built from copies: the thread owns the task itself.
        let reply = TaskView {
            id: TaskId(0),
            url: task.url.clone(),
            filename: task.filename.clone().unwrap_or_default(),
            status: TaskStatus::Queued,
            received_bytes: 0,
            total_bytes: None,
            speed: 0,
            error: None,
        };
        std::thread::spawn(move || {
            runtime.block_on(async move {
                let url = url::Url::parse(&task.url).expect("the bridge passed a URL");
                let name = task
                    .filename
                    .clone()
                    .map(|n| omad_core::filename::sanitize(&n))
                    .unwrap_or_else(|| omad_core::filename::from_url(&url));
                let path = root.resolve(None, &name).expect("inside the root");
                let record = store
                    .create(NewTask {
                        url: task.url.clone(),
                        filename: name.clone(),
                        dest: "downloads".into(),
                        file_path: path.to_string_lossy().to_string(),
                        connections: 4,
                        allow_private: true,
                        referer: None,
                    })
                    .expect("a record");
                board.lock().unwrap().push(TaskView {
                    id: record.id,
                    url: task.url.clone(),
                    filename: name,
                    status: TaskStatus::Queued,
                    received_bytes: 0,
                    total_bytes: None,
                    speed: 0,
                    error: None,
                });

                /// The store, the board the test reads, and the id this task has.
                struct Sink(Store, Arc<Mutex<Vec<TaskView>>>, #[allow(dead_code)] TaskId);
                impl StoreSink for Sink {
                    fn on_plan(
                        &self,
                        id: TaskId,
                        probe: &omad_engine::http::Probe,
                        plan: &Plan,
                        c: u8,
                    ) {
                        let _ = self.0.set_plan(
                            id,
                            probe.total_bytes,
                            probe.etag.as_deref(),
                            probe.sha256.as_deref(),
                            &plan.to_json(),
                            c,
                        );
                        let _ = self.0.set_status(id, TaskStatus::Running, None);
                    }
                    fn on_progress(&self, _: TaskId, _: &Progress) {}
                    fn on_commit(&self, id: TaskId, received: u64, total: Option<u64>, speed: u64) {
                        let _ = self.0.set_progress(id, received, total, speed);
                    }
                    fn on_finished(
                        &self,
                        id: TaskId,
                        outcome: Outcome,
                        recorded: u64,
                        total: Option<u64>,
                    ) {
                        let _ = self.0.set_progress(id, recorded, total, 0);
                        let status = match outcome {
                            Outcome::Completed => TaskStatus::Completed,
                            Outcome::Paused => TaskStatus::Paused,
                        };
                        let _ = self.0.set_status(id, status, None);
                        for task in self.1.lock().unwrap().iter_mut() {
                            if task.id == id {
                                task.status = status;
                            }
                        }
                    }
                    fn on_failed(&self, id: TaskId, error: &str, _: bool) {
                        let _ = self.0.set_status(id, TaskStatus::Failed, Some(error));
                    }
                }

                let engine = Engine::new(EngineConfig {
                    max_connections: 4,
                    chunk_size: 64 * 1024,
                    policy: UrlPolicy::permissive(),
                    user_agent: None,
                    referer: None,
                })
                .expect("an engine");
                let (_tx, rx) = tokio::sync::watch::channel(false);
                let sink = Arc::new(Sink(store.clone(), board.clone(), record.id));
                if let Err(error) = engine
                    .run_task(record.id, url, path, None, 4, sink, rx, None)
                    .await
                {
                    eprintln!("the download failed: {error}");
                    for task in board.lock().unwrap().iter_mut() {
                        if task.id == record.id {
                            task.error = Some(error.to_string());
                        }
                    }
                }
            });
        });
        Ok(reply)
    }

    fn pause(&self, _: &str, _: TaskId) -> Result<(), String> {
        Ok(())
    }
    fn resume(&self, _: &str, _: TaskId) -> Result<(), String> {
        Ok(())
    }
    fn remove(&self, _: &str, _: TaskId) -> Result<(), String> {
        Ok(())
    }
}

fn desktop_len(dir: &std::path::Path) -> String {
    let mut out = String::new();
    if let Ok(entries) = std::fs::read_dir(dir.join("downloads")) {
        for entry in entries.flatten() {
            let meta = entry.metadata().ok();
            out.push_str(&format!(
                "{:?} {} bytes; ",
                entry.file_name(),
                meta.map(|m| m.len()).unwrap_or(0)
            ));
        }
    }
    out
}

type Client =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

async fn connect(addr: std::net::SocketAddr) -> Client {
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    let mut request = format!("ws://{addr}/").into_client_request().unwrap();
    request
        .headers_mut()
        .insert("origin", ORIGIN.parse().unwrap());
    let (ws, _) = tokio_tungstenite::connect_async(request).await.unwrap();
    ws
}

async fn recv(ws: &mut Client) -> ServerMessage {
    loop {
        let frame = tokio::time::timeout(std::time::Duration::from_secs(10), ws.next())
            .await
            .expect("the desktop answered")
            .expect("a frame")
            .expect("no error");
        if let Message::Text(text) = frame {
            return serde_json::from_str(&text).unwrap();
        }
    }
}

async fn send(ws: &mut Client, message: ClientMessage) {
    ws.send(Message::Text(
        serde_json::to_string(&message).unwrap().into(),
    ))
    .await
    .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_download_sent_by_the_extension_lands_on_disk() {
    let body: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
    let port = serve_file(body.clone());
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("state/omalldownloader.sqlite3")).unwrap();
    std::fs::create_dir_all(dir.path().join("downloads")).unwrap();
    let root = omad_core::path::DownloadRoot::new(dir.path().join("downloads")).unwrap();

    let desktop = Arc::new(Desktop {
        store,
        root,
        board: Arc::new(Mutex::new(Vec::new())),
        runtime: tokio::runtime::Handle::current(),
    });
    let bridge = Bridge::start(
        BridgeConfig {
            bind: "127.0.0.1:0".parse().unwrap(),
            paired_origins: vec![],
            // This test pairs by hand, so the automatic trust is off here.
            trust_first_origin: false,
            server_version: "test".into(),
        },
        desktop.clone(),
        std::collections::HashMap::new(),
    )
    .await
    .unwrap();

    // 1. Pair: the desktop shows a code, the extension proves it against the challenge.
    let code = bridge.begin_pairing().await;
    let mut ws = connect(bridge.addr()).await;
    let nonce = match recv(&mut ws).await {
        ServerMessage::Challenge { nonce, .. } => nonce,
        other => panic!("{other:?}"),
    };
    send(
        &mut ws,
        ClientMessage::Pair {
            origin: ORIGIN.into(),
            code_proof: hmac_hex(&code, &nonce),
        },
    )
    .await;
    let secret = match recv(&mut ws).await {
        ServerMessage::Paired { secret } => secret,
        other => panic!("{other:?}"),
    };
    ws.close(None).await.ok();

    // 2. Coming back: a fresh challenge, an HMAC over it, then the task.
    let mut ws = connect(bridge.addr()).await;
    let nonce = match recv(&mut ws).await {
        ServerMessage::Challenge { nonce, .. } => nonce,
        other => panic!("{other:?}"),
    };
    send(
        &mut ws,
        ClientMessage::Hello {
            major: omad_rpc::PROTOCOL_MAJOR,
            minor: omad_rpc::PROTOCOL_MINOR,
            extension_version: "0.1.0".into(),
            proof: hmac_hex(&secret, &nonce),
        },
    )
    .await;
    assert!(matches!(recv(&mut ws).await, ServerMessage::Welcome { .. }));

    let mut task = AddTask::new(format!("http://127.0.0.1:{port}/x"));
    task.filename = Some("from-extension.bin".into());
    send(&mut ws, ClientMessage::AddTask(task)).await;
    // Two frames can arrive: the push for the mutation and the list the worker also
    // sends. Accept either, but a refusal is the test failing.
    let reply = recv(&mut ws).await;
    assert!(
        matches!(
            reply,
            ServerMessage::Task { .. } | ServerMessage::Tasks { .. }
        ),
        "adding a task was refused: {reply:?}"
    );

    // 3. The file arrives, with the bytes the server sent.
    let target = dir.path().join("downloads").join("from-extension.bin");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    while std::time::Instant::now() < deadline {
        if let Ok(bytes) = std::fs::read(&target)
            && bytes == body
        {
            bridge.shutdown();
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    let state = desktop_len(dir.path());
    panic!(
        "the download never produced the expected bytes at {} ({state})",
        target.display()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_page_that_guesses_the_code_still_cannot_get_in() {
    // The model of the attack R1 describes: someone else finds the port and tries the
    // pairing endpoint with a guessed code, then the hello path with no secret.
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("state/omalldownloader.sqlite3")).unwrap();
    std::fs::create_dir_all(dir.path().join("downloads")).unwrap();
    let root = omad_core::path::DownloadRoot::new(dir.path().join("downloads")).unwrap();
    let desktop = Arc::new(Desktop {
        store,
        root,
        board: Arc::new(Mutex::new(Vec::new())),
        runtime: tokio::runtime::Handle::current(),
    });
    // Port 0: the default port belongs to the running app, and a test must not need it.
    let bridge = Bridge::start(
        BridgeConfig {
            bind: "127.0.0.1:0".parse().unwrap(),
            // The manual pair path is what this test exercises, so automatic trust is off.
            trust_first_origin: false,
            ..BridgeConfig::default()
        },
        desktop,
        std::collections::HashMap::new(),
    )
    .await
    .unwrap();
    let _code = bridge.begin_pairing().await;

    for guess in ["000000", "111111", "999999"] {
        let mut ws = connect(bridge.addr()).await;
        let nonce = match recv(&mut ws).await {
            ServerMessage::Challenge { nonce, .. } => nonce,
            other => panic!("{other:?}"),
        };
        send(
            &mut ws,
            ClientMessage::Pair {
                origin: ORIGIN.into(),
                code_proof: hmac_hex(guess, &nonce),
            },
        )
        .await;
        match recv(&mut ws).await {
            ServerMessage::Error { code, .. } => {
                assert_eq!(code.as_str(), ErrorCode::BadProof.as_str())
            }
            other => panic!("a wrong code must not pair, got {other:?}"),
        }
        ws.close(None).await.ok();
    }
    assert_eq!(bridge.paired_count().await, 0, "no guess produced a secret");
    bridge.shutdown();
}
