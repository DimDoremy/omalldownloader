/**
 * Taking a download away from the browser.
 *
 * `chrome.downloads.onDeterminingFilename` is the only reliable seam: it is synchronous,
 * once per download, and lets the extension supply the name. A download is "taken" when
 * the user asked for it (the context menu, the popup) or when the "take everything"
 * switch is on and the file passes the size and extension filters — the same behaviour a
 * user expects from a download manager.
 *
 * The review's rule is kept: the extension sends a **name**, never a path, and it says
 * where a link came from so the desktop can record the origin.
 */

import { MSG } from "./protocol.js";
import { desktopBridge } from "./background-state.js";

const SETTINGS = Object.freeze({
  takeAll: "takeAllDownloads",
  minSizeKb: "minTakeSizeKb",
  skipExtensions: "skipExtensions",
});

const settings = {
  takeAll: true,
  minSizeKb: 0,
  skipExtensions: new Set(),
};

export async function loadSettings() {
  const stored = await chrome.storage.local.get(Object.values(SETTINGS));
  settings.takeAll = stored[SETTINGS.takeAll] ?? true;
  settings.minSizeKb = stored[SETTINGS.minSizeKb] ?? 0;
  const raw = stored[SETTINGS.skipExtensions] ?? "";
  settings.skipExtensions = new Set(
    String(raw)
      .split(",")
      .map((part) => part.trim().replace(/^\./, "").toLowerCase())
      .filter(Boolean),
  );
  return settings;
}

export function currentSettings() {
  return {
    takeAll: settings.takeAll,
    minSizeKb: settings.minSizeKb,
    skipExtensions: [...settings.skipExtensions].join(","),
  };
}

export async function saveSettings(next) {
  if (typeof next.takeAll === "boolean") { settings.takeAll = next.takeAll; }
  if (Number.isFinite(next.minSizeKb)) { settings.minSizeKb = Math.max(0, next.minSizeKb); }
  if (typeof next.skipExtensions === "string") {
    settings.skipExtensions = new Set(
      next.skipExtensions.split(",").map((p) => p.trim().replace(/^\./, "").toLowerCase()).filter(Boolean),
    );
  }
  await chrome.storage.local.set({
    [SETTINGS.takeAll]: settings.takeAll,
    [SETTINGS.minSizeKb]: settings.minSizeKb,
    [SETTINGS.skipExtensions]: currentSettings().skipExtensions,
  });
  await refreshRules();
}

/**
 * Which extensions the browser keeps. A file the download manager cannot improve on — a
 * tiny text file, a page saved as HTML — stays in the browser.
 */
function isSkipped(url, filename) {
  const name = (filename || new URL(url).pathname).toLowerCase();
  const ext = name.includes(".") ? name.split(".").pop() : "";
  if (!ext) { return false; }
  return settings.skipExtensions.has(ext);
}

/**
 * Decides whether this download is ours, and hands it to the desktop if so.
 * @param {chrome.downloads.DownloadItem} item
 * @param {(info?: {filename?: string}) => void} suggest
 */
export async function onDeterminingFilename(item, suggest) {
  // Bytes the user did not ask for a file from (a blob, a data URI) stay in the browser.
  if (!/^https?:/i.test(item.url)) { suggest(); return; }
  if (item.byExtensionId) { suggest(); return; }

  const filename = item.filename ? item.filename.split(/[\\/]/).pop() : "";
  const tooSmall = Number.isFinite(item.fileSize) && item.fileSize > 0
    ? item.fileSize / 1024 < settings.minSizeKb
    : false;

  const shouldTake = settings.takeAll && !tooSmall && !isSkipped(item.url, filename);
  if (!shouldTake) { suggest(); return; }

  // Cancel the browser's copy and ask the desktop for it.
  try { await chrome.downloads.cancel(item.id); } catch { /* already gone */ }

  const result = await desktopBridge.addTask({
    url: item.url,
    filename: filename || undefined,
    referer: item.referrer || undefined,
  });

  if (result.queued) {
    notify("Queued in OmaLLDownloader", "The desktop app is not running; the link is waiting.");
  }
  // The browser still needs an answer; the file it would have saved is cancelled.
  suggest({ filename: filename || "omalldownloader-download" });
}

/** Takes one link the user chose explicitly. Returns what the popup should say. */
export async function takeLink({ url, filename, referer }) {
  if (!/^https?:/i.test(url)) { return { ok: false, message: "Only http and https links are supported." }; }
  const result = await desktopBridge.addTask({ url, filename, referer });
  return {
    ok: true,
    message: result.queued ? "Queued: the desktop app is not running yet." : "Sent to OmaLLDownloader.",
  };
}

/** The task list, as the popup shows it. */
export async function fetchTasks() {
  desktopBridge.send({ type: MSG.LIST_TASKS });
}

function notify(title, message) {
  chrome.notifications?.create({
    type: "basic",
    iconUrl: "icon128.png",
    title,
    message,
  });
}

/**
 * The popup needs the settings to show them, and `takeAll` decides the badge.
 */
export async function updateBadge() {
  const on = settings.takeAll;
  await chrome.action.setBadgeText({ text: on ? "ON" : "" });
  await chrome.action.setBadgeBackgroundColor({ color: on ? "#2f6f4f" : "#8a6d3b" });
}

/**
 * Keeps `chrome.storage` and the in-memory settings in step when another tab changes them.
 */
export function watchSettings() {
  chrome.storage.onChanged.addListener((changes, area) => {
    if (area !== "local") { return; }
    if (changes[SETTINGS.takeAll] || changes[SETTINGS.minSizeKb] || changes[SETTINGS.skipExtensions]) {
      loadSettings().then(updateBadge);
    }
  });
}

/** Wires the one seam that intercepts downloads. */
export function installDownloadInterceptor() {
  if (!chrome.downloads?.onDeterminingFilename) {
    // A browser without it (any Gecko build, some Chromium forks) keeps its own downloads.
    return false;
  }
  chrome.downloads.onDeterminingFilename.addListener(onDeterminingFilename);
  return true;
}

/** Exported for the popup's "rescan" of the active tab's links. */
export async function pageLinks(tabId) {
  const results = await chrome.scripting.executeScript({
    target: { tabId },
    func: () => [...document.querySelectorAll("a[href]")]
      .map((a) => a.href)
      .filter((href) => /^https?:/i.test(href))
      .slice(0, 200),
  });
  return results?.[0]?.result ?? [];
}
