#!/usr/bin/env node
// Browser checks against the production React build, real HTTP and isolated SQLite.
import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { createRequire } from "node:module";
const skill = dirname(dirname(fileURLToPath(import.meta.url)));
const require = createRequire(join(skill, "web/package.json"));
const { chromium } = require("playwright-core");
const root = await mkdtemp(join(tmpdir(), "codex-react-ui-"));
const proc = spawn(
  "python3",
  ["-B", join(skill, "tests/simple-ui-fixture.py"), root],
  { stdio: ["pipe", "pipe", "pipe"] },
);
let log = "",
  browser;
proc.stderr.on("data", (d) => (log += d));
const poll = async (fn, label) => {
  for (let i = 0; i < 100; i++) {
    if (await fn()) return;
    await new Promise((r) => setTimeout(r, 100));
  }
  throw Error(label + " " + log);
};
try {
  const port = await new Promise((resolve, reject) => {
    proc.stdout.once("data", (d) => resolve(Number(String(d).trim())));
    proc.once("exit", () => reject(Error(log)));
  });
  const origin = `http://127.0.0.1:${port}`;
  browser = await chromium.launch({
    executablePath:
      process.env.CHROME_BIN ||
      "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
    headless: true,
    args: ["--disable-extensions", "--no-first-run"],
  });
  const page = await browser.newPage({
    viewport: { width: 1440, height: 960 },
  });
  const errors = [];
  const visibleInViewport = async (locator) => {
    await locator.waitFor({ state: "visible" });
    await poll(async () => {
      const r = await locator.boundingBox();
      const v = page.viewportSize();
      return (
        r &&
        r.x >= 0 &&
        r.y >= 0 &&
        r.x + r.width <= v.width + 1 &&
        r.y + r.height <= v.height + 1
      );
    }, "control fits the viewport");
  };
  const shot = async (name) => {
    await page.evaluate(async () => {
      await new Promise(requestAnimationFrame);
      await new Promise(requestAnimationFrame);
    });
    await page.screenshot({
      path: join(root, name + ".png"),
      animations: "disabled",
    });
  };
  page.on("pageerror", (e) => errors.push(e.message));
  globalThis.testPage = page;
  await page.goto(origin);
  await page.locator("[data-chat]").first().waitFor();
  assert.equal(await page.locator("[data-chat]").count(), 2);
  await page.locator("[data-chat]").filter({ hasText: "Release lead" }).click();
  await page.getByText("I assigned 40 workers", { exact: false }).waitFor();
  assert.equal(await page.locator("[data-worker]").count(), 0);
  await page.locator("#team-toggle").click();
  assert.equal(await page.locator("[data-worker]").count(), 40);
  await page
    .getByRole("button", { name: "Chat settings", exact: true })
    .click();
  await page
    .getByRole("button", { name: "Main agent settings", exact: true })
    .click();
  await page.locator("#model").waitFor();
  assert.equal(await page.locator("#model option").count(), 2);
  await page.keyboard.press("Escape");
  await page.keyboard.press("Escape");
  assert.equal(
    await page
      .locator('#messages img,#messages script,#messages [href^="javascript:"]')
      .count(),
    0,
  );
  assert.ok(await page.locator("#messages .prose strong").count());
  const activity = page.locator(".tool-group").first();
  if (!(await activity.evaluate((node) => node.open)))
    await activity.locator(":scope > summary").click();
  const tool = activity.locator(".tool-card").first();
  if (!(await tool.evaluate((node) => node.open)))
    await tool.locator(":scope > summary").click();
  await activity.getByText("Hidden tool fixture", { exact: false }).waitFor();
  await activity.locator(":scope > summary").click();
  await page.locator("#requests [data-answer]").click();
  const answerDialog = page.getByRole("form", { name: "Reply to the agent" });
  await visibleInViewport(answerDialog);
  await shot("question");
  await answerDialog
    .getByRole("textbox", { name: "Which scope?" })
    .press("Escape");
  await answerDialog.waitFor({ state: "hidden" });
  await poll(
    () =>
      page
        .locator("[data-answer]")
        .evaluate((el) => el === document.activeElement),
    "focus returns to Answer",
  );
  await page.locator("[data-answer]").click();
  await answerDialog
    .getByRole("button", { name: "One file", exact: true })
    .click();
  await page
    .locator(".request-answer-form")
    .getByRole("button", { name: "Send answer" })
    .click();
  await poll(
    async () => (await page.locator(".request-answer-form").count()) === 0,
    "answer submitted",
  );
  assert.match(
    await page.locator("#usage-footer").textContent(),
    /Context 40%/,
  );
  await page.getByRole("button", { name: "Chat context", exact: true }).click();
  await page
    .getByText("2 automatic context summaries.", { exact: true })
    .waitFor();
  await page.keyboard.press("Escape");
  await page.screenshot({ path: join(root, "ai-conversation.png") });
  assert.equal(
    await page
      .locator(
        ".ai-conversation .bubble, #chat-list .mantine-Avatar-root, #chat-list time",
      )
      .count(),
    0,
  );
  await page.locator("#message").fill("Lead draft");
  await page.locator("#worker-search").fill("Worker 07");
  await page.locator("[data-worker]").click();
  await page.locator("#message").fill("Worker draft");
  await page.locator("#back-lead").click();
  assert.equal(await page.locator("#message").inputValue(), "Lead draft");
  assert.equal(await page.locator("#view-toggle").count(), 0);
  assert.equal(await page.locator("[data-node]").count(), 0);
  assert.equal(await page.locator("#open-complaints").count(), 0);
  await page.locator("#messages-toggle").click();
  await page.getByRole("tab", { name: "Team", exact: true }).click();
  assert.equal(
    await page.locator("[data-room]").count(),
    60,
    "bounded initial room list",
  );
  await page
    .getByRole("textbox", { name: "Search team chats" })
    .fill("Release lead");
  const snapshot = await (await fetch(origin + "/api/state")).json();
  const privateRoom = snapshot.runtime.rooms.find(
    (r) =>
      r.kind === "private" &&
      r.name.includes("Worker 39") &&
      r.name.includes("Release lead"),
  );
  await page.locator(`[data-room="${privateRoom.id}"]`).click();
  await page
    .locator(".team-room-messages")
    .getByText("A private update before the final answer.", { exact: true })
    .waitFor();
  assert.equal(await page.locator("#message").inputValue(), "Lead draft");
  assert.ok(await page.locator(".team-message").count());
  await page
    .getByRole("button", { name: "Earlier messages", exact: true })
    .click();
  await poll(
    async () => (await page.locator(".team-message").count()) === 106,
    "earlier history",
  );
  await page.waitForTimeout(1100);
  assert.equal(
    await page
      .getByRole("button", { name: "Earlier messages", exact: true })
      .count(),
    0,
  );
  await page.locator(".team-room-messages").evaluate((el) => {
    el.scrollTop = el.scrollHeight;
  });
  await page.screenshot({ path: join(root, "agent-chat.png") });
  await page.keyboard.press("Escape");
  await page.locator("[data-chat]").filter({ hasText: "Release lead" }).click();
  // Creation reply lost after the database commit.
  let lose = true;
  await page.route("**/api/leads", async (route) => {
    const response = await route.fetch();
    if (lose) {
      lose = false;
      await route.abort("failed");
    } else await route.fulfill({ response });
  });
  await page.locator(".project-tree-heading").first().hover();
  await page
    .getByRole("button", { name: /^New chat in / })
    .first()
    .click();
  await page.locator("#toast").waitFor();
  await page.locator(".project-tree-heading").first().hover();
  await page
    .getByRole("button", { name: /^New chat in / })
    .first()
    .click();
  await poll(
    async () =>
      (await page.locator("#conversation-title").textContent()) === "New chat",
    "creation retry",
  );
  const afterRetry = await (await fetch(origin + "/api/state")).json();
  const retryLeads = afterRetry.runtime.agents.filter((a) => a.quickCreate);
  assert.equal(retryLeads.length, 1, "creation retry retains one chat");
  const newLead = retryLeads[0];
  await page.locator("#message").fill("Keep this draft");
  await page.locator(".project-tree-heading").first().hover();
  await page
    .getByRole("button", { name: /^New chat in / })
    .first()
    .click();
  await poll(
    async () => (await page.locator("#message").inputValue()) === "",
    "new chat has a separate draft",
  );
  const afterCreate = await (await fetch(origin + "/api/state")).json();
  assert.equal(
    afterCreate.runtime.agents.filter((a) => a.quickCreate).length,
    2,
    "a new action creates a separate chat",
  );
  await page.locator(`[data-chat="${newLead.id}"]`).click();
  assert.equal(await page.locator("#message").inputValue(), "Keep this draft");
  await page
    .getByRole("button", { name: "Chat settings", exact: true })
    .click();
  await page
    .getByRole("button", { name: "Main agent settings", exact: true })
    .click();
  await page.locator("#model").selectOption("gpt-5.6-sol");
  await page.keyboard.press("Escape");
  await page.keyboard.press("Escape");
  let loseMessage = true,
    messageAttempts = 0;
  await page.route("**/api/messages", async (route) => {
    if (route.request().method() !== "POST") return route.continue();
    messageAttempts++;
    const response = await route.fetch();
    if (loseMessage) {
      loseMessage = false;
      await route.abort("failed");
    } else await route.fulfill({ response });
  });
  await page.locator("#message").fill("First task");
  await page.locator("#send").click();
  await poll(() => !loseMessage, "message response was lost after commit");
  await poll(
    async () => (await page.locator("#message").inputValue()) === "",
    "message retained in the outbox",
  );
  await page.locator("[data-chat]").filter({ hasText: "Release lead" }).click();
  await page.locator(`[data-chat="${newLead.id}"]`).click();
  assert.equal(await page.locator("#message").inputValue(), "");
  await page
    .locator("#messages")
    .getByText("First task", { exact: true })
    .waitFor();
  await poll(() => messageAttempts >= 2, "outbox retried the lost response");
  assert.equal(
    (await (await fetch(origin + "/api/messages?room=" + newLead.id)).json())
      .length,
    1,
    "no duplicate user message",
  );
  assert.equal(await page.locator('[data-action="monitor"]').count(), 0);
  proc.stdin.write(
    JSON.stringify({
      method: "fixture/agent-monitor",
      params: {
        id: crypto.randomUUID(),
        agent: newLead.id,
        command: "fixture-command",
        timeout_ms: 1800000,
      },
    }) + "\n",
  );
  await poll(
    async () =>
      (await (await fetch(origin + "/api/state")).json()).runtime.monitors.some(
        (monitor) =>
          monitor.agent === newLead.id &&
          monitor.command === "fixture-command" &&
          monitor.exitCode === 7,
      ),
    "agent monitor completes",
  );
  await page.getByRole("button", { name: "Chat actions", exact: true }).click();
  await page.locator("#tasks-toggle").click();
  await page.getByText("History", { exact: true }).click();
  await page
    .locator("[data-task]")
    .filter({ hasText: "fixture-command" })
    .click();
  await page.getByText("Exit 7", { exact: true }).waitFor();
  await page.keyboard.press("Escape");
  await page
    .getByRole("dialog", { name: /Background tasks/ })
    .waitFor({ state: "hidden" });
  await page.locator("#usage-footer").waitFor();
  await page.locator(".limits-toggle").click();
  await page.getByText("5h", { exact: true }).waitFor();
  await page.getByText("58% left", { exact: true }).waitFor();
  await visibleInViewport(page.getByRole("dialog"));
  await shot("limits");
  await page.locator(".limits-toggle").click();
  await page
    .locator("#messages")
    .getByText("First task", { exact: true })
    .waitFor();
  await page.waitForTimeout(5200);
  await page.screenshot({ path: join(root, "lead-chat.png") });
  await page.setViewportSize({ width: 390, height: 844 });
  assert.equal(await page.evaluate(() => document.body.scrollWidth), 390);
  await page.screenshot({ path: join(root, "mobile.png") });
  await page.locator("#messages-toggle").click();
  await visibleInViewport(
    page.getByRole("dialog", { name: "Messages", exact: true }),
  );
  await shot("mobile-messages");
  await page.keyboard.press("Escape");
  await page
    .getByRole("dialog", { name: "Messages", exact: true })
    .waitFor({ state: "hidden" });
  await page.locator("#sidebar-toggle").click();
  await page
    .locator("#sidebar [data-chat]")
    .filter({ hasText: "Release lead" })
    .click();
  await page.locator("#sidebar").waitFor({ state: "hidden" });
  await page.locator("#messages .prose table").waitFor();
  await page.setViewportSize({ width: 320, height: 640 });
  await visibleInViewport(page.locator("#send"));
  assert.equal(await page.evaluate(() => document.body.scrollWidth), 320);
  await shot("narrow-ai-chat");
  await page.locator("#message").fill("A long draft.\n".repeat(60));
  await visibleInViewport(page.locator("#send"));
  await visibleInViewport(page.locator("#message"));
  await shot("narrow-draft");
  await page.locator("#message").fill("Lead draft");
  await page
    .getByRole("button", { name: "Account limits", exact: true })
    .click();
  await visibleInViewport(
    page.getByRole("region", { name: "Account limits details", exact: true }),
  );
  await shot("narrow-limits");
  await page.keyboard.press("Escape");
  await page.setViewportSize({ width: 1440, height: 960 });
  await page.locator(`[data-chat="${newLead.id}"]`).click();
  const leadRow = page
    .locator(".sidebar-row")
    .filter({ has: page.locator(`[data-chat="${newLead.id}"]`) });
  await leadRow.hover();
  await leadRow.locator(".row-actions").click();
  await page.getByRole("menuitem", { name: "Rename", exact: true }).click();
  await leadRow
    .getByRole("textbox", { name: "Chat name" })
    .fill("Renamed lead");
  await leadRow.getByRole("textbox", { name: "Chat name" }).press("Enter");
  await poll(
    async () =>
      (await page.locator("#conversation-title").textContent()) ===
      "Renamed lead",
    "lead rename",
  );
  await leadRow.hover();
  await leadRow.locator(".row-actions").click();
  await page.getByRole("menuitem", { name: "Delete", exact: true }).click();
  await page.locator("[data-delete-chat]").click();
  await poll(
    async () =>
      !(await (await fetch(origin + "/api/state")).json()).runtime.agents.some(
        (a) => a.id === newLead.id,
      ),
    "lead deletion",
  );
  // Read long message content at a wide viewport, then check an empty lead.
  await page.locator("[data-chat]").filter({ hasText: "Release lead" }).click();
  await page.setViewportSize({ width: 1920, height: 1080 });
  await page.locator("#messages .prose table").waitFor();
  await shot("wide-ai-chat");
  await page
    .locator("[data-chat]")
    .filter({ hasText: "Other project" })
    .click();
  await page.locator(".empty-chat").waitFor();
  await shot("empty-chat");
  assert.deepEqual(errors, [], "no React errors");
  console.log(
    "React product UI: PASS (production build, sidebar rename/delete, agent chat, history, unified messages, model, agent monitor, limits, desktop/mobile)",
  );
  console.log("Browser evidence:", root);
} catch (error) {
  await globalThis.testPage?.screenshot({ path: join(root, "failure.png") });
  console.error("Failure evidence:", root);
  throw error;
} finally {
  await browser?.close();
  proc.kill("SIGTERM");
  if (proc.exitCode === null) await new Promise((r) => proc.once("exit", r));
  // Preserve screenshots and the isolated database for inspection.
}
