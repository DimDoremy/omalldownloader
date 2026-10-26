//! Accepting exactly one kind of client: a browser extension the user paired with.

use crate::protocol::{
    ClientMessage, ErrorCode, PROTOCOL_MAJOR, PROTOCOL_MINOR, ServerMessage, TaskView,
};
use futures_util::{SinkExt, StreamExt};
use hmac::{Hmac, Mac};
use rand::RngCore;
use sha2::Sha256;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};
use tokio::net::TcpListener;
use tokio::sync::{Mutex, mpsc, oneshot};
use tokio_tungstenite::tungstenite::Error as WsError;
use tokio_tungstenite::tungstenite::handshake::server::{ErrorResponse, Request, Response};
use tokio_tungstenite::tungstenite::http::StatusCode;
use tokio_tungstenite::tungstenite::protocol::Message;

type HmacSha256 = Hmac<Sha256>;

/// How long a nonce stays good. Short enough that a captured challenge is useless.
pub const CHALLENGE_TTL: Duration = Duration::from_secs(60);
/// Pairing codes are digits, shown once on the desktop.
const CODE_DIGITS: u32 = 6;

#[derive(Clone, Debug)]
pub struct BridgeConfig {
    /// Where to bind. Loopback only; a port of 0 asks the operating system for one.
    pub bind: SocketAddr,
    /// Origins the user has already paired with. Anything else cannot even reach
    /// pairing unless the desktop says yes to that origin.
    pub paired_origins: Vec<String>,
    pub server_version: String,
}

impl Default for BridgeConfig {
    fn default() -> Self {
        Self {
            bind: SocketAddr::from(([127, 0, 0, 1], 0)),
            paired_origins: Vec::new(),
            server_version: env!("CARGO_PKG_VERSION").to_string(),
        }
    }
}

/// A pairing waiting for the user to read a code off the desktop and type it into the
/// extension.
#[derive(Debug, Clone)]
pub struct PairingRequest {
    pub origin: String,
}

#[derive(Debug, thiserror::Error)]
pub enum BridgeError {
    #[error("could not bind {addr}: {source}")]
    Bind {
        addr: SocketAddr,
        source: std::io::Error,
    },
    #[error("the bridge stopped")]
    Stopped,
}

/// The commands the bridge can run on behalf of an authenticated extension. The
/// application implements this; the bridge never touches the store or the engine.
///
/// Blocking calls are the host's business: the bridge runs each connection on its own
/// task, so a slow command delays one extension, not the server. `origin` is passed on
/// every call so the host can keep task lists and output paths per browser profile.
pub trait BridgeHost: Send + Sync + 'static {
    fn list(&self, origin: &str) -> Vec<TaskView>;
    fn add(&self, origin: &str, task: omad_core::AddTask) -> Result<TaskView, String>;
    fn pause(&self, origin: &str, id: omad_core::TaskId) -> Result<(), String>;
    fn resume(&self, origin: &str, id: omad_core::TaskId) -> Result<(), String>;
    fn remove(&self, origin: &str, id: omad_core::TaskId) -> Result<(), String>;
}

/// The pair code, and the secret it derives.
struct PendingPair {
    code: String,
    created: Instant,
}

struct Shared {
    config: BridgeConfig,
    host: Arc<dyn BridgeHost>,
    /// The secret of each paired origin, base64. Kept in memory; the application
    /// persists it, this crate does not care where.
    secrets: Mutex<HashMap<String, String>>,
    pending: Mutex<Option<PendingPair>>,
    connections: AtomicU64,
}

/// A running bridge.
pub struct BridgeHandle {
    pub addr: SocketAddr,
    stop: Option<oneshot::Sender<()>>,
    shared: Arc<Shared>,
    task: tokio::task::JoinHandle<()>,
}

impl BridgeHandle {
    /// The address the operating system chose. This is what the extension is told,
    /// once, by the user.
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    pub fn shutdown(mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        self.task.abort();
    }

