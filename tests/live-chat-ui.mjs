#!/usr/bin/env node
// Real app-server notification handlers, SQLite, event stream, and browser. No inference.
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
const root = await mkdtemp(join(tmpdir(), "codex-stream-ui-"));
const proc = spawn(
  "python3",
  ["-B", join(skill, "tests/simple-ui-fixture.py"), root],
  { stdio: ["pipe", "pipe", "pipe"] },
);
let log = "",
  browser,
  page;
proc.stderr.on("data", (d) => (log += d));
const poll = async (fn, label) => {
  for (let i = 0; i < 100; i++) {
    if (await fn()) return;
    await new Promise((r) => setTimeout(r, 50));
  }
  throw Error(label + " " + log);
};
try {
  const port = await new Promise((resolve, reject) => {
    proc.stdout.once("data", (d) => resolve(Number(String(d).trim())));
    proc.once("exit", () => reject(Error(log)));
  });
  const origin = `http://127.0.0.1:${port}`;
  const state = async () => await (await fetch(origin + "/api/state")).json();
  browser = await chromium.launch({
    executablePath:
      process.env.CHROME_BIN ||
      "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
    headless: true,
  });
  page = await browser.newPage({ viewport: { width: 1200, height: 900 } });
  const errors = [];
  let transcriptPolls = 0;
  page.on("pageerror", (e) => errors.push(e.message));
  page.on("request", (r) => {
    if (r.url().includes("/api/transcript?id=")) transcriptPolls++;
  });
  await page.goto(origin);
  await page
    .locator("[data-chat]")
    .filter({ hasText: "Other project" })
    .click();
  await page.locator("#message").fill("Exercise the live stream");
  await page.locator("#send").click();
  let agent;
  await poll(async () => {
    agent = (await state()).runtime.agents.find(
      (a) => a.name === "Other project",
    );
    return agent.status === "running" && agent.turnId;
  }, "turn starts");
  const event = (method, params) =>
    proc.stdin.write(
      JSON.stringify({
        method,
        params: { threadId: agent.threadId, turnId: agent.turnId, ...params },
      }) + "\n",
    );
  event("item/started", { item: { id: "reason-live", type: "reasoning" } });
  await page.locator('[data-phase="thinking"]').waitFor();
  event("item/started", {
    item: { id: "live-text", type: "agentMessage", text: "" },
  });
  event("item/agentMessage/delta", {
    itemId: "live-text",
    delta: "First paragraph",
  });
  await page.locator('[data-phase="writing"]').waitFor();
  assert.equal(
    await page
      .locator("#messages .prose")
      .filter({ hasText: "First paragraph" })
      .count(),
    0,
    "hold unfinished paragraph",
  );
  event("item/agentMessage/delta", {
    itemId: "live-text",
    delta: ".\n\nSecond paragraph",
  });
  await page.getByText("First paragraph.", { exact: true }).waitFor();
  await page
    .getByText("First paragraph.", { exact: true })
    .evaluate((el) => (el.dataset.retained = "yes"));
  assert.equal(
    await page.getByText("Second paragraph", { exact: true }).count(),
    0,
  );
  event("item/agentMessage/delta", {
    itemId: "live-text",
    delta: '.\n\n```json\n{"ok":true}\n\n',
  });
  await page.getByText("Second paragraph.", { exact: true }).waitFor();
  assert.equal(
    await page.locator("#messages .prose pre").count(),
    0,
    "hold open fenced code",
  );
  event("item/agentMessage/delta", {
    itemId: "live-text",
    delta: "```\nFinal tail",
  });
  await page.locator("#messages .prose pre").waitFor();
  assert.equal(
    await page
      .getByText("First paragraph.", { exact: true })
      .getAttribute("data-retained"),
    "yes",
    "prior paragraphs stay mounted",
  );
  await page.screenshot({ path: join(root, "writing.png") });
  const full =
    'First paragraph.\n\nSecond paragraph.\n\n```json\n{"ok":true}\n\n```\nFinal tail';
  event("item/completed", {
    item: { id: "live-text", type: "agentMessage", text: full },
  });
  await page.getByText("Final tail", { exact: true }).waitFor();
  const command = {
    id: "command-live",
    type: "commandExecution",
    command: "npm run test -- --reporter=json",
    cwd: "/workspace/project",
    status: "inProgress",
  };
  event("item/started", { item: command });
  await page.locator('[data-phase="tool"]').waitFor();
  const card = page.locator(".tool-card").filter({ hasText: "npm run test" });
  await card.waitFor();
  event("item/commandExecution/outputDelta", {
    itemId: command.id,
    delta:
      "Test output before exit\n" +
      JSON.stringify({ nested: { longValue: "x".repeat(4000) } }),
  });
  await card
    .locator(".tool-output")
    .filter({ hasText: "Test output before exit" })
    .waitFor();
  event("item/started", {
    item: {
      id: "second-tool",
      type: "dynamicToolCall",
      tool: "orchestration_status",
      arguments: { scope: "team" },
      status: "inProgress",
    },
  });
  event("item/completed", {
    item: {
      ...command,
      status: "completed",
      exitCode: 7,
      durationMs: 1400,
      aggregatedOutput:
        "One check failed\n" +
        JSON.stringify({ nested: { longValue: "x".repeat(4000) } }),
    },
  });
  await poll(
    () => card.getAttribute("data-tool-status").then((v) => v === "failed"),
    "nonzero exit shown as failure",
  );
  await page.locator('[data-phase="tool"]').waitFor();
  await page.screenshot({ path: join(root, "tools-desktop.png") });
  await page.setViewportSize({ width: 390, height: 844 });
  await card.locator(".tool-raw > summary").click();
  await page.screenshot({ path: join(root, "tools-mobile.png") });
  assert.equal(await page.evaluate(() => document.body.scrollWidth), 390);
  for (const box of await card.locator("pre").all()) {
    const r = await box.boundingBox();
    assert.ok(
      r && r.x >= 0 && r.x + r.width <= 390,
      "payload stays inside viewport",
    );
    assert.ok(
      await box.evaluate((e) => e.scrollWidth <= e.clientWidth + 1),
      "long JSON wraps inside card",
    );
  }
  event("item/completed", {
    item: {
      id: "second-tool",
      type: "dynamicToolCall",
      tool: "orchestration_status",
      status: "completed",
      success: true,
      contentItems: [{ type: "inputText", text: "Team check complete" }],
    },
  });
  event("turn/completed", { turn: { id: agent.turnId, status: "completed" } });
  await page.locator('[data-phase="completed"]').waitFor();
  assert.equal(
    await page.locator('.tool-card[data-tool-status="running"]').count(),
    0,
  );
  assert.equal(transcriptPolls, 0, "healthy stream does not poll transcript");
  // A reconnect replaces the snapshot instead of duplicating text.
  await page.context().setOffline(true);
  await page
    .locator(".agent-phase")
    .filter({ hasText: "Reconnecting" })
    .waitFor();
  await page.context().setOffline(false);
  await poll(
    () =>
      page
        .locator(".agent-phase")
        .textContent()
        .then((t) => !t.includes("Reconnecting")),
    "stream reconnects",
  );
  assert.equal(
    await page.getByText("First paragraph.", { exact: true }).count(),
    1,
  );
  await page.locator("#message").fill("Stop an incomplete paragraph");
  await page.locator("#send").click();
  const oldTurn = agent.turnId;
  await poll(async () => {
    agent = (await state()).runtime.agents.find((a) => a.id === agent.id);
    return agent.turnId && agent.turnId !== oldTurn;
  }, "next turn");
  event("item/agentMessage/delta", {
    itemId: "unfinished",
    delta: "Partial text retained after stop",
  });
  await page.locator('[data-phase="writing"]').waitFor();
  await page.locator("#stop").click();
  await page.locator('[data-phase="paused"]').waitFor();
  await page
    .getByText("Partial text retained after stop", { exact: true })
    .waitFor();
  assert.deepEqual(errors, []);
  console.log(
    "Live chat UI: PASS (paragraph boundaries, stable blocks, code fences, phase changes, concurrent tools, live output, failed exit, mobile JSON, reconnect, stop)",
  );
  console.log("Browser evidence:", root);
} catch (e) {
  await page?.screenshot({ path: join(root, "failure.png") });
  console.error("Evidence:", root);
  throw e;
} finally {
  await browser?.close();
  proc.kill("SIGTERM");
  if (proc.exitCode === null) await new Promise((r) => proc.once("exit", r));
}
