//! The bridge against real websocket clients.
//!
//! These are the regression cases the review asked for under R1. Each one starts a
//! real server on a real loopback port and connects with a real client, because the
//! properties being tested (a rejected `Origin`, a rejected `Host`, a captured code)
//! live in the handshake and cannot be tested by calling a function.

use futures_util::{SinkExt, StreamExt};
use omad_core::{AddTask, TaskId, TaskStatus};
use omad_rpc::protocol::{ClientMessage, ErrorCode, ServerMessage, TaskView};
use omad_rpc::server::{Bridge, BridgeConfig, BridgeHost, hmac_hex};
use std::collections::HashMap;
use std::sync::Arc;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

struct FakeHost;

impl BridgeHost for FakeHost {
    fn list(&self, _origin: &str) -> Vec<TaskView> {
        vec![TaskView {
            id: TaskId(1),
            url: "https://example.com/a.bin".into(),
            filename: "a.bin".into(),
            status: TaskStatus::Running,
            received_bytes: 10,
            total_bytes: Some(100),
            speed: 0,
            error: None,
        }]
    }
    fn add(&self, origin: &str, task: AddTask) -> Result<TaskView, String> {
        // The host records who asked; the bridge passes the true origin, never a claim.
        assert!(
            origin.starts_with("chrome-extension://") || origin.starts_with("moz-extension://")
        );
        Ok(TaskView {
            id: TaskId(2),
            url: task.url,
            filename: "new.bin".into(),
            status: TaskStatus::Queued,
            received_bytes: 0,
            total_bytes: None,
            speed: 0,
            error: None,
        })
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

async fn start(paired: bool, secret: Option<&str>) -> omad_rpc::BridgeHandle {
    let origin = "chrome-extension://testextensionid".to_string();
    let mut secrets = HashMap::new();
    if paired {
        secrets.insert(origin.clone(), secret.unwrap_or("s3cret").to_string());
    }
    let config = BridgeConfig {
        bind: "127.0.0.1:0".parse().unwrap(),
        paired_origins: if paired { vec![origin] } else { vec![] },
        trust_first_origin: true,
        server_version: "test".into(),
    };
    Bridge::start(config, Arc::new(FakeHost), secrets)
        .await
        .unwrap()
}

async fn connect(
    addr: std::net::SocketAddr,
    origin: &str,
) -> Result<
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
    tokio_tungstenite::tungstenite::Error,
> {
    let mut request = format!("ws://{addr}/").into_client_request().unwrap();
    request
        .headers_mut()
        .insert("origin", origin.parse().unwrap());
    let (ws, _) = tokio_tungstenite::connect_async(request).await?;
    Ok(ws)
}

async fn recv<T: serde::de::DeserializeOwned>(
    ws: &mut tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
) -> T {
    loop {
        let frame = tokio::time::timeout(std::time::Duration::from_secs(5), ws.next())
            .await
            .expect("the server answered in time")
            .expect("a frame")
            .expect("a frame without error");
        if let Message::Text(text) = frame {
            return serde_json::from_str(&text).expect("a protocol message");
        }
    }
}

// ------------------------------------------------------------------ R1 cases

#[tokio::test]
async fn a_web_page_origin_is_refused_at_the_handshake() {
    let bridge = start(false, None).await;
    // This is what `new WebSocket("ws://127.0.0.1:PORT")` from a web page looks like.
    let result = connect(bridge.addr(), "https://evil.example").await;
    assert!(result.is_err(), "a page origin must not get a socket");
    bridge.shutdown();
}

#[tokio::test]
async fn the_null_origin_and_other_schemes_are_refused() {
    let bridge = start(false, None).await;
    for origin in [
        "null",
        "file://",
        "http://localhost:8080",
        "chrome://settings",
    ] {
        let result = connect(bridge.addr(), origin).await;
        assert!(result.is_err(), "{origin} must be refused");
    }
    bridge.shutdown();
}

#[tokio::test]
async fn dns_rebinding_hosts_are_refused() {
    let bridge = start(false, None).await;
    // The connection really goes to 127.0.0.1:PORT, but the Host header names the
    // attacker's domain, which is exactly what a rebinding page sends.
    let mut request = format!("ws://{}/", bridge.addr())
        .into_client_request()
        .unwrap();
    request
        .headers_mut()
        .insert("origin", "chrome-extension://ok".parse().unwrap());
    request.headers_mut().insert(
        "host",
        format!("attacker.example:{}", bridge.addr().port())
            .parse()
            .unwrap(),
    );
    let result = tokio_tungstenite::connect_async(request).await;
    assert!(result.is_err(), "a non-loopback Host must be refused");
    bridge.shutdown();
}

#[tokio::test]
async fn a_first_contact_extension_is_trusted_and_given_a_secret() {
    // What "the extension just connects" means: no code, one connection, a secret.
    let bridge = start(false, None).await;
    let mut ws = connect(bridge.addr(), "chrome-extension://testextensionid")
        .await
        .unwrap();

    let challenge: ServerMessage = recv(&mut ws).await;
    assert!(matches!(challenge, ServerMessage::Challenge { .. }));

    ws.send(Message::Text(
        serde_json::to_string(&ClientMessage::Hello {
            major: omad_rpc::PROTOCOL_MAJOR,
            minor: omad_rpc::PROTOCOL_MINOR,
            extension_version: "1.0.0".into(),
            // No secret yet, so no proof: the desktop answers with one.
            proof: String::new(),
        })
        .unwrap()
        .into(),
    ))
    .await
    .unwrap();

    let paired: ServerMessage = recv(&mut ws).await;
    let secret = match paired {
        ServerMessage::Paired { secret } => secret,
        other => panic!("expected a secret on first contact, got {other:?}"),
    };
    assert_eq!(secret.len(), 64);
    let welcome: ServerMessage = recv(&mut ws).await;
    assert!(
        matches!(welcome, ServerMessage::Welcome { .. }),
        "got {welcome:?}"
    );
    assert!(bridge.is_paired("chrome-extension://testextensionid").await);
    bridge.shutdown();
}

#[tokio::test]
async fn a_page_origin_cannot_use_first_contact() {
    // The handshake refuses a page before any of this, so the automatic trust can only
    // ever apply to an extension origin.
    let bridge = start(false, None).await;
    assert!(
        connect(bridge.addr(), "https://evil.example")
            .await
            .is_err()
    );
    assert_eq!(bridge.paired_count().await, 0, "nothing was trusted");
    bridge.shutdown();
}

#[tokio::test]
async fn a_client_cannot_pair_as_another_origin() {
    let bridge = start(false, None).await;
    let _code = bridge.begin_pairing().await;
    let mut ws = connect(bridge.addr(), "chrome-extension://mine")
        .await
        .unwrap();
    let challenge: ServerMessage = recv(&mut ws).await;
    let nonce = match challenge {
        ServerMessage::Challenge { nonce, .. } => nonce,
        other => panic!("got {other:?}"),
    };
    ws.send(Message::Text(
        serde_json::to_string(&ClientMessage::Pair {
            // Claims to be someone else's extension.
            origin: "chrome-extension://someoneelse".into(),
            code_proof: hmac_hex("000000", &nonce),
        })
        .unwrap()
        .into(),
    ))
    .await
    .unwrap();
    let reply: ServerMessage = recv(&mut ws).await;
    match reply {
        ServerMessage::Error { code, .. } => {
            assert_eq!(code.as_str(), ErrorCode::ForbiddenOrigin.as_str())
        }
        other => panic!("expected a forbidden origin, got {other:?}"),
    }
    assert_eq!(bridge.paired_count().await, 0);
    bridge.shutdown();
}

#[tokio::test]
async fn a_paired_extension_authenticates_with_an_hmac_proof() {
    let bridge = start(true, Some("the-shared-secret")).await;
    let mut ws = connect(bridge.addr(), "chrome-extension://testextensionid")
        .await
        .unwrap();
    let challenge: ServerMessage = recv(&mut ws).await;
    let nonce = match challenge {
        ServerMessage::Challenge {
            nonce,
            capabilities,
            ..
        } => {
            assert!(capabilities.contains(&"tasks.read".to_string()));
            nonce
        }
        other => panic!("got {other:?}"),
    };

    ws.send(Message::Text(
        serde_json::to_string(&ClientMessage::Hello {
            major: omad_rpc::PROTOCOL_MAJOR,
            minor: omad_rpc::PROTOCOL_MINOR,
            extension_version: "1.0.0".into(),
            proof: hmac_hex("the-shared-secret", &nonce),
        })
        .unwrap()
        .into(),
    ))
    .await
    .unwrap();

    let welcome: ServerMessage = recv(&mut ws).await;
    assert!(
        matches!(welcome, ServerMessage::Welcome { .. }),
        "got {welcome:?}"
    );
    let tasks: ServerMessage = recv(&mut ws).await;
    match tasks {
        ServerMessage::Tasks { tasks } => assert_eq!(tasks.len(), 1),
        other => panic!("expected tasks, got {other:?}"),
    }

    // A mutation is accepted and answered.
    ws.send(Message::Text(
        serde_json::to_string(&ClientMessage::AddTask(AddTask::new(
            "https://example.com/x.zip",
        )))
        .unwrap()
        .into(),
    ))
    .await
    .unwrap();
    let added: ServerMessage = recv(&mut ws).await;
    match added {
        ServerMessage::Task { task } => assert_eq!(task.url, "https://example.com/x.zip"),
        other => panic!("expected a task, got {other:?}"),
    }
    bridge.shutdown();
}

#[tokio::test]
async fn a_wrong_proof_is_refused() {
    let bridge = start(true, Some("the-shared-secret")).await;
    let mut ws = connect(bridge.addr(), "chrome-extension://testextensionid")
        .await
        .unwrap();
    let _challenge: ServerMessage = recv(&mut ws).await;
    ws.send(Message::Text(
        serde_json::to_string(&ClientMessage::Hello {
            major: omad_rpc::PROTOCOL_MAJOR,
            minor: omad_rpc::PROTOCOL_MINOR,
            extension_version: "1.0.0".into(),
            proof: hmac_hex("wrong-secret", "some-nonce"),
        })
        .unwrap()
        .into(),
    ))
    .await
    .unwrap();
    let reply: ServerMessage = recv(&mut ws).await;
    match reply {
        ServerMessage::Error { code, .. } => {
            assert_eq!(code.as_str(), ErrorCode::Unauthenticated.as_str())
        }
        other => panic!("expected an error, got {other:?}"),
    }
    bridge.shutdown();
}

#[tokio::test]
async fn commands_before_hello_are_refused() {
    let bridge = start(true, Some("the-shared-secret")).await;
    let mut ws = connect(bridge.addr(), "chrome-extension://testextensionid")
        .await
        .unwrap();
    let _challenge: ServerMessage = recv(&mut ws).await;
    ws.send(Message::Text(
        serde_json::to_string(&ClientMessage::ListTasks)
            .unwrap()
            .into(),
    ))
    .await
    .unwrap();
    let reply: ServerMessage = recv(&mut ws).await;
    match reply {
        ServerMessage::Error { code, .. } => {
            assert_eq!(code.as_str(), ErrorCode::Unauthenticated.as_str())
        }
        other => panic!("expected an error, got {other:?}"),
    }
    bridge.shutdown();
}

#[tokio::test]
async fn the_port_is_never_a_well_known_constant() {
    let a = start(false, None).await;
    let b = start(false, None).await;
    assert_ne!(a.addr().port(), 0);
    assert_ne!(a.addr().port(), 14370, "the old fixed port is gone");
    assert_ne!(
        a.addr().port(),
        b.addr().port(),
        "the operating system picks the port"
    );
    a.shutdown();
    b.shutdown();
}
