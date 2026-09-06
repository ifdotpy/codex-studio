#!/usr/bin/env node
// Browser contract for process controls. The HTTP transport is a deterministic fixture.
import assert from "node:assert/strict";
import { createServer } from "node:http";
import { readFile, mkdtemp } from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, join, extname } from "node:path";
import { fileURLToPath } from "node:url";
import { createRequire } from "node:module";
const skill = dirname(dirname(fileURLToPath(import.meta.url)));
const { chromium } = createRequire(join(skill, "web/package.json"))(
  "playwright-core",
);
const root = await mkdtemp(join(tmpdir(), "codex-background-controls-"));
const server = createServer(async (req, res) => {
  try {
    const name = new URL(req.url, "http://localhost").pathname;
    const path = join(skill, "web/dist", name === "/" ? "index.html" : name);
    res.setHeader(
      "Content-Type",
      { ".js": "text/javascript", ".css": "text/css", ".html": "text/html" }[
        extname(path)
      ] || "application/octet-stream",
    );
    res.end(await readFile(path));
  } catch {
    res.statusCode = 404;
    res.end();
  }
});
await new Promise((r) => server.listen(0, "127.0.0.1", r));
const agent = {
  id: "lead",
  rootId: "lead",
  isLead: true,
  source: "managed",
  status: "idle",
  name: "Release lead",
  model: "gpt-5.6-sol",
  created: Date.now() / 1000 - 30,
  canSend: true,
};
const native = {
  id: "native-task",
  agent: "lead",
  kind: "command",
  name: "commandExecution",
  command: "native test process",
  processId: "9042",
  status: "running",
  created: Date.now() / 1000 - 20,
  tail: "Waiting for input",
};
const monitor = {
  id: "6f2ed4dd-f6ab-4d46-94a5-8a774333876a",
  agent: "lead",
  kind: "monitor",
  command: "read value; echo $value",
  interactive: true,
  timeout_ms: 1800000,
  status: "running",
  created: Date.now() / 1000,
  tail: "ready",
  log: "/fixture/monitor.log",
};
const state = {
  token: "fixture-token",
  stateDir: root,
  threads: [agent],
  chats: [],
  runtime: {
    agents: [agent],
    rooms: [],
    complaints: [],
    monitors: [monitor],
    tasks: [native],
    requests: [],
  },
};
const writes = [];
let failInput = false;
let browser;
try {
  browser = await chromium.launch({
    executablePath:
      process.env.CHROME_BIN ||
      "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
    headless: true,
  });
  const page = await browser.newPage({
    viewport: { width: 1440, height: 980 },
    acceptDownloads: true,
  });
  const errors = [];
  page.on("pageerror", (e) => errors.push(e.message));
  await page.route("**/api/**", async (route) => {
    const req = route.request(),
      path = new URL(req.url()).pathname;
    let body = {};
    if (req.method() === "POST") {
      body = req.postDataJSON();
      assert.equal(req.headers()["x-canvas-token"], "fixture-token");
      writes.push({ path, body });
    }
    let value = {};
    if (path === "/api/state") value = state;
    else if (path === "/api/task") value = native;
    else if (path === "/api/transcript") value = { items: [], agent };
    else if (path === "/api/transcript/stream")
      return route.fulfill({ status: 503, body: "fixture polling" });
    else if (path === "/api/limits") value = { data: null };
    else if (path === "/api/monitor")
      return route.fulfill({ status: 404, json: { error: "Not found" } });
    else if (path === "/api/monitor/log")
      value = {
        name: "monitor.log",
        mime: "text/plain",
        base64: Buffer.from("full saved log\n").toString("base64"),
      };
    else if (path === "/api/monitor/input" && failInput) {
      failInput = false;
      return route.fulfill({
        status: 409,
        json: { error: "The command already exited" },
      });
    }
    await route.fulfill({ json: value });
  });
  await page.goto(`http://127.0.0.1:${server.address().port}`);
  await page.locator("#tasks-toggle").click();
  const drawer = page.getByRole("dialog", { name: /Background tasks/ });
  assert.equal(
    await drawer
      .getByRole("button", { name: /^(New monitor|Start monitor)$/ })
      .count(),
    0,
  );
  await drawer.locator(`[data-task="${monitor.id}"]`).click();
  await drawer
    .getByRole("button", { name: "Send line", exact: true })
    .waitFor();
  assert.equal(
    writes.some((w) => w.path === "/api/monitor"),
    false,
    "opening an agent monitor does not start a command",
  );
  const created = monitor;
  const input = drawer.getByLabel("Terminal input");
  await input.fill("hello");
  await drawer.getByRole("button", { name: "Send line", exact: true }).click();
  await page.waitForFunction(
    () => document.querySelector('[aria-label="Terminal input"]').value === "",
  );
  assert.deepEqual(writes.at(-1), {
    path: "/api/monitor/input",
    body: { id: created.id, text: "hello\n" },
  });
  failInput = true;
  await input.fill("keep this input");
  await drawer.getByRole("button", { name: "Send line", exact: true }).click();
  await page.getByText("The command already exited", { exact: true }).waitFor();
  assert.equal(await input.inputValue(), "keep this input");
  await drawer.getByRole("button", { name: "Ctrl+C", exact: true }).click();
  assert.equal(writes.at(-1).body.text, "\u0003");
  await drawer.locator(".process-resize summary").click();
  await drawer.getByLabel("Rows", { exact: true }).fill("30");
  await drawer.getByLabel("Columns", { exact: true }).fill("100");
  await drawer.getByRole("button", { name: "Resize", exact: true }).click();
  assert.deepEqual(writes.at(-1).body, { id: created.id, rows: 30, cols: 100 });
  const downloaded = page.waitForEvent("download");
  await drawer.getByLabel("Download task log").click();
  const download = await downloaded;
  assert.equal(download.suggestedFilename(), "monitor.log");
  assert.equal(
    await readFile(await download.path(), "utf8"),
    "full saved log\n",
  );
  await page.screenshot({ path: join(root, "interactive-desktop.png") });
  await page.setViewportSize({ width: 320, height: 740 });
  assert.equal(
    await page.evaluate(() => document.documentElement.scrollWidth),
    320,
  );
  await drawer.locator(".process-input").scrollIntoViewIfNeeded();
  await page.screenshot({ path: join(root, "interactive-mobile.png") });
  await drawer
    .getByRole("button", { name: "Close input (EOF)", exact: true })
    .click();
  await drawer
    .getByRole("button", { name: "Input closed", exact: true })
    .waitFor();
  assert.equal(writes.at(-1).body.closeStdin, true);
  assert.equal(await input.isDisabled(), true);
  await drawer.getByRole("button", { name: "Tasks", exact: true }).click();
  await drawer.locator('[data-task="native-task"]').click();
  await drawer
    .getByRole("button", { name: "Send via agent", exact: true })
    .waitFor();
  await drawer.getByLabel("Terminal input").fill("continue");
  await drawer
    .getByRole("button", { name: "Send via agent", exact: true })
    .click();
  assert.deepEqual(writes.at(-1), {
    path: "/api/native-command",
    body: { id: "native-task", text: "continue\n", action: "input" },
  });
  await drawer
    .getByRole("button", { name: "Ask agent to stop command", exact: true })
    .click();
  assert.deepEqual(writes.at(-1), {
    path: "/api/native-command",
    body: { id: "native-task", action: "cancel" },
  });
  assert.equal(
    writes.some((w) => w.path === "/api/monitor/input" && w.body.id === "9042"),
    false,
  );
  assert.deepEqual(errors, []);
  console.log(`PASS background controls browser contract. Evidence: ${root}`);
} finally {
  if (browser) await browser.close();
  await new Promise((r) => server.close(r));
}
