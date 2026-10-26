/**
 * Builds `dist/`: the manifest plus every module, copied as they are.
 *
 * There is no bundler on purpose. Chrome 116 and later load a module service worker, the
 * ES modules here are already the shape the browser wants, and a build step that rewrote
 * them would be one more thing between the source and what Chrome reviews.
 */

import { cp, mkdir, readFile, rm, writeFile } from "node:fs/promises";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const root = join(here, "..");
const dist = join(root, "dist");

await rm(dist, { recursive: true, force: true });
await mkdir(dist, { recursive: true });

const manifest = JSON.parse(await readFile(join(root, "public/manifest.json"), "utf8"));
if (manifest.manifest_version !== 3) {
  throw new Error("only Manifest V3 is built");
}
if (manifest.permissions?.includes("webRequest")) {
  // MV3 cannot block on webRequest; a download interceptor that relied on it would fail
  // silently in production. The extension uses `downloads.onDeterminingFilename` instead.
  throw new Error("webRequest does not belong in a Manifest V3 download interceptor");
}

await cp(join(root, "public"), dist, { recursive: true });
for (const file of ["background.js", "background-state.js", "bridge.js", "downloads.js", "protocol.js", "popup.html", "popup.css", "popup.js"]) {
  await cp(join(root, "src", file), join(dist, file));
}

// The port is fixed by design (so a fresh install connects with nothing to type), and it
// must be the same number on both sides. The desktop's copy is the source of truth.
const rust = await readFile(join(root, "..", "crates/omad-rpc/src/server.rs"), "utf8");
const port = rust.match(/pub const DEFAULT_PORT: u16 = (\d+);/)?.[1];
if (!port) {
  throw new Error("crates/omad-rpc/src/server.rs has no DEFAULT_PORT");
}
const bridge = await readFile(join(dist, "bridge.js"), "utf8");
if (!bridge.includes(`127.0.0.1:${port}`)) {
  throw new Error(`bridge.js does not use the desktop's default port ${port}`);
}
for (const file of ["bridge.js", "background.js", "popup.js"]) {
  const text = await readFile(join(dist, file), "utf8");
  if (/localStorage\.setItem\(\s*["']secret/.test(text)) {
    throw new Error(`${file} stores a secret outside chrome.storage`);
  }
}

await writeFile(join(dist, "BUILD.txt"), `OmaLLDownloader extension ${manifest.version}\n`);
console.log(`built ${dist}`);
