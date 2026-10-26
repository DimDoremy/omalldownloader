/**
 * The extension's pure parts, under Node's test runner.
 *
 * The browser APIs are not available here, so these tests cover what actually decides
 * safety and behaviour: the address the extension will accept, the HMAC proof it sends,
 * and the comparison it uses on one. The parts that need a browser (the download
 * interceptor, the popup) are written so their decisions live in functions that could be
 * extracted the same way; they are not asserted here, and nothing below pretends they are.
 *
 * Run with: node --test extension/tests/
 */

import assert from "node:assert/strict";
import test from "node:test";

import { PROTOCOL_MAJOR, constantTimeEquals, hmacHex } from "../src/protocol.js";
import { normaliseAddress } from "../src/bridge.js";

test("only loopback websocket addresses are accepted", () => {
  assert.equal(normaliseAddress("ws://127.0.0.1:54321"), "ws://127.0.0.1:54321");
  assert.equal(normaliseAddress("127.0.0.1:54321"), "ws://127.0.0.1:54321");
  assert.equal(normaliseAddress("ws://localhost:54321"), "ws://localhost:54321");
  assert.equal(normaliseAddress("  ws://127.0.0.1:54321  "), "ws://127.0.0.1:54321");

  // The interesting refusals: anywhere a proof must never be sent.
  assert.equal(normaliseAddress("ws://evil.example:54321"), null);
  assert.equal(normaliseAddress("wss://evil.example:443"), null);
  assert.equal(normaliseAddress("ws://192.168.1.10:54321"), null);
  assert.equal(normaliseAddress("http://127.0.0.1:54321"), null);
  assert.equal(normaliseAddress("ws://127.0.0.1"), null);
  assert.equal(normaliseAddress("ws://127.0.0.1:80"), null);
  assert.equal(normaliseAddress("ws://127.0.0.1:54321/some/path"), null);
  assert.equal(normaliseAddress(""), null);
  assert.equal(normaliseAddress(null), null);
});

test("the proof is HMAC-SHA256 in lowercase hex, and depends on both inputs", async () => {
  const a = await hmacHex("123456", "abcd");
  const b = await hmacHex("123456", "abcd");
  const c = await hmacHex("654321", "abcd");
  const d = await hmacHex("123456", "efgh");
  assert.equal(a, b, "the same inputs make the same proof");
  assert.notEqual(a, c, "a different code makes a different proof");
  assert.notEqual(a, d, "a different nonce makes a different proof");
  assert.match(a, /^[0-9a-f]{64}$/);
});

test("the proof matches what the desktop computes", async () => {
  // A fixed vector, so a change to the construction cannot pass unnoticed. The same
  // value is asserted on the Rust side (`omad-rpc`'s `hmac_hex("code", "nonce")`), which is
  // what makes the two implementations provably agree rather than probably agree.
  assert.equal(
    await hmacHex("code", "nonce"),
    "19e5e5ae0835a97f94fc6537978c10438fc15109add0b535489c4fc5ffa383a5",
  );
});

test("comparison is constant time and length safe", () => {
  assert.equal(constantTimeEquals("abc", "abc"), true);
  assert.equal(constantTimeEquals("abc", "abd"), false);
  assert.equal(constantTimeEquals("abc", "ab"), false);
  assert.equal(constantTimeEquals("", "a"), false);
  assert.equal(constantTimeEquals(undefined, "a"), false);
});

test("the protocol major is pinned so a mismatch is visible", () => {
  assert.equal(PROTOCOL_MAJOR, 1);
});
