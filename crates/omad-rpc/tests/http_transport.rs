//! The HTTP transport, as a browser actually makes it: a preflight `OPTIONS`, then a
//! "simple" `POST` with the secret in the body and no custom headers.
//!
//! The bug this exists for: only the preflight-less GET worked from a real browser, so the
//! popup could ping the app but never talk to it.

use std::sync::Arc;

use omad_core::{AddTask, TaskId};
use omad_rpc::protocol::TaskView;
use omad_rpc::server::{Bridge, BridgeConfig, BridgeHost};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

struct Host;

impl BridgeHost for Host {
    fn list(&self, _origin: &str) -> Vec<TaskView> {
        Vec::new()
    }
    fn add(&self, _origin: &str, task: AddTask) -> Result<TaskView, String> {
        Ok(TaskView {
            id: TaskId(7),
            url: task.url,
            filename: task.filename.unwrap_or_else(|| "download".into()),
            status: omad_core::TaskStatus::Queued,
            total_bytes: None,
            received_bytes: 0,
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

async fn request(addr: std::net::SocketAddr, text: &str) -> String {
    // A browser sends the port in `Host`; the bridge checks it against the socket.
    let text = text.replace(
        "Host: 127.0.0.1\r\n",
        &format!("Host: 127.0.0.1:{}\r\n", addr.port()),
    );
    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    stream.write_all(text.as_bytes()).await.unwrap();
    let mut reply = Vec::new();
    let _ = stream.read_to_end(&mut reply).await;
    String::from_utf8_lossy(&reply).to_string()
}

async fn bridge() -> omad_rpc::server::BridgeHandle {
    Bridge::start(
        BridgeConfig {
            bind: "127.0.0.1:0".parse().unwrap(),
            trust_first_origin: true,
            ..BridgeConfig::default()
        },
        Arc::new(Host),
        std::collections::HashMap::new(),
    )
    .await
    .unwrap()
}

#[tokio::test]
async fn a_preflight_is_answered_with_the_headers_a_browser_needs() {
    let bridge = bridge().await;
    let reply = request(
        bridge.addr(),
        "OPTIONS /rpc HTTP/1.1\r\nHost: 127.0.0.1\r\nOrigin: chrome-extension://abc\r\n\
         Access-Control-Request-Method: POST\r\n\
         Access-Control-Request-Headers: content-type\r\n\r\n",
    )
    .await;
    assert!(reply.starts_with("HTTP/1.1 204"), "{reply}");
    assert!(reply.to_lowercase().contains("access-control-allow-origin"));
    assert!(
        reply
            .to_lowercase()
            .contains("access-control-allow-methods")
    );
    bridge.shutdown();
}

#[tokio::test]
async fn a_simple_post_with_the_secret_in_the_body_is_accepted() {
    let bridge = bridge().await;
    let addr = bridge.addr();

    // First contact: no secret, so one is issued.
    let body = r#"{"op":"list"}"#;
    let reply = request(
        addr,
        &format!(
            "POST /rpc HTTP/1.1\r\nHost: 127.0.0.1\r\nOrigin: chrome-extension://abc\r\n\
             Content-Type: text/plain;charset=UTF-8\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        ),
    )
    .await;
    assert!(reply.starts_with("HTTP/1.1 200"), "{reply}");
    let json: serde_json::Value =
        serde_json::from_str(reply.split("\r\n\r\n").nth(1).unwrap()).unwrap();
    let secret = json["secret"]
        .as_str()
        .expect("a secret was issued")
        .to_string();

    // A command with it in the body, still a simple request.
    let body = format!(r#"{{"op":"add","url":"https://example.com/a.bin","secret":"{secret}"}}"#);
    let reply = request(
        addr,
        &format!(
            "POST /rpc HTTP/1.1\r\nHost: 127.0.0.1\r\nOrigin: chrome-extension://abc\r\n\
             Content-Type: text/plain;charset=UTF-8\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        ),
    )
    .await;
    assert!(reply.starts_with("HTTP/1.1 200"), "{reply}");
    assert!(reply.contains("\"id\":7"), "{reply}");
    bridge.shutdown();
}

#[tokio::test]
async fn a_web_page_is_refused_over_http_too() {
    let bridge = bridge().await;
    let body = r#"{"op":"list"}"#;
    let reply = request(
        bridge.addr(),
        &format!(
            "POST /rpc HTTP/1.1\r\nHost: 127.0.0.1\r\nOrigin: https://evil.example\r\n\
             Content-Type: text/plain\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        ),
    )
    .await;
    assert!(reply.starts_with("HTTP/1.1 403"), "{reply}");
    bridge.shutdown();
}
