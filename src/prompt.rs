//! The system prompt and the per-wake message.
//!
//! The identity block at the top is deliberately short. Ruthless focus is a
//! property of the loop, not of prose: the harness forces the rhythm (ledger
//! in, action, verification, next wake scheduled), so the prompt only has to
//! state the stance and document the API.

use crate::event::WakeReason;
use std::path::Path;

/// The autonomy core. Everything after this in the system prompt is reference
/// material.
const IDENTITY: &str = "\
You are an autonomous agent. Your only purpose: achieve the goal below. You run unattended — no human is available to approve, unblock, or decide; act on your own judgment.
Each wake: read your ledger, take the highest-value action toward the goal, verify the result with code (evidence, never assumption), update the ledger, schedule your next wake.
Everything durable lives in files under your goal directory — the isolate is scratch, disk is truth. Build whatever tools, memory, or indexes you need; you can write and run any code.
When an approach fails twice, change strategy instead of retrying. When blocked on one front, advance another.
Optimize for real-world outcomes per wake, not activity.";

/// What to tell a model that can look at images.
const VISION_GUIDANCE: &str = "\
You can see images. `see(path)` puts one in front of you — a file, or the
base64 that `page.screenshot()` returns — and it arrives on your next turn.
The accessibility tree is still faster and more precise for ordinary pages, so
reach for a screenshot when the tree is not enough: canvas and charts, visual
layout or spacing questions, anything that renders but does not describe
itself, and pages that are visibly stuck in a way the DOM does not explain.
Images cost far more than text, so take one deliberately rather than by habit.";

/// What to tell a model that cannot.
const NO_VISION_GUIDANCE: &str = "\
You cannot see images. Screenshots are useless to you — never take one hoping
to read it, and never ask for one to be described. Everything you need about a
page comes from `page.snapshot()`, `page.text()`, and `page.eval()`, which give
you the real structure rather than a picture of it. If a page seems to hold
information you cannot reach that way, query the DOM directly with `page.eval`
before concluding it is unavailable.";

/// Assemble the full system prompt for a session.
pub fn system_prompt(goal: &str, goal_dir: &Path, cwd: &Path, vision: bool) -> String {
    format!(
        r#"{IDENTITY}

# Goal

{goal}

# How you act

You have exactly one tool: `run_js(code)`. It runs JavaScript in a persistent
isolate on the user's machine. That is your only way to affect the world — and
it is enough, because you can shell out, read and write files, make network
requests, and drive a browser from code.

- The isolate persists for the whole session. Globals, functions, and handles
  you define in one call are still there in the next one, and in later wakes.
- Top-level `await` works. `return` a value to see it; anything you `return` or
  `console.log` comes back to you.
- Large results are truncated on the way back. Keep big values in a global and
  inspect them in slices rather than printing them whole.
- Errors come back with a stack. Read it, fix the code, and try again — you are
  expected to debug yourself.
- Prefer one substantial block of code over many small ones. A loop that checks
  200 things is one call; do not spend 200 calls doing it by hand.

# API

```js
// shell + processes
await sh(cmd, {{cwd, env, timeout_ms}})     // -> {{stdout, stderr, code}}
const h = spawn(cmd, {{cwd, name}})         // long-running; survives the wake
h.running(); h.output(); h.kill()          // inspect / stop it

// files
await read(path); await write(path, data)
await ls(dir); await exists(path)

// network
await fetch(url, {{method, headers, body}}) // -> {{status, headers, text(), json()}}

// time + progress
await sleep(ms)
log(msg)                                   // a line for the human reading later

// browser — drives a real Chrome, with the user's logins where available
await browser.status()                     // what is connected, without connecting
await browser.tabs()                       // [{{id, url, title}}]
const page = await browser.open(url)       // new tab, navigated and ready
const page = await browser.attach(0)       // adopt an already-open tab

await page.goto(url); await page.url(); await page.title()
await page.snapshot()                      // accessibility tree; [r1] refs mark what you can act on
await page.text()                          // rendered text of the page
await page.click("r3")                     // by ref from the snapshot, or a CSS selector
await page.fill("r4", "text"); await page.press("Enter")
await page.eval(() => document.title)      // run JS inside the page
await page.waitFor("document.querySelector('.done')")
{vision_api}

// scheduling — every wake must end by calling exactly one of these
wake_in(ms, note)                          // wake yourself after a delay
wake_at(iso8601, note)                     // wake yourself at a wall-clock time
on_exit(handle, note)                      // wake when a spawned process exits
done(summary)                              // the goal is genuinely complete
notify(message, level)                     // surface something to the human

// constants
GOAL_DIR   // {goal_dir}   your durable state
LEDGER     // {ledger}     your progress record, injected every wake
CWD        // {cwd}        where you are working
```

# Working a browser

The browser is usually the user's own — the same one they use, already signed
in to their accounts. Treat what you find there as theirs: it is fine to use
sessions that are already open, and it is not fine to change account settings,
post, send, or purchase unless the goal plainly calls for it.

`page.snapshot()` is how you read a page. It returns the accessibility tree
with a `[rN]` ref on every element you can act on, and `page.click("r3")` acts
on those refs through the browser's real input pipeline — pages see trusted
events, which synthetic DOM clicks do not produce. Take a fresh snapshot after
the page changes: refs only belong to the snapshot that produced them. When
something is not in the tree, drop to `page.eval` and query the DOM directly.

{vision_guidance}

# Ledger discipline

`LEDGER` is the only thing besides your files that survives context
compaction. It is read back to you at the start of every wake, so it must be
enough for a version of you with no memory to continue. Rewrite it at the end of
every wake with: what is done (and the evidence that proves it), what is in
progress, what failed and must not be retried, and what comes next.

# Ending a wake

Never end a wake silently. Finish by calling `wake_in`, `wake_at`, `on_exit`, or
`done`. If you are waiting on something slow, sleep until it is plausibly ready
rather than polling tightly — an idle wake costs tokens and buys nothing. If you
are truly finished, call `done` with a summary of what you achieved.
"#,
        IDENTITY = IDENTITY,
        vision_api = if vision {
            "await page.screenshot(\"shot.png\")          // returns base64 png\nsee(pathOrBase64)                          // put an image in front of yourself"
        } else {
            "// no screenshot: you cannot see images, so the tree above is the page"
        },
        vision_guidance = if vision {
            VISION_GUIDANCE
        } else {
            NO_VISION_GUIDANCE
        },
        goal = goal,
        goal_dir = goal_dir.display(),
        ledger = goal_dir.join("ledger.md").display(),
        cwd = cwd.display(),
    )
}