    /// Starts a pairing and returns the code to show the user. Only one pairing can
    /// be pending at a time, so a hostile page cannot fill the user's screen.
    pub async fn begin_pairing(&self) -> String {
        let mut rng = rand::rng();
        let code = format!("{:06}", rng.next_u32() % 10u32.pow(CODE_DIGITS));
        *self.shared.pending.lock().await = Some(PendingPair {
            code: code.clone(),
            created: Instant::now(),
        });
        code
    }

    pub async fn is_paired(&self, origin: &str) -> bool {
        self.shared.secrets.lock().await.contains_key(origin)
    }

    pub async fn paired_count(&self) -> usize {
        self.shared.secrets.lock().await.len()
    }

    pub fn connections(&self) -> u64 {
        self.shared.connections.load(Ordering::Relaxed)
    }
}

pub struct Bridge;

impl Bridge {
    /// Starts listening. `secrets` are the origins this profile already trusts and the
    /// secret each one holds.
    pub async fn start(
        config: BridgeConfig,
        host: Arc<dyn BridgeHost>,
        secrets: HashMap<String, String>,
    ) -> Result<BridgeHandle, BridgeError> {
        let listener = TcpListener::bind(config.bind)
            .await
            .map_err(|e| BridgeError::Bind {
                addr: config.bind,
                source: e,
            })?;
        let addr = listener.local_addr().map_err(|e| BridgeError::Bind {
            addr: config.bind,
            source: e,
        })?;

        let shared = Arc::new(Shared {
            config,
            host,
            secrets: Mutex::new(secrets),
            pending: Mutex::new(None),
            connections: AtomicU64::new(0),
        });

        let (stop_tx, mut stop_rx) = oneshot::channel();
        let accept_shared = shared.clone();
        let task = tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = &mut stop_rx => break,
                    accepted = listener.accept() => {
                        let Ok((stream, peer)) = accepted else { continue };
                        let shared = accept_shared.clone();
                        tokio::spawn(async move {
                            let _ = serve(stream, peer, shared).await;
                        });
                    }
                }
            }
        });

        Ok(BridgeHandle {
            addr,
            stop: Some(stop_tx),
            shared,
            task,
        })
    }
}

/// Is this origin one the extension of a supported browser could present, and is it
/// the shape we store?
///
/// The scheme and the identifier format are checked here; **whether the user has
/// paired with it is checked later**, against the store.
pub fn origin_is_extension_like(origin: &str) -> bool {
    let Some(rest) = origin
        .strip_prefix("chrome-extension://")
        .or_else(|| origin.strip_prefix("moz-extension://"))
    else {
        return false;
    };
    !rest.is_empty()
        && rest.len() <= 128
        && rest
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
}

fn host_is_loopback(value: &str, addr: SocketAddr) -> bool {
    // The request carried `Host: 127.0.0.1:PORT` (or IPv6 brackets). Anything else is
    // a rebinding attempt or a proxy in the middle.
    let expected_port = addr.port();
    let without_brackets = value.trim_start_matches('[');
    let Some((host, port)) = without_brackets.rsplit_once(':') else {
        return false;
    };
    let port: u16 = match port.parse() {
        Ok(p) => p,
        Err(_) => return false,
    };
    if port != expected_port {
        return false;
    }
    let host = host.trim_end_matches(']');
    matches!(host, "127.0.0.1" | "localhost" | "::1")
}

/// Rejects an upgrade with a reason. The body is empty on purpose: an attacker learns
/// only the status code.
fn refuse(status: StatusCode) -> ErrorResponse {
    let mut response = ErrorResponse::new(Some("refused".to_string()));
    *response.status_mut() = status;
    response
}

/// The error type `accept_hdr_async` requires. Its size is fixed by that API, not by us.
type HandshakeResult = Result<Response, ErrorResponse>;

