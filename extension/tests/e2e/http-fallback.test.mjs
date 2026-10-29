/**
 * The HTTP fallback: when a browser refuses `ws://` on loopback, the same commands work
 * over `POST /rpc` on the same port. This drives it directly, because forcing a real Edge
 * to block WebSockets is not something a test can do.
 */

import assert from "node:assert/strict";
import { createServer } from "node:http";
import { spawn } from "node:child_process";
import { existsSync, mkdirSync, readFileSync, readdirSync, rmSync, statSync } from "node:fs";
import { createServer as createNetServer } from "node:net";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import test from "node:test";

const here = dirname(fileURLToPath(import.meta.url));
const dist = join(here, "..", "..", "dist");
const chromium = process.env.CHROMIUM ?? "chromium";
const app = process.env.OMAD_APP ?? join(here, "..", "..", "..", "target", "debug", "omalldownloader");
const BODY = Buffer.from(Array.from({ length: 120 * 1024 }, (_, i) => (i * 5) % 251));

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
      const reply = await send("Runtime.evaluate", { expression, awaitPromise: true, returnByValue: true });
      if (reply.result?.exceptionDetails) {
        throw new Error(JSON.stringify(reply.result.exceptionDetails).slice(0, 300));
      }
      return reply.result?.result?.value;
    },
    close() { socket.close(); },
  };
}

test("a task can be added over the HTTP fallback when the WebSocket is blocked", async (t) => {
  if (!existsSync(app)) { t.skip(`the app is not built at ${app}`); return; }
  const work = join(tmpdir(), `omad-http-${process.pid}-${Date.now()}`);
  const downloads = join(work, "downloads");
  const profile = join(work, "chromium");
  mkdirSync(downloads, { recursive: true });
  mkdirSync(profile, { recursive: true });

  const server = createServer((request, response) => {
    response.writeHead(200, {
      "Content-Type": "application/octet-stream",
      "Content-Disposition": 'attachment; filename="over-http.bin"',
      "Content-Length": String(BODY.length),
    });
    response.end(BODY);
  });
  await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
  const fileUrl = `http://127.0.0.1:${server.address().port}/get?id=9`;

  const bridgePort = await new Promise((resolve) => {
    const probe = createNetServer();
    probe.listen(0, "127.0.0.1", () => {
      const { port } = probe.address();
      probe.close(() => resolve(port));
    });
  });

  const appProcess = spawn(app, [], {
    env: { ...process.env, OMAD_BRIDGE_PORT: String(bridgePort), XDG_DATA_HOME: join(work, "data"),
      XDG_DOWNLOAD_DIR: downloads, OMAD_ALLOW_PRIVATE: "1", OMAD_LANG: "en" },
    stdio: ["ignore", "ignore", "pipe"], detached: true,
  });
  const appLog = [];
  appProcess.stderr.on("data", (chunk) => appLog.push(String(chunk)));

  const browser = spawn(chromium, ["--headless=new", "--no-sandbox", "--no-first-run", "--disable-gpu",
    `--user-data-dir=${profile}`, `--load-extension=${dist}`, `--disable-extensions-except=${dist}`,
    "--remote-debugging-port=0", "about:blank"], { stdio: ["ignore", "ignore", "pipe"], detached: true });

  t.after(() => {
    for (const child of [browser, appProcess]) {
      try { process.kill(-child.pid, "SIGKILL"); } catch { /* gone */ }
      child.stderr?.destroy();
    }
    server.close();
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

  await waitFor(
    () => workerCdp.evaluate("typeof globalThis.__gfPairForTest === 'function'"),
    15000, "the worker's hook",
  );
  await workerCdp.evaluate(`globalThis.__gfPairForTest("ws://127.0.0.1:${bridgePort}", "")`);

  // Force the fallback, as a browser that blocks ws:// would.
  const mode = await workerCdp.evaluate(
    `(async () => { await globalThis.desktopBridgeForTest.enterHttpMode("test: ws blocked"); return globalThis.desktopBridgeForTest.mode; })()`,
  );
  assert.equal(mode, "http", "the bridge switched to the HTTP transport");

  const added = await workerCdp.evaluate(
    `(async () => { const r = await globalThis.desktopBridgeForTest.addTask({ url: ${JSON.stringify(fileUrl)}, filename: "over-http.bin", referer: "http://127.0.0.1/page" }); return JSON.stringify(r); })()`,
  );
  assert.match(added, /"queued":false/, `add returned ${added}`);

  const saved = await waitFor(() => {
    try {
      const walk = (dir) => readdirSync(dir, { withFileTypes: true }).flatMap((entry) => {
        const path = join(dir, entry.name);
        return entry.isDirectory() ? walk(path) : [path];
      });
      for (const path of walk(downloads)) {
        if (statSync(path).size === BODY.length) { return path; }
      }
      return null;
    } catch { return null; }
  }, 30000, "the app to write the file").catch((error) => {
    throw new Error(`${error.message}\napp log:\n${appLog.join("")}`);
  });
  assert.equal(readFileSync(saved).equals(BODY), true, "the bytes are the ones served");
});
