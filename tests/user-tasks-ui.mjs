#!/usr/bin/env node
// UI contract with deterministic HTTP fixtures. Backend tests prove notification delivery.
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
const root = await mkdtemp(join(tmpdir(), "codex-user-tasks-ui-"));
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
const now = Date.now() / 1000;
const lead = {
  id: "lead",
  rootId: "lead",
  isLead: true,
  source: "managed",
  status: "completed",
  name: "Release lead",
  model: "gpt-5.6-sol",
  created: now,
  canSend: true,
  autoWake: true,
};
const worker = {
  ...lead,
  id: "worker",
  isLead: false,
  parentId: "lead",
  name: "Build reviewer",
};
const other = { ...lead, id: "other", rootId: "other", name: "Other team" };
const makeTask = (id, agent, title) => ({
  id,
  agent: agent.id,
  rootId: agent.rootId,
  title,
  description: "Open the staging release and check the sign-in flow.",
  criteria: "Sign in and attach the result URL.",
  status: "open",
  version: 1,
  created: now,
  updated: now,
  reason: "",
  completionNote: "",
  history: [{ action: "create", text: "", actor: agent.id, at: now }],
});
const task = makeTask("release-check", lead, "Check the staging release");
const stoppedTask = makeTask(
  "stopped-task",
  lead,
  "Confirm the release window",
);
const outsideTask = makeTask(
  "other-task",
  other,
  "Private task for another team",
);
const state = {
  token: "fixture-token",
  stateDir: root,
  threads: [lead, worker, other],
  chats: [],
  runtime: {
    agents: [lead, worker, other],
    rooms: [],
    complaints: [],
    monitors: [],
    requests: [],
    tasks: [],
    userTasks: [task, stoppedTask, outsideTask],
  },
};
const writes = [];
let failNext = false,
  browser,
  page;
