/**
 * The popup. It reads state from the worker over a port, shows the pairing form when the
 * profile has no secret, and never touches a WebSocket itself.
 *
 * The code the user types is turned into a proof here and nowhere else: it signs the
 * desktop's challenge and is never stored, so a code that was read off the wrong screen
 * cannot outlive the pairing.
 */

import { MSG } from "./protocol.js";
import { DEFAULT_ADDRESS } from "./bridge.js";

const el = {
  status: document.getElementById("status"),
  offline: document.getElementById("offline"),
  address: document.getElementById("address"),
  reconnect: document.getElementById("reconnect"),
  pairError: document.getElementById("pair-error"),
  connected: document.getElementById("connected"),
  tasks: document.getElementById("tasks"),
  noTasks: document.getElementById("no-tasks"),
  takeAll: document.getElementById("take-all"),
  minSize: document.getElementById("min-size"),
  skip: document.getElementById("skip"),
  sendPage: document.getElementById("send-page"),
  reset: document.getElementById("reset"),
};

const port = chrome.runtime.connect({ name: "omalldownloader" });
let paired = false;
let address = "";

port.onMessage.addListener((message) => {
  switch (message.type) {
    case "state":
      paired = message.paired;
      address = message.address ?? "";
      paintStatus(message.connected, message.paired);
      paintSettings(message.settings);
      paintTasks(message.tasks ?? []);
      break;
    case "tasks":
      paintTasks(message.tasks ?? []);
      break;
    case "settings":
      paintSettings(message.settings);
      break;
    case "page-links":
      if (message.links?.length === 0) { showResult("No links on this page."); }
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

function paintStatus(connected, isPaired) {
  el.status.dataset.state = connected ? "connected" : "offline";
  // There is nothing to pair: the extension is either talking to the desktop or not.
  el.status.textContent = connected ? "Desktop connected" : "Desktop offline";
  el.offline.hidden = connected;
  el.connected.hidden = !isPaired;
  el.reset.hidden = !isPaired;
  el.address.value = address || DEFAULT_ADDRESS;
}

function paintSettings(settings) {
  if (!settings) { return; }
  el.takeAll.checked = Boolean(settings.takeAll);
  el.minSize.value = String(settings.minSizeKb ?? 0);
  el.skip.value = settings.skipExtensions ?? "";
}

function paintTasks(tasks) {
  el.tasks.replaceChildren();
  el.noTasks.hidden = tasks.length > 0;
  for (const task of tasks) {
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
    // The state is a word, not just a colour.
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

    item.append(head, bar, meta);
    if (task.error) {
      const error = document.createElement("p");
      error.className = "error";
      error.textContent = task.error;
      item.append(error);
    }
    el.tasks.append(item);
  }
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
  el.pairError.hidden = !text;
  el.pairError.textContent = text ?? "";
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

el.reset.addEventListener("click", async () => {
  await chrome.storage.local.set({ bridgeAddress: DEFAULT_ADDRESS, bridgeSecret: "" });
  portraitReset();
});

/** Forget the secret and start over; the desktop trusts a fresh extension again. */
function portraitReset() {
  paired = false;
  paintStatus(false, false);
  showResult("Forgotten. Reopen the popup to connect again.");
}


/**
 * Reconnect, and remember the address if the user changed the port.
 * There is no code: the desktop trusts a browser extension on first contact and hands it
 * a secret, which the worker stores for every later connection.
 */
el.reconnect.addEventListener("click", async () => {
  const address = el.address.value.trim() || DEFAULT_ADDRESS;
  await chrome.storage.local.set({ bridgeAddress: address });
  showResult("Connecting…");
  chrome.runtime.sendMessage({ type: "reconnect" }).catch(() => {});
});

el.address.value = DEFAULT_ADDRESS;
