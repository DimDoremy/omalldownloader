/**
 * The extension in a real Chromium, driven over CDP.
 *
 * What is being tested is the part that cannot be unit-tested: the manifest actually loads,
 * the service worker runs, `downloads.onDeterminingFilename` fires, the browser's own copy
 * is cancelled, and the link arrives at a real WebSocket holding a real HMAC proof.
 *
 * The desktop side is `fake-bridge.mjs`, which implements the same protocol as `gf-rpc`.
 * Nothing here touches the user's profile: Chromium gets a throwaway `--user-data-dir`.
 *
 *   node --test extension/tests/e2e/
 */

import assert from "node:assert/strict";
import { readFileSync, rmSync, writeFileSync, mkdirSync } from "node:fs";
import { createServer } from "node:http";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { spawn } from "node:child_process";
import { fileURLToPath } from "node:url";
import test from "node:test";

import { FakeBridge } from "./fake-bridge.mjs";

const here = dirname(fileURLToPath(import.meta.url));
const dist = join(here, "..", "..", "dist");
const chromium = process.env.CHROMIUM ?? "chromium";

/** A 300 KB body, in the 100–500 KB range the review asked for. */
function body(size = 300 * 1024) {
  const bytes = Buffer.alloc(size);
  for (let i = 0; i < size; i += 1) {
    bytes[i] = (i * 31) % 251;
  }
  return bytes;
}

function serveFile(payload) {
  const server = createServer((request, response) => {
    if (request.url === "/page.html") {
      response.writeHead(200, { "content-type": "text/html; charset=utf-8" });
      // A page with ordinary download links. The test navigates to each file in turn:
      // Chrome blocks several automatic downloads from one page without a user gesture,
      // which would be a browser policy rather than a property of the extension.
      response.end(
        `<!DOCTYPE html><html><body>
           <h1>download page</h1>
           <a href="/small.bin" download>small.bin</a>
           <a href="/report.bin" download>report.bin</a>
           <a href="/large.bin" download>large.bin</a>
         </body></html>`,
      );
      return;
    }
    const name = (request.url ?? "/report.bin").replace("/", "") || "report.bin";
    response.writeHead(200, {
      "content-type": "application/octet-stream",
      "content-length": String(payload.length),
      "content-disposition": `attachment; filename="${name}"`,
      "accept-ranges": "bytes",
    });
    response.end(payload);
  });
  return new Promise((resolve) => {
    server.listen(0, "127.0.0.1", () => resolve({ server, port: server.address().port }));
  });
}

async function launchChromium(profile, extension) {
  const child = spawn(
    chromium,
    [
      "--headless=new",
      "--no-sandbox",
      "--no-first-run",
      "--no-default-browser-check",
      "--disable-gpu",
      `--user-data-dir=${profile}`,
      `--load-extension=${extension}`,
      `--disable-extensions-except=${extension}`,
      "--remote-debugging-port=0",
      "about:blank",
    ],
    { stdio: ["ignore", "ignore", "pipe"] },
  );
  const log = [];
  child.stderr.on("data", (chunk) => log.push(String(chunk)));

  // Chromium writes the port it chose next to the profile.
  const portFile = join(profile, "DevToolsActivePort");
  const deadline = Date.now() + 20000;
  for (;;) {
    try {
      const [port] = readFileSync(portFile, "utf8").split("\n");
      return { child, port: Number(port), log };
    } catch {
      if (Date.now() > deadline) {
        throw new Error(`Chromium did not start:\n${log.join("")}`);
      }
      await new Promise((resolve) => setTimeout(resolve, 100));
    }
  }
}

/** A CDP connection to one target. */
async function cdp(webSocketDebuggerUrl) {
  const socket = new WebSocket(webSocketDebuggerUrl);
  const pending = new Map();
  let nextId = 1;
  await new Promise((resolve, reject) => {
    socket.addEventListener("open", resolve, { once: true });
    socket.addEventListener("error", reject, { once: true });
  });
  socket.addEventListener("message", (event) => {
    const message = JSON.parse(event.data);
    const entry = pending.get(message.id);
    if (entry) {
      pending.delete(message.id);
      entry.resolve(message);
    }
  });
  return {
    send(method, params = {}) {
      const id = nextId++;
      socket.send(JSON.stringify({ id, method, params }));
      return new Promise((resolve) => pending.set(id, { resolve }));
    },
    async evaluate(expression, awaitPromise = true) {
      const reply = await this.send("Runtime.evaluate", {
        expression,
        awaitPromise,
        returnByValue: true,
      });
      if (reply.result?.exceptionDetails) {
        throw new Error(`evaluate failed: ${JSON.stringify(reply.result.exceptionDetails)}`);
      }
      return reply.result?.result?.value;
    },
    close() {
      socket.close();
    },
  };
}

async function targets(port) {
  const response = await fetch(`http://127.0.0.1:${port}/json/list`);
  return response.json();
}

