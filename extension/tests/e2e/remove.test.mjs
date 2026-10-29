/**
 * Removing a task must work while another download is running.
 *
 * The bridge loop awaited each download, so `remove` (and `pause`) sat in the queue behind
 * a 1.4 GB transfer — the button appeared dead.
 */

import assert from "node:assert/strict";
import { createServer } from "node:http";
import { spawn } from "node:child_process";
import { existsSync, mkdirSync, rmSync } from "node:fs";
import { createServer as createNetServer } from "node:net";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import test from "node:test";
import { DatabaseSync } from "node:sqlite";

const here = dirname(fileURLToPath(import.meta.url));
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

test("a task can be removed while another download runs", async (t) => {
  if (!existsSync(app)) { t.skip(`the app is not built at ${app}`); return; }
  const work = join(tmpdir(), `omad-remove-${process.pid}-${Date.now()}`);
  const downloads = join(work, "downloads");
  mkdirSync(downloads, { recursive: true });

  // One small file that finishes at once, and one that trickles for a long time.
  const server = createServer((request, response) => {
    if (request.url.startsWith("/slow")) {
      response.writeHead(200, { "Content-Type": "application/octet-stream",
        "Content-Length": String(64 * 1024 * 1024) });
      const tick = setInterval(() => response.write(Buffer.alloc(4096)), 50);
      response.on("close", () => clearInterval(tick));
      return;
    }
    response.writeHead(200, { "Content-Type": "application/octet-stream",
      "Content-Disposition": 'attachment; filename="small.bin"', "Content-Length": "1024" });
    response.end(Buffer.alloc(1024, 7));
  });
  await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
  const base = `http://127.0.0.1:${server.address().port}`;

  const port = await new Promise((resolve) => {
    const probe = createNetServer();
    probe.listen(0, "127.0.0.1", () => { const { port } = probe.address(); probe.close(() => resolve(port)); });
  });

  const appProcess = spawn(app, [], {
    env: { ...process.env, OMAD_BRIDGE_PORT: String(port), XDG_DATA_HOME: join(work, "data"),
      XDG_DOWNLOAD_DIR: downloads, OMAD_ALLOW_PRIVATE: "1", OMAD_LANG: "en" },
    stdio: ["ignore", "ignore", "pipe"], detached: true,
  });
  const appLog = [];
  appProcess.stderr.on("data", (chunk) => appLog.push(String(chunk)));
  t.after(() => {
    try { process.kill(-appProcess.pid, "SIGKILL"); } catch { /* gone */ }
    appProcess.stderr?.destroy();
    server.close();
    rmSync(work, { recursive: true, force: true });
  });

  const rpc = async (op, extra = {}) => {
    const response = await fetch(`http://127.0.0.1:${port}/rpc`, {
      method: "POST",
      headers: { "Content-Type": "text/plain;charset=UTF-8", Origin: "chrome-extension://removetest" },
      body: JSON.stringify({ op, ...extra }),
    });
    return response.json();
  };

  await waitFor(async () => {
    try { const reply = await rpc("list"); return reply.tasks ? reply.tasks : null; } catch { return null; }
  }, 20000, "the app to answer");

  const slow = await rpc("add", { url: `${base}/slow.bin` });
  const small = await rpc("add", { url: `${base}/small.bin` });
  assert.ok(slow.task && small.task, `both tasks were added: ${JSON.stringify({ slow, small })}`);

  // The slow task is running by now; removing the other one must not wait for it.
  const started = Date.now();
  const removed = await rpc("remove", { id: small.task.id });
  assert.equal(removed.ok, true, `remove answered ${JSON.stringify(removed)}`);

  const dbPath = join(work, "data", "omalldownloader", "omalldownloader.sqlite3");
  await waitFor(() => {
    try {
      const db = new DatabaseSync(dbPath, { readOnly: true });
      const rows = db.prepare("select id from tasks").all().map((row) => row.id);
      db.close();
      return rows.includes(small.task.id) ? null : true;
    } catch { return null; }
  }, 10000, "the removed task to leave the store");
  assert.ok(Date.now() - started < 5000, "the remove did not wait for the running download");
});