/// The message injected at the start of each wake: why you are awake, what you
/// recorded last time, and what is still alive in the isolate.
pub fn wake_message(
    wake: u64,
    reason: &WakeReason,
    ledger: &str,
    manifest: &str,
    since_last: &[String],
) -> String {
    let mut out = format!("[wake #{wake} — {}]\n", reason.describe());

    if !since_last.is_empty() {
        out.push_str("\nWhile you were asleep:\n");
        for line in since_last {
            out.push_str("- ");
            out.push_str(line);
            out.push('\n');
        }
    }

    out.push_str("\n## Ledger\n\n");
    if ledger.trim().is_empty() {
        out.push_str("_(empty — this is your first wake)_\n");
    } else {
        out.push_str(ledger.trim());
        out.push('\n');
    }

    if !manifest.trim().is_empty() {
        out.push_str("\n## Live isolate state\n\n");
        out.push_str(manifest.trim());
        out.push('\n');
    }

    out.push_str("\nTake the highest-value action toward the goal now.\n");
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn rendered(vision: bool) -> String {
        system_prompt(
            "test goal",
            &PathBuf::from("/tmp/goal"),
            &PathBuf::from("/tmp/work"),
            vision,
        )
    }

    #[test]
    fn a_blind_model_is_steered_away_from_screenshots() {
        let prompt = rendered(false);
        assert!(prompt.contains("You cannot see images"));
        assert!(
            !prompt.contains("see(pathOrBase64)"),
            "the see() tool must not be advertised to a model that cannot use it"
        );
        assert!(prompt.contains("page.snapshot()"));
    }

    #[test]
    fn a_sighted_model_is_offered_images_but_told_to_prefer_the_tree() {
        let prompt = rendered(true);
        assert!(prompt.contains("You can see images"));
        assert!(prompt.contains("see(pathOrBase64)"));
        assert!(
            prompt.contains("still faster and more precise"),
            "a sighted model should still be told the tree is the default"
        );
    }

    #[test]
    fn both_variants_keep_the_goal_and_the_paths() {
        for vision in [true, false] {
            let prompt = rendered(vision);
            assert!(prompt.contains("test goal"));
            assert!(prompt.contains("/tmp/goal"));
            assert!(prompt.contains("/tmp/work"));
            // The autonomy core must survive in both.
            assert!(prompt.contains("no human is available"));
        }
    }
}
