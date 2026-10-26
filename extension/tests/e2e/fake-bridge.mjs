/**
 * A stand-in for the desktop bridge, with no dependencies.
 *
 * It speaks the same protocol the Rust bridge does (`gf-rpc`): a challenge, an HMAC proof
 * over it, then `add_task` and friends. The test asserts on what it records, so a passing
 * run means the extension really opened a socket, proved itself and sent a task — not that
 * a stub was called.
 *
 * A WebSocket server in a hundred lines: handshake, then frames. Only text frames are
 * needed, and only one client at a time.
 */

import { createHash, createHmac } from "node:crypto";
import { createServer } from "node:http";

const GUID = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";

export class FakeBridge {
  constructor(secret = "test-secret") {
    this.secret = secret;
    this.received = [];
    this.connections = [];
    this.origin = null;
    this.host = null;
  }

  async start() {
    this.server = createServer((request, response) => {
      response.writeHead(426).end("upgrade required");
    });
    this.server.on("upgrade", (request, socket) => this.onUpgrade(request, socket));
    await new Promise((resolve) => this.server.listen(0, "127.0.0.1", resolve));
    this.port = this.server.address().port;
    return this;
  }

  get address() {
    return `ws://127.0.0.1:${this.port}`;
  }

  async stop() {
    for (const socket of this.connections) {
      socket.destroy();
    }
    await new Promise((resolve) => this.server.close(resolve));
  }

  onUpgrade(request, socket) {
    this.origin = request.headers.origin ?? null;
    this.host = request.headers.host ?? null;

    // The desktop refuses anything that is not an extension origin; so does this.
    if (!/^(chrome|moz)-extension:\/\//.test(this.origin ?? "")) {
      socket.end("HTTP/1.1 403 Forbidden\r\n\r\n");
      return;
    }

    const key = request.headers["sec-websocket-key"];
    const accept = createHash("sha1").update(key + GUID).digest("base64");
    socket.write(
      "HTTP/1.1 101 Switching Protocols\r\n" +
        "Upgrade: websocket\r\n" +
        "Connection: Upgrade\r\n" +
        `Sec-WebSocket-Accept: ${accept}\r\n\r\n`,
    );
    this.connections.push(socket);

    const nonce = `nonce-${this.connections.length}`;
    this.send(socket, {
      type: "challenge",
      nonce,
      expires_in_ms: 60000,
      capabilities: ["tasks.read", "tasks.add"],
    });

    let buffer = Buffer.alloc(0);
    socket.on("data", (chunk) => {
      buffer = Buffer.concat([buffer, chunk]);
      for (;;) {
        const frame = decodeFrame(buffer);
        if (!frame) {
          break;
        }
        buffer = buffer.subarray(frame.length);
        if (frame.opcode === 0x8) {
          socket.destroy();
          return;
        }
        if (frame.opcode !== 0x1) {
          continue;
        }
        const message = JSON.parse(frame.payload);
        this.onMessage(socket, message, nonce);
      }
    });
    socket.on("error", () => {});
  }

  onMessage(socket, message, nonce) {
    if (message.type === "hello") {
      const expected = createHmac("sha256", this.secret).update(nonce).digest("hex");
      if (message.proof !== expected) {
        this.send(socket, { type: "error", code: "unauthenticated", message: "bad proof" });
        socket.destroy();
        return;
      }
      this.authenticated = true;
      this.send(socket, { type: "welcome", major: 1, minor: 0, server_version: "fake" });
      this.send(socket, { type: "tasks", tasks: [] });
      return;
    }
    if (message.type === "list_tasks") {
      this.send(socket, { type: "tasks", tasks: [] });
      return;
    }
    this.received.push(message);
    if (message.type === "add_task") {
      this.send(socket, {
        type: "task",
        task: {
          id: this.received.length,
          url: message.url,
          filename: message.filename ?? "",
          status: "queued",
          received_bytes: 0,
          total_bytes: null,
          speed: 0,
          error: null,
        },
      });
    }
  }

  send(socket, message) {
    socket.write(encodeFrame(JSON.stringify(message)));
  }

  /** Waits for an `add_task` whose URL is `url`, or throws after `timeout` ms. */
  async waitForTask(url, timeout = 10000) {
    const deadline = Date.now() + timeout;
    for (;;) {
      const found = this.received.find((m) => m.type === "add_task" && m.url === url);
      if (found) {
        return found;
      }
      if (Date.now() > deadline) {
        throw new Error(
          `the bridge never received a task for ${url}; it saw ${JSON.stringify(this.received)}`,
        );
      }
      await new Promise((resolve) => setTimeout(resolve, 50));
    }
  }
}

function encodeFrame(text) {
  const body = Buffer.from(text, "utf8");
  const length = body.length;
  let header;
  if (length < 126) {
    header = Buffer.from([0x81, length]);
  } else if (length < 65536) {
    header = Buffer.alloc(4);
    header[0] = 0x81;
    header[1] = 126;
    header.writeUInt16BE(length, 2);
  } else {
    header = Buffer.alloc(10);
    header[0] = 0x81;
    header[1] = 127;
    header.writeBigUInt64BE(BigInt(length), 2);
  }
  return Buffer.concat([header, body]);
}

function decodeFrame(buffer) {
  if (buffer.length < 2) {
    return null;
  }
  const opcode = buffer[0] & 0x0f;
  const masked = (buffer[1] & 0x80) !== 0;
  let length = buffer[1] & 0x7f;
  let offset = 2;
  if (length === 126) {
    if (buffer.length < 4) {
      return null;
    }
    length = buffer.readUInt16BE(2);
    offset = 4;
  } else if (length === 127) {
    if (buffer.length < 10) {
      return null;
    }
    length = Number(buffer.readBigUInt64BE(2));
    offset = 10;
  }
  const maskLength = masked ? 4 : 0;
  if (buffer.length < offset + maskLength + length) {
    return null;
  }
  let payload = buffer.subarray(offset + maskLength, offset + maskLength + length);
  if (masked) {
    const mask = buffer.subarray(offset, offset + 4);
    payload = Buffer.from(payload.map((byte, index) => byte ^ mask[index % 4]));
  }
  return { opcode, payload: payload.toString("utf8"), length: offset + maskLength + length };
}