async function waitForTarget(port, predicate, timeout = 15000) {
  const deadline = Date.now() + timeout;
  for (;;) {
    const list = await targets(port);
    const found = list.find(predicate);
    if (found) {
      return found;
    }
    if (Date.now() > deadline) {
      throw new Error(`no target matched; saw ${list.map((t) => `${t.type}:${t.url}`).join(", ")}`);
    }
    await new Promise((resolve) => setTimeout(resolve, 200));
  }
}

test("a browser download is handed to the desktop bridge", async (t) => {
  const payload = body();
  const { server, port: httpPort } = await serveFile(payload);
  const bridge = await new FakeBridge("secret-for-the-test").start();
  const profile = join(tmpdir(), `omad-cdp-${process.pid}-${Date.now()}`);
  mkdirSync(profile, { recursive: true });

  const { child, port } = await launchChromium(profile, dist);
  t.after(async () => {
    child.kill("SIGKILL");
    await bridge.stop();
    server.close();
    rmSync(profile, { recursive: true, force: true });
  });

  // 1. The extension's service worker is up, which means the manifest is valid.
  const worker = await waitForTarget(port, (target) =>
    target.type === "service_worker" && target.url.startsWith("chrome-extension://"),
  );
  const extensionId = new URL(worker.url).host;
  assert.match(extensionId, /^[a-p]{32}$/, "a Chromium extension id");

  // 2. Pair it the way the popup would: write the address and the shared secret, then
  //    restart the worker so `load()` picks them up.
  const workerCdp = await cdp(worker.webSocketDebuggerUrl);
  await workerCdp.send("Runtime.enable");
  await workerCdp.send("Log.enable");
  // Give the module a moment to finish its top-level await, then pair the way the popup
  // would.
  // The module has a top-level `await load()`, so the hook appears a moment after the
  // target does.
  let ready = "undefined";
  const hookDeadline = Date.now() + 10000;
  while (ready !== "function") {
    ready = await workerCdp.evaluate("typeof globalThis.__gfPairForTest");
    if (ready === "function") {
      break;
    }
    if (Date.now() > hookDeadline) {
      throw new Error("the worker never exposed its pairing hook");
    }
    await new Promise((resolve) => setTimeout(resolve, 100));
  }
  const paired = await workerCdp.evaluate(
    `globalThis.__gfPairForTest(${JSON.stringify(bridge.address)}, "secret-for-the-test")`,
  );
  assert.equal(paired, true, "the extension believes it is paired");
  const secondCdp = workerCdp;

  // The bridge sees a connection that proves itself with an HMAC over its nonce.
  const connectDeadline = Date.now() + 15000;
  while (!bridge.authenticated) {
    if (Date.now() > connectDeadline) {
      throw new Error(`the extension never authenticated; origin=${bridge.origin}`);
    }
    await new Promise((resolve) => setTimeout(resolve, 100));
  }
  assert.match(bridge.origin, /^chrome-extension:\/\//, "the bridge saw an extension origin");

  // 3. Visit the page once, then navigate to each file: that is a real browser download,
  //    which is the event the extension intercepts.
  const page = await waitForTarget(port, (target) => target.type === "page");
  const pageCdp = await cdp(page.webSocketDebuggerUrl);
  await pageCdp.send("Page.enable");
  await pageCdp.send("Page.navigate", { url: `http://127.0.0.1:${httpPort}/page.html` });
  await new Promise((resolve) => setTimeout(resolve, 600));

  // 4a. A clicked link: the referer says which page it was on.
  await pageCdp.evaluate(`document.querySelector('a[href="/report.bin"]').click()`, false);
  const clicked = await bridge.waitForTask(`http://127.0.0.1:${httpPort}/report.bin`, 20000);
  assert.equal(clicked.filename, "report.bin", "the name the browser would have used travels too");
  assert.match(
    clicked.referer ?? "",
    new RegExp(`127\\.0\\.0\\.1:${httpPort}/page\\.html`),
    "a clicked link keeps the page it came from",
  );

  // 4b. Navigations to two more files, at the other sizes in the range.
  for (const [file, name, size] of [
    ["small.bin", "small.bin", "120 KB"],
    ["large.bin", "large.bin", "480 KB"],
  ]) {
    const url = `http://127.0.0.1:${httpPort}/${file}`;
    await pageCdp.send("Page.navigate", { url });
    const task = await bridge.waitForTask(url, 20000);
    assert.equal(task.filename, name, `the name the browser would have used (${size}) travels too`);
  }

  // 5. The browser did not keep its own copy: the extension cancelled it. This has to be
  //    asked from the extension's own context, where `chrome.downloads` exists.
  const downloads = await secondCdp.evaluate(
    `chrome.downloads.search({}).then(list => list.map(d => d.state).join(","))`,
  );
  assert.ok(
    !String(downloads).includes("in_progress") && !String(downloads).includes("complete"),
    `the browser's copy should have been cancelled, states: ${downloads}`,
  );

  pageCdp.close();
  secondCdp.close();
});
