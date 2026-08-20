# ax Browser Bridge

Lets `ax` drive **your** browser — the one already signed in to your accounts —
instead of a blank profile with no logins.

## Why an extension is needed

Chrome used to allow `--remote-debugging-port` against any profile. Since
Chrome 136 it refuses that flag on the default profile, precisely because that
profile holds your cookies and sessions. The `chrome.debugger` extension API is
the sanctioned way in, so this extension relays the DevTools protocol between
Chrome and the `ax` process on your machine.

## Install

```bash
ax pair
```

That prints your pairing token and waits for the extension to connect. While it
waits:

1. Open `chrome://extensions`
2. Turn on **Developer mode** (top right)
3. **Load unpacked** → choose `~/.ax/extension`
4. Click the ax icon in the toolbar, paste the token, **Save & connect**

`ax pair` prints `connected` once it works. Chrome shows a
"…is debugging this browser" banner whenever ax is driving a tab — that is the
extension working, and it is worth leaving visible so you can always see when
the agent is acting.

Works in any Chromium browser with `chrome.debugger`: Chrome, Edge, Brave,
Arc, Chromium.

## What it can do

Once paired, ax can list your tabs, open new ones, read pages, and click and
type in them. It attaches its debugger only to tabs it is actually driving.

New tabs open in the background so the agent does not steal focus while you are
using the browser.

## Security

The relay listens on `127.0.0.1:8317` only, and refuses any connection that
does not present your pairing token **and** originate from a
`chrome-extension://` origin. The token is 256 bits from `/dev/urandom`, stored
at `~/.ax/relay-token` with mode 600.

Treat the token like a password: anything holding it can drive a browser that is
logged in as you. If you think it leaked, delete `~/.ax/relay-token`, run
`ax pair` again, and paste the new token into the extension.

## Troubleshooting

**`ax pair` times out.** Check `chrome://extensions` shows the extension enabled
with no errors, and that the token matches exactly.

**It worked, then stopped.** Chrome suspends extension service workers when
idle; this one wakes on an alarm and reconnects with backoff. Give it about
thirty seconds, or open the extension popup to force a reconnect.

**"is debugging this browser" will not go away.** Close the tab ax was driving,
or disable the extension. It appears whenever a debugger is attached.

**No extension at all.** ax falls back to a browser it launches itself, on a
profile under `~/.ax/chrome`. Everything works there except your logins — you
can sign in inside that window once and it persists.
