//! The local bridge the browser extension talks to.
//!
//! This is the trust boundary the review called out (R1) and it is built to four rules:
//!
//! 1. **The port is random and chosen at start.** Nothing scans for `14370` any more,
//!    and the extension learns where to connect from the user, once.
//! 2. **The `Origin` header is checked before the upgrade is accepted.** Only
//!    `chrome-extension://` and `moz-extension://` origins, and only an origin the user
//!    has paired with, plus a `Host` that is exactly the loopback address we bound.
//!    A web page calling `new WebSocket("ws://127.0.0.1:...")` carries its own origin
//!    and is refused before it gets a socket.
//! 3. **Authentication is a fresh challenge, never a long-lived token sent in clear.**
//!    The server sends a nonce that expires; the client answers with
//!    `HMAC-SHA256(shared_secret, nonce)`. The secret never travels after pairing.
//! 4. **Pairing is out of band.** The desktop shows a six-digit code; the user types it
//!    into the extension; both sides derive the secret from the code and the nonces.
//!    A client that connects cannot obtain the secret by asking for it.
//!
//! The protocol itself is in [`protocol`], the handshake and server in [`server`].

pub mod protocol;
pub mod server;

pub use protocol::{ClientMessage, PROTOCOL_MAJOR, PROTOCOL_MINOR, ServerMessage};
pub use server::{Bridge, BridgeConfig, BridgeHandle, PairingRequest};
