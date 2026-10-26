/**
 * The popup. It reads state from the worker over a port, shows the pairing form when the
 * profile has no secret, and never touches a WebSocket itself.
 *
 * The code the user types is turned into a proof here and nowhere else: it signs the
 * desktop's challenge and is never stored, so a code that was read off the wrong screen
 * cannot outlive the pairing.
 */

import { MSG } from "./protocol.js";
import { hmacHex } from "./protocol.js";

const el = {
  status: document.getElementById("status"),
  pairing: document.getElementById("pairing"),
  address: document.getElementById("address"),
  code: document.getElementById("code"),
  pair: document.getElementById("pair"),
  pairError: document.getElementById("pair-error"),
  connected: document.getElementById("connected"),
  tasks: document.getElementById("tasks"),
  noTasks: document.getElementById("no-tasks"),
  takeAll: document.getElementById("take-all"),
  minSize: document.getElementById("min-size"),
  skip: document.getElementById("skip"),
  sendPage: document.getElementById("send-page"),
  unpair: document.getElementById("unpair"),
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
  el.status.textContent = connected ? "Desktop connected" : isPaired ? "Desktop offline" : "Not paired";
  el.pairing.hidden = isPaired;
  el.connected.hidden = !isPaired;
  el.unpair.hidden = !isPaired;
  if (isPaired && address) { el.address.value = address; }
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

el.unpair.addEventListener("click", async () => {
  await chrome.storage.local.set({ bridgeAddress: "", bridgeSecret: "" });
  paired = false;
  paintStatus(false, false);
  showResult("Unpaired. Restart the desktop app to pair again.");
});

/**
 * Pairing, entirely through the worker: the popup signs the desktop's challenge and hands
 * the derived secret to the worker, which stores it. The *code* itself is not stored.
 */
el.pair.addEventListener("click", async () => {
  const candidate = el.address.value.trim();
  const code = el.code.value.trim();
  if (!candidate || !/^\d{6}$/.test(code)) {
    showResult("Type the address and the six digit code from the desktop.");
    return;
  }
  el.pair.disabled = true;
  showResult("");
  try {
    const url = /^wss?:\/\//i.test(candidate) ? candidate : `ws://${candidate}`;
    const socket = new WebSocket(url);
    const outcome = await new Promise((resolve, reject) => {
      const timer = setTimeout(() => reject(new Error("The desktop did not answer.")), 10000);
      socket.addEventListener("message", async (event) => {
        const message = JSON.parse(String(event.data));
        if (message.type !== MSG.CHALLENGE) { return; }
        socket.send(JSON.stringify({
          type: MSG.PAIR,
          origin: chrome.runtime.getURL("").replace(/\/$/, ""),
          code_proof: await hmacHex(code, message.nonce),
        }));
      });
      socket.addEventListener("message", (event) => {
        const message = JSON.parse(String(event.data));
        if (message.type === MSG.PAIRED) { clearTimeout(timer); resolve(message.secret); }
        if (message.type === MSG.ERROR) { clearTimeout(timer); reject(new Error(message.message)); }
      });
      socket.addEventListener("error", () => {
        clearTimeout(timer);
        reject(new Error("Could not reach the desktop. Is OmaLLDownloader running?"));
      });
    });
    socket.close();
    await chrome.storage.local.set({ bridgeAddress: url, bridgeSecret: outcome });
    paired = true;
    paintStatus(true, true);
    showResult("Paired. Restarting the connection…");
    chrome.runtime.sendMessage({ type: "reconnect" }).catch(() => {});
  } catch (error) {
    showResult(error instanceof Error ? error.message : String(error));
  } finally {
    el.pair.disabled = false;
  }
});