try {
  browser = await chromium.launch({
    executablePath:
      process.env.CHROME_BIN ||
      "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
    headless: true,
  });
  page = await browser.newPage({ viewport: { width: 1440, height: 980 } });
  const errors = [];
  page.on("pageerror", (e) => {
    errors.push(e.message);
    console.error(e.stack);
  });
  await page.route("**/api/**", async (route) => {
    const req = route.request(),
      url = new URL(req.url()),
      path = url.pathname;
    let value = {};
    if (path.startsWith("/api/sync/"))
      return route.fulfill({ status: 404, body: "Legacy fixture" });
    if (path === "/api/state") value = state;
    else if (path === "/api/accounts")
      value = { accounts: [], defaultAccountKey: "" };
    else if (path === "/api/voice/records")
      value = { records: [], cursor: 0, delivered: [] };
    else if (path === "/api/transcript/stream")
      return route.fulfill({ status: 503, body: "Use fixture polling" });
    else if (path === "/api/transcript")
      value = {
        items: [],
        agent:
          state.threads.find((a) => a.id === url.searchParams.get("agent")) ||
          lead,
      };
    else if (path === "/api/limits") value = { data: null };
    else if (path === "/api/workspace")
      value = {
        inbox: [
          {
            id: task.id,
            kind: "user_task",
            agent: lead.id,
            title: task.title,
            text: "Your action is needed",
          },
        ],
      };
    else if (path === "/api/user-tasks/complete") {
      const body = req.postDataJSON();
      assert.equal(req.headers()["x-canvas-token"], "fixture-token");
      writes.push(body);
      if (failNext) {
        failNext = false;
        return route.fulfill({
          status: 503,
          json: { error: "Temporary connection failure. Try again." },
        });
      }
      const record = state.runtime.userTasks.find((t) => t.id === body.task_id);
      assert.equal(body.version, record.version);
      assert.equal(record.status, "open");
      Object.assign(record, {
        status: "review",
        version: record.version + 1,
        completionNote: body.note,
        completedAt: Date.now() / 1000,
        delivery: record.id === stoppedTask.id ? "cancelled" : "pending",
        agentStopped: record.id === stoppedTask.id,
      });
      record.history.push({
        action: "complete",
        text: body.note,
        actor: "user",
        at: Date.now() / 1000,
      });
      value = record;
    }
    await route.fulfill({ json: value });
  });
  await page.goto(`http://127.0.0.1:${server.address().port}`);
  await page.locator('[data-chat="lead"]').click();
  await page.getByRole("button", { name: "Chat actions", exact: true }).click();
  await page.locator('[data-workspace-section="user-tasks"]').click();
  const compact = page.getByRole("region", { name: "Your tasks", exact: true });
  assert.equal(
    await page.locator(".user-tasks-compact").count(),
    0,
    "Desktop tasks have one entry point",
  );
  await compact.locator("[data-user-task]").first().waitFor();
  assert.equal(await compact.locator("[data-user-task]").count(), 2);
  assert.equal(await compact.getByText(outsideTask.title).count(), 0);
  const row = compact.locator('[data-user-task="release-check"]');
  await row.getByRole("button", { name: task.title, exact: true }).click();
  await row.getByText(task.criteria, { exact: true }).waitFor();
  await row
    .getByLabel("Result note (optional)")
    .fill("Verified: https://staging.example/result");
  failNext = true;
  await row.getByRole("button", { name: "Send for review" }).click();
  await row.getByRole("alert").waitFor();
  assert.equal(
    await row.getByLabel("Result note (optional)").inputValue(),
    "Verified: https://staging.example/result",
  );
  assert.equal(await row.getByRole("checkbox").count(), 0);
  await row.getByRole("button", { name: "Send for review" }).click();
  await row.getByText("Agent reviewing", { exact: true }).waitFor();
  assert.equal(writes[0].id, writes[1].id, "Retry uses the same operation ID");
  assert.equal(
    await row.getByRole("button", { name: "Send for review" }).count(),
    0,
  );
  assert.equal(await row.getByRole("checkbox").count(), 0);
  task.agentStopped = true;
  await row
    .getByRole("status")
    .filter({ hasText: "Saved; agent stopped" })
    .waitFor();
  task.agentStopped = false;
  await row.getByRole("status").waitFor({ state: "detached" });
  Object.assign(task, {
    status: "open",
    version: 3,
    reason: "The result lacks the screenshot. Add the screenshot URL.",
    updated: Date.now() / 1000,
  });
  task.history.push({
    action: "return",
    text: task.reason,
    actor: lead.id,
    at: Date.now() / 1000,
  });
  await row
    .locator(".user-task-reason")
    .getByText(task.reason, { exact: true })
    .waitFor();
  assert.equal(
    await row.getByRole("button", { name: "Send for review" }).isEnabled(),
    true,
  );
  await row
    .getByLabel("Result note (optional)")
    .fill("Screenshot: https://staging.example/screenshot.png");
  await row.getByRole("button", { name: "Send for review" }).click();
  await row.getByText("Agent reviewing", { exact: true }).waitFor();
  Object.assign(task, {
    status: "accepted",
    version: 5,
    reason: "The screenshot confirms the release works.",
    updated: Date.now() / 1000,
  });
  task.history.push({
    action: "accept",
    text: task.reason,
    actor: lead.id,
    at: Date.now() / 1000,
  });
  await row.waitFor({ state: "detached" });
  const stoppedRow = compact.locator('[data-user-task="stopped-task"]');
  await stoppedRow.locator(".user-task-toggle").click();
  await stoppedRow.getByRole("button", { name: "Send for review" }).click();
  await stoppedRow
    .getByText("Saved; agent stopped. Your result waits for the agent.")
    .waitFor();

  const full = page.getByRole("region", { name: "Your tasks", exact: true });
  await full.getByLabel("Task status").selectOption("accepted");
  await full.getByRole("button", { name: task.title, exact: true }).click();
  await full
    .locator(".user-task-criteria")
    .getByText(task.reason, { exact: true })
    .waitFor();
  await full.locator("summary").click();
  assert.equal(
    await full.getByRole("button", { name: "Send for review" }).count(),
    0,
  );
  assert.equal(
    await full.getByRole("button", { name: /accept/i }).count(),
    0,
    "Only the orchestrator can accept",
  );
  await full.getByLabel("Task status").selectOption("all");
  await full.getByLabel("Search your tasks").fill("another team");
  assert.equal(await full.locator("[data-user-task]").count(), 0);
  assert.equal(
    await full.getByText(outsideTask.title, { exact: true }).count(),
    0,
  );
  await full.getByLabel("Search your tasks").fill(task.title);
  assert.equal(await full.locator("[data-user-task]").count(), 1);
  await full.getByText(task.title, { exact: true }).waitFor();
  await full.getByLabel("Search your tasks").fill("");
  await full.getByLabel("Task owner").selectOption("lead");
  assert.equal(await full.locator("[data-user-task]").count(), 2);
  assert.equal(
    await full
      .getByLabel("Task owner")
      .locator('option[value="worker"]')
      .count(),
    0,
  );
  await full.getByRole("button", { name: task.title, exact: true }).click();
  await page.screenshot({ path: join(root, "user-tasks-desktop.png") });
  await full.getByRole("button", { name: "Open agent chat" }).click();
  await page
    .locator("#conversation-title")
    .filter({ hasText: "Release lead" })
    .waitFor();
  await page.setViewportSize({ width: 390, height: 844 });
  await full.waitFor({ state: "detached" });
  await page
    .locator("#conversation-title")
    .filter({ hasText: "Release lead" })
    .waitFor();
  assert.equal(
    await page.evaluate(() => document.documentElement.scrollWidth),
    390,
  );
  await page.screenshot({ path: join(root, "user-tasks-mobile.png") });
  await page.setViewportSize({ width: 320, height: 740 });
  assert.equal(
    await page.evaluate(() => document.documentElement.scrollWidth),
    320,
  );
  assert.equal(
    await page
      .getByRole("region", { name: "Tasks for you from this team" })
      .evaluate((el) => el.scrollWidth <= el.clientWidth + 1),
    true,
    "Task content stays within the panel",
  );
  assert.deepEqual(errors, []);
  console.log(
    JSON.stringify({
      ok: true,
      evidence: root,
      checks: [
        "team isolation",
        "completion note",
        "safe retry",
        "explicit review action",
        "orchestrator return",
        "orchestrator acceptance",
        "stopped delivery",
        "team history and search",
        "owner filter",
        "mobile overflow",
      ],
    }),
  );
} catch (error) {
  if (page)
    await page.screenshot({ path: join(root, "failure.png") }).catch(() => {});
  throw error;
} finally {
  if (browser) await browser.close();
  await new Promise((r) => server.close(r));
}
