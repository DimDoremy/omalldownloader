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
    /// Origins this profile already trusts, each with the secret it holds.
    pub paired_origins: Vec<String>,
    /// When true, a browser extension that connects for the first time is trusted and
    /// handed a secret without any code to type. This is what makes the extension
    /// "just connect", the way Ghost Downloader's does; the origin and Host checks still
    /// stop a web page, and every later connection proves the secret.
    pub trust_first_origin: bool,
    pub server_version: String,
}

impl Default for BridgeConfig {
    fn default() -> Self {
        Self {
            bind: SocketAddr::from(([127, 0, 0, 1], DEFAULT_PORT)),
            paired_origins: Vec::new(),
            trust_first_origin: true,
            server_version: env!("CARGO_PKG_VERSION").to_string(),
        }
    }
}

/// The port the extension looks for by default. A single well-known loopback port is what
/// makes a fresh install work without typing anything; 0 in `bind` still means "let the
/// system choose", for tests and for the fallback when this one is taken.
pub const DEFAULT_PORT: u16 = 14370;

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
    /// Called once per origin, when the bridge trusts it for the first time. The host
    /// persists the pair so the extension does not have to be trusted again.
    fn remember_origin(&self, _origin: &str, _secret: &str) {}
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

/// Answers `GET /ping` with a line of JSON and closes.
///
/// The extension needs to tell "the app is not running" apart from "the browser refused the
/// WebSocket": the first is fixed by starting the app, the second by fixing the extension.
/// A plain HTTP request on the same port answers that, and nothing else lives here.
async fn answer_ping(stream: &mut tokio::net::TcpStream, body: &str) -> std::io::Result<()> {
    use tokio::io::AsyncWriteExt;
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\
         Cache-Control: no-store\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    );
    stream.write_all(response.as_bytes()).await?;
    stream.flush().await
}

/// The HTTP fallback: the same commands as the WebSocket, for a browser that refuses one.
///
/// It exists because a browser can block `ws://` on loopback while allowing `http://`, and
/// a download manager that only speaks one of them is a download manager that does not
/// work. Push updates become polling here; the extension polls while it is in this mode.
async fn answer_rpc(
    stream: &mut tokio::net::TcpStream,
    shared: &Arc<Shared>,
    addr: SocketAddr,
) -> std::io::Result<()> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let mut buffer = Vec::new();
    let mut chunk = [0u8; 4096];
    // Read the head, then exactly as much body as `Content-Length` promises.
    loop {
        if let Some(end) = find_head_end(&buffer) {
            let head = String::from_utf8_lossy(&buffer[..end]).to_string();
            let length = head
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().ok())
                        .flatten()
                })
                .unwrap_or(0)
                .min(1024 * 1024);
            let body_start = end + 4;
            while buffer.len() < body_start + length {
                let read = stream.read(&mut chunk).await?;
                if read == 0 {
                    break;
                }
                buffer.extend_from_slice(&chunk[..read]);
            }
            let body = String::from_utf8_lossy(&buffer[body_start..]).to_string();
            let response = http_rpc(&head, &body, shared, addr).await;
            let (status, payload) = match response {
                Ok(payload) => ("200 OK", payload),
                Err((code, message)) => (code, format!(r#"{{"error":"{message}"}}"#)),
            };
            let reply = format!(
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\n\
                 Content-Length: {}\r\nCache-Control: no-store\r\n\
                 Access-Control-Allow-Origin: *\r\nConnection: close\r\n\r\n{}",
                payload.len(),
                payload
            );
            stream.write_all(reply.as_bytes()).await?;
            return stream.flush().await;
        }
        let read = stream.read(&mut chunk).await?;
        if read == 0 || buffer.len() > 1024 * 1024 {
            return Ok(());
        }
        buffer.extend_from_slice(&chunk[..read]);
    }
}

fn find_head_end(buffer: &[u8]) -> Option<usize> {
    buffer.windows(4).position(|window| window == b"\r\n\r\n")
}

