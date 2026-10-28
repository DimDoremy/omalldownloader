/**
 * The bridge to the desktop application.
 *
 * What the desktop requires, and what this file therefore does:
 *
 * * connect only to the address the user typed (`ws://127.0.0.1:<port>`), stored in
 *   `chrome.storage.local` after a successful pairing;
 * * prove identity with `HMAC(secret, nonce)` where the nonce is the one the desktop just
 *   sent — never a stored token;
 * * keep the secret and the address together: changing the address drops the secret, so a
 *   typo or a malicious write cannot send a proof to the wrong host;
 * * reconnect with backoff, and queue tasks while disconnected so a click is not lost.
 */

import { MSG, PROTOCOL_MAJOR, PROTOCOL_MINOR, hmacHex } from "./protocol.js";

const STORAGE_KEYS = Object.freeze({
  address: "bridgeAddress",
  secret: "bridgeSecret",
  pending: "pendingTasks",
});

/** Where the desktop listens unless the user changed it. Fixed, so a fresh install
 *  connects with nothing to type. */
export const DEFAULT_ADDRESS = "ws://127.0.0.1:14370";

/** Reconnect delays, in milliseconds. */
const BACKOFF = [500, 1000, 2000, 5000, 10000];

export class DesktopBridge extends EventTarget {
  constructor() {
    super();
    /** @type {WebSocket|null} */
    this.socket = null;
    this.ready = false;
    this.attempt = 0;
    /** @type {Map<string, {resolve: Function, reject: Function}>} */
    this.waiters = new Map();
    this.queue = [];
    this.disposed = false;
    /** Why the last connection attempt failed, for the popup to show. */
    this.lastError = "";
    /** True when someone chose the address on purpose, so it is not treated as stale. */
    this.explicit = false;
  }

  async load() {
    const stored = await chrome.storage.local.get(Object.values(STORAGE_KEYS));
    // An address chosen before loading (a test, or a caller that knows better) wins.
    this.address = this.explicit
      ? this.address
      : (stored[STORAGE_KEYS.address] || DEFAULT_ADDRESS);
    this.secret = stored[STORAGE_KEYS.secret] ?? "";
    this.queue = stored[STORAGE_KEYS.pending] ?? [];
    return this;
  }

  /** The address is fixed, so a new profile is "paired" the moment it can connect. */
  get configured() {
    return Boolean(this.address);
  }

  /**
   * Remembers where the desktop is. A change of address always forgets the secret: the
   * proof for A must never be offered to B.
   * @param {string} address @param {string} secret
   */
  async pair(address, secret) {
    const normalised = normaliseAddress(address);
    if (!normalised) { throw new Error("the address must be ws://127.0.0.1:<port>"); }
    // Changing where we talk to means dropping the old socket first, or `connect` would
    // return early on a socket that belongs to the previous address.
    this.close();
    await chrome.storage.local.set({
      [STORAGE_KEYS.address]: normalised,
      [STORAGE_KEYS.secret]: secret,
      [STORAGE_KEYS.pending]: this.queue,
    });
    this.address = normalised;
    this.secret = secret;
    this.explicit = true;
    return normalised;
  }

  /** @param {string} address */
  async unpair(address) {
    await chrome.storage.local.set({
      [STORAGE_KEYS.address]: "",
      [STORAGE_KEYS.secret]: "",
    });
    this.address = "";
    this.secret = "";
    this.close();
  }

  get isPaired() {
    return this.configured;
  }

  connect() {
    if (this.disposed) {
      this.lastError = "the worker is shutting down";
      return;
    }
    if (this.socket) { return; }
    if (!this.isPaired) {
      // Never sit silent: say why, and try the fixed address anyway.
      this.lastError = "no address to connect to";
      this.address = DEFAULT_ADDRESS;
    }
    let socket;
    try {
      socket = new WebSocket(this.address);
    } catch {
      this.scheduleReconnect();
      return;
    }
    this.socket = socket;

    const openTimeout = setTimeout(() => {
      if (!this.ready && this.socket === socket) {
        this.lastError = `no answer from ${this.address}`;
        socket.close();
      }
    }, 4000);
    socket.addEventListener("open", () => clearTimeout(openTimeout));

    socket.addEventListener("message", (event) => {
      let message;
      try { message = JSON.parse(String(event.data)); } catch { return; }
      this.onMessage(message);
    });
    socket.addEventListener("close", () => {
      this.ready = false;
      this.socket = null;
      this.emitState();
      this.scheduleReconnect();
    });
    socket.addEventListener("error", () => {
      // `close` follows and is where the retry lives.
      this.lastError = `cannot reach ${this.address}`;
    });
  }

