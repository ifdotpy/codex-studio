#!/usr/bin/env node
// Real runtime records, lost HTTP response, and responsive message navigation.
import assert from "node:assert/strict";
import { spawn, execFileSync } from "node:child_process";
import { mkdtemp } from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { createRequire } from "node:module";
const repo = dirname(dirname(fileURLToPath(import.meta.url)));
const { chromium } = createRequire(join(repo, "web/package.json"))(
  "playwright-core",
);
const root = await mkdtemp(join(tmpdir(), "studio-messages-ui-"));
const fixture = spawn(
  "python3",
  ["-B", join(repo, "tests/simple-ui-fixture.py"), root],
  {
    stdio: ["pipe", "pipe", "pipe"],
    env: { ...process.env, MESSAGES_UI_FIXTURE: "1" },
  },
);
let browser,
  page,
  log = "";
fixture.stderr.on("data", (data) => {
  log += data;
});
try {
  const port = await new Promise((resolve, reject) => {
    fixture.stdout.once("data", (data) => resolve(Number(String(data).trim())));
    fixture.once("exit", () => reject(Error(log)));
  });
  const origin = `http://127.0.0.1:${port}`;
  const state = await (await fetch(origin + "/api/state")).json();
  const lead = state.threads.find((a) => a.name === "Release lead");
  const records = state.runtime.complaints;
  assert.equal(records.length, 2);
  assert.equal(records.find((c) => c.author === lead.id).recipient, "user");
  assert.equal(records.find((c) => c.author !== lead.id).recipient, "lead");
  const leadMessage = records.find((c) => c.author === lead.id);
  const direct = state.runtime.rooms.find(
    (r) => r.kind === "private" && r.members.includes(lead.id),
  );
  browser = await chromium.launch({
    headless: true,
    executablePath:
      process.env.CHROME_BIN ||
      "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
  });
  page = await browser.newPage({ viewport: { width: 1280, height: 900 } });
  page.setDefaultTimeout(10000);
  const errors = [];
  page.on("pageerror", (error) => errors.push(error.message));
  await page.goto(origin);
  const leadChat = page.locator(`[data-chat="${lead.id}"]`);
  await leadChat.click();
  await page.waitForFunction(
    (id) =>
      document
        .querySelector(`[data-chat="${CSS.escape(id)}"]`)
        ?.getAttribute("aria-current") === "true",
    lead.id,
  );
  await page.locator("#message").fill("Keep my main conversation draft.");
  await page.locator("#messages-toggle").click();
  const drawer = page.getByRole("dialog", { name: "Messages", exact: true });
  await drawer.waitFor();
  assert.equal(await drawer.locator(".workspace-nav").count(), 0);
  const youRow = drawer.locator('.team-room-row[data-room="you"]');
  await youRow.waitFor();
  assert.equal(
    await youRow.getAttribute("aria-pressed"),
    "true",
    "Messages opens on the For you conversation",
  );
  assert.equal(await drawer.locator(".team-chat-empty").count(), 0);
  const complaint = drawer.locator(`[data-complaint="${leadMessage.id}"]`);
  await complaint.getByRole("button", { name: "Reply", exact: true }).click();
  await complaint
    .getByLabel("Reply", { exact: true })
    .fill("Unsent reply\nwith two lines");
  await complaint
    .getByRole("button", { name: "Close reply", exact: true })
    .click();
  await drawer.locator(".mantine-Drawer-close").click();
  await page.locator("#messages-toggle").click();
  await complaint.getByRole("button", { name: "Reply", exact: true }).click();
  assert.equal(
    await complaint.getByLabel("Reply", { exact: true }).inputValue(),
    "Unsent reply\nwith two lines",
  );
  const attempts = [];
  await page.route("**/api/complaints", async (route) => {
    attempts.push(route.request().postDataJSON());
    const response = await route.fetch();
    assert.equal(response.status(), 200);
    if (attempts.length === 1) await route.abort("failed");
    else await route.fulfill({ response });
  });
  await complaint
    .getByRole("button", { name: "Send reply", exact: true })
    .click();
  await complaint
    .getByRole("button", { name: "Retry response", exact: true })
    .waitFor();
  await drawer.locator(".mantine-Drawer-close").click();
  await page.locator("#messages-toggle").click();
  const retry = complaint.getByRole("button", {
    name: "Retry response",
    exact: true,
  });
  const replyToggle = complaint.getByRole("button", {
    name: "Reply",
    exact: true,
  });
  const replyState = await Promise.race([
    retry.waitFor().then(() => "retry"),
    replyToggle.waitFor().then(() => "closed"),
  ]);
  if (replyState === "closed") await replyToggle.click();
  await retry.waitFor();
  await complaint
    .getByRole("button", { name: "Retry response", exact: true })
    .click();
  await complaint
    .getByRole("button", { name: "Send reply", exact: true })
    .waitFor();
  assert.equal(attempts.length, 2);
  assert.deepEqual(
    attempts[1],
    attempts[0],
    "retry preserves the request after drawer unmount",
  );
  const saved = await (
    await fetch(origin + "/api/complaint?id=" + leadMessage.id)
  ).json();
  assert.equal(saved.responses.length, 1);
  assert.equal(saved.responses[0].id, `user:${attempts[0].id}`);
  assert.equal(saved.responses[0].author, "user");
  const delivery = JSON.parse(
    execFileSync(
      "python3",
      [
        "-c",
        `import sqlite3,json,sys
c=sqlite3.connect(sys.argv[1]); c.row_factory=sqlite3.Row
print(json.dumps([dict(r) for r in c.execute("select * from runtime_events where kind='complaint_response'")]))`,
        join(root, "canvas.sqlite3"),
      ],
      { encoding: "utf8" },
    ),
  );
  assert.equal(delivery.length, 1);
  assert.equal(
    delivery[0].agent,
    leadMessage.author,
    "response reaches the lead who sent the message",
  );
  await complaint
    .getByRole("button", { name: "Close reply", exact: true })
    .click();
  const broadcast = state.runtime.rooms.find(
    (room) => room.kind === "broadcast" && room.rootId === lead.id,
  );
  assert.ok(broadcast);
  if (!(await drawer.locator(`[data-room="${broadcast.id}"]`).count()))
    await drawer
      .getByRole("button", { name: "More chats", exact: true })
      .click();
  await drawer.locator(`[data-room="${broadcast.id}"]`).waitFor();
  await drawer.locator(`[data-room="${direct.id}"]`).waitFor();
  await drawer.locator(`[data-room="${direct.id}"]`).click();
  await drawer
    .locator(".team-message")
    .filter({ hasText: "A private update before the final answer." })
    .waitFor();
  await drawer
    .getByRole("button", { name: "Earlier messages", exact: true })
    .click();
  await drawer
    .locator(".team-message")
    .filter({ hasText: "Earlier finding 0" })
    .waitFor();
  await drawer
    .getByRole("button", { name: "Latest messages", exact: true })
    .click();
  await youRow.click();
  for (const width of [1280, 390, 320]) {
    await page.setViewportSize({ width, height: 900 });
    await page.waitForTimeout(500);
    if (!(await drawer.isVisible())) {
      if (await page.locator("#messages-toggle").isVisible())
        await page.locator("#messages-toggle").click();
      else {
        await page
          .getByRole("button", { name: "Chat settings", exact: true })
          .click();
        await page
          .getByRole("dialog", { name: "Chat settings" })
          .getByRole("button", { name: "Messages", exact: true })
          .click();
      }
    }
    if (!(await youRow.isVisible()))
      await drawer
        .getByRole("button", { name: "Back to chats", exact: true })
        .click();
    await youRow.waitFor();
    assert.equal(await youRow.getAttribute("aria-pressed"), "true");
    const row = await youRow.boundingBox();
    assert.ok(
      row.x >= 0 && row.x + row.width <= width,
      `selected conversation visible at ${width}`,
    );
    assert.ok(
      await drawer
        .locator(".unified-message-scroll")
        .evaluate((el) => el.scrollWidth <= el.clientWidth + 1),
    );
    await page.screenshot({
      path: join(root, `messages-${width}.png`),
      animations: "disabled",
    });
  }
  // Slow reads must not change the layout when the server data is unchanged.
  const stable = await browser.newPage({
    viewport: { width: 1280, height: 900 },
  });
  stable.on("pageerror", (error) => errors.push(error.message));
  const quietState = structuredClone(state);
  quietState.runtime.requests = [];
  quietState.runtime.complaints = [];
  await stable.route("**/api/sync/identity", (route) =>
    route.fulfill({ status: 404, json: { error: "Fixture uses polling" } }),
  );
  await stable.route(/\/api\/state(?:\?.*)?$/, (route) =>
    route.fulfill({ json: quietState }),
  );
  quietState.runtime.userTasks = [];
  quietState.runtime.work = [];
  quietState.runtime.monitors = [];
  quietState.runtime.rules = [];
  quietState.threads = quietState.threads.map((agent) => ({
    ...agent,
    status: "completed",
  }));
  let reads = 0;
  await stable.route("**/api/workspace?*", async (route) => {
    reads++;
    await route.fulfill({
      status: 503,
      json: { error: "Unrelated workspace read unavailable" },
    });
  });
  await stable.goto(origin);
  await stable.locator(`[data-chat="${lead.id}"]`).click();
  await stable.locator("#messages-toggle").click();
  const stableDrawer = stable.getByRole("dialog", {
    name: "Messages",
    exact: true,
  });
  await stableDrawer.waitFor();
  const empty = stableDrawer.getByText("No messages yet.", {
    exact: true,
  });
  await empty.waitFor();
  await stable.evaluate(() => {
    const empty = document.querySelector(".team-chat-empty");
    window.stability = { empty, failures: [] };
    window.observer = new MutationObserver(() => {
      if (!empty.isConnected) window.stability.failures.push("empty removed");
    });
    window.observer.observe(document.querySelector(".workspace-message-body"), {
      childList: true,
      subtree: true,
    });
  });
  await stable.waitForTimeout(5500);
  assert.equal(
    reads,
    0,
    "Messages uses the existing snapshot, not the full workspace API",
  );
  assert.deepEqual(await stable.evaluate(() => window.stability.failures), []);
  await stable.evaluate(() => window.observer.disconnect());
  quietState.runtime.requests = [
    {
      id: "stability-request",
      createdAt: Date.now() / 1000,
      method: "agent/asyncQuestion",
      agent: lead.id,
      epoch: 0,
      status: "pending",
      params: {
        questions: [{ id: "0", question: "Review the new result" }],
      },
    },
  ];
  await stableDrawer.getByRole("button", { name: "Refresh messages" }).click();
  await stableDrawer
    .locator("p.request-prompt")
    .getByText("Review the new result", { exact: true })
    .waitFor();
  assert.equal(
    await empty.count(),
    0,
    "snapshot updates replace the empty state",
  );
  assert.equal(reads, 0);
  await stable.screenshot({
    path: join(root, "messages-stable-1280.png"),
    animations: "disabled",
  });
  await stableDrawer.locator(".mantine-Drawer-close").click();
  let planError = false;
  let planReads = 0,
    concurrentPlans = 0,
    maxPlans = 0;
  await stable.route("**/api/plan?*", async (route) => {
    planReads++;
    concurrentPlans++;
    maxPlans = Math.max(maxPlans, concurrentPlans);
    await new Promise((resolve) =>
      setTimeout(resolve, planReads === 1 ? 6500 : 400),
    );
    await route.fulfill(
      planError
        ? { status: 503, json: { error: "Plan read unavailable" } }
        : { json: { plan: [] } },
    );
    concurrentPlans--;
  });
  await stable.getByRole("button", { name: /^(Chat actions|More)$/ }).click();
  await stable.locator('[data-workspace-section="plan"]').click();
  const workDrawer = stable.getByRole("dialog", {
    name: "Workspace",
    exact: true,
  });
  await workDrawer.waitFor();
  const noPlan = workDrawer.getByText("This agent has not reported a plan.", {
    exact: true,
  });
  assert.equal(await noPlan.count(), 0, "initial load does not claim no plan");
  await noPlan.waitFor({ timeout: 9000 });
  assert.equal(
    maxPlans,
    1,
    "slow reads coalesce instead of overlapping or losing results",
  );
  await stable.waitForTimeout(500);
  const planNode = await noPlan.elementHandle();
  for (const fail of [false, true]) {
    planError = fail;
    const planResponse = stable.waitForResponse((response) =>
      response.url().includes("/api/plan?"),
    );
    await workDrawer.getByRole("button", { name: "Refresh workspace" }).click();
    await stable.waitForTimeout(100);
    assert.ok(
      await planNode.evaluate((el) => el.isConnected),
      "empty plan survives an in-flight refresh",
    );
    await planResponse;
    if (fail) await workDrawer.getByRole("alert").waitFor();
    await stable.waitForTimeout(100);
    assert.ok(
      await planNode.evaluate((el) => el.isConnected),
      "last settled empty plan survives refresh failure",
    );
  }
  await stable.close();
  assert.deepEqual(errors, []);
  console.log(
    JSON.stringify({
      ok: true,
      evidence: root,
      cases: [
        "reply draft survives close",
        "durable same-payload retry",
        "single empty state",
        "refresh stable",
        "Plan initial/refresh/error",
        "recipient groups",
        "room pagination",
        "mobile visible tabs",
      ],
    }),
  );
} catch (error) {
  await page?.screenshot({ path: join(root, "failure.png") });
  console.error("Evidence:", root, error);
  throw error;
} finally {
  await browser?.close();
  fixture.kill("SIGTERM");
}
