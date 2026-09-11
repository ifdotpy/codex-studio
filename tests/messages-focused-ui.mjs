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
  await page.locator(`[data-chat="${lead.id}"]`).click();
  await page.locator("#message").fill("Keep my main conversation draft.");
  await page.locator("#messages-toggle").click();
  const drawer = page.getByRole("dialog", { name: "Messages", exact: true });
  await drawer.waitFor();
  assert.equal(await drawer.locator(".workspace-nav").count(), 0);
  assert.equal(
    await drawer
      .getByRole("tab", { name: "For you" })
      .getAttribute("aria-selected"),
    "true",
  );
  assert.equal(await drawer.locator(".team-chat-placeholder").count(), 0);
  await drawer.locator(`[data-complaint="${leadMessage.id}"]`).click();
  let modal = page.getByRole("dialog", { name: "Message to you", exact: true });
  await modal
    .getByLabel("Reply", { exact: true })
    .fill("Unsent reply\nwith two lines");
  await modal.getByRole("button", { name: "Close", exact: true }).click();
  await drawer.locator(".mantine-Drawer-close").click();
  await page.locator("#messages-toggle").click();
  await drawer.locator(`[data-complaint="${leadMessage.id}"]`).click();
  assert.equal(
    await modal.getByLabel("Reply", { exact: true }).inputValue(),
    "Unsent reply\nwith two lines",
  );
  assert.equal(
    await modal.getByLabel("Message status", { exact: true }).count(),
    0,
  );
  const attempts = [];
  await page.route("**/api/complaints", async (route) => {
    attempts.push(route.request().postDataJSON());
    const response = await route.fetch();
    assert.equal(response.status(), 200);
    if (attempts.length === 1) await route.abort("failed");
    else await route.fulfill({ response });
  });
  await modal.getByRole("button", { name: "Send reply", exact: true }).click();
  await modal
    .getByRole("button", { name: "Retry response", exact: true })
    .waitFor();
  await modal.getByRole("button", { name: "Close", exact: true }).click();
  await drawer.locator(".mantine-Drawer-close").click();
  await page.locator("#messages-toggle").click();
  await drawer.getByLabel("Show messages to you").selectOption("all");
  await drawer.locator(`[data-complaint="${leadMessage.id}"]`).click();
  await modal
    .getByRole("button", { name: "Retry response", exact: true })
    .click();
  await modal
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
  await modal.getByRole("button", { name: "Close", exact: true }).click();
  await drawer.getByLabel("Show messages to you").selectOption("pending");
  assert.equal(
    await drawer.locator(".messages-for-you [data-complaint]").count(),
    0,
  );
  await drawer.getByRole("tab", { name: "Team", exact: true }).click();
  for (const group of ["orchestrator", "broadcast", "agents"]) {
    assert.ok(
      await drawer
        .locator(`[data-message-group="${group}"] [data-room]`)
        .count(),
      group,
    );
  }
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
    await drawer.getByRole("tab", { name: "For you", exact: true }).click();
    const tab = await drawer
      .getByRole("tab", { name: "For you", exact: true })
      .boundingBox();
    assert.ok(
      tab.x >= 0 && tab.x + tab.width <= width,
      `selected tab visible at ${width}`,
    );
    assert.ok(
      await drawer
        .locator(".messages-for-you")
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
  await stable.route("**/api/state", (route) =>
    route.fulfill({ json: quietState }),
  );
  let releaseFirst;
  const firstRead = new Promise((resolve) => {
    releaseFirst = resolve;
  });
  let reads = 0;
  let inbox = [];
  let readError = false;
  await stable.route("**/api/workspace?*", async (route) => {
    reads++;
    if (reads === 1) await firstRead;
    else await new Promise((resolve) => setTimeout(resolve, 650));
    await route.fulfill(
      readError
        ? { status: 503, json: { error: "Messages read unavailable" } }
        : { json: { inbox } },
    );
  });
  await stable.goto(origin);
  await stable.locator(`[data-chat="${lead.id}"]`).click();
  await stable.locator("#messages-toggle").click();
  const stableDrawer = stable.getByRole("dialog", {
    name: "Messages",
    exact: true,
  });
  await stableDrawer.waitFor();
  const empty = stableDrawer.getByText("No messages need your attention.", {
    exact: true,
  });
  assert.equal(
    await empty.count(),
    0,
    "no empty claim before the initial read",
  );
  releaseFirst();
  await empty.waitFor();
  await stable.evaluate(() => {
    const empty = document.querySelector(".workspace-empty");
    window.stability = { empty, failures: [] };
    window.observer = new MutationObserver(() => {
      if (!empty.isConnected) window.stability.failures.push("empty removed");
    });
    window.observer.observe(document.querySelector(".workspace-message-body"), {
      childList: true,
      subtree: true,
    });
  });
  await stable.waitForTimeout(11000);
  assert.ok(reads >= 3);
  assert.deepEqual(await stable.evaluate(() => window.stability.failures), []);
  await stable.evaluate(() => window.observer.disconnect());
  inbox = [
    {
      kind: "work",
      id: "stability-work",
      agent: lead.id,
      title: "Review the new result",
      text: "A new result needs review.",
    },
  ];
  await stableDrawer.getByRole("button", { name: "Refresh workspace" }).click();
  await stableDrawer
    .getByText("Review the new result", { exact: true })
    .waitFor();
  assert.equal(await empty.count(), 0, "real updates replace the empty state");
  readError = true;
  await stableDrawer.getByRole("button", { name: "Refresh workspace" }).click();
  await stableDrawer
    .getByRole("alert")
    .filter({ hasText: "Messages read unavailable" })
    .waitFor();
  assert.equal(
    await stableDrawer
      .getByText("Review the new result", { exact: true })
      .count(),
    1,
    "failed refresh retains the last result",
  );
  assert.equal(
    await empty.count(),
    0,
    "a read failure never means an empty inbox",
  );
  readError = false;
  await stableDrawer.getByRole("button", { name: "Refresh workspace" }).click();
  await stableDrawer.getByRole("alert").waitFor({ state: "hidden" });
  await stable.screenshot({
    path: join(root, "messages-stable-1280.png"),
    animations: "disabled",
  });
  await stableDrawer.locator(".mantine-Drawer-close").click();
  let releasePlan;
  let planError = false;
  await stable.route("**/api/plan?*", async (route) => {
    await new Promise((resolve) => {
      releasePlan = resolve;
    });
    await route.fulfill(
      planError
        ? { status: 503, json: { error: "Plan read unavailable" } }
        : { json: { plan: [] } },
    );
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
  await stable.waitForTimeout(200);
  releasePlan();
  await noPlan.waitFor();
  const planNode = await noPlan.elementHandle();
  for (const fail of [false, true]) {
    planError = fail;
    await workDrawer.getByRole("button", { name: "Refresh workspace" }).click();
    await stable.waitForTimeout(300);
    assert.ok(
      await planNode.evaluate((el) => el.isConnected),
      "empty plan survives an in-flight refresh",
    );
    releasePlan();
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
