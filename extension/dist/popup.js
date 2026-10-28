/**
 * The popup: the download list, and what to do when the desktop is not there.
 *
 * It owns no socket. Everything goes through the service worker over a port, so closing the
 * popup never cancels a request and the worker keeps the connection.
 */

import { MSG } from "./protocol.js";
import { DEFAULT_ADDRESS } from "./bridge.js";

const el = {
  status: document.getElementById("status"),
  offline: document.getElementById("offline"),
  reason: document.getElementById("reason"),
  address: document.getElementById("address"),
  reconnect: document.getElementById("reconnect"),
  openApp: document.getElementById("open-app"),
  listError: document.getElementById("pair-error"),
  connected: document.getElementById("connected"),
  summary: document.getElementById("summary"),
  tasks: document.getElementById("tasks"),
  noTasks: document.getElementById("no-tasks"),
  takeAll: document.getElementById("take-all"),
  minSize: document.getElementById("min-size"),
  skip: document.getElementById("skip"),
  sendPage: document.getElementById("send-page"),
  reset: document.getElementById("reset"),
};

const port = chrome.runtime.connect({ name: "omalldownloader" });
let connected = false;
let tasks = [];

port.onMessage.addListener((message) => {
  switch (message.type) {
    case "state":
      connected = Boolean(message.connected);
      paintStatus(connected, message.reason);
      paintSettings(message.settings);
      if (Array.isArray(message.tasks)) { paintTasks(message.tasks); }
      el.address.value = message.address || DEFAULT_ADDRESS;
      break;
    case "tasks":
      paintTasks(message.tasks ?? []);
      break;
    case "settings":
      paintSettings(message.settings);
      break;
    case "result":
      showResult(message.message);
      break;
    case "error":
      showResult(message.message);
      break;
    default:
      break;
  }
});

port.postMessage({ type: "state" });
// The worker may still be dialling; ask again shortly rather than showing a stale Offline.
for (const delay of [400, 1200, 3000]) {
  setTimeout(() => port.postMessage({ type: "state" }), delay);
}

function paintStatus(isConnected, reason) {
  el.status.dataset.state = isConnected ? "connected" : "offline";
  el.status.textContent = isConnected ? "Connected" : "Offline";
  el.offline.hidden = isConnected;
  el.connected.hidden = !isConnected;
  el.reason.textContent = isConnected ? "" : (reason || "The desktop app is not answering.");
}

function paintSettings(settings) {
  if (!settings) { return; }
  el.takeAll.checked = Boolean(settings.takeAll);
  el.minSize.value = String(settings.minSizeKb ?? 0);
  el.skip.value = settings.skipExtensions ?? "";
}

function paintTasks(next) {
  tasks = next;
  el.tasks.replaceChildren();
  el.noTasks.hidden = tasks.length > 0;

  const running = tasks.filter((task) => task.status === "running");
  const speed = running.reduce((sum, task) => sum + (Number(task.speed) || 0), 0);
  el.summary.textContent = running.length > 0
    ? `${running.length} downloading · ${bytes(speed)}/s`
    : `${tasks.length} task${tasks.length === 1 ? "" : "s"}`;

  for (const task of tasks) {
    el.tasks.append(taskRow(task));
  }
}

function taskRow(task) {
  const item = document.createElement("li");
  item.className = "task";

  const head = document.createElement("div");
  head.className = "task-head";
  const name = document.createElement("span");
  name.className = "task-name";
  name.textContent = task.filename || task.url;
  name.title = task.url;
  const state = document.createElement("span");
  state.className = "task-state";
  state.dataset.state = task.status;
  state.textContent = label(task.status);
  head.append(name, state);

  const bar = document.createElement("div");
  bar.className = "bar";
  const fill = document.createElement("span");
  const fraction = task.total_bytes > 0
    ? Math.min(1, task.received_bytes / task.total_bytes)
    : 0;
  fill.style.width = `${(fraction * 100).toFixed(1)}%`;
  bar.append(fill);

  const meta = document.createElement("div");
  meta.className = "task-meta";
  const left = document.createElement("span");
  left.textContent = task.total_bytes > 0
    ? `${bytes(task.received_bytes)} / ${bytes(task.total_bytes)}`
    : bytes(task.received_bytes);
  const right = document.createElement("span");
  right.textContent = task.status === "running" && task.speed > 0 ? `${bytes(task.speed)}/s` : "";
  meta.append(left, right);

  const actions = document.createElement("div");
  actions.className = "task-actions";
  if (task.status === "running" || task.status === "queued") {
    actions.append(action("Pause", () => command({ type: MSG.PAUSE, id: task.id })));
  }
  if (task.status === "paused" || task.status === "failed") {
    actions.append(action("Resume", () => command({ type: MSG.RESUME, id: task.id })));
  }
  actions.append(action("Remove", () => command({ type: MSG.REMOVE, id: task.id })));

  item.append(head, bar, meta, actions);
  if (task.error) {
    const error = document.createElement("p");
    error.className = "error";
    error.textContent = task.error;
    item.append(error);
  }
  return item;
}

function action(text, onClick) {
  const button = document.createElement("button");
  button.type = "button";
  button.className = "link";
  button.textContent = text;
  button.addEventListener("click", onClick);
  return button;
}

function command(message) {
  port.postMessage({ type: "control", message });
}

function label(status) {
  return {
    queued: "Queued",
    running: "Downloading",
    paused: "Paused",
    completed: "Done",
    failed: "Failed",
  }[status] ?? status;
}

function bytes(value) {
  const units = ["B", "KB", "MB", "GB", "TB"];
  let amount = Number(value) || 0;
  let unit = 0;
  while (amount >= 1024 && unit < units.length - 1) {
    amount /= 1024;
    unit += 1;
  }
  return unit === 0 ? `${Math.round(amount)} B` : `${amount.toFixed(1)} ${units[unit]}`;
}

function showResult(text) {
  el.listError.hidden = !text;
  el.listError.textContent = text ?? "";
}

async function sendSettings() {
  port.postMessage({
    type: "set-settings",
    settings: {
      takeAll: el.takeAll.checked,
      minSizeKb: Number(el.minSize.value) || 0,
      skipExtensions: el.skip.value,
    },
  });
}

el.takeAll.addEventListener("change", sendSettings);
el.minSize.addEventListener("change", sendSettings);
el.skip.addEventListener("change", sendSettings);

el.sendPage.addEventListener("click", async () => {
  const [tab] = await chrome.tabs.query({ active: true, currentWindow: true });
  if (!tab?.url) { return; }
  port.postMessage({ type: "take-link", task: { url: tab.url, referer: tab.url } });
});

el.openApp.addEventListener("click", () => {
  chrome.tabs.create({ url: "omalldownloader://open" }).catch(() => {});
});

el.reconnect.addEventListener("click", async () => {
  const address = el.address.value.trim() || DEFAULT_ADDRESS;
  await chrome.storage.local.set({ bridgeAddress: address });
  showResult("Connecting…");
  chrome.runtime.sendMessage({ type: "reconnect" }).catch(() => {});
});

el.reset.addEventListener("click", async () => {
  await chrome.storage.local.set({ bridgeAddress: DEFAULT_ADDRESS, bridgeSecret: "" });
  connected = false;
  paintStatus(false, "Forgotten this browser; reconnecting…");
  chrome.runtime.sendMessage({ type: "reconnect" }).catch(() => {});
});

el.address.value = DEFAULT_ADDRESS;
