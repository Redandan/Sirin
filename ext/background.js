// Sirin Companion — service worker
//
// Pushes authoritative tab events to Sirin Rust core via WebSocket.  The
// chrome.tabs.* / chrome.webNavigation.* APIs are owned by the browser
// process and fire on every navigation (including about:blank reset, hash
// changes, history pushState) — which CDP-attach misses or delivers stale.
//
// Wire format (one JSON message per stdout line on the Rust side):
//   { type: "hello", version, chrome_version, ts }
//   { type: "tab",   event: "created"|"updated"|"removed"|"activated",
//                    tab_id, url?, title?, status?, ts }
//   { type: "nav",   event: "committed"|"history"|"dom_loaded",
//                    tab_id, frame_id, url, ts }
//   { type: "pong",  ts }       // reply to ping keep-alive
//
// Reconnect with exponential backoff up to 30s.  Service worker is kept
// awake by sending a ping every 20s (Manifest V3 idles SW at ~30s).

const SIRIN_PORTS = [7720, 7700, 7701, 7702, 7703, 7704, 7705];
const RECONNECT_MAX_MS = 30_000;
const PING_INTERVAL_MS = 20_000;

let ws = null;
let reconnectMs = 1_000;
let portIdx = 0;
let pingTimer = null;

function log(...args) { console.log("[sirin-ext]", ...args); }

function send(obj) {
  if (ws && ws.readyState === WebSocket.OPEN) {
    obj.ts = Date.now();
    try { ws.send(JSON.stringify(obj)); } catch (e) { log("send fail:", e); }
  }
}

async function connect() {
  const port = SIRIN_PORTS[portIdx % SIRIN_PORTS.length];
  const url = `ws://127.0.0.1:${port}/ext/ws`;
  log("connecting:", url);
  try {
    ws = new WebSocket(url);
  } catch (e) {
    log("ws ctor fail:", e);
    scheduleReconnect();
    return;
  }

  ws.onopen = async () => {
    log("connected on port", port);
    reconnectMs = 1_000;       // reset backoff
    portIdx = SIRIN_PORTS.indexOf(port);  // remember which one worked
    let chromeVersion = "unknown";
    try {
      const info = await chrome.runtime.getPlatformInfo();
      chromeVersion = `${navigator.userAgent}`;
    } catch (e) { /* ignore */ }
    send({ type: "hello", version: chrome.runtime.getManifest().version, chrome_version: chromeVersion });
    // Initial snapshot of all tabs so Sirin has full state on connect
    try {
      const tabs = await chrome.tabs.query({});
      for (const t of tabs) {
        send({ type: "tab", event: "snapshot", tab_id: t.id, url: t.url, title: t.title, status: t.status, active: t.active, window_id: t.windowId });
      }
    } catch (e) { log("initial snapshot fail:", e); }
    startPing();
  };

  ws.onmessage = (ev) => {
    try {
      const msg = JSON.parse(ev.data);
      if (msg.type === "ping") send({ type: "pong" });
      // Future: msg.type === "query" → respond with chrome.tabs.get(msg.tab_id)
    } catch (e) { /* ignore non-JSON */ }
  };

  ws.onerror = (e) => log("ws error", e?.message || e);
  ws.onclose = () => {
    log("disconnected");
    stopPing();
    portIdx = (portIdx + 1) % SIRIN_PORTS.length;  // rotate to next port
    scheduleReconnect();
  };
}

function scheduleReconnect() {
  setTimeout(connect, reconnectMs);
  reconnectMs = Math.min(reconnectMs * 2, RECONNECT_MAX_MS);
}

function startPing() {
  stopPing();
  pingTimer = setInterval(() => send({ type: "ping" }), PING_INTERVAL_MS);
}
function stopPing() {
  if (pingTimer) { clearInterval(pingTimer); pingTimer = null; }
}

// ── Tab lifecycle ──────────────────────────────────────────────────────────

chrome.tabs.onCreated.addListener((tab) => {
  send({ type: "tab", event: "created", tab_id: tab.id, url: tab.url, title: tab.title, window_id: tab.windowId });
});

chrome.tabs.onRemoved.addListener((tabId, removeInfo) => {
  send({ type: "tab", event: "removed", tab_id: tabId, window_id: removeInfo.windowId });
});

chrome.tabs.onActivated.addListener((info) => {
  send({ type: "tab", event: "activated", tab_id: info.tabId, window_id: info.windowId });
});

chrome.tabs.onUpdated.addListener((tabId, changeInfo, tab) => {
  // Only push when something agent cares about changed
  if (changeInfo.url || changeInfo.title || changeInfo.status) {
    send({
      type: "tab", event: "updated",
      tab_id: tabId,
      url: tab.url, title: tab.title, status: tab.status,
      change: changeInfo,
    });
  }
});

// ── Navigation events (more granular than tabs.onUpdated) ──────────────────

chrome.webNavigation.onCommitted.addListener((details) => {
  send({ type: "nav", event: "committed", tab_id: details.tabId, frame_id: details.frameId, url: details.url, transition: details.transitionType });
});

chrome.webNavigation.onHistoryStateUpdated.addListener((details) => {
  // SPA hash / pushState navigation — CDP misses many of these
  send({ type: "nav", event: "history", tab_id: details.tabId, frame_id: details.frameId, url: details.url });
});

chrome.webNavigation.onDOMContentLoaded.addListener((details) => {
  if (details.frameId === 0) {  // main frame only
    send({ type: "nav", event: "dom_loaded", tab_id: details.tabId, url: details.url });
  }
});

// ── Boot ───────────────────────────────────────────────────────────────────

connect();
