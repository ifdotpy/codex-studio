#!/usr/bin/env node
// Production client with an isolated runtime. No model service or user state.
import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { mkdtemp } from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { createRequire } from "node:module";
const skill = dirname(dirname(fileURLToPath(import.meta.url)));
const { chromium } = createRequire(join(skill, "web/package.json"))(
  "playwright-core",
);
const root = await mkdtemp(join(tmpdir(), "codex-chat-navigation-"));
const proc = spawn(
  "python3",
  ["-B", join(skill, "tests/simple-ui-fixture.py"), root],
  { stdio: ["pipe", "pipe", "pipe"] },
);
let browser,
  log = "";
proc.stderr.on("data", (d) => (log += d));
try {
  const port = await new Promise((resolve, reject) => {
    proc.stdout.once("data", (d) => resolve(Number(String(d).trim())));
    proc.once("exit", () => reject(Error(log)));
  });
  const url = `http://127.0.0.1:${port}`;
  browser = await chromium.launch({
    headless: true,
    executablePath:
      process.env.CHROME_BIN ||
      "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
  });
  const page = await browser.newPage({
    viewport: { width: 1440, height: 960 },
  });
  const errors = [];
  page.on("pageerror", (e) => errors.push(e.message));
  await page.goto(url);
  await page.locator("#new-chat").click();
  await page.getByRole("heading", { name: "New chat", exact: true }).waitFor();
  const state = await (await fetch(url + "/api/state")).json();
  assert.ok(
    state.runtime.agents.some(
      (a) => a.name === "New chat" && a.status === "idle",
    ),
  );
  await page.locator("#message").waitFor();
  assert.equal(
    await page.locator('.agent-phase[data-phase="idle"]').count(),
    0,
    "idle has no loading status below messages",
  );
  await page.locator("#message").fill("Keep my draft");
  for (const [section, title] of [
    ["user-tasks", "Your tasks"],
    ["inbox", "Inbox"],
    ["changes", "Changes"],
    ["search", "Search"],
    ["plan", "Plan"],
    ["rules", "Rules"],
  ]) {
    await page.locator(`[data-workspace-section="${section}"]`).click();
    const drawer = page.locator(".workspace-drawer");
    await drawer.getByRole("heading", { name: title, exact: true }).waitFor();
    await page.keyboard.press("Escape");
    await drawer.waitFor({ state: "hidden" });
  }
  assert.equal(
    await page.locator('[data-action="monitor"]').count(),
    0,
    "no manual monitor header action",
  );
  const background = page.getByRole("dialog", { name: /Background tasks/ });
  assert.equal(await page.locator("#message").inputValue(), "Keep my draft");
  await page.locator("#tasks-toggle").click();
  assert.equal(
    await background.getByLabel("Command", { exact: true }).count(),
    0,
    "background tasks have no manual command form",
  );
  assert.equal(
    await background
      .getByRole("button", { name: /^(New monitor|Start monitor)$/ })
      .count(),
    0,
  );
  await page.keyboard.press("Escape");
  await background.waitFor({ state: "hidden" });
  for (const width of [1440, 768, 390, 320]) {
    await page.setViewportSize({ width, height: 960 });
    const nav = page.getByRole("navigation", { name: "Workspace shortcuts" });
    for (const button of await nav.getByRole("button").all()) {
      const r = await button.boundingBox();
      assert.ok(
        r && r.x >= 0 && r.x + r.width <= width + 1,
        "named shortcut fits " + width,
      );
      assert.ok((await button.innerText()).trim().length, "label is visible");
    }
    assert.ok(
      await page.evaluate(
        () => document.documentElement.scrollWidth <= innerWidth + 1,
      ),
      "page fits " + width,
    );
    await page.screenshot({ path: join(root, `navigation-${width}.png`) });
  }
  await page.locator("#message").fill("/monitor must-not-run");
  await page.locator("#send").click();
  await page.getByText("/monitor must-not-run", { exact: true }).waitFor();
  const afterMessage = await (await fetch(url + "/api/state")).json();
  assert.equal(
    afterMessage.runtime.monitors.length,
    state.runtime.monitors.length,
    "a literal /monitor message does not run a command",
  );
  await page.setViewportSize({ width: 1440, height: 960 });
  const stops = [];
  await page.route("**/api/stop", async (route) => {
    stops.push(route.request().postDataJSON());
    await route.fulfill({ json: { ok: true } });
  });
  for (const command of ["/stop", "/stop-team"]) {
    await page.locator("#message").fill(command);
    await page.locator("#send").click();
    await page.waitForFunction(
      () => document.querySelector("#message").value === "",
    );
  }
  assert.equal(
    stops.length,
    2,
    "both slash stop commands use the stop endpoint",
  );
  assert.equal(stops[0].descendants, false);
  assert.equal(stops[1].descendants, true);
  assert.ok(
    stops.every((request) => request.id),
    "stop requests include target",
  );
  assert.deepEqual(errors, []);
  console.log(
    "PASS chat navigation: direct sections, no manual monitor, preserved draft, idle status, labelled controls at 320/390/768/1440px. Evidence " +
      root,
  );
} finally {
  await browser?.close();
  proc.kill("SIGTERM");
}
