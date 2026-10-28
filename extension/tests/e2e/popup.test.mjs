/**
 * The popup must stop saying "Offline" once the worker connects.
 *
 * The bug this exists for: the popup opened while the socket was still dialling, painted
 * "Offline", and the worker never told it otherwise — so a running app looked dead.
 *
 *   OMAD_APP=target/debug/omalldownloader node --test extension/tests/e2e/popup.test.mjs
 */

import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { existsSync, mkdirSync, readFileSync, rmSync } from "node:fs";
import { createServer as createNetServer } from "node:net";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import test from "node:test";

const here = dirname(fileURLToPath(import.meta.url));
const dist = join(here, "..", "..", "dist");
const chromium = process.env.CHROMIUM ?? "chromium";
const app = process.env.OMAD_APP ?? join(here, "..", "..", "..", "target", "debug", "omalldownloader");

const wait = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

async function waitFor(check, timeoutMs, what) {
  const deadline = Date.now() + timeoutMs;
  for (;;) {
    const value = await check();
    if (value) { return value; }
    if (Date.now() > deadline) { throw new Error(`timed out waiting for ${what}`); }
    await wait(150);
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
  const send = (method, params = {}) => {
    const callId = id++;
    socket.send(JSON.stringify({ id: callId, method, params }));
    return new Promise((resolve) => pending.set(callId, resolve));
  };
  return {
    send,
    async evaluate(expression) {
      const reply = await send("Runtime.evaluate", {
        expression, awaitPromise: true, returnByValue: true,
      });
      return reply.result?.result?.value;
    },
    close() { socket.close(); },
  };
}

test("the popup turns from Offline to Connected when the app answers", async (t) => {
  if (!existsSync(app)) { t.skip(`the app is not built at ${app}`); return; }
  const work = join(tmpdir(), `omad-popup-${process.pid}-${Date.now()}`);
  const profile = join(work, "chromium");
  mkdirSync(profile, { recursive: true });
  mkdirSync(join(work, "downloads"), { recursive: true });

  const port = await new Promise((resolve) => {
    const probe = createNetServer();
    probe.listen(0, "127.0.0.1", () => {
      const { port } = probe.address();
      probe.close(() => resolve(port));
    });
  });

  // The app starts *later*: that is the user's situation — popup open on a closed app,
  // then the app is started and the popup must recover without being reopened.
  let appProcess = null;
  const appLog = [];
  const startApp = () => {
    appProcess = spawn(app, [], {
      env: {
        ...process.env,
        OMAD_BRIDGE_PORT: String(port),
        XDG_DATA_HOME: join(work, "data"),
        XDG_DOWNLOAD_DIR: join(work, "downloads"),
        OMAD_LANG: "en",
      },
      stdio: ["ignore", "ignore", "pipe"],
      detached: true,
    });
    appProcess.stderr.on("data", (chunk) => appLog.push(String(chunk)));
  };

  const browser = spawn(chromium, [
    "--headless=new", "--no-sandbox", "--no-first-run", "--disable-gpu",
    `--user-data-dir=${profile}`,
    `--load-extension=${dist}`, `--disable-extensions-except=${dist}`,
    "--remote-debugging-port=0", "about:blank",
  ], { stdio: ["ignore", "ignore", "pipe"], detached: true });

  t.after(() => {
    for (const child of [browser, appProcess].filter(Boolean)) {
      try { process.kill(-child.pid, "SIGKILL"); } catch { /* already gone */ }
      child.stderr?.destroy();
    }
    rmSync(work, { recursive: true, force: true });
  });

  const portFile = join(profile, "DevToolsActivePort");
  const devtools = await waitFor(() => {
    try { return Number(readFileSync(portFile, "utf8").split("\n")[0]); } catch { return 0; }
  }, 20000, "Chromium's debugging port");

  const worker = await waitFor(async () => {
    const list = await (await fetch(`http://127.0.0.1:${devtools}/json/list`)).json();
    return list.find((target) => target.type === "service_worker");
  }, 20000, "the extension's service worker");
  const workerCdp = await cdp(worker.webSocketDebuggerUrl);
  t.after(() => workerCdp.close());
  // Wait for the worker's own start-up before steering it, or its `load()` overwrites us.
  await waitFor(
    () => workerCdp.evaluate("typeof globalThis.desktopBridgeForTest === 'object'"),
    15000,
    "the worker to expose its bridge",
  );

  // Point the worker at this test's port, the way a user's stored address would.
  await workerCdp.evaluate(`globalThis.__gfPairForTest("ws://127.0.0.1:${port}", "")`);
  await wait(600);

  // Open the popup as a page, immediately — that is the race the bug lived in.
  const extensionId = new URL(worker.url).host;
  const popup = await (await fetch(
    `http://127.0.0.1:${devtools}/json/new?chrome-extension://${extensionId}/popup.html`,
    { method: "PUT" },
  )).json();
  const popupCdp = await cdp(popup.webSocketDebuggerUrl);
  t.after(() => popupCdp.close());
  await popupCdp.send("Runtime.enable");

  // Offline first, with the app not running.
  const offline = await waitFor(async () => {
    const text = await popupCdp.evaluate('document.getElementById("status")?.textContent ?? ""');
    return text.trim() === "Offline" ? text.trim() : null;
  }, 15000, 'the popup to say "Offline" before the app starts');
  assert.equal(offline, "Offline");

  // Now start the app; the popup must recover on its own.
  startApp();

  const status = await waitFor(async () => {
    const text = await popupCdp.evaluate('document.getElementById("status")?.textContent ?? ""');
    return text && text.trim() === "Connected" ? text.trim() : null;
  }, 20000, 'the popup to say "Connected"');

  assert.equal(status, "Connected");
  const reason = await popupCdp.evaluate('document.getElementById("reason")?.textContent ?? ""');
  assert.equal(reason.trim(), "", "no reason is shown while connected");
});
