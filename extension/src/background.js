/**
 * The service worker: connects to the desktop, intercepts downloads, keeps the badge
 * honest, and answers the popup.
 *
 * It holds no secret of its own. The secret lives in `chrome.storage.local` and is used
 * only to sign the desktop's one-time challenge (see `bridge.js`).
 */

import { MSG } from "./protocol.js";
import { desktopBridge, taskState } from "./background-state.js";
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
  await desktopBridge.load();
  await loadSettings();
  await updateBadge();
  watchSettings();
  installDownloadInterceptor();

  desktopBridge.addEventListener("state", (event) => {
    taskState.connected = event.detail.connected;
    taskState.paired = event.detail.paired;
    taskState.address = event.detail.address;
    if (!event.detail.connected) { fetchTasks(); }
  });
  desktopBridge.addEventListener("tasks", (event) => {
    const message = event.detail;
    if (message.type === MSG.TASKS) { paintBadge(message.tasks ?? []); }
    if (message.type === MSG.TASK && message.task) {
      const tasks = taskState.tasks.filter((task) => task.id !== message.task.id);
      tasks.push(message.task);
      paintBadge(tasks);
    }
    if (message.type === MSG.REMOVED) {
      paintBadge(taskState.tasks.filter((task) => task.id !== message.id));
    }
  });

  desktopBridge.connect();
  fetchTasks();
}

start();

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
chrome.runtime.onConnect.addListener((port) => {
  if (port.name !== "omalldownloader") { return; }

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
      case "pair":
        port.postMessage({ type: "pair-required" });
        break;
      default:
        port.postMessage({ type: "error", message: "unknown request" });
    }
  });
});
