const input = document.getElementById("token");
const status = document.getElementById("status");

chrome.storage.local.get("token").then(({ token }) => {
  if (token) input.value = token;
  report();
});

document.getElementById("save").addEventListener("click", async () => {
  await chrome.storage.local.set({ token: input.value.trim() });
  // Wake the service worker so it reconnects immediately rather than waiting
  // for the next keepalive alarm.
  await chrome.runtime.sendMessage({ type: "reconnect" }).catch(() => {});
  status.textContent = "Saved. Connecting…";
  status.className = "";
  setTimeout(report, 1200);
});

async function report() {
  const badge = await chrome.action.getBadgeText({}).catch(() => "");
  if (badge === "on") {
    status.textContent = "● Connected to the agent";
    status.className = "ok";
  } else if (!input.value.trim()) {
    status.textContent = "Not paired yet";
    status.className = "warn";
  } else {
    status.textContent = "○ Not connected — is the agent running?";
    status.className = "warn";
  }
}
