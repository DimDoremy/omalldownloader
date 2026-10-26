/**
 * Zero configuration: a fresh extension must reach the running app with nothing typed.
 *
 * This talks to the *real* desktop app on its fixed port, so it is skipped when the app is
 * not running. It is the test for the complaint that the popup said "Offline".
 *
 *   node --test extension/tests/e2e/connect.test.mjs
 */

import assert from "node:assert/strict";
import { readFileSync, rmSync, mkdirSync } from "node:fs";
import { createConnection } from "node:net";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { spawn } from "node:child_process";
import { fileURLToPath } from "node:url";
import test from "node:test";

const here = dirname(fileURLToPath(import.meta.url));
const dist = join(here, "..", "..", "dist");
const chromium = process.env.CHROMIUM ?? "chromium";
const PORT = 14370;

function appIsListening() {
  return new Promise((resolve) => {
    const socket = createConnection({ host: "127.0.0.1", port: PORT }, () => {
      socket.end();
      resolve(true);
    });
    socket.on("error", () => resolve(false));
    socket.setTimeout(1000, () => {
      socket.destroy();
      resolve(false);
    });
  });
}

async function launch(profile) {
  const child = spawn(
    chromium,
    [
      "--headless=new", "--no-sandbox", "--no-first-run", "--disable-gpu",
      `--user-data-dir=${profile}`,
      `--load-extension=${dist}`, `--disable-extensions-except=${dist}`,
      "--remote-debugging-port=0", "about:blank",
    ],
    { stdio: ["ignore", "ignore", "pipe"] },
  );
  const log = [];
  child.stderr.on("data", (chunk) => log.push(String(chunk)));
  const portFile = join(profile, "DevToolsActivePort");
  const deadline = Date.now() + 20000;
  for (;;) {
    try {
      const [port] = readFileSync(portFile, "utf8").split("\n");
      return { child, port: Number(port) };
    } catch {
      if (Date.now() > deadline) {
        throw new Error(`Chromium did not start:\n${log.join("")}`);
      }
      await new Promise((r) => setTimeout(r, 100));
    }
  }
}

async function cdp(url) {
  const socket = new WebSocket(url);
  const pending = new Map();
  let id = 1;
  await new Promise((resolve, reject) => {
    socket.addEventListener("open", resolve, { once: true });
    socket.addEventListener("error", reject, { once: true });
  });
  socket.addEventListener("message", (event) => {
    const message = JSON.parse(event.data);
    const entry = pending.get(message.id);
    if (entry) { pending.delete(message.id); entry(message); }
  });
  return {
    evaluate(expression) {
      const callId = id++;
      socket.send(JSON.stringify({
        id: callId,
        method: "Runtime.evaluate",
        params: { expression, awaitPromise: true, returnByValue: true },
      }));
      return new Promise((resolve) => pending.set(callId, (reply) => {
        resolve(reply.result?.result?.value);
      }));
    },
    close() { socket.close(); },
  };
}

test("a fresh extension connects to the desktop with nothing typed", async (t) => {
  if (!(await appIsListening())) {
    t.skip("the desktop app is not running on 14370");
    return;
  }
  const profile = join(tmpdir(), `omad-connect-${process.pid}-${Date.now()}`);
  mkdirSync(profile, { recursive: true });
  const { child, port } = await launch(profile);
  t.after(() => {
    child.kill("SIGKILL");
    rmSync(profile, { recursive: true, force: true });
  });

  // The worker is the extension; ask it whether it is connected, without touching storage.
  const deadline = Date.now() + 20000;
  for (;;) {
    const list = await (await fetch(`http://127.0.0.1:${port}/json/list`)).json();
    const worker = list.find((target) => target.type === "service_worker");
    if (worker) {
      const session = await cdp(worker.webSocketDebuggerUrl);
      const state = await session.evaluate(
        `JSON.stringify({ connected: globalThis.__omadState?.connected ?? null,
                          ready: globalThis.desktopBridgeForTest?.ready ?? null })`,
      );
      session.close();
      if (state && state.includes('"ready":true')) {
        assert.ok(true);
        return;
      }
    }
    if (Date.now() > deadline) {
      throw new Error("the extension never connected to the desktop on its own");
    }
    await new Promise((r) => setTimeout(r, 250));
  }
});
