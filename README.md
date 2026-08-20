# ax

An autonomous, goal-oriented agent that acts by writing and running code.

One binary. One tool. The model never gets `read_file` or `bash` — it gets a
persistent JavaScript isolate with a small host API, and everything else
(shell, files, network, browser) is reached *through* code it writes.

## Install

```bash
curl -fsSL https://raw.githubusercontent.com/cloudycotton/ax/main/install.sh | sh
```

Then run `ax`. The first run asks for your endpoint, API key, and model, checks
they work with one real request, and saves them to `~/.ax/env` (mode 600).

```bash
ax                    # first run: setup. after that: status
ax run "keep the test suite green and open a PR for each fix"
ax ls                 # what is running
ax attach <id>        # replay everything it has done, then follow live
ax update             # upgrade in place from the latest release
```

## Providers and models

Setup asks where to send requests, takes your key, then lists the models the
provider actually serves — you pick from that list rather than typing an id
from memory. Every answer is written to `~/.ax/config.toml` (mode 600) the
moment you give it, so a wizard you abandon halfway loses nothing.

Keep as many providers as you like — a paid one for real work, a local model
for cheap iteration — and switch between them:

```bash
ax model              # pick a model from what this provider offers
ax provider           # switch between saved providers, or add one
ax config             # what is configured (keys redacted)
ax setup              # add or reconfigure a provider
```

`ax model gpt-4.1` and `ax provider local` set things directly, without the
picker. Environment variables still win over the saved config, which is what
lets a one-off `AX_MODEL=o3 ax run "…"` work.


Sessions belong to a background daemon, not to your terminal: close the window
and the goal keeps running. Attaching replays the whole history and then tails
it, the way reconnecting to a tmux pane does.

## Why this shape

**Code as the only tool.** A persistent isolate means state accumulates:
helpers the model writes, data it collects, and process handles it holds all
survive from one call to the next. "Check 200 things and retry the failures" is
one tool call with a loop, not 200 model turns.

**Wakes, not conversations.** The agent is not in a chat. Something wakes it —
a timer it set for itself, a background process exiting — it takes one high-value
action, updates its ledger, and schedules its next wake. Between wakes it sleeps
and costs nothing.

**Each wake starts from a fresh context.** System prompt, plus a wake message
carrying the ledger, the live isolate manifest, and why it woke. Nothing else
carries over. Cost per wake stays flat no matter how long a session runs, and
the ledger is forced to stay good enough to work from — which is exactly the
discipline a week-long goal needs.

**The log is not the context.** `events.jsonl` is the complete history: every
wake, every block of code, every line printed, forever. The model's context is a
small subset. That separation is what lets you attach to a session that has been
running for days and read the whole story.

## Layout

| file | role |
|---|---|
| `src/isolate.rs` | QuickJS context on its own thread; the only way the model acts |
| `src/prelude.js` | The API the model actually calls, in JavaScript so it can be read and extended at runtime |
| `src/host.rs` | Host capabilities: shell, files, network, background processes |
| `src/cdp.rs` | DevTools protocol transport — moves messages, nothing more |
| `src/relay.rs` | The authenticated socket the browser extension connects to |
| `src/chrome.rs` | Picks and establishes a browser connection |
| `extension/` | MV3 extension that bridges `chrome.debugger` to the relay |
| `src/agent.rs` | The wake loop |
| `src/daemon.rs` | The supervisor: owns running sessions, serves attach clients |
| `src/ipc.rs` | The CLI ↔ daemon protocol |
| `src/launchd.rs` | Registering the daemon as a login agent |
| `src/llm.rs` | OpenAI chat-completions client (streaming, retries, weak-model fallback) |
| `src/event.rs` | The durable append-only session log |
| `src/prompt.rs` | System prompt and per-wake message |

Session state lives in `~/.ax/sessions/<id>/`: `events.jsonl`, `ledger.md`,
`meta.json`, `schedule.json`, `memory/`, `artifacts/`.

## The daemon

`ax install` registers a launchd agent that starts at login and keeps running.
`ax run` hands it the session — and starts one on demand if it is not up yet —
so nothing depends on the terminal staying open.

launchd gives the daemon almost no environment, so credentials cannot come from
your shell profile. `ax install` captures them into `~/.ax/env` at mode 600,
which also keeps the API key out of the plist, where it would sit
world-readable.

A restarted daemon resumes any session that was still in flight. The isolate
does not survive — that is exactly why the ledger and the files on disk are the
durable state — but `schedule.json` does, so a session told to check back in six
hours does not lose those six hours to a reboot. The model is told it restarted
and reads its ledger to work out where it was.

## Design notes

- **QuickJS, not V8.** Cold start from process launch to running JavaScript is
  ~30ms and the binary is ~11MB. Heavy compute is delegated to subprocesses
  anyway, so the JIT would buy nothing.
- **The isolate is a composition layer, not a sandbox.** The agent is meant to
  reach the whole machine. Safety comes from bounded instruments (spend limits
  on the card, scoped API keys, token budgets), not from the JS engine.
- **Thin Rust, thick JavaScript.** Rust exposes ~12 low-level bindings that
  speak JSON strings. Everything ergonomic lives in `prelude.js`, so it
  iterates without recompiling and the model can inspect it.
- **Chat completions as the wire format.** Stateless and universally spoken, so
  swapping providers is two environment variables. Models with unreliable
  function-calling still work: a fenced ```js block in prose is executed as if
  it had been a proper tool call.
- **Guards live in the harness.** Runaway JS loops die on a QuickJS interrupt
  handler at the deadline; a minimum wake interval stops a confused model from
  self-waking in a tight loop; per-wake turn caps and token budgets bound cost.

## Browser

The agent drives a real Chrome. Two transports, one interface:

- **The user's own browser**, through the extension in `extension/`. This is the
  one that matters — it has their sessions and logins. Chrome 136+ refuses
  `--remote-debugging-port` on the default profile, so `chrome.debugger` inside
  an extension is the only sanctioned way in. Run `ax pair` for setup.
- **A browser the agent launches**, on its own profile under `~/.ax/chrome`.
  No install required, but signed into nothing until someone signs in.

Reading a page is designed for models without vision:

```js
const page = await browser.open("https://example.com");
await page.snapshot({ interactiveOnly: true });
//   textbox "Username" [r1]
//   textbox "Password" [r2]
//   button  "Sign in"  [r3]
await page.fill("r1", "…"); await page.click("r3");
```

Clicks and keystrokes go through `Input.dispatchMouseEvent` rather than
synthetic DOM events, so pages see them as trusted — plenty of real sites
ignore anything else.

The relay socket can drive a logged-in browser, so it is guarded three ways:
loopback-only binding, a 256-bit pairing token from `/dev/urandom`, and an
`Origin` check that only accepts `chrome-extension://`.

## Status

Working end to end, covered by `cargo test` (no credentials or network needed):
the isolate and its API, the wake loop, the daemon and its attach protocol,
restart recovery, the durable event log and replay, the session CLI, browser
control over CDP, and the relay's access checks.

Not built yet:

1. **Notification sinks** — desktop and webhook, so `notify()` reaches a human
   who is not watching a terminal. `notify()` currently only reaches the log.
2. **In-wake compaction** — a very long single wake can still outgrow the
   context window; wake-to-wake cost is already flat.
3. **Bounded spend** — the design calls for a virtual card with issuer-level
   limits so the agent can transact unattended inside a hard envelope. Nothing
   here implements that yet.