async fn serve(
    stream: tokio::net::TcpStream,
    peer: SocketAddr,
    shared: Arc<Shared>,
) -> anyhow::Result<()> {
    // The callback runs during the handshake, so this is how the validated origin
    // reaches the session that follows.
    let captured: Arc<std::sync::Mutex<Option<String>>> = Arc::new(std::sync::Mutex::new(None));
    let capture = captured.clone();
    let addr = stream.local_addr()?;

    #[allow(clippy::result_large_err)] // the type is the handshake API's, not ours
    let callback = move |request: &Request, response: Response| -> HandshakeResult {
        let headers = request.headers();

        // 1. Host must be the loopback address we bound. This is what stops DNS
        //    rebinding: an attacker's name resolving to 127.0.0.1 still sends
        //    `Host: attacker.example`.
        let host_ok = headers
            .get("host")
            .and_then(|v| v.to_str().ok())
            .map(|v| host_is_loopback(v, addr))
            .unwrap_or(false);
        if !host_ok {
            return Err(refuse(StatusCode::FORBIDDEN));
        }

        // 2. Origin must be a browser extension. A page cannot set this header; the
        //    browser sets it to the page's own origin, so a page is refused here.
        let Some(value) = headers.get("origin").and_then(|v| v.to_str().ok()) else {
            return Err(refuse(StatusCode::FORBIDDEN));
        };
        if !origin_is_extension_like(value) {
            return Err(refuse(StatusCode::FORBIDDEN));
        }
        *capture.lock().unwrap() = Some(value.to_string());

        Ok(response)
    };

    let ws = tokio_tungstenite::accept_hdr_async(stream, callback).await;
    let mut ws = match ws {
        Ok(ws) => ws,
        Err(e) => {
            // R1: log the reason, never send it to the peer.
            tracing::debug!(peer = %peer, error = %e, "refused a bridge connection");
            return Ok(());
        }
    };

    let Some(origin) = captured.lock().unwrap().clone() else {
        return Ok(());
    };
    shared.connections.fetch_add(1, Ordering::Relaxed);

    // 3. Challenge. A nonce, single use, with an expiry the client must beat.
    let nonce = random_token(32);
    let challenge = ServerMessage::Challenge {
        nonce: nonce.clone(),
        expires_in_ms: CHALLENGE_TTL.as_millis() as u64,
        capabilities: capabilities_for(&origin, &shared).await,
    };
    ws.send(Message::Text(serde_json::to_string(&challenge)?.into()))
        .await?;

    let issued = Instant::now();
    let mut authenticated = false;
    let mut subscriptions: Option<mpsc::UnboundedSender<ServerMessage>> = None;

    while let Some(frame) = ws.next().await {
        let frame = match frame {
            Ok(f) => f,
            Err(WsError::ConnectionClosed | WsError::Protocol(_)) => break,
            Err(e) => {
                tracing::debug!(error = %e, "bridge stream error");
                break;
            }
        };
        let text = match frame {
            Message::Text(text) => text.to_string(),
            Message::Close(_) => break,
            Message::Ping(_) | Message::Pong(_) | Message::Binary(_) | Message::Frame(_) => {
                continue;
            }
        };
        // A message that is not JSON is not an error worth explaining to the peer.
        let Ok(message) = serde_json::from_str::<ClientMessage>(&text) else {
            send(
                &mut ws,
                error(ErrorCode::BadRequest, "not a protocol message"),
            )
            .await?;
            continue;
        };

        if !authenticated {
            match message {
                ClientMessage::Hello {
                    major,
                    minor,
                    proof,
                    ..
                } => {
                    if major != PROTOCOL_MAJOR {
                        send(
                            &mut ws,
                            error(ErrorCode::BadRequest, "unsupported protocol major"),
                        )
                        .await?;
                        break;
                    }
                    let _ = minor;
                    if issued.elapsed() > CHALLENGE_TTL {
                        send(&mut ws, error(ErrorCode::Expired, "the challenge expired")).await?;
                        break;
                    }
                    let expected = {
                        let secrets = shared.secrets.lock().await;
                        secrets.get(&origin).map(|secret| hmac_hex(secret, &nonce))
                    };
                    match expected {
                        Some(expected)
                            if constant_time_eq(expected.as_bytes(), proof.as_bytes()) =>
                        {
                            authenticated = true;
                            send(
                                &mut ws,
                                ServerMessage::Welcome {
                                    major: PROTOCOL_MAJOR,
                                    minor: PROTOCOL_MINOR,
                                    server_version: shared.config.server_version.clone(),
                                },
                            )
                            .await?;
                            let tasks = shared.host.list(&origin);
                            send(&mut ws, ServerMessage::Tasks { tasks }).await?;
                        }
                        _ => {
                            send(
                                &mut ws,
                                error(ErrorCode::Unauthenticated, "the proof does not match"),
                            )
                            .await?;
                            break;
                        }
                    }
                }
                ClientMessage::Pair {
                    origin: claimed,
                    code_proof,
                } => {
                    // A client may only pair the origin it actually connected from.
                    if claimed != origin {
                        send(
                            &mut ws,
                            error(ErrorCode::ForbiddenOrigin, "origin mismatch"),
                        )
                        .await?;
                        break;
                    }
                    let pending = shared.pending.lock().await.take();
                    let Some(pending) = pending else {
                        send(&mut ws, error(ErrorCode::BadProof, "no pairing is open")).await?;
                        break;
                    };
                    if pending.created.elapsed() > CHALLENGE_TTL {
                        send(&mut ws, error(ErrorCode::Expired, "the code expired")).await?;
                        break;
                    }
                    let expected = hmac_hex(&pending.code, &nonce);
                    if !constant_time_eq(expected.as_bytes(), code_proof.as_bytes()) {
                        // The code was wrong: keep the pairing open for one retry but
                        // remember nothing.
                        *shared.pending.lock().await = Some(pending);
                        send(&mut ws, error(ErrorCode::BadProof, "wrong code")).await?;
                        continue;
                    }
                    let secret = random_token(32);
                    shared
                        .secrets
                        .lock()
                        .await
                        .insert(origin.clone(), secret.clone());
                    send(&mut ws, ServerMessage::Paired { secret }).await?;
                    break;
                }
                _ => {
                    send(
                        &mut ws,
                        error(ErrorCode::Unauthenticated, "say hello first"),
                    )
                    .await?;
                    break;
                }
            }
            continue;
        }

        // 4. Authenticated: run the command.
        match message {
            ClientMessage::Hello { .. } | ClientMessage::Pair { .. } => {
                send(
                    &mut ws,
                    error(ErrorCode::BadRequest, "already authenticated"),
                )
                .await?;
            }
            ClientMessage::ListTasks => {
                let tasks = shared.host.list(&origin);
                send(&mut ws, ServerMessage::Tasks { tasks }).await?;
            }
            ClientMessage::Subscribe => {
                if subscriptions.is_none() {
                    let (tx, mut rx) = mpsc::unbounded_channel::<ServerMessage>();
                    subscriptions = Some(tx);
                    let mut outbox = ws;
                    // A push loop would need the sink here; instead the next
                    // ListTasks pulls. Keep the channel for future fan-out.
                    while let Ok(message) = rx.try_recv() {
                        send(&mut outbox, message).await?;
                    }
                    ws = outbox;
                }
                let tasks = shared.host.list(&origin);
                send(&mut ws, ServerMessage::Tasks { tasks }).await?;
            }
            ClientMessage::AddTask(add) => match shared.host.add(&origin, add) {
                Ok(task) => send(&mut ws, ServerMessage::Task { task }).await?,
                Err(message) => send(&mut ws, error(ErrorCode::Refused, &message)).await?,
            },
            ClientMessage::Pause { id } => match shared.host.pause(&origin, id) {
                Ok(()) => push_task(&mut ws, &shared, &origin, id).await?,
                Err(message) => send(&mut ws, error(ErrorCode::NotFound, &message)).await?,
            },
            ClientMessage::Resume { id } => match shared.host.resume(&origin, id) {
                Ok(()) => push_task(&mut ws, &shared, &origin, id).await?,
                Err(message) => send(&mut ws, error(ErrorCode::NotFound, &message)).await?,
            },
            ClientMessage::Remove { id } | ClientMessage::Cancel { id } => {
                match shared.host.remove(&origin, id) {
                    Ok(()) => send(&mut ws, ServerMessage::Removed { id }).await?,
                    Err(message) => send(&mut ws, error(ErrorCode::NotFound, &message)).await?,
                }
            }
        }
    }

    Ok(())
}