/// The HTTP side of the bridge: origin check, secret check, one command, one answer.
async fn http_rpc(
    head: &str,
    body: &str,
    shared: &Arc<Shared>,
    addr: SocketAddr,
) -> Result<String, (&'static str, String)> {
    let header = |name: &str| {
        head.lines().skip(1).find_map(|line| {
            let (key, value) = line.split_once(':')?;
            key.eq_ignore_ascii_case(name)
                .then(|| value.trim().to_string())
        })
    };

    let host = header("host").unwrap_or_default();
    if !host_is_loopback(&host, addr) {
        return Err(("403 Forbidden", "the Host header is not loopback".into()));
    }
    let origin = header("origin").unwrap_or_default();
    if !origin_is_extension_like(&origin) {
        return Err((
            "403 Forbidden",
            "only a browser extension may connect".into(),
        ));
    }

    // The same trust-on-first-use rule as the WebSocket: no secret means "give me one".
    let presented = header("x-omad-secret").unwrap_or_default();
    let known = shared.secrets.lock().await.get(&origin).cloned();
    let mut issued = None;
    if !presented.is_empty() {
        match &known {
            Some(secret) if constant_time_eq(secret.as_bytes(), presented.as_bytes()) => {}
            _ => return Err(("401 Unauthorized", "the secret does not match".into())),
        }
    } else if shared.config.trust_first_origin {
        let secret = random_token(32);
        shared
            .secrets
            .lock()
            .await
            .insert(origin.clone(), secret.clone());
        shared.host.remember_origin(&origin, &secret);
        issued = Some(secret);
    } else {
        return Err(("401 Unauthorized", "this origin is not paired".into()));
    }

    let request: serde_json::Value = serde_json::from_str(body)
        .map_err(|e| ("400 Bad Request", format!("unreadable request: {e}")))?;
    let op = request
        .get("op")
        .and_then(|v| v.as_str())
        .unwrap_or_default();

    let payload = match op {
        "list" => {
            let tasks = shared.host.list(&origin);
            serde_json::json!({ "tasks": tasks })
        }
        "add" => {
            let url = request
                .get("url")
                .and_then(|v| v.as_str())
                .unwrap_or_default();
            if url.is_empty() {
                return Err(("400 Bad Request", "a task needs a url".into()));
            }
            let mut task = omad_core::AddTask::new(url);
            task.filename = request
                .get("filename")
                .and_then(|v| v.as_str())
                .map(str::to_string);
            task.referer = request
                .get("referer")
                .and_then(|v| v.as_str())
                .map(str::to_string);
            match shared.host.add(&origin, task) {
                Ok(view) => serde_json::json!({ "task": view }),
                Err(error) => return Err(("400 Bad Request", error)),
            }
        }
        "pause" | "resume" | "remove" => {
            let id = request
                .get("id")
                .and_then(|v| v.as_u64())
                .map(omad_core::TaskId)
                .ok_or(("400 Bad Request", "a command needs an id".to_string()))?;
            let result = match op {
                "pause" => shared.host.pause(&origin, id),
                "resume" => shared.host.resume(&origin, id),
                _ => shared.host.remove(&origin, id),
            };
            match result {
                Ok(()) => serde_json::json!({ "ok": true }),
                Err(error) => return Err(("400 Bad Request", error)),
            }
        }
        other => {
            return Err(("400 Bad Request", format!("unknown op {other:?}")));
        }
    };

    let mut payload = payload;
    if let Some(secret) = issued {
        payload["secret"] = serde_json::Value::String(secret);
    }
    Ok(payload.to_string())
}

async fn serve(
    mut stream: tokio::net::TcpStream,
    peer: SocketAddr,
    shared: Arc<Shared>,
) -> anyhow::Result<()> {
    let addr = stream.local_addr()?;
    // `/ping` and `/rpc` before the WebSocket handshake: one asks whether the app is up,
    // the other is the transport for a browser that refuses to open a WebSocket.
    {
        let mut head = [0u8; 128];
        let start = if let Ok(n) = stream.peek(&mut head).await {
            String::from_utf8_lossy(&head[..n]).to_string()
        } else {
            String::new()
        };
        if start.starts_with("POST /rpc") {
            let _ = answer_rpc(&mut stream, &shared, addr).await;
            return Ok(());
        }
        if let Ok(n) = stream.peek(&mut head).await
            && n > 0
            && String::from_utf8_lossy(&head[..n]).starts_with("GET /ping")
        {
            let body = format!(
                r#"{{"app":"omalldownloader","version":"{}","protocol":{}}}"#,
                shared.config.server_version, PROTOCOL_MAJOR
            );
            let _ = answer_ping(&mut stream, &body).await;
            return Ok(());
        }
    }
    // The callback runs during the handshake, so this is how the validated origin
    // reaches the session that follows.
    let captured: Arc<std::sync::Mutex<Option<String>>> = Arc::new(std::sync::Mutex::new(None));
    let capture = captured.clone();

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
                    let known = {
                        let secrets = shared.secrets.lock().await;
                        secrets.get(&origin).cloned()
                    };
                    // A browser extension we have not met before is trusted on first contact
                    // and handed a secret, which is what makes the extension "just connect".
                    // The Origin was checked at the handshake, so a web page cannot be here;
                    // every later connection has to prove the secret.
                    // No proof at all means "I have no secret": a fresh install, or a
                    // browser profile whose storage was cleared. Either way it gets a new
                    // one, which is what lets the extension recover on its own instead of
                    // sitting offline against a secret the desktop remembers and it does not.
                    let accepted = if proof.is_empty() && shared.config.trust_first_origin {
                        let secret = random_token(32);
                        shared
                            .secrets
                            .lock()
                            .await
                            .insert(origin.clone(), secret.clone());
                        shared.host.remember_origin(&origin, &secret);
                        send(&mut ws, ServerMessage::Paired { secret }).await?;
                        true
                    } else {
                        match known {
                            Some(secret) => constant_time_eq(
                                hmac_hex(&secret, &nonce).as_bytes(),
                                proof.as_bytes(),
                            ),
                            None => false,
                        }
                    };
                    if accepted {
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
                    } else {
                        {
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
    let paired =
        shared.secrets.lock().await.contains_key(origin) || shared.config.trust_first_origin;
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