  close() {
    this.socket?.close();
    this.socket = null;
    this.ready = false;
  }

  scheduleReconnect() {
    if (this.disposed || !this.isPaired) { return; }
    // A stored address may be stale (an older build used a random port). After a couple
    // of misses, try the fixed one too, so an upgrade needs no manual repair.
    // Only a *stored* address is presumed stale (an older build used a random port); an
    // address the user or a test chose explicitly is kept.
    if (!this.explicit && this.attempt >= 2 && this.address !== DEFAULT_ADDRESS) {
      this.address = DEFAULT_ADDRESS;
      this.attempt = 0;
    }
    const delay = BACKOFF[Math.min(this.attempt, BACKOFF.length - 1)];
    this.attempt += 1;
    setTimeout(() => this.connect(), delay);
  }

  emitState() {
    this.dispatchEvent(new CustomEvent("state", {
      detail: {
        connected: this.ready,
        paired: this.isPaired,
        address: this.address,
        // The popup shows this verbatim when it cannot reach the desktop.
        reason: this.ready ? "" : this.lastError || "the desktop app is not answering",
      },
    }));
  }

  send(message) {
    if (!this.socket || this.socket.readyState !== WebSocket.OPEN) { return false; }
    this.socket.send(JSON.stringify(message));
    return true;
  }

  /** @param {any} message */
  async onMessage(message) {
    switch (message.type) {
      case MSG.CHALLENGE: {
        // A fresh challenge on every connection: the proof is never reused. The first
        // time there is no secret yet, and the desktop answers with one (`paired`).
        const proof = this.secret ? await hmacHex(this.secret, message.nonce) : "";
        this.send({
          type: MSG.HELLO,
          major: PROTOCOL_MAJOR,
          minor: PROTOCOL_MINOR,
          extension_version: chrome.runtime.getManifest().version,
          proof,
        });
        return;
      }
      case MSG.WELCOME: {
        if (message.major !== PROTOCOL_MAJOR) {
          // A major mismatch is a refusal, not something to paper over.
          this.close();
          return;
        }
        this.ready = true;
        this.attempt = 0;
        this.emitState();
        this.flush();
        this.send({ type: MSG.LIST_TASKS });
        return;
      }
      case MSG.TASKS:
      case MSG.TASK:
      case MSG.REMOVED:
        this.dispatchEvent(new CustomEvent("tasks", { detail: message }));
        return;
      case MSG.PAIRED: {
        // Trust on first use: the desktop generated this secret for us. Keep it, so every
        // later connection proves it instead of asking again.
        this.secret = message.secret ?? "";
        await chrome.storage.local.set({ [STORAGE_KEYS.secret]: this.secret });
        this.dispatchEvent(new CustomEvent("paired", { detail: message }));
        return;
      }
      default:
        this.dispatchEvent(new CustomEvent("message", { detail: message }));
    }
  }

  flush() {
    if (!this.ready || this.queue.length === 0) { return; }
    for (const task of this.queue) { this.send({ type: MSG.ADD_TASK, ...task }); }
    this.queue = [];
    chrome.storage.local.set({ [STORAGE_KEYS.pending]: this.queue });
  }

  /**
   * Asks the desktop to download something. If the desktop is not up, the request is
   * queued and sent the moment it is; the user sees "queued" rather than a failure.
   * @param {{url: string, filename?: string, referer?: string}} task
   */
  async addTask(task) {
    if (this.ready && this.send({ type: MSG.ADD_TASK, ...task })) { return { queued: false }; }
    this.queue.push(task);
    await chrome.storage.local.set({ [STORAGE_KEYS.pending]: this.queue });
    return { queued: true };
  }
}

/**
 * Only a loopback websocket address is accepted. This is the one place the extension
 * decides where it will send a proof, so it is also the one place to be strict.
 * @param {string} raw
 * @returns {string|null}
 */
export function normaliseAddress(raw) {
  if (typeof raw !== "string") { return null; }
  const text = raw.trim();
  if (!text) { return null; }
  const withScheme = /^wss?:\/\//i.test(text) ? text : `ws://${text}`;
  let url;
  try { url = new URL(withScheme); } catch { return null; }
  if (url.protocol !== "ws:") { return null; }
  const host = url.hostname.toLowerCase();
  if (host !== "127.0.0.1" && host !== "localhost" && host !== "[::1]" && host !== "::1") {
    return null;
  }
  if (url.pathname !== "/" && url.pathname !== "") { return null; }
  const port = Number(url.port);
  if (!Number.isInteger(port) || port < 1024 || port > 65535) { return null; }
  return `ws://${host}:${port}`;
}
