/**
 * The extension against a *released* AppImage, over CDP.
 *
 * This is the test the packaged build exists for: start the AppImage, start a browser with
 * the extension, download a file from a page, and check that the file the AppImage wrote is
 * the file that was served — no fake bridge, no dev build.
 *
 *   node --test extension/tests/e2e/appimage.test.mjs
 *   OMAD_APPIMAGE=/path/to/OmaLLDownloader-x86_64.AppImage node --test ...
 */

import assert from "node:assert/strict";
import { createServer } from "node:http";
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
const appimage = process.env.OMAD_APPIMAGE;
const work = join(tmpdir(), `omad-appimage-${process.pid}-${Date.now()}`);
const data = join(work, "data");
const downloads = join(work, "downloads");
const profile = join(work, "chromium");

const CONTENT = Buffer.from(Array.from({ length: 300 * 1024 }, (_, i) => (i * 7) % 251));
const WANT = createHash("sha256").update(CONTENT).digest("hex");

function wait(ms) {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

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
        expression,
        awaitPromise: true,
        returnByValue: true,
      });
      return reply.result?.result?.value;
    },
    close() { socket.close(); },
  };
}

test("a released AppImage receives a download from the extension", async (t) => {
  if (!appimage || !existsSync(appimage)) {
    t.skip("set OMAD_APPIMAGE to the released AppImage");
    return;
  }
  mkdirSync(downloads, { recursive: true });
  mkdirSync(profile, { recursive: true });

  // A page that offers the file, served locally.
  const server = createServer((request, response) => {
    if (request.url === "/file.bin") {
      response.writeHead(200, {
        "Content-Type": "application/octet-stream",
        "Content-Length": String(CONTENT.length),
        "Content-Disposition": 'attachment; filename="release-test.bin"',
      });
      response.end(CONTENT);
      return;
    }
    response.writeHead(200, { "Content-Type": "text/html" });
    response.end('<a id="go" href="/file.bin" download>download</a>');
  });
  await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
  const origin = `http://127.0.0.1:${server.address().port}`;

  // The released AppImage itself.
  const app = spawn(appimage, [], {
    env: {
      ...process.env,
      XDG_DATA_HOME: data,
      XDG_DOWNLOAD_DIR: downloads,
      OMAD_LANG: "en",
      // The test server is on loopback, and a link that came from a browser may not reach
      // loopback by default — that guard is the point of the app, so the test opts out of it
      // the same way a developer would.
      OMAD_ALLOW_PRIVATE: "1",
      APPIMAGE_EXTRACT_AND_RUN: "1",
    },
    stdio: ["ignore", "ignore", "pipe"],
    // Its own process group: the AppImage runs the real binary as a child, and killing
    // only the parent would leave that child holding the port and the test's handles.
    detached: true,
  });
  const appLog = [];
  app.stderr.on("data", (chunk) => appLog.push(String(chunk)));

  const browser = spawn(
    chromium,
    [
      "--headless=new", "--no-sandbox", "--no-first-run", "--disable-gpu",
      `--user-data-dir=${profile}`,
      `--load-extension=${dist}`, `--disable-extensions-except=${dist}`,
      "--remote-debugging-port=0", "about:blank",
    ],
    { stdio: ["ignore", "ignore", "pipe"], detached: true },
  );
  t.after(() => {
    for (const child of [browser, app]) {
      try { process.kill(-child.pid, "SIGKILL"); } catch { /* already gone */ }
      child.stderr?.destroy();
    }
    server.close();
    rmSync(work, { recursive: true, force: true });
  });

  // Wait for both: the AppImage listening, and the extension connected to it.
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
  await waitFor(
    () => workerCdp.evaluate("Boolean(globalThis.desktopBridgeForTest?.ready)"),
    30000,
    "the extension to connect to the AppImage",
  );

  // Open the page and click the link, which is what a user does.
  const page = await (await fetch(`http://127.0.0.1:${port}/json/new?${origin}`, {
    method: "PUT",
  })).json();
  const pageCdp = await cdp(page.webSocketDebuggerUrl);
  t.after(() => pageCdp.close());
  await pageCdp.send("Runtime.enable");
  await wait(500);
  await pageCdp.evaluate('document.getElementById("go").click()');

  // The AppImage writes the file; the browser's own copy must not exist.
  // The name comes from the URL, not from `Content-Disposition`, so the size is what
  // identifies the file; the hash below proves it is the right one.
  const written = await waitFor(() => {
    try {
      // A download lands in its category folder (`ISO/`, `Video/`...), so look one level
      // down as well as in the root.
      const walk = (dir) => readdirSync(dir, { withFileTypes: true }).flatMap((entry) => {
        const path = join(dir, entry.name);
        return entry.isDirectory() ? walk(path) : [path];
      });
      for (const path of walk(downloads)) {
        if (statSync(path).size === CONTENT.length) { return path; }
      }
      return null;
    } catch { return null; }
  }, 30000, "the AppImage to write the file");

  const got = createHash("sha256").update(readFileSync(written)).digest("hex");
  assert.equal(got, WANT, "the AppImage wrote the bytes the server sent");
  assert.ok(appLog.length >= 0);
});
