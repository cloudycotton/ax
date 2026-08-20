// The JavaScript prelude.
//
// Rust exposes only a handful of low-level `__`-prefixed bindings that speak
// JSON strings. Everything the model actually calls is defined here, in
// JavaScript, for two reasons: it iterates without recompiling, and the model
// can read and extend it at runtime. Keep this file dependency-free — it is
// evaluated once into a bare QuickJS context at startup.

(() => {
  // Everything already on globalThis before the prelude runs — QuickJS
  // built-ins plus the constants Rust injected. The state manifest reports
  // what the *model* added, so anything in here is excluded from it.
  const BASELINE = new Set(Object.keys(globalThis));

  // ---------------------------------------------------------------- helpers

  function unwrap(raw) {
    const parsed = JSON.parse(raw);
    if (parsed && parsed.ok === false) {
      const err = new Error(parsed.error || "host call failed");
      err.name = parsed.name || "HostError";
      throw err;
    }
    return parsed ? parsed.value : undefined;
  }

  // ---------------------------------------------------------------- inspect

  // A depth- and length-limited value formatter. Never throws, never recurses
  // forever, and always produces something a model can read.
  function inspect(value, depth = 0, seen = new WeakSet()) {
    const INDENT = "  ".repeat(depth + 1);
    const CLOSE = "  ".repeat(depth);

    if (value === null) return "null";
    if (value === undefined) return "undefined";

    const type = typeof value;
    if (type === "string") return depth === 0 ? value : JSON.stringify(value);
    if (type === "number" || type === "boolean" || type === "bigint") return String(value);
    if (type === "symbol") return value.toString();
    if (type === "function") {
      return `[Function: ${value.name || "anonymous"}]`;
    }

    if (value instanceof Error) {
      return value.stack || `${value.name}: ${value.message}`;
    }
    if (value instanceof Date) return value.toISOString();
    if (value instanceof RegExp) return value.toString();
    if (value instanceof Promise) return "[Promise]";

    if (seen.has(value)) return "[Circular]";
    seen.add(value);

    try {
      if (Array.isArray(value)) {
        if (value.length === 0) return "[]";
        const shown = value.slice(0, 100).map((v) => inspect(v, depth + 1, seen));
        const more = value.length > 100 ? `, … ${value.length - 100} more` : "";
        const oneLine = `[ ${shown.join(", ")}${more} ]`;
        if (oneLine.length <= 100 && !oneLine.includes("\n")) return oneLine;
        return `[\n${shown.map((s) => INDENT + s).join(",\n")}${more}\n${CLOSE}]`;
      }

      if (value instanceof Map) {
        const entries = [...value.entries()].slice(0, 50);
        const body = entries.map(([k, v]) => `${inspect(k, depth + 1, seen)} => ${inspect(v, depth + 1, seen)}`);
        return `Map(${value.size}) { ${body.join(", ")}${value.size > 50 ? ", …" : ""} }`;
      }
      if (value instanceof Set) {
        const items = [...value.values()].slice(0, 50).map((v) => inspect(v, depth + 1, seen));
        return `Set(${value.size}) { ${items.join(", ")}${value.size > 50 ? ", …" : ""} }`;
      }
      if (ArrayBuffer.isView(value)) {
        return `${value.constructor.name}(${value.length})`;
      }

      if (depth >= 4) return "[Object]";

      const keys = Object.keys(value);
      if (keys.length === 0) return "{}";
      const shown = keys.slice(0, 60).map((k) => {
        const key = /^[A-Za-z_$][A-Za-z0-9_$]*$/.test(k) ? k : JSON.stringify(k);
        return `${key}: ${inspect(value[k], depth + 1, seen)}`;
      });
      const more = keys.length > 60 ? `, … ${keys.length - 60} more` : "";
      const oneLine = `{ ${shown.join(", ")}${more} }`;
      if (oneLine.length <= 100 && !oneLine.includes("\n")) return oneLine;
      return `{\n${shown.map((s) => INDENT + s).join(",\n")}${more}\n${CLOSE}}`;
    } catch (e) {
      return `[unprintable: ${e && e.message}]`;
    } finally {
      seen.delete(value);
    }
  }

  // ---------------------------------------------------------------- console

  function format(args) {
    return args.map((a) => (typeof a === "string" ? a : inspect(a, 1))).join(" ");
  }

  const console = {
    log: (...args) => __emit("stdout", format(args)),
    info: (...args) => __emit("stdout", format(args)),
    debug: (...args) => __emit("stdout", format(args)),
    warn: (...args) => __emit("stderr", format(args)),
    error: (...args) => __emit("stderr", format(args)),
  };

  // ------------------------------------------------------------------ shell

  async function sh(cmd, opts = {}) {
    return unwrap(await __sh(String(cmd), JSON.stringify(opts || {})));
  }

  // A background process handle. Survives the wake that created it, so a dev
  // server or a long scrape keeps running while the agent sleeps.
  class Process {
    constructor(name, pid, command) {
      this.name = name;
      this.pid = pid;
      this.command = command;
    }
    running() {
      return unwrap(__proc(this.name, "running", "{}"));
    }
    exitCode() {
      return unwrap(__proc(this.name, "exit", "{}"));
    }
    output(lines = 200) {
      return unwrap(__proc(this.name, "output", JSON.stringify({ lines })));
    }
    kill() {
      return unwrap(__proc(this.name, "kill", "{}"));
    }
    toString() {
      return `[Process ${this.name}${this.running() ? " running" : " exited"}]`;
    }
  }

  function spawn(cmd, opts = {}) {
    const info = unwrap(__spawn(String(cmd), JSON.stringify(opts || {})));
    return new Process(info.name, info.pid, String(cmd));
  }

  function processes() {
    return unwrap(__proc("", "list", "{}"));
  }

  // ------------------------------------------------------------------ files

  const read = async (path) => unwrap(await __read(String(path)));
  const write = async (path, data) =>
    unwrap(await __write(String(path), typeof data === "string" ? data : inspect(data)));
  const ls = async (path = ".") => unwrap(await __ls(String(path)));
  const exists = async (path) => unwrap(await __exists(String(path)));

  // ---------------------------------------------------------------- network

  async function fetch(url, opts = {}) {
    const raw = unwrap(await __fetch(String(url), JSON.stringify(opts || {})));
    return {
      status: raw.status,
      ok: raw.ok,
      headers: raw.headers,
      body: raw.body,
      text: () => raw.body,
      json: () => JSON.parse(raw.body),
    };
  }

  const sleep = (ms) => __sleep(Number(ms) || 0);

  // ------------------------------------------------------------- scheduling

  function decide(payload) {
    __decide(JSON.stringify(payload));
  }

  const wake_in = (ms, note = "") => decide({ type: "wake_in", ms: Number(ms) || 0, note: String(note) });
  const wake_at = (iso, note = "") => decide({ type: "wake_at", at: String(iso), note: String(note) });
  const on_exit = (handle, note = "") =>
    decide({ type: "on_exit", name: typeof handle === "string" ? handle : handle.name, note: String(note) });
  const done = (summary = "") => decide({ type: "done", summary: String(summary) });

  // Put an image in front of the model. Accepts a file path, the base64 a
  // screenshot returns, or a data: URI. Only works when the model can see;
  // otherwise it throws with what to do instead.
  const see = (imageOrPath) => unwrap(__see(String(imageOrPath)));

  const notify = (message, level = "info") => __notify(String(message), String(level));
  const log = (...args) => __emit("stdout", format(args));

  // ---------------------------------------------------------------- browser
  //
  // Built on raw CDP. The design goal is that everything works for a model
  // with no vision: `snapshot()` returns the accessibility tree with a stable
  // `ref` on every interactive node, and actions take those refs. Screenshots
  // are available but never required.

  const cdp = {
    send: async (method, params = {}, session = "") =>
      unwrap(await __cdp_send(String(method), JSON.stringify(params || {}), String(session || ""))),
    wait: async (method, { session = "", timeout = 30000 } = {}) =>
      unwrap(await __cdp_wait(String(method), String(session || ""), Number(timeout))),
    control: async (action, argument = "") =>
      unwrap(await __cdp_control(String(action), String(argument))),
  };

  // Roles worth handing to a model as actionable. Everything else is context.
  const INTERACTIVE = new Set([
    "button", "link", "textbox", "searchbox", "checkbox", "radio", "combobox",
    "listbox", "option", "menuitem", "menuitemcheckbox", "menuitemradio", "tab",
    "switch", "slider", "spinbutton", "textarea", "colorwell",
  ]);

  const KEYS = {
    Enter: { code: "Enter", key: "Enter", keyCode: 13, text: "\r" },
    Tab: { code: "Tab", key: "Tab", keyCode: 9 },
    Escape: { code: "Escape", key: "Escape", keyCode: 27 },
    Backspace: { code: "Backspace", key: "Backspace", keyCode: 8 },
    Delete: { code: "Delete", key: "Delete", keyCode: 46 },
    ArrowUp: { code: "ArrowUp", key: "ArrowUp", keyCode: 38 },
    ArrowDown: { code: "ArrowDown", key: "ArrowDown", keyCode: 40 },
    ArrowLeft: { code: "ArrowLeft", key: "ArrowLeft", keyCode: 37 },
    ArrowRight: { code: "ArrowRight", key: "ArrowRight", keyCode: 39 },
    PageDown: { code: "PageDown", key: "PageDown", keyCode: 34 },
    PageUp: { code: "PageUp", key: "PageUp", keyCode: 33 },
  };

  class Page {
    constructor(id, session) {
      this.id = String(id);
      this.session = String(session);
      this.refs = new Map();
    }

    send(method, params = {}) {
      return cdp.send(method, params, this.session);
    }

    async ready() {
      await this.send("Page.enable", {}).catch(() => {});
      await this.send("Runtime.enable", {}).catch(() => {});
      await this.send("DOM.enable", {}).catch(() => {});
      return this;
    }

    /// Evaluate in the page. Accepts an expression string or a function.
    async eval(code, ...args) {
      let expression;
      if (typeof code === "function") {
        expression = `(${code.toString()})(${args.map((a) => JSON.stringify(a)).join(",")})`;
      } else {
        expression = String(code);
      }
      const result = await this.send("Runtime.evaluate", {
        expression,
        returnByValue: true,
        awaitPromise: true,
        // Let the page see this as ordinary script.
        userGesture: true,
      });
      if (result.exceptionDetails) {
        const detail = result.exceptionDetails;
        throw new Error(
          `page.eval threw: ${detail.exception?.description || detail.text || "unknown error"}`,
        );
      }
      return result.result?.value;
    }

    async url() {
      return this.eval("location.href");
    }
    async title() {
      return this.eval("document.title");
    }
    async text() {
      return this.eval("document.body ? document.body.innerText : ''");
    }
    async html() {
      return this.eval("document.documentElement.outerHTML");
    }

    /// Navigate and wait for the document to finish loading.
    async goto(url, { timeout = 30000, settle = 250 } = {}) {
      await this.ready();
      const result = await this.send("Page.navigate", { url: String(url) });
      if (result.errorText) throw new Error(`navigation failed: ${result.errorText}`);
      await this.waitForLoad({ timeout });
      if (settle) await sleep(settle);
      return this;
    }

    /// Poll readyState rather than racing load events — by the time we
    /// subscribe, a fast page may already have fired them.
    async waitForLoad({ timeout = 30000 } = {}) {
      const deadline = Date.now() + timeout;
      while (Date.now() < deadline) {
        try {
          const state = await this.eval("document.readyState");
          if (state === "complete" || state === "interactive") return this;
        } catch {
          // Navigation tears down the context; retry until it settles.
        }
        await sleep(100);
      }
      throw new Error(`page did not finish loading within ${timeout}ms`);
    }

    /// Wait until an expression becomes truthy in the page.
    async waitFor(expression, { timeout = 30000, interval = 200 } = {}) {
      const deadline = Date.now() + timeout;
      while (Date.now() < deadline) {
        try {
          const value = await this.eval(expression);
          if (value) return value;
        } catch {
          // Ignore transient errors while the page changes underneath us.
        }
        await sleep(interval);
      }
      throw new Error(`waitFor timed out after ${timeout}ms: ${expression}`);
    }

    /// The accessibility tree, with a stable ref on every actionable node.
    /// This is the primary way to read a page — no vision required.
    async snapshot({ interactiveOnly = false, max = 2000 } = {}) {
      await this.ready();
      const { nodes } = await this.send("Accessibility.getFullAXTree", {});
      this.refs = new Map();

      const byId = new Map(nodes.map((n) => [n.nodeId, n]));
      const lines = [];
      let counter = 0;

      const visit = (node, depth, parentName) => {
        if (!node || lines.length >= max) return;
        const role = node.role?.value || "";
        const name = node.name?.value || "";
        const ignored = node.ignored === true;

        // InlineTextBox is layout detail, and a StaticText that merely repeats
        // its parent's accessible name is noise. Dropping both roughly halves
        // the size of a real page's snapshot without losing information.
        if (role === "InlineTextBox") return;
        if (role === "StaticText" && name && name === parentName) {
          for (const childId of node.childIds || []) visit(byId.get(childId), depth, name);
          return;
        }

        if (!ignored && role && role !== "none" && role !== "generic") {
          const actionable = INTERACTIVE.has(role);
          if (!interactiveOnly || actionable) {
            let line = "  ".repeat(Math.min(depth, 12)) + role;
            if (name) line += ` "${name.slice(0, 120)}"`;
            const value = node.value?.value;
            if (value !== undefined && value !== "") line += ` = ${JSON.stringify(String(value).slice(0, 80))}`;
            if (actionable && node.backendDOMNodeId) {
              const ref = `r${++counter}`;
              this.refs.set(ref, node.backendDOMNodeId);
              line += ` [${ref}]`;
            }
            lines.push(line);
          }
        }
        for (const childId of node.childIds || []) visit(byId.get(childId), depth + 1, name || parentName);
      };

      const root = nodes.find((n) => !n.parentId) || nodes[0];
      visit(root, 0, "");
      if (lines.length >= max) lines.push(`… truncated at ${max} lines`);
      return lines.join("\n");
    }

    /// Resolve a ref from the last snapshot, or a CSS selector, to a point.
    async locate(target) {
      let backendNodeId = this.refs.get(target);
      if (!backendNodeId) {
        // Treat it as a CSS selector.
        const { root } = await this.send("DOM.getDocument", { depth: 0 });
        const found = await this.send("DOM.querySelector", {
          nodeId: root.nodeId,
          selector: String(target),
        });
        if (!found.nodeId) throw new Error(`no element matched: ${target}`);
        const described = await this.send("DOM.describeNode", { nodeId: found.nodeId });
        backendNodeId = described.node.backendNodeId;
      }
      await this.send("DOM.scrollIntoViewIfNeeded", { backendNodeId }).catch(() => {});
      const { model } = await this.send("DOM.getBoxModel", { backendNodeId });
      const [x1, y1, x2, y2, x3, y3, x4, y4] = model.content;
      return { x: (x1 + x2 + x3 + x4) / 4, y: (y1 + y2 + y3 + y4) / 4, backendNodeId };
    }

    /// Click by ref or selector. Dispatched through the browser's input
    /// pipeline so the page sees a trusted event — synthetic DOM clicks are
    /// ignored by plenty of real sites.
    async click(target, { button = "left", clickCount = 1 } = {}) {
      const { x, y } = await this.locate(target);
      const base = { x, y, button, clickCount, buttons: 1 };
      await this.send("Input.dispatchMouseEvent", { ...base, type: "mousePressed" });
      await this.send("Input.dispatchMouseEvent", { ...base, type: "mouseReleased" });
      return this;
    }

    async hover(target) {
      const { x, y } = await this.locate(target);
      await this.send("Input.dispatchMouseEvent", { type: "mouseMoved", x, y });
      return this;
    }

    /// Click a field and type into it.
    async fill(target, text) {
      await this.click(target);
      await this.send("Input.insertText", { text: String(text) });
      return this;
    }

    async type(text) {
      await this.send("Input.insertText", { text: String(text) });
      return this;
    }

    async press(key) {
      const spec = KEYS[key];
      if (!spec) throw new Error(`unknown key: ${key}. Known: ${Object.keys(KEYS).join(", ")}`);
      const common = {
        key: spec.key,
        code: spec.code,
        windowsVirtualKeyCode: spec.keyCode,
        nativeVirtualKeyCode: spec.keyCode,
      };
      await this.send("Input.dispatchKeyEvent", {
        ...common,
        type: spec.text ? "keyDown" : "rawKeyDown",
        text: spec.text,
      });
      await this.send("Input.dispatchKeyEvent", { ...common, type: "keyUp" });
      return this;
    }

    async scroll(deltaY = 400) {
      await this.send("Input.dispatchMouseEvent", {
        type: "mouseWheel",
        x: 100,
        y: 100,
        deltaX: 0,
        deltaY: Number(deltaY),
      });
      return this;
    }

    /// Save a PNG. Only useful to models that can see; snapshot() is the
    /// primary interface.
    /// Capture the page. Returns base64 PNG, which `see()` accepts directly:
    ///   see(await page.screenshot())
    async screenshot(path) {
      const { data } = await this.send("Page.captureScreenshot", { format: "png" });
      if (path) {
        // QuickJS has no base64 decoder and no binary file writes, so decode
        // via the shell rather than hand-rolling one.
        const quoted = JSON.stringify(String(path));
        const encoded = quoted.slice(0, -1) + '.b64"';
        await write(String(path) + ".b64", data);
        const result = await sh(`base64 -d < ${encoded} > ${quoted} && rm -f ${encoded}`);
        if (result.code !== 0) throw new Error(`could not save screenshot: ${result.stderr}`);
      }
      return data;
    }

    async close() {
      await cdp.send("Target.closeTarget", { targetId: this.id }).catch(() => {});
    }

    toString() {
      return `[Page ${this.id}]`;
    }
  }

  const browser = {
    /// Does not force a connection — safe to call to find out what is available.
    status: () => cdp.control("status"),
    tabs: () => cdp.control("targets"),
    /// Open a new tab and attach to it. The tab is created blank and then
    /// navigated, so there is exactly one navigation path to reason about.
    open: async (url = "about:blank") => {
      const info = await cdp.control("new_tab", "about:blank");
      const page = new Page(info.id, info.session);
      await page.ready();
      if (url && url !== "about:blank") await page.goto(url);
      return page;
    },
    /// Attach to an already-open tab, by index or id.
    attach: async (which = 0) => {
      const tabs = await cdp.control("targets");
      const tab = typeof which === "number" ? tabs[which] : tabs.find((t) => String(t.id) === String(which));
      if (!tab) throw new Error(`no such tab: ${which}`);
      const { session } = await cdp.control("attach", String(tab.id));
      const page = new Page(tab.id, session);
      return page.ready();
    },
    activate: (page) => cdp.control("activate", String(page.id ?? page)),
  };

  // ----------------------------------------------------------- installation

  const api = {
    console, inspect,
    sh, spawn, processes,
    read, write, ls, exists,
    fetch, sleep,
    log, notify, see,
    wake_in, wake_at, on_exit, done,
    browser, cdp, Page,
  };

  for (const [name, value] of Object.entries(api)) {
    Object.defineProperty(globalThis, name, {
      value,
      writable: true,
      configurable: true,
      enumerable: false,
    });
    BASELINE.add(name);
  }

  // ----------------------------------------------------------- state report
  //
  // After every execution the harness asks for this. It is what a future,
  // context-compacted version of the model is told about what is still alive
  // in the isolate — without it, a long session forgets the tools it built.

  Object.defineProperty(globalThis, "__manifest", {
    value: () => {
      const lines = [];
      for (const key of Object.keys(globalThis)) {
        if (BASELINE.has(key) || key.startsWith("__")) continue;
        let value;
        try {
          value = globalThis[key];
        } catch {
          continue;
        }
        let desc;
        if (typeof value === "function") {
          desc = `function(${value.length} args)`;
        } else if (Array.isArray(value)) {
          desc = `Array(${value.length})`;
        } else if (value instanceof Process) {
          desc = `Process ${value.name}, ${value.running() ? "running" : "exited"}`;
        } else if (value && typeof value === "object") {
          const keys = Object.keys(value);
          desc = `Object{${keys.slice(0, 6).join(", ")}${keys.length > 6 ? ", …" : ""}}`;
        } else {
          desc = `${typeof value} = ${String(value).slice(0, 60)}`;
        }
        lines.push(`${key}: ${desc}`);
      }
      return lines.join("\n");
    },
    enumerable: false,
    configurable: true,
  });

  Object.defineProperty(globalThis, "__inspect", {
    value: inspect,
    enumerable: false,
    configurable: true,
  });
})();