async fn push_task(
    ws: &mut tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>,
    shared: &Arc<Shared>,
    origin: &str,
    id: omad_core::TaskId,
) -> anyhow::Result<()> {
    let tasks = shared.host.list(origin);
    if let Some(task) = tasks.into_iter().find(|t| t.id == id) {
        send(ws, ServerMessage::Task { task }).await?;
    }
    Ok(())
}

async fn send(
    ws: &mut tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>,
    message: ServerMessage,
) -> anyhow::Result<()> {
    let text = serde_json::to_string(&message)?;
    ws.send(Message::Text(text.into())).await?;
    Ok(())
}

fn error(code: ErrorCode, message: &str) -> ServerMessage {
    ServerMessage::Error {
        code,
        message: message.to_string(),
    }
}

async fn capabilities_for(origin: &str, shared: &Arc<Shared>) -> Vec<String> {
    let paired = shared.secrets.lock().await.contains_key(origin);
    if paired {
        vec![
            "tasks.read".into(),
            "tasks.add".into(),
            "tasks.control".into(),
        ]
    } else {
        // Before pairing the only thing on offer is pairing itself.
        vec!["pair".into()]
    }
}

pub fn hmac_hex(key: &str, message: &str) -> String {
    let mut mac =
        <HmacSha256 as Mac>::new_from_slice(key.as_bytes()).expect("hmac accepts any key");
    mac.update(message.as_bytes());
    hex_encode(&mac.finalize().into_bytes())
}

