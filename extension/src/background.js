/**
 * The service worker: connects to the desktop, intercepts downloads, keeps the badge
 * honest, and answers the popup.
 *
 * It holds no secret of its own. The secret lives in `chrome.storage.local` and is used
 * only to sign the desktop's one-time challenge (see `bridge.js`).
 */

import { MSG } from "./protocol.js";
import { desktopBridge, taskState } from "./background-state.js";
import { DEFAULT_ADDRESS } from "./bridge.js";
import {
  currentSettings,
  fetchTasks,
  installDownloadInterceptor,
  loadSettings,
  pageLinks,
  saveSettings,
  takeLink,
  updateBadge,
  watchSettings,
} from "./downloads.js";

/** Maps a task's progress to the action badge: a percentage, or a dot while unknown. */
function paintBadge(tasks) {
  taskState.tasks = tasks;
  const running = tasks.filter((task) => task.status === "running");
  if (running.length === 0) {
    updateBadge();
    return;
  }
  const known = running.filter((task) => task.total_bytes > 0);
  const total = known.reduce((sum, task) => sum + task.total_bytes, 0);
  const done = known.reduce((sum, task) => sum + task.received_bytes, 0);
  const text = total > 0 ? `${Math.floor((done / total) * 100)}%` : "…";
  chrome.action.setBadgeText({ text });
  chrome.action.setBadgeBackgroundColor({ color: "#3b6ea5" });
}

async function start() {
  // Connect before anything else. A badge, a setting or an interceptor that fails must not
  // leave the app unreachable: that failure looked exactly like "the desktop is not
  // answering", which is the one message a user cannot act on.
  try {
    await desktopBridge.load();
  } catch (error) {
    globalThis.__startError = `load: ${error}`;
    desktopBridge.address = desktopBridge.address || DEFAULT_ADDRESS;
  }
  attachBridgeListeners();
  desktopBridge.connect();
  fetchTasks();

  for (const [what, step] of [
    ["settings", loadSettings],
    ["badge", updateBadge],
    ["interceptor", installDownloadInterceptor],
  ]) {
    try {
      await step();
    } catch (error) {
      globalThis.__startError = `${what}: ${error}`;
      console.warn(`omad: ${what} failed`, error);
    }
  }
  try { watchSettings(); } catch (error) { globalThis.__startError = `watch: ${error}`; }
}

/// The bridge events. Kept apart so `start` can attach them before connecting.
function attachBridgeListeners() {

  desktopBridge.addEventListener("state", (event) => {
    const detail = event.detail;
    taskState.connected = detail.connected;
    taskState.paired = detail.paired;
    taskState.address = detail.address;
    // A popup that opened before the connection was up would otherwise stay on "Offline"
    // for as long as it is open: this is the message that turns it back.
    broadcast({
      type: "state",
      connected: detail.connected,
      paired: detail.paired,
      address: detail.address,
      reason: detail.reason,
      tasks: taskState.tasks,
    });
    if (!detail.connected) { fetchTasks(); }
  });
  desktopBridge.addEventListener("tasks", (event) => {
    const message = event.detail;
    if (message.type === MSG.TASKS) {
      paintBadge(message.tasks ?? []);
      broadcast({ type: "tasks", tasks: taskState.tasks });
    }
    if (message.type === MSG.TASK && message.task) {
      const tasks = taskState.tasks.filter((task) => task.id !== message.task.id);
      tasks.push(message.task);
      paintBadge(tasks);
    }
    if (message.type === MSG.REMOVED) {
      paintBadge(taskState.tasks.filter((task) => task.id !== message.id));
    }
  });

}

start();

// Read-only handle for the end-to-end test (`tests/e2e/connect.test.mjs`).
globalThis.desktopBridgeForTest = desktopBridge;

// A hook the end-to-end test uses to pair without a popup: it writes the same two storage
// keys the popup writes and asks the bridge to connect now. It is inert otherwise.
globalThis.__gfPairForTest = async (address, secret) => {
  await desktopBridge.pair(address, secret);
  desktopBridge.connect();
  return desktopBridge.isPaired;
};

// The context menu: "Download with OmaLLDownloader" on a link, a page, or a selection.
chrome.runtime.onInstalled.addListener(() => {
  chrome.contextMenus.removeAll(() => {
    chrome.contextMenus.create({
      id: "gf-link",
      title: "Download with OmaLLDownloader",
      contexts: ["link"],
    });
    chrome.contextMenus.create({
      id: "gf-page",
      title: "Download this page with OmaLLDownloader",
      contexts: ["page"],
    });
  });
});

chrome.contextMenus.onClicked.addListener(async (info, tab) => {
  const url = info.linkUrl || info.pageUrl;
  if (!url) { return; }
  await takeLink({ url, referer: tab?.url });
});

// The popup and the options page talk to the worker over one long-lived port, so a
// popup that closes does not cancel what it asked for.
// One place that knows about open popups, so a task change reaches every one of them.
const popups = new Set();
function broadcast(message) {
  for (const popup of popups) {
    try { popup.postMessage(message); } catch { popups.delete(popup); }
  }
}

chrome.runtime.onConnect.addListener((port) => {
  if (port.name !== "omalldownloader" && port.name !== "ghost-flow") { return; }
  popups.add(port);
  port.onDisconnect.addListener(() => popups.delete(port));

  port.onMessage.addListener(async (request) => {
    switch (request?.type) {
      case "state":
        port.postMessage({
          type: "state",
          connected: taskState.connected,
          paired: taskState.paired,
          address: taskState.address,
          tasks: taskState.tasks,
          settings: currentSettings(),
        });
        break;
      case "tasks":
        fetchTasks();
        port.postMessage({ type: "tasks", tasks: taskState.tasks });
        break;
      case "set-settings":
        await saveSettings(request.settings ?? {});
        await updateBadge();
        port.postMessage({ type: "settings", settings: currentSettings() });
        break;
      case "take-link": {
        const result = await takeLink(request.task ?? {});
        port.postMessage({ type: "result", ...result });
        break;
      }
      case "page-links": {
        const [tab] = await chrome.tabs.query({ active: true, currentWindow: true });
        const links = tab?.id ? await pageLinks(tab.id) : [];
        port.postMessage({ type: "page-links", links });
        break;
      }
      case "control":
        // A button in the popup: forward the command the user asked for.
        desktopBridge.send(request.message);
        break;
      case "reconnect":
        desktopBridge.close();
        desktopBridge.connect();
        port.postMessage({ type: "state", connected: desktopBridge.ready, address: desktopBridge.address });
        break;
      default:
        port.postMessage({ type: "error", message: "unknown request" });
    }
  });
});

// The popup asks for a reconnect after the user changes the port.
chrome.runtime.onMessage.addListener((message, _sender, sendResponse) => {
  if (message?.type === "use-http") {
    desktopBridge.enterHttpMode("the popup asked for the HTTP transport").catch(() => {});
    sendResponse({ ok: true });
    return false;
  }
  if (message?.type === "reconnect") {
    desktopBridge.close();
    desktopBridge.connect();
    sendResponse({ ok: true });
  }
  return false;
});
