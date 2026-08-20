// The bridge between the local agent and this browser.
//
// The agent speaks the DevTools protocol; we forward each command to
// `chrome.debugger` against a real tab in the user's real profile, and stream
// protocol events back. This exists because Chrome 136+ refuses
// `--remote-debugging-port` on the default profile — the extension API is the
// only sanctioned way into the browser that holds the user's logins.
//
// A tab id doubles as the session id: the agent sends `sessionId: "4711"`, we
// route to tab 4711. That keeps one interface across both transports.

const RELAY_URL = "ws://127.0.0.1:8317";
const PROTOCOL_VERSION = "1.3";
const RECONNECT_MIN_MS = 1000;
const RECONNECT_MAX_MS = 30000;

let socket = null;
let authenticated = false;
let reconnectDelay = RECONNECT_MIN_MS;
const attached = new Set();

// MV3 service workers are evicted aggressively. An alarm wakes us up to
// re-establish the connection; without it the bridge silently dies after a few
// minutes idle.
chrome.alarms.create("keepalive", { periodInMinutes: 0.5 });
chrome.alarms.onAlarm.addListener(() => connect());
chrome.runtime.onStartup.addListener(() => connect());
chrome.runtime.onInstalled.addListener(() => connect());

async function token() {
  const stored = await chrome.storage.local.get("token");
  return stored.token || "";
}

async function connect() {
  if (socket && (socket.readyState === WebSocket.OPEN || socket.readyState === WebSocket.CONNECTING)) {
    return;
  }
  const pairingToken = await token();
  if (!pairingToken) {
    // Not paired yet; the options page tells the user what to do.
    return;
  }

  try {
    socket = new WebSocket(RELAY_URL);
  } catch (err) {
    scheduleReconnect();
    return;
  }
  authenticated = false;

  socket.onopen = () => {
    socket.send(JSON.stringify({ token: pairingToken }));
  };

  socket.onmessage = async (event) => {
    let message;
    try {
      message = JSON.parse(event.data);
    } catch {
      return;
    }

    // First reply is the handshake result.
    if (!authenticated) {
      if (message.ok === true) {
        authenticated = true;
        reconnectDelay = RECONNECT_MIN_MS;
        setBadge("on");
      } else {
        setBadge("!");
        socket.close();
      }
      return;
    }

    await handleCommand(message);
  };

  socket.onclose = () => {
    authenticated = false;
    setBadge("");
    scheduleReconnect();
  };
  socket.onerror = () => {
    try {
      socket.close();
    } catch {}
  };
}

function scheduleReconnect() {
  setTimeout(connect, reconnectDelay);
  reconnectDelay = Math.min(reconnectDelay * 2, RECONNECT_MAX_MS);
}

function send(payload) {
  if (socket && socket.readyState === WebSocket.OPEN) {
    socket.send(JSON.stringify(payload));
  }
}

function setBadge(text) {
  try {
    chrome.action.setBadgeText({ text });
  } catch {}
}

async function handleCommand({ id, method, params, sessionId }) {
  try {
    const result = method.startsWith("agent.")
      ? await control(method, params || {})
      : await forward(method, params || {}, sessionId);
    send({ id, result: result ?? {} });
  } catch (err) {
    send({ id, error: { message: String(err && err.message ? err.message : err) } });
  }
}

/// Operations that have no CDP equivalent at the browser level.
async function control(method, params) {
  switch (method) {
    case "agent.tabs": {
      const tabs = await chrome.tabs.query({});
      return {
        tabs: tabs
          // Chrome refuses to attach a debugger to its own pages.
          .filter((tab) => tab.url && !tab.url.startsWith("chrome://") && !tab.url.startsWith("edge://"))
          .map((tab) => ({
            id: String(tab.id),
            url: tab.url,
            title: tab.title,
            active: tab.active,
          })),
      };
    }

    case "agent.attach": {
      const tabId = Number(params.tabId);
      await attach(tabId);
      return { sessionId: String(tabId) };
    }

    case "agent.newTab": {
      // Opened in the background so the agent does not steal focus from the
      // person using the browser.
      const tab = await chrome.tabs.create({ url: params.url || "about:blank", active: false });
      await attach(tab.id);
      return { tabId: String(tab.id) };
    }

    case "agent.activate": {
      const tabId = Number(params.tabId);
      await chrome.tabs.update(tabId, { active: true });
      return {};
    }

    case "agent.detach": {
      const tabId = Number(params.tabId);
      if (attached.has(tabId)) {
        await chrome.debugger.detach({ tabId });
        attached.delete(tabId);
      }
      return {};
    }

    default:
      throw new Error(`unknown control method: ${method}`);
  }
}

async function attach(tabId) {
  if (attached.has(tabId)) return;
  await chrome.debugger.attach({ tabId }, PROTOCOL_VERSION);
  attached.add(tabId);
}

async function forward(method, params, sessionId) {
  const tabId = Number(sessionId);
  if (!Number.isFinite(tabId)) {
    throw new Error(`command ${method} needs a session (attach to a tab first)`);
  }
  await attach(tabId);
  return chrome.debugger.sendCommand({ tabId }, method, params);
}

// Protocol events flow back with the tab id as the session, mirroring what the
// agent sees from a directly-attached browser.
chrome.debugger.onEvent.addListener((source, method, params) => {
  send({ method, params, sessionId: String(source.tabId) });
});

chrome.debugger.onDetach.addListener((source) => {
  if (source.tabId !== undefined) attached.delete(source.tabId);
});

chrome.tabs.onRemoved.addListener((tabId) => {
  attached.delete(tabId);
});

// The options page asks for an immediate reconnect after the token changes,
// rather than waiting for the next keepalive alarm.
chrome.runtime.onMessage.addListener((message, _sender, respond) => {
  if (message && message.type === "reconnect") {
    try {
      if (socket) socket.close();
    } catch {}
    socket = null;
    connect();
    respond({ ok: true });
  }
  return true;
});

connect();
