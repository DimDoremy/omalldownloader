/**
 * The case that made this unusable in real life: a file host that
 *
 *   * refuses HEAD with 403,
 *   * refuses GET without a `Referer` (hotlink protection),
 *   * puts no file name in the URL — only in `Content-Disposition`.
 *
 * The extension has to pass both the page it came from and the name the browser found, and
 * the app has to use them. The assertion is the file, its name and its bytes.
 *
 *   node --test extension/tests/e2e/hotlink.test.mjs
 *   OMAD_APP=target/debug/omalldownloader node --test ...
 */

import assert from "node:assert/strict";
import { createServer } from "node:http";
import { createServer as createNetServer } from "node:net";
import { spawn } from "node:child_process";
import { createHash } from "node:crypto";
import { existsSync, mkdirSync, readFileSync, rmSync, readdirSync, statSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import test from "node:test";

const here = dirname(fileURLToPath(import.meta.url));
const dist = join(here, "..", "..", "dist");
const chromium = process.env.CHROMIUM ?? "chromium";
const app = process.env.OMAD_APP ?? join(here, "..", "..", "..", "target", "debug", "omalldownloader");
const NAME = "pond-linux-2024.iso";
const BODY = Buffer.from(Array.from({ length: 400 * 1024 }, (_, i) => (i * 11) % 251));
const WANT = createHash("sha256").update(BODY).digest("hex");

const wait = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

async function waitFor(check, timeoutMs, what) {
  const deadline = Date.now() + timeoutMs;
  for (;;) {
    const value = await check();
    if (value) { return value; }
    if (Date.now() > deadline) { throw new Error(`timed out waiting for ${what}`); }
    await wait(200);
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

test("a hotlink-protected file without a name in its URL arrives with the server's name", async (t) => {
  if (!existsSync(app)) { t.skip(`the app is not built at ${app}`); return; }
  const work = join(tmpdir(), `omad-hotlink-${process.pid}-${Date.now()}`);
  const downloads = join(work, "downloads");
  const data = join(work, "data");
  const profile = join(work, "chromium");
  mkdirSync(downloads, { recursive: true });
  mkdirSync(profile, { recursive: true });

  const server = createServer((request, response) => {
    if (request.url.startsWith("/page")) {
      response.writeHead(200, { "Content-Type": "text/html" });
      response.end('<a id="go" href="/get?id=42" download>download</a>');
      return;
    }
    if (request.method === "HEAD") {
      response.writeHead(403, { "Content-Length": "0" });
      response.end();
      return;
    }
    if (!(request.headers.referer ?? "").startsWith("http://127.0.0.1")) {
      response.writeHead(403, { "Content-Length": "0" });
      response.end();
      return;
    }
    const range = request.headers.range;
    let start = 0;
    let end = BODY.length - 1;
    if (range?.startsWith("bytes=")) {
      const [first, last] = range.slice(6).split("-");
      start = Number(first);
      if (last) { end = Math.min(Number(last), BODY.length - 1); }
    }
    const chunk = BODY.subarray(start, end + 1);
    response.writeHead(range ? 206 : 200, {
      "Content-Type": "application/octet-stream",
      "Content-Disposition": `attachment; filename="${NAME}"`,
      "Accept-Ranges": "bytes",
      "Content-Length": String(chunk.length),
      ...(range ? { "Content-Range": `bytes ${start}-${end}/${BODY.length}` } : {}),
    });
    response.end(chunk);
  });
  await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
  const origin = `http://127.0.0.1:${server.address().port}`;

  // A port of our own: another build of the app may be running on the fixed one, and this
  // test must not depend on that. The extension is pointed here through its own hook.
  const bridgePort = await new Promise((resolve) => {
    const probe = createNetServer();
    probe.listen(0, "127.0.0.1", () => {
      const { port } = probe.address();
      probe.close(() => resolve(port));
    });
  });

  const appProcess = spawn(app, [], {
    env: {
      ...process.env,
      OMAD_BRIDGE_PORT: String(bridgePort),
      XDG_DATA_HOME: data,
      XDG_DOWNLOAD_DIR: downloads,
      // The test server is on loopback; a browser-sent link may not reach it otherwise.
      OMAD_ALLOW_PRIVATE: "1",
      OMAD_LANG: "en",
    },
    stdio: ["ignore", "ignore", "pipe"],
    detached: true,
  });
  const appLog = [];
  appProcess.stderr.on("data", (chunk) => appLog.push(String(chunk)));

  const browser = spawn(chromium, [
    "--headless=new", "--no-sandbox", "--no-first-run", "--disable-gpu",
    `--user-data-dir=${profile}`,
    `--load-extension=${dist}`, `--disable-extensions-except=${dist}`,
    "--remote-debugging-port=0", "about:blank",
  ], { stdio: ["ignore", "ignore", "pipe"], detached: true });

  t.after(() => {
    for (const child of [browser, appProcess]) {
      try { process.kill(-child.pid, "SIGKILL"); } catch { /* already gone */ }
      child.stderr?.destroy();
    }
    server.close();
    rmSync(work, { recursive: true, force: true });
  });

  const portFile = join(profile, "DevToolsActivePort");
  const port = await waitFor(() => {
    try { return Number(readFileSync(portFile, "utf8").split("\n")[0]); } catch { return 0; }
  }, 20000, "Chromium's debugging port");

  const worker = await waitFor(async () => {
    const list = await (await fetch(`http://127.0.0.1:${port}/json/list`)).json();
    return list.find((target) => target.type === "service_worker");
  }, 20000, "the extension's service worker");
  const workerCdp = await cdp(worker.webSocketDebuggerUrl);
  t.after(() => workerCdp.close());
  await workerCdp.evaluate(
    `globalThis.__gfPairForTest("ws://127.0.0.1:${bridgePort}", "")`,
  );
  await waitFor(
    () => workerCdp.evaluate("Boolean(globalThis.desktopBridgeForTest?.ready)"),
    30000,
    "the extension to connect to the app",
  );

  const page = await (await fetch(`http://127.0.0.1:${port}/json/new?${origin}/page`, { method: "PUT" })).json();
  const pageCdp = await cdp(page.webSocketDebuggerUrl);
  t.after(() => pageCdp.close());
  await pageCdp.send("Runtime.enable");
  await waitFor(
    () => pageCdp.evaluate('Boolean(document.getElementById("go"))'),
    15000,
    "the link to be rendered",
  );
  await pageCdp.evaluate('document.getElementById("go").click()');

  const saved = await waitFor(() => {
    try {
      for (const name of readdirSync(downloads)) {
        const path = join(downloads, name);
        if (statSync(path).size === BODY.length) { return path; }
      }
      return null;
    } catch { return null; }
  }, 40000, "the app to write the file").catch((error) => {
    throw new Error(`${error.message}\napp log:\n${appLog.join("")}`);
  });

  assert.equal(dirname(saved).endsWith(downloads), true);
  assert.equal(saved.endsWith(NAME), true, `saved as ${saved}, expected the server's name`);
  const got = createHash("sha256").update(readFileSync(saved)).digest("hex");
  assert.equal(got, WANT, "the bytes are the ones the server sent");
});
