// A Chrome DevTools Protocol client, in about a hundred lines.
//
// There is no dependency here on purpose. `ui/` has no bundler, no package.json
// and no `node_modules`, and the point of the window being three files that
// WebKit can load as they are is lost the moment testing it needs a toolchain.
// Node has had a global `WebSocket` since v22 and Chrome speaks CDP over one,
// so the whole client is a request/response map over that socket.

import { spawn } from "node:child_process";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

/// Where Chrome is on this machine. Overridable, because a CI box will not have
/// it in /Applications and a hard-coded path that only works on one laptop is a
/// test suite that only runs on one laptop.
const CHROME =
  process.env.STS_CHROME ??
  "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome";

/// A headless browser and one page in it.
export class Browser {
  #child;
  #profile;
  #socket;
  #nextId = 1;
  #pending = new Map();
  #listeners = new Map();

  static async launch({ port = 0 } = {}) {
    const browser = new Browser();
    await browser.#start(port);
    return browser;
  }

  async #start(port) {
    this.#profile = mkdtempSync(join(tmpdir(), "sts-ui-test-"));
    this.#child = spawn(
      CHROME,
      [
        "--headless=new",
        `--remote-debugging-port=${port}`,
        `--user-data-dir=${this.#profile}`,
        // A test that reaches the network is a test whose result depends on the
        // network. The window is offline by design and so is this.
        "--no-first-run",
        "--no-default-browser-check",
        "--disable-sync",
        "--disable-extensions",
        "--disable-background-networking",
        "--disable-component-update",
        "--disable-default-apps",
        "--metrics-recording-only",
        "--no-sandbox",
        "--disable-gpu",
        // Layout has to be the same number every run. A device pixel ratio the
        // host picks would make every width assertion a property of the display.
        "--force-device-scale-factor=1",
        "about:blank",
      ],
      { stdio: ["ignore", "pipe", "pipe"] },
    );

    const endpoint = await this.#readEndpoint();
    this.#socket = new WebSocket(endpoint);
    await new Promise((resolve, reject) => {
      this.#socket.addEventListener("open", resolve, { once: true });
      this.#socket.addEventListener("error", reject, { once: true });
    });
    this.#socket.addEventListener("message", (event) => this.#onMessage(event.data));
  }

  /// Chrome prints the browser socket on stderr as it comes up. Reading it is
  /// more reliable than polling /json/version on a port we may not have chosen.
  #readEndpoint() {
    return new Promise((resolve, reject) => {
      let buffered = "";
      const timer = setTimeout(
        () => reject(new Error("chrome did not report a devtools endpoint")),
        20_000,
      );
      const onData = (chunk) => {
        buffered += chunk.toString();
        const match = buffered.match(/ws:\/\/[^\s]+/);
        if (match) {
          clearTimeout(timer);
          this.#child.stderr.off("data", onData);
          resolve(match[0]);
        }
      };
      this.#child.stderr.on("data", onData);
      this.#child.on("exit", (code) => {
        clearTimeout(timer);
        reject(new Error(`chrome exited with ${code} before it was ready`));
      });
    });
  }

  #onMessage(raw) {
    const message = JSON.parse(raw);
    if (message.id !== undefined) {
      const pending = this.#pending.get(message.id);
      if (!pending) return;
      this.#pending.delete(message.id);
      if (message.error) pending.reject(new Error(message.error.message));
      else pending.resolve(message.result);
      return;
    }
    for (const listener of this.#listeners.get(message.method) ?? []) {
      listener(message.params);
    }
  }

  send(method, params = {}, sessionId) {
    const id = this.#nextId++;
    const payload = { id, method, params };
    if (sessionId) payload.sessionId = sessionId;
    this.#socket.send(JSON.stringify(payload));
    return new Promise((resolve, reject) => {
      this.#pending.set(id, { resolve, reject });
    });
  }

  on(method, listener) {
    if (!this.#listeners.has(method)) this.#listeners.set(method, []);
    this.#listeners.get(method).push(listener);
  }

  /// Opens a tab and returns a `Page` bound to its session.
  async newPage() {
    const { targetId } = await this.send("Target.createTarget", { url: "about:blank" });
    const { sessionId } = await this.send("Target.attachToTarget", {
      targetId,
      flatten: true,
    });
    const page = new Page(this, sessionId);
    await page.send("Page.enable");
    await page.send("Runtime.enable");
    return page;
  }

  async close() {
    try {
      this.#socket?.close();
    } catch {}
    this.#child?.kill("SIGTERM");
    await new Promise((resolve) => {
      if (!this.#child || this.#child.exitCode !== null) return resolve();
      this.#child.once("exit", resolve);
      setTimeout(() => {
        this.#child.kill("SIGKILL");
        resolve();
      }, 3_000);
    });
    if (this.#profile) rmSync(this.#profile, { recursive: true, force: true });
  }
}

export class Page {
  #browser;
  #sessionId;
  #consoleErrors = [];

  constructor(browser, sessionId) {
    this.#browser = browser;
    this.#sessionId = sessionId;
    // A page that logged an uncaught exception has not passed its assertions
    // whatever the assertions said, so the runner gets to see them.
    browser.on("Runtime.exceptionThrown", (params) => {
      if (params?.exceptionDetails) {
        this.#consoleErrors.push(
          params.exceptionDetails.exception?.description ??
            params.exceptionDetails.text ??
            "unknown exception",
        );
      }
    });
  }

  get exceptions() {
    return [...this.#consoleErrors];
  }

  send(method, params) {
    return this.#browser.send(method, params, this.#sessionId);
  }

  /// Runs a script before anything on the page does, on every navigation.
  async onNewDocument(source) {
    await this.send("Page.addScriptToEvaluateOnNewDocument", { source });
  }

  async goto(url) {
    const loaded = new Promise((resolve) => {
      const done = (params) => {
        if (params?.frameId === undefined || params.frameId) resolve();
      };
      this.#browser.on("Page.loadEventFired", done);
    });
    await this.send("Page.navigate", { url });
    await loaded;
  }

  /// Evaluates an expression in the page and returns its value.
  ///
  /// Anything the page throws is re-thrown here with the page's own message, so
  /// a broken assertion body reads as a broken assertion rather than as
  /// `undefined`.
  async evaluate(fn, ...args) {
    const expression = `(${fn.toString()})(...${JSON.stringify(args)})`;
    const result = await this.send("Runtime.evaluate", {
      expression,
      awaitPromise: true,
      returnByValue: true,
      userGesture: true,
    });
    if (result.exceptionDetails) {
      throw new Error(
        result.exceptionDetails.exception?.description ?? result.exceptionDetails.text,
      );
    }
    return result.result.value;
  }

  async setViewport(width, height) {
    await this.send("Emulation.setDeviceMetricsOverride", {
      width,
      height,
      deviceScaleFactor: 1,
      mobile: false,
    });
  }

  /// Two animation frames plus a task. One frame runs the layout the change
  /// asked for; the second is where the layout-shift observer reports it.
  async settle() {
    await this.evaluate(
      () =>
        new Promise((resolve) => {
          requestAnimationFrame(() => requestAnimationFrame(() => setTimeout(resolve, 0)));
        }),
    );
  }

  async close() {
    await this.send("Page.close").catch(() => {});
  }
}