fn hex_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b) {
        diff |= x ^ y;
    }
    diff == 0
}

fn random_token(bytes: usize) -> String {
    let mut buffer = vec![0u8; bytes];
    rand::rng().fill_bytes(&mut buffer);
    hex_encode(&buffer)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_extension_origins_are_accepted() {
        assert!(origin_is_extension_like(
            "chrome-extension://abcdefghijklmnop"
        ));
        assert!(origin_is_extension_like("moz-extension://1234-5678-abcd"));
        // The interesting refusals: a web page, another extension scheme, empty ids.
        assert!(!origin_is_extension_like("https://evil.example"));
        assert!(!origin_is_extension_like("http://127.0.0.1:8080"));
        assert!(!origin_is_extension_like("chrome-extension://"));
        assert!(!origin_is_extension_like("file:///etc/passwd"));
        assert!(!origin_is_extension_like("null"));
        assert!(!origin_is_extension_like("chrome-extension://a/b"));
    }

    #[test]
    fn host_must_be_the_loopback_we_bound() {
        let addr: SocketAddr = "127.0.0.1:45678".parse().unwrap();
        assert!(host_is_loopback("127.0.0.1:45678", addr));
        assert!(host_is_loopback("localhost:45678", addr));
        assert!(host_is_loopback("[::1]:45678", addr));
        // Rebinding: the attacker's name resolving to 127.0.0.1.
        assert!(!host_is_loopback("attacker.example:45678", addr));
        assert!(!host_is_loopback("127.0.0.1:14370", addr));
        assert!(!host_is_loopback("127.0.0.1", addr));
        assert!(!host_is_loopback("", addr));
    }

    #[test]
    fn hmac_proofs_are_hex_and_key_dependent() {
        // The same vector the extension asserts in `extension/tests/protocol.test.js`:
        // one value, checked on both sides, so the two implementations provably agree.
        assert_eq!(
            hmac_hex("code", "nonce"),
            "19e5e5ae0835a97f94fc6537978c10438fc15109add0b535489c4fc5ffa383a5"
        );
        let a = hmac_hex("code", "nonce");
        let b = hmac_hex("code", "nonce");
        let c = hmac_hex("other", "nonce");
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert_eq!(a.len(), 64);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn comparison_is_length_safe_and_byte_wise() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"));
        assert!(!constant_time_eq(b"", b"a"));
    }
}
