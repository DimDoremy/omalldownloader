/**
 * The protocol, mirroring `omad-rpc/src/protocol.rs`.
 *
 * One rule from the review matters more than the shapes: the extension never holds the
 * protocol version in a second place — `PROTOCOL_MAJOR`/`PROTOCOL_MINOR` are compared with
 * what the desktop greets it with, and a major mismatch is a refusal, not a silent degrade.
 */

export const PROTOCOL_MAJOR = 1;
export const PROTOCOL_MINOR = 0;

/** @typedef {{kind: "cli"|"ui"} | {kind: "extension", id: string}} Origin */

/**
 * @typedef {object} AddTask
 * @property {string} url
 * @property {string} [filename]  A bare name. The desktop sanitises it again.
 * @property {string} [referer]
 */

/**
 * @typedef {object} TaskView
 * @property {number} id
 * @property {string} url
 * @property {string} filename
 * @property {"queued"|"running"|"paused"|"completed"|"failed"} status
 * @property {number} received_bytes
 * @property {number|null} total_bytes
 * @property {number} speed
 * @property {string|null} error
 */

export const MSG = Object.freeze({
  HELLO: "hello",
  PAIR: "pair",
  LIST_TASKS: "list_tasks",
  ADD_TASK: "add_task",
  PAUSE: "pause",
  RESUME: "resume",
  REMOVE: "remove",
  CANCEL: "cancel",
  SUBSCRIBE: "subscribe",
  CHALLENGE: "challenge",
  WELCOME: "welcome",
  PAIRED: "paired",
  TASKS: "tasks",
  TASK: "task",
  REMOVED: "removed",
  ERROR: "error",
});

/**
 * A challenge is `{type:"challenge", nonce, expires_in_ms, capabilities}`. A nonce is
 * single use; a client that reuses one gets `bad_proof`.
 * @typedef {object} Challenge
 * @property {"challenge"} type
 * @property {string} nonce
 * @property {number} expires_in_ms
 * @property {string[]} capabilities
 */

/**
 * HMAC-SHA256(secret, nonce), lowercase hex. The same construction the desktop checks.
 * @param {string} secret
 * @param {string} nonce
 * @returns {Promise<string>}
 */
export async function hmacHex(secret, nonce) {
  const enc = new TextEncoder();
  const key = await crypto.subtle.importKey(
    "raw",
    enc.encode(secret),
    { name: "HMAC", hash: "SHA-256" },
    false,
    ["sign"],
  );
  const signature = await crypto.subtle.sign("HMAC", key, enc.encode(nonce));
  return [...new Uint8Array(signature)].map((b) => b.toString(16).padStart(2, "0")).join("");
}

/**
 * Timing-safe comparison of two hex strings, so a wrong proof cannot be brute forced one
 * character at a time.
 * @param {string} a @param {string} b
 */
export function constantTimeEquals(a, b) {
  if (typeof a !== "string" || typeof b !== "string" || a.length !== b.length) { return false; }
  let diff = 0;
  for (let i = 0; i < a.length; i += 1) { diff |= a.charCodeAt(i) ^ b.charCodeAt(i); }
  return diff === 0;
}
