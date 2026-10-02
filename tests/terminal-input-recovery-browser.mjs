// Actual TerminalDock input callbacks and HTTP, with an isolated xterm fixture.
import assert from "node:assert/strict";
import { createRequire } from "node:module";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const root = dirname(dirname(fileURLToPath(import.meta.url)));
const require = createRequire(join(root, "web/package.json"));
const { chromium } = require("playwright-core");
const { createServer } = await import(require.resolve("vite"));
const temporary = await mkdtemp(
  join(tmpdir(), "studio-terminal-input-recovery-"),
);
const server = await createServer({
  configFile: false,
  root: join(root, "web"),
  cacheDir: temporary,
  optimizeDeps: {
    include: [
      "react",
      "react-dom/client",
      "react/jsx-dev-runtime",
      "@mantine/core",
      "lucide-react",
    ],
  },
  plugins: [
    {
      name: "terminal-input-harness",
      enforce: "pre",
      resolveId(id) {
        if (id === "virtual:terminal-input") return "\0" + id;
        if (id === "@xterm/xterm") return "\0terminal-stub";
        if (id === "@xterm/addon-fit") return "\0fit-stub";
      },
      load(id) {
        if (id === "\0terminal-stub")
          return `export class Terminal {
            constructor(options) {
              this.options = options; this.cols = 100; this.rows = 24;
              window.terminals.push(this); window.currentTerminal = this;
            }
            loadAddon() {} open() {} reset() {} write() {}
            dispose() { this.disposed = true; }
            onData(callback) { this.input = callback; }
          }`;
        if (id === "\0fit-stub") return "export class FitAddon { fit() {} }";
        if (id === "\0virtual:terminal-input")
          return `
            import React from "react";
            import {createRoot} from "react-dom/client";
            import {MantineProvider} from "@mantine/core";
            import TerminalDock from "/src/components/TerminalDock.tsx";
            import {setToken} from "/src/api.ts";
            export function mount(shell) {
              window.reactRoot?.unmount();
              localStorage.setItem("codex.terminal.open", "true");
              localStorage.setItem("codex.terminal.selected", JSON.stringify("shell:" + shell));
              setToken("fixture-token");
              const agent = {id:"agent", name:"Fixture", source:"managed", status:"idle"};
              const root = createRoot(document.getElementById("app"));
              root.render(React.createElement(MantineProvider, null,
                React.createElement(TerminalDock, {
                  data:{threads:[agent]}, agent,
                  notify: value => window.notices.push(value),
                })));
              window.reactRoot = root;
            }
          `;
      },
    },
  ],
  server: { host: "127.0.0.1", port: 0, hmr: false },
});
await server.listen();
let browser;
try {
  browser = await chromium.launch({
    headless: true,
    executablePath:
      process.env.CHROME_BIN ||
      "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
  });
  const page = await browser.newPage();
  page.setDefaultTimeout(5000);
  const errors = [],
    posts = [],
    expected = [];
  const held = new Map(),
    receipts = new Map(),
    holdTexts = new Set();
  const failOutput = new Set();
  page.on("pageerror", (error) => errors.push(error.message));
  await page.route("**/check", (route) =>
    route.fulfill({
      contentType: "text/html",
      body: '<!doctype html><div id="app"></div>',
    }),
  );
  await page.route("**/api/terminals", (route) =>
    route.fulfill({
      json: {
        items: ["a", "b"].map((id) => ({
          id: "shell-" + id,
          agent: "agent",
          title: "Shell " + id.toUpperCase(),
          cwd: "/fixture",
          status: "running",
          created: 1,
        })),
      },
    }),
  );
  await page.route(/\/api\/terminals\/output(?:\?.*)?$/, (route) => {
    const id = new URL(route.request().url()).searchParams.get("id");
    if (failOutput.delete(id))
      return route.fulfill({
        status: 503,
        json: { error: "Output connection lost" },
      });
    return route.fulfill({
      json: { text: "", offset: 0, truncated: false, status: "running" },
    });
  });
  await page.route("**/api/terminals/resize", (route) =>
    route.fulfill({ json: { ok: true } }),
  );
  await page.route("**/api/terminals/input", (route) => {
    const body = route.request().postDataJSON();
    assert.equal(route.request().headers()["x-canvas-token"], "fixture-token");
    assert.ok(body.request_id);
    posts.push(body);
    const previous = receipts.get(body.request_id);
    if (previous) {
      assert.deepEqual(body, previous.body);
      return route.fulfill({ json: previous.reply });
    }
    // One native effect happens before the fixture can lose its HTTP response.
    receipts.set(body.request_id, {
      body,
      reply: { ok: false, delivery: "uncertain" },
    });
    if (holdTexts.has(body.text)) {
      held.set(body.text, route);
      return;
    }
    return route.fulfill({ json: { ok: true, delivery: "sent" } });
  });
  await page.goto(server.resolvedUrls.local[0] + "check");
  await page.evaluate(async () => {
    window.terminals = [];
    window.notices = [];
    window.advance = 0;
    const now = Date.now;
    Date.now = () => now() + window.advance;
    window.fixture = await import("/@id/__x00__virtual:terminal-input");
  });
  const until = async (check, message) => {
    const deadline = Date.now() + 5000;
    while (!(await check())) {
      assert.ok(Date.now() < deadline, message);
      await new Promise((resolve) => setTimeout(resolve, 10));
    }
  };
  const count = () => page.evaluate(() => window.terminals.length);
  const mount = async () => {
    const before = await count();
    await page.evaluate(() => window.fixture.mount("shell-a"));
    await page.waitForFunction(
      (before) =>
        window.terminals.length > before && window.currentTerminal.input,
      before,
    );
    assert.equal(
      await page.evaluate(() => window.currentTerminal.options.disableStdin),
      false,
    );
  };
  const input = (text) =>
    page.evaluate((text) => window.currentTerminal.input(text), text);
  const observed = (text) =>
    until(
      () => posts.some((post) => post.text === text),
      `Input ${text} must start`,
    );
  const accepted = async (text, hold = false) => {
    expected.push(text);
    if (hold) holdTexts.add(text);
    await input(text);
    await observed(text);
  };
  const reply = async (text, outcome = "success") => {
    const route = held.get(text);
    assert.ok(route, `Expected held input ${text}`);
    const body = route.request().postDataJSON();
    const response =
      outcome === "uncertain"
        ? {
            ok: false,
            delivery: "uncertain",
            error: "Late input outcome unknown",
          }
        : { ok: true, delivery: "sent" };
    receipts.get(body.request_id).reply = response;
    await route.fulfill(
      outcome === "error"
        ? { status: 503, json: { error: "Late old input error" } }
        : { json: response },
    );
    held.delete(text);
    await page.waitForTimeout(50);
  };
  const assertUnpaused = async () => {
    assert.equal(
      await page.evaluate(() => window.currentTerminal.options.disableStdin),
      false,
    );
    assert.equal(
      await page
        .getByText(/Input paused\. Reconnect before you continue\./)
        .count(),
      0,
    );
  };
  const reconnect = async (shell = "shell-a") => {
    const before = await count();
    failOutput.add(shell);
    await page.getByRole("button", { name: "Reconnect", exact: true }).click();
    await page.waitForFunction(
      (before) =>
        window.terminals.length > before && window.currentTerminal.input,
      before,
    );
    await assertUnpaused();
  };

  // Healthy input stays ordered behind one exact in-flight command.
  await mount();
  await accepted("healthy-1", true);
  expected.push("healthy-2");
  await input("healthy-2");
  await page.waitForTimeout(50);
  assert.deepEqual(
    posts.map((post) => post.text),
    ["healthy-1"],
  );
  await reply("healthy-1");
  await observed("healthy-2");
  assert.deepEqual(
    posts.map((post) => post.text),
    ["healthy-1", "healthy-2"],
  );

  // The original hang: a lost response expires after a suspended page resumes.
  await mount();
  await accepted("timeout-old", true);
  await input("discard-timeout");
  await page.evaluate(() => {
    window.advance += 16000;
    window.dispatchEvent(new Event("pageshow"));
  });
  await page
    .getByText(/Input paused\. Reconnect before you continue\./)
    .waitFor();
  assert.equal(
    await page.evaluate(() => window.currentTerminal.options.disableStdin),
    true,
  );
  await page.getByText(/Previous input delivery is uncertain\./).waitFor();
  await input("discard-while-paused");
  const beforeTimeoutReconnect = await count();
  await page.getByRole("button", { name: "Reconnect", exact: true }).click();
  await page.waitForFunction(
    (before) =>
      window.terminals.length > before && window.currentTerminal.input,
    beforeTimeoutReconnect,
  );
  await assertUnpaused();
  await page.getByText(/Previous input delivery is uncertain\./).waitFor();
  await accepted("timeout-fresh");
  await reply("timeout-old");

  // Reconnect before the deadline fences every old outcome and queued callback.
  for (const outcome of ["success", "error", "uncertain"]) {
    await mount();
    await accepted(`${outcome}-old`, true);
    await input(`discard-${outcome}`);
    await page.evaluate(
      () => (window.retiredTerminal = window.currentTerminal),
    );
    await reconnect();
    await page.getByText(/Previous input delivery is uncertain\./).waitFor();
    await page.evaluate(() =>
      window.retiredTerminal.input("discard-retired-callback"),
    );
    await accepted(`${outcome}-fresh`, true);
    await reply(`${outcome}-old`, outcome);
    await assertUnpaused();
    expected.push(`${outcome}-fresh-2`);
    await input(`${outcome}-fresh-2`);
    assert.equal(
      posts.filter((post) => post.text === `${outcome}-fresh-2`).length,
      0,
    );
    await reply(`${outcome}-fresh`);
    await observed(`${outcome}-fresh-2`);
    await page.getByText(/Previous input delivery is uncertain\./).waitFor();
  }

  // Unmount prevents old queued keys and late errors from reaching a new view.
  await mount();
  await accepted("unmount-old", true);
  await input("discard-unmount");
  await page.evaluate(() => {
    window.retiredTerminal = window.currentTerminal;
    window.reactRoot.unmount();
    window.reactRoot = null;
  });
  await mount();
  await accepted("unmount-fresh", true);
  await reply("unmount-old", "error");
  await page.evaluate(() =>
    window.retiredTerminal.input("discard-unmounted-callback"),
  );
  await assertUnpaused();
  await reply("unmount-fresh");

  // Switching shells uses the new shell identity and a separate input chain.
  await mount();
  await accepted("switch-old", true);
  await input("discard-switch");
  const beforeSwitch = await count();
  await page
    .locator(".terminal-session")
    .filter({ hasText: "Shell B" })
    .click();
  await page.waitForFunction(
    (before) =>
      window.terminals.length > before && window.currentTerminal.input,
    beforeSwitch,
  );
  await accepted("switch-fresh", true);
  assert.equal(
    posts.find((post) => post.text === "switch-fresh").id,
    "shell-b",
  );
  await reply("switch-old");
  await assertUnpaused();
  await reply("switch-fresh");

  assert.deepEqual(
    posts.map((post) => post.text),
    expected,
  );
  assert.equal(
    new Set(posts.map((post) => post.request_id)).size,
    posts.length,
  );
  assert.equal(
    receipts.size,
    expected.length,
    "Every intended input has one logical native effect",
  );
  assert.deepEqual(errors, []);
  console.log(
    "PASS: ordered input, resume deadline, manual reconnect, late outcomes, unmount, shell switch, and no stale input replay",
  );
} finally {
  await browser?.close();
  await server.close();
  await rm(temporary, { recursive: true, force: true });
}
