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
  { stdio: ["ignore", "pipe", "pipe"] },
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
  page.on("pageerror", (e) => errors.push(e.message));
  await page.goto(origin);
  await page.locator("[data-chat]").first().waitFor();
  assert.equal(await page.locator("[data-chat]").count(), 2);
  await page.locator("[data-chat]").filter({ hasText: "Release lead" }).click();
  await page.getByText("I assigned 40 workers", { exact: false }).waitFor();
  assert.equal(await page.locator("[data-worker]").count(), 40);
  assert.equal(await page.locator("#model option").count(), 2);
  assert.equal(
    await page
      .locator('#messages img,#messages script,#messages [href^="javascript:"]')
      .count(),
    0,
  );
  assert.ok(await page.locator("#messages .prose strong").count());
  await page.locator("#requests [data-answer]").click();
  await page.locator("#answer-fields select").selectOption("One file");
  await page
    .locator("#answer-form")
    .getByRole("button", { name: "Send answer" })
    .click();
  await poll(
    async () => (await page.locator("#answer-form").count()) === 0,
    "answer submitted",
  );
  assert.match(
    await page.locator("#usage-footer").textContent(),
    /Context 40%/,
  );
  assert.match(
    await page.locator("#usage-footer").textContent(),
    /2 compactions/,
  );
  await page.locator("#message").fill("Lead draft");
  await page.locator("#worker-search").fill("Worker 07");
  await page.locator("[data-worker]").click();
  await page.locator("#message").fill("Worker draft");
  await page.locator("#back-lead").click();
  assert.equal(await page.locator("#message").inputValue(), "Lead draft");
  await page.locator("#view-toggle").click();
  assert.equal(await page.locator("[data-node]").count(), 43);
  assert.equal(await page.locator("#edges path").count(), 40);
  assert.equal(
    await page.locator("#conversation-title").textContent(),
    "All agents",
  );
  await page.locator("#fit").click();
  await page.screenshot({ path: join(root, "canvas.png") });
  await page.locator("#view-toggle").click();
  assert.equal(await page.locator("#message").inputValue(), "Lead draft");
  await page.getByRole("tab", { name: /Agents/ }).click();
  assert.equal(
    await page.locator("[data-room]").count(),
    60,
    "bounded initial room list",
  );
  await page.locator("#chat-search").fill("Release lead");
  const snapshot = await (await fetch(origin + "/api/state")).json();
  const privateRoom = snapshot.runtime.rooms.find(
    (r) =>
      r.kind === "private" &&
      r.name.includes("Worker 39") &&
      r.name.includes("Release lead"),
  );
  await page.locator(`[data-room="${privateRoom.id}"]`).click();
  await page
    .getByText("A private update before the final answer.", { exact: true })
    .waitFor();
  assert.equal(await page.locator("#composer").count(), 0);
  assert.ok(await page.locator(".message.bubble").count());
  await page.locator("#earlier-messages").click();
  await poll(
    async () => (await page.locator("[data-message]").count()) === 106,
    "earlier history",
  );
  await page.waitForTimeout(1100);
  assert.equal(await page.locator("#earlier-messages").count(), 0);
  await page.screenshot({ path: join(root, "agent-chat.png") });
  // Rename in the actual sidebar row, then verify persistence through HTTP.
  const row = page
    .locator(".sidebar-row")
    .filter({ has: page.locator(`[data-room="${privateRoom.id}"]`) });
  await row.locator("summary").click();
  await row.getByRole("button", { name: "Rename", exact: true }).click();
  await row.getByRole("textbox", { name: "Chat name" }).fill("Release notes");
  await row.getByRole("textbox", { name: "Chat name" }).press("Enter");
  await poll(
    async () =>
      (await (await fetch(origin + "/api/state")).json()).runtime.rooms.some(
        (r) => r.name === "Release notes",
      ),
    "room rename",
  );
  await page.locator("#open-complaints").click();
  await page.locator("#new-complaint").click();
  await page
    .locator("#complaint-text")
    .fill("Fixture complaint: the test log is missing.");
  await page.locator("#submit-complaint").click();
  await page
    .getByText("Fixture complaint: the test log is missing.", { exact: true })
    .waitFor();
  await page
    .locator("[data-complaint]")
    .filter({ hasText: "Fixture complaint" })
    .click();
  await page
    .getByText("The lead has not read this complaint.", { exact: true })
    .waitFor();
  await page.screenshot({ path: join(root, "complaint.png") });
  await page
    .getByRole("dialog", { name: "Complaint", exact: true })
    .getByRole("button", { name: "Close", exact: true })
    .click();
  await page.getByRole("tab", { name: "Leads", exact: true }).click();
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
  await page.locator("#new-chat").click();
  await page.locator("#toast").waitFor();
  await page.locator("#new-chat").click();
  await poll(
    async () =>
      (await page.locator("#conversation-title").textContent()) === "New chat",
    "creation retry",
  );
  await page.locator("#message").fill("Keep this draft");
  await page.locator("#new-chat").click();
  assert.equal(await page.locator("#message").inputValue(), "Keep this draft");
  const afterCreate = await (await fetch(origin + "/api/state")).json();
  assert.equal(
    afterCreate.runtime.agents.filter((a) => a.quickCreate).length,
    1,
  );
  const newLead = afterCreate.runtime.agents.find((a) => a.quickCreate);
  await page.locator("#model").selectOption("gpt-5.6-sol");
  let loseMessage = true;
  await page.route("**/api/messages", async (route) => {
    if (route.request().method() !== "POST") return route.continue();
    const response = await route.fetch();
    if (loseMessage) {
      loseMessage = false;
      await route.abort("failed");
    } else await route.fulfill({ response });
  });
  await page.locator("#message").fill("First task");
  await page.locator("#send").click();
  await poll(
    async () => (await page.locator("#toast").count()) > 0,
    "lost send visible",
  );
  await page.locator("[data-chat]").filter({ hasText: "Release lead" }).click();
  await page.locator(`[data-chat="${newLead.id}"]`).click();
  assert.equal(await page.locator("#message").inputValue(), "First task");
  await page.locator("#send").click();
  await poll(
    async () => (await page.locator("#message").inputValue()) === "",
    "message retry",
  );
  assert.equal(
    (await (await fetch(origin + "/api/messages?room=" + newLead.id)).json())
      .length,
    1,
    "no duplicate user message",
  );
  await page.locator("#message").fill("/monitor fixture-command");
  await page.locator("#send").click();
  await poll(
    async () =>
      (await page.locator("#monitors").textContent()).includes("Exit: 7"),
    "monitor",
  );
  await page.locator("#usage-footer").waitFor();
  await page.locator(".limits summary").click();
  await page.getByText("5h: 42% used", { exact: false }).waitFor();
  await page.locator(".limits summary").click();
  await page.getByText("First task", { exact: true }).waitFor();
  await page.waitForTimeout(5200);
  await page.screenshot({ path: join(root, "lead-chat.png") });
  await page.setViewportSize({ width: 390, height: 844 });
  assert.equal(await page.evaluate(() => document.body.scrollWidth), 390);
  await page.screenshot({ path: join(root, "mobile.png") });
  await page.setViewportSize({ width: 1440, height: 960 });
  const leadRow = page
    .locator(".sidebar-row")
    .filter({ has: page.locator(`[data-chat="${newLead.id}"]`) });
  await leadRow.locator("summary").click();
  await leadRow.getByRole("button", { name: "Rename", exact: true }).click();
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
  await leadRow.locator("summary").click();
  await leadRow.getByRole("button", { name: "Delete", exact: true }).click();
  await page.locator("[data-delete-chat]").click();
  await poll(
    async () =>
      !(await (await fetch(origin + "/api/state")).json()).runtime.agents.some(
        (a) => a.id === newLead.id,
      ),
    "lead deletion",
  );
  assert.deepEqual(errors, [], "no React errors");
  console.log(
    "React product UI: PASS (production build, sidebar rename/delete, agent chat, history, complaint book, model, monitor, limits, desktop/mobile)",
  );
  console.log("Browser evidence:", root);
} finally {
  await browser?.close();
  proc.kill("SIGTERM");
  if (proc.exitCode === null) await new Promise((r) => proc.once("exit", r));
  // Preserve screenshots and the isolated database for inspection.
}
