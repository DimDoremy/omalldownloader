/**
 * One bridge per service worker, imported by the files that need it.
 *
 * Extension modules are cached, so importing this from two places yields the same
 * instance — and a second `WebSocket` per worker would get a second challenge and a
 * second task list.
 */

import { DesktopBridge } from "./bridge.js";

export const desktopBridge = new DesktopBridge();

/** The latest task list, for the popup to read when it opens. */
export const taskState = {
  /** @type {import("./protocol.js").TaskView[]} */
  tasks: [],
  connected: false,
  paired: false,
  address: "",
};
