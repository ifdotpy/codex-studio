#!/usr/bin/env node
// Real fixture, HTTP API, and headless browser. No model calls or user state.
import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { mkdtemp } from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { createRequire } from "node:module";

const project = dirname(dirname(fileURLToPath(import.meta.url)));
const { chromium } = createRequire(join(project, "web/package.json"))(
  "playwright-core",
);
const root = await mkdtemp(join(tmpdir(), "codex-chat-scope-"));
const proc = spawn(
  "python3",
  ["-B", join(project, "tests/simple-ui-fixture.py"), root],
  {
    stdio: ["pipe", "pipe", "pipe"],
    env: {
      ...process.env,
      BACKGROUND_UI_FIXTURE: "1",
      CODEX_BOARD_STATE_DIR: join(root, "board"),
    },
  },
);
let browser, page, releaseResponse;
let log = "";
proc.stderr.on("data", (data) => (log += data));
const poll = async (check, label) => {
  for (let i = 0; i < 200; i++) {
    if (await check()) return;
    await new Promise((resolve) => setTimeout(resolve, 50));
  }
  throw Error(label + "\n" + log);
};

try {
  const port = await new Promise((resolve, reject) => {
    const timeout = setTimeout(
      () => reject(Error("Fixture startup timeout\n" + log)),
      30000,
    );
    proc.stdout.once("data", (data) => {
      clearTimeout(timeout);
      resolve(Number(String(data).trim()));
    });
    proc.once("exit", () => {
      clearTimeout(timeout);
      reject(Error(log));
    });
  });
  const origin = `http://127.0.0.1:${port}`;
  const initial = await (await fetch(origin + "/api/state")).json();
  const lead = initial.runtime.agents.find(
    (agent) => agent.name === "Release lead",
  );
  const other = initial.runtime.agents.find(
    (agent) => agent.name === "Other project",
  );
  const worker = initial.runtime.agents.find(
    (agent) => agent.name === "Worker 07",
  );
  assert.equal(lead.cwd, other.cwd, "different chats share the same folder");
  assert.equal(
    lead.accountKey,
    other.accountKey,
    "different chats share the same account",
  );

  browser = await chromium.launch({
    headless: true,
    executablePath:
      process.env.CHROME_BIN ||
      "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
  });
  page = await browser.newPage({ viewport: { width: 1440, height: 980 } });
  page.setDefaultTimeout(10000);
  const errors = [];
  page.on("pageerror", (error) => errors.push(error.message));
  await page.goto(origin);
  const drawer = page.locator(".workspace-drawer .mantine-Drawer-content");
  const background = page.getByRole("dialog", { name: /Background tasks/ });
  const selectChat = async (name, covered = false) => {
    const button = page.locator("[data-chat]").filter({ hasText: name });
    // Trigger the app's navigation handler while a modal covers the sidebar.
    if (covered) await button.evaluate((node) => node.click());
    else await button.click();
    await page.getByRole("heading", { name, exact: true }).waitFor();
  };
  const openSection = async (section) => {
    await page.locator(`[data-workspace-section="${section}"]`).click();
    await drawer
      .getByRole("heading", {
        name: section === "inbox" ? "Inbox" : "Changes",
        exact: true,
      })
      .waitFor();
  };
  const closeWorkspace = async () => {
    await page.keyboard.press("Escape");
    await drawer.waitFor({ state: "hidden" });
  };
  const checkOtherInbox = async () => {
    await drawer
      .getByText("No questions or unresolved problems.", { exact: true })
      .waitFor();
    assert.equal(await drawer.locator("[data-answer]").count(), 0);
    assert.equal(
      await drawer.getByText("Which scope?", { exact: true }).count(),
      0,
    );
    assert.equal(
      await drawer.getByText("Worker 07", { exact: true }).count(),
      0,
    );
    assert.equal(await drawer.locator(".workspace-count").count(), 0);
  };
  const options = () =>
    drawer.getByLabel("Agent", { exact: true }).locator("option");
  const checkOptions = async (ids) => {
    const values = await options().evaluateAll((nodes) =>
      nodes.map((node) => node.value),
    );
    assert.deepEqual(
      values.sort(),
      ["", ...ids].sort(),
      "agent choices contain only the current chat tree",
    );
  };

  await selectChat("Other project");
  assert.equal(
    await page.locator("#workspace-toggle .attention-count").count(),
    0,
  );
  assert.equal(await page.locator("#tasks-toggle .tasks-count").count(), 0);
  await openSection("inbox");
  await checkOtherInbox();
  await closeWorkspace();
  let changesScope;
  const legacyChanges = async (route) => {
    const params = new URL(route.request().url()).searchParams;
    if (params.get("agent") !== other.id) return route.continue();
    changesScope = params.get("scope");
    await route.fulfill({
      json: {
        git: true,
        diff: "diff --git a/foreign.txt b/foreign.txt\n+++ b/foreign.txt\n@@ -0,0 +1 @@\n+FOREIGN CHAT CHANGE",
        files: [{ path: "foreign.txt", status: "M" }],
      },
    });
  };
  await page.route("**/api/changes?**", legacyChanges);
  await openSection("changes");
  await checkOptions([other.id]);
  await drawer
    .getByText("Chat changes require the updated server.", { exact: true })
    .waitFor();
  assert.equal(changesScope, "chat", "Changes requests explicit chat scope");
  assert.equal(
    await drawer.getByRole("region", { name: "Changes diff" }).count(),
    0,
  );
  assert.equal(
    await drawer.getByText("FOREIGN CHAT CHANGE", { exact: false }).count(),
    0,
  );
  assert.equal(
    await drawer.getByRole("button", { name: /foreign\.txt/ }).count(),
    0,
  );
  await closeWorkspace();
  await page.unroute("**/api/changes?**", legacyChanges);

  await selectChat("Release lead");
  assert.equal(
    await page.locator("#workspace-toggle .attention-count").innerText(),
    "3",
  );
  assert.equal(
    await page.locator("#tasks-toggle .tasks-count").innerText(),
    "1",
  );
  await openSection("changes");
  const members = initial.runtime.agents.filter(
    (agent) => agent.id === lead.id || agent.rootId === lead.id,
  );
  assert.equal(members.length, 41);
  await checkOptions(members.map((agent) => agent.id));
  await drawer.getByLabel("Agent", { exact: true }).selectOption(worker.id);
  await drawer.getByRole("button", { name: "Open chat", exact: false }).click();
  await drawer.waitFor({ state: "hidden" });
  await openSection("inbox");
  await drawer
    .locator(".workspace-inbox-group .workspace-row")
    .filter({ hasText: "Worker 07" })
    .waitFor();
  assert.equal(await drawer.locator(".workspace-count").innerText(), "2");
  await drawer.locator('[data-answer="async-question"]').click();
  const answer = page.getByRole("dialog", {
    name: "Reply to the agent",
    exact: true,
  });
  await answer.getByText("Which scope?", { exact: true }).waitFor();
  await selectChat("Other project", true);
  await answer.waitFor({ state: "hidden" });
  await checkOtherInbox();
  await closeWorkspace();

  await selectChat("Release lead");
  await page.locator("#tasks-toggle").click();
  await background
    .locator("[data-task]")
    .filter({ hasText: "watch-fixture --deploy production" })
    .click();
  await background.locator(".task-detail").waitFor();
  await selectChat("Other project", true);
  await poll(
    async () => (await background.locator("[data-task]").count()) === 0,
    "background rows reset after a chat change",
  );
  assert.equal(
    await background.locator(".task-detail").count(),
    0,
    "old task detail closes after a chat change",
  );
  assert.equal(
    await background
      .getByText("watch-fixture --deploy production", { exact: true })
      .count(),
    0,
  );
  await page.keyboard.press("Escape");
  await background.waitFor({ state: "hidden" });

  // Hold a real response until the selected chat has changed.
  let captured = false,
    delivered = false;
  const gate = new Promise((resolve) => (releaseResponse = resolve));
  await page.route("**/api/workspace?**", async (route) => {
    if (
      captured ||
      new URL(route.request().url()).searchParams.get("agent") !== lead.id
    ) {
      return route.continue();
    }
    captured = true;
    const response = await route.fetch();
    await gate;
    await route.fulfill({ response });
    delivered = true;
  });
  await selectChat("Release lead");
  await openSection("inbox");
  await poll(() => captured, "capture Release lead workspace response");
  await selectChat("Other project", true);
  await checkOtherInbox();
  releaseResponse();
  await poll(() => delivered, "deliver obsolete workspace response");
  await page.evaluate(
    () =>
      new Promise((resolve) =>
        requestAnimationFrame(() => requestAnimationFrame(resolve)),
      ),
  );
  await checkOtherInbox();
  assert.equal(
    await page.locator("#workspace-toggle .attention-count").count(),
    0,
  );
  assert.deepEqual(errors, []);
  console.log(
    "PASS chat scope: same folder/account isolation, 41-member agent selector, legacy changes rejection, descendant Inbox, scoped counters, modal/detail reset, stale workspace response. Evidence: " +
      root,
  );
} catch (error) {
  await page?.screenshot({ path: join(root, "failure.png"), fullPage: true });
  console.error("Evidence:", root);
  throw error;
} finally {
  releaseResponse?.();
  await browser?.close();
  proc.kill("SIGTERM");
}
