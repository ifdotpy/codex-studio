#!/usr/bin/env node
// Real runtime, HTTP API and browser, with deterministic app-server events.
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
const root = await mkdtemp(join(tmpdir(), "codex-background-ui-"));
const proc = spawn(
  "python3",
  ["-B", join(skill, "tests/simple-ui-fixture.py"), root],
  {
    stdio: ["pipe", "pipe", "pipe"],
    env: { ...process.env, BACKGROUND_UI_FIXTURE: "1" },
  },
);
let log = "",
  browser;
proc.stderr.on("data", (d) => (log += d));
const poll = async (fn, label) => {
  for (let i = 0; i < 150; i++) {
    if (await fn()) return;
    await new Promise((r) => setTimeout(r, 50));
  }
  throw Error(label + log);
};
try {
  const port = await new Promise((resolve, reject) => {
    proc.stdout.once("data", (d) => resolve(Number(String(d).trim())));
    proc.once("exit", () => reject(Error(log)));
  });
  const origin = `http://127.0.0.1:${port}`;
  const state = async () => (await fetch(origin + "/api/state")).json();
  const initial = await state();
  const manual = await fetch(origin + "/api/monitor", {
    method: "POST",
    headers: {
      "Content-Type": "application/json",
      "X-Canvas-Token": initial.token,
      Origin: origin,
    },
    body: JSON.stringify({
      agent: initial.runtime.agents[0].id,
      command: "must-not-run",
    }),
  });
  assert.equal(manual.status, 404, "manual monitor creation route is absent");
  browser = await chromium.launch({
    executablePath:
      process.env.CHROME_BIN ||
      "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
    headless: true,
  });
  const page = await browser.newPage({
    viewport: { width: 1440, height: 980 },
  });
  const errors = [];
  page.on("pageerror", (e) => errors.push(e.message));
  await page.goto(origin);
  await page
    .locator("[data-chat]")
    .filter({ hasText: "Other project" })
    .click();
  await page.locator("#message").fill("Check background tasks");
  await page.locator("#send").click();
  let agent;
  await poll(async () => {
    agent = (await state()).runtime.agents.find(
      (a) => a.name === "Other project",
    );
    return agent.turnId && agent.status === "running";
  }, "start turn");
  const event = (method, params) =>
    proc.stdin.write(
      JSON.stringify({
        method,
        params: { threadId: agent.threadId, turnId: agent.turnId, ...params },
      }) + "\n",
    );
  const command = {
    id: "long-build",
    type: "commandExecution",
    command: "npm run build -- --reporter=json",
    cwd: "/workspace/release",
    processId: "9042",
    status: "inProgress",
  };
  event("item/started", { item: command });
  event("item/commandExecution/outputDelta", {
    itemId: command.id,
    delta:
      "Building 48 packages\n" +
      Array.from(
        { length: 30 },
        (_, i) => `[${i + 1}/48] compiled package`,
      ).join("\n"),
  });
  event("item/started", {
    item: {
      id: "connected-tool",
      type: "mcpToolCall",
      tool: "check_deployment",
      arguments: { service: "release", environment: "staging" },
    },
  });
  proc.stdin.write(
    JSON.stringify({
      method: "fixture/agent-monitor",
      params: {
        id: crypto.randomUUID(),
        agent: agent.id,
        command: "watch-fixture --tests",
        timeout_ms: 1800000,
      },
    }) + "\n",
  );
  let monitor;
  await poll(async () => {
    monitor = (await state()).runtime.monitors.find(
      (m) => m.agent === agent.id && m.command === "watch-fixture --tests",
    );
    return monitor?.status === "running";
  }, "agent monitor starts");
  await page.getByRole("button", { name: "Chat actions", exact: true }).click();
  await page.locator("#tasks-toggle").click();
  const drawer = page.getByRole("dialog", { name: /Background tasks/ });
  await drawer.waitFor();
  assert.equal(
    await drawer
      .getByRole("button", { name: /^(New monitor|Start monitor)$/ })
      .count(),
    0,
  );
  await poll(
    async () => (await drawer.locator("[data-task]").count()) === 3,
    "only tasks from the selected chat",
  );
  const nativeRow = drawer
    .locator("[data-task]")
    .filter({ hasText: "npm run build" });
  await nativeRow.click();
  await drawer
    .locator(".task-output")
    .filter({ hasText: "Building 48 packages" })
    .waitFor();
  await page.screenshot({ path: join(root, "tasks-desktop.png") });
  const output = drawer.locator(".task-output");
  await output.evaluate((el) => {
    el.scrollTop = 0;
    el.dispatchEvent(new Event("scroll", { bubbles: true }));
  });
  await poll(
    async () =>
      (await drawer
        .getByRole("button", { name: "Follow", exact: true })
        .getAttribute("aria-pressed")) === "false",
    "manual scroll pauses follow",
  );
  event("item/commandExecution/outputDelta", {
    itemId: command.id,
    delta: "\nNew output while reading earlier lines",
  });
  await output
    .filter({ hasText: "New output while reading earlier lines" })
    .waitFor();
  assert.equal(await output.evaluate((el) => el.scrollTop), 0);
  await drawer.getByRole("button", { name: "Follow", exact: true }).click();
  await poll(
    async () =>
      output.evaluate(
        (el) => el.scrollHeight - el.scrollTop - el.clientHeight < 2,
      ),
    "follow resumes at latest output",
  );

  assert.equal(await drawer.getByLabel("Task team").count(), 0);
  assert.equal(
    await drawer
      .getByText("watch-fixture --deploy production", { exact: true })
      .count(),
    0,
  );
  assert.equal(await drawer.locator("[data-task]").count(), 3);
  await drawer.getByLabel("Task type").selectOption("tool");
  assert.equal(await drawer.locator("[data-task]").count(), 1);
  await drawer.locator(".task-input summary").click();
  await drawer
    .locator(".task-input pre")
    .filter({ hasText: "staging" })
    .waitFor();
  await drawer.getByLabel("Task type").selectOption("all");
  await drawer.getByLabel("Find a task").fill("not-a-task");
  await drawer.getByText("No matching tasks", { exact: true }).waitFor();
  await drawer.getByLabel("Find a task").fill("");
  await nativeRow.click();
  event("turn/completed", { turn: { id: agent.turnId, status: "completed" } });
  event("item/commandExecution/outputDelta", {
    itemId: command.id,
    delta:
      "\nStill active after the final answer\n" +
      JSON.stringify({ value: "x".repeat(5000) }),
  });
  await drawer
    .locator(".task-output")
    .filter({ hasText: "Still active after the final answer" })
    .waitFor();
  await page.setViewportSize({ width: 390, height: 844 });
  await drawer.locator(".task-detail").waitFor();
  const overflow = await drawer.evaluate((el) =>
    [...el.querySelectorAll("pre")]
      .filter((p) => p.getBoundingClientRect().width > 0)
      .some((p) => p.scrollWidth > p.clientWidth + 1),
  );
  assert.equal(overflow, false, "long JSON wraps within task output");
  assert.equal(
    await page.evaluate(() => document.documentElement.scrollWidth),
    390,
  );
  await page.screenshot({ path: join(root, "tasks-mobile-output.png") });
  await drawer.getByRole("button", { name: "Tasks", exact: true }).click();
  await page.screenshot({ path: join(root, "tasks-mobile-list.png") });
  await page.setViewportSize({ width: 320, height: 740 });
  assert.equal(
    await page.evaluate(() => document.documentElement.scrollWidth),
    320,
  );
  await nativeRow.click();
  assert.equal(
    await drawer
      .locator(".task-detail")
      .evaluate((el) => el.scrollWidth <= el.clientWidth),
    true,
  );
  await page.screenshot({ path: join(root, "tasks-narrow.png") });
  await page.setViewportSize({ width: 1440, height: 980 });
  event("item/completed", {
    item: {
      ...command,
      status: "completed",
      exitCode: 7,
      durationMs: 62100,
      aggregatedOutput: "Build failed\npackages/runtime: type check failed",
    },
  });
  await drawer.getByText("Exit 7", { exact: true }).waitFor();
  assert.equal(
    await drawer.locator(".task-detail").getAttribute("data-task-detail"),
    agent.id + ":long-build",
    "keep selected task open on exit",
  );
  await drawer.getByText("History", { exact: true }).click();
  await drawer
    .locator("[data-task]")
    .filter({ hasText: "npm run build" })
    .click();
  await drawer.getByText("Exit 7", { exact: true }).waitFor();
  await drawer
    .locator(".task-output")
    .filter({ hasText: "type check failed" })
    .waitFor();
  await page.screenshot({ path: join(root, "tasks-failed.png") });
  await drawer.getByText("Active", { exact: true }).click();
  await drawer
    .locator("[data-task]")
    .filter({ hasText: "watch-fixture --tests" })
    .click();
  await drawer
    .getByRole("button", { name: "Cancel monitor", exact: true })
    .click();
  await poll(
    async () =>
      (await state()).runtime.monitors.find((m) => m.id === monitor.id)
        .status === "cancelled",
    "monitor cancelled",
  );
  await drawer.getByText("History", { exact: true }).click();
  await drawer
    .locator("[data-task]")
    .filter({ hasText: "watch-fixture --tests" })
    .click();
  await drawer
    .locator(".task-detail-top")
    .getByText("Cancelled", { exact: true })
    .waitFor();
  assert.equal(
    await drawer
      .getByRole("button", { name: "Cancel monitor", exact: true })
      .count(),
    0,
  );
  await page.keyboard.press("Escape");
  await page.locator("[data-chat]").filter({ hasText: "Release lead" }).click();
  await page.getByRole("button", { name: "Chat actions", exact: true }).click();
  await page.locator("#tasks-toggle").click();
  await drawer.waitFor();
  await poll(
    async () => (await drawer.locator("[data-task]").count()) === 1,
    "other chat has its own monitor",
  );
  assert.equal(
    await drawer
      .locator("[data-task]")
      .filter({ hasText: "npm run build" })
      .count(),
    0,
  );
  assert.equal(
    await drawer
      .locator(".task-output")
      .filter({ hasText: "type check failed" })
      .count(),
    0,
  );
  await drawer.getByText("Active", { exact: true }).click();
  await drawer.locator("[data-task]").filter({ hasText: "production" }).click();
  await drawer
    .getByRole("button", { name: "Approve command", exact: true })
    .waitFor();
  await page.screenshot({ path: join(root, "tasks-approval.png") });
  await drawer
    .getByRole("button", { name: "Approve command", exact: true })
    .click();
  await drawer
    .getByText("The agent receives the result when this command exits.", {
      exact: true,
    })
    .waitFor();
  await drawer
    .getByRole("button", { name: "Cancel monitor", exact: true })
    .click();
  await drawer.getByText("No active tasks", { exact: true }).waitFor();
  await page.screenshot({ path: join(root, "tasks-empty.png") });
  await page.keyboard.press("Escape");
  await drawer.waitFor({ state: "hidden" });
  await poll(
    () =>
      page
        .getByRole("button", { name: "Chat actions", exact: true })
        .evaluate((element) => element === document.activeElement),
    "focus returns after the drawer exit transition",
  );
  assert.equal(
    await page
      .getByRole("button", { name: "Chat actions", exact: true })
      .evaluate((el) => el === document.activeElement),
    true,
    "focus returns to Chat actions",
  );
  assert.deepEqual(errors, []);
  console.log(
    "Background UI: PASS (chat isolation, live output after turn, failure, approval, cancellation, history, 320px/390px/1440px, focus, no overflow)",
  );
  console.log(root);
} finally {
  await browser?.close();
  proc.kill("SIGTERM");
  if (log) console.error(log);
}
