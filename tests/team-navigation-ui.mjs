#!/usr/bin/env node
// Production client and an isolated fixture. No model calls or user state.
import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { mkdtemp } from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { createRequire } from "node:module";

const repo = dirname(dirname(fileURLToPath(import.meta.url)));
const { chromium } = createRequire(join(repo, "web/package.json"))(
  "playwright-core",
);
const root = await mkdtemp(join(tmpdir(), "codex-team-navigation-"));
const proc = spawn(
  "python3",
  ["-B", join(repo, "tests/simple-ui-fixture.py"), root],
  {
    stdio: ["pipe", "pipe", "pipe"],
    env: { ...process.env, CODEX_BOARD_STATE_DIR: join(root, "board") },
  },
);
let browser,
  page,
  log = "";
proc.stderr.on("data", (data) => (log += data));
try {
  const port = await new Promise((resolve, reject) => {
    const timeout = setTimeout(
      () => reject(Error("Fixture timeout\n" + log)),
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
  const lead = initial.threads.find((agent) => agent.name === "Release lead");
  const workers = initial.threads.filter(
    (agent) => agent.rootId === lead.id && !agent.isLead,
  );
  const worker = (n) =>
    workers.find(
      (agent) => agent.name === `Worker ${String(n).padStart(2, "0")}`,
    );
  const longName =
    "Check approval and interrupted worker navigation across a large release team";
  const overrides = new Map([
    ...workers.map((agent) => {
      const index = Number(agent.name.split(" ")[1]);
      return [
        agent.id,
        {
          status:
            index === 7
              ? "failed"
              : index < 8
                ? "running"
                : index < 25
                  ? "queued"
                  : "completed",
        },
      ];
    }),
    [worker(1).id, { status: "starting" }],
    [worker(2).id, { status: "approval", name: longName }],
    [worker(3).id, { status: "interrupted" }],
    [worker(4).id, { status: "waiting" }],
  ]);
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
  await page.route("**/api/state", async (route) => {
    const response = await route.fetch();
    const data = await response.json();
    for (const agent of data.threads)
      Object.assign(agent, overrides.get(agent.id));
    await route.fulfill({ response, json: data });
  });
  await page.goto(origin);
  const selectLead = () =>
    page.locator("[data-chat]").filter({ hasText: "Release lead" }).click();
  await selectLead();
  const team = page.getByRole("complementary", { name: "Team", exact: true });
  const search = team.getByRole("searchbox", { name: "Find a worker" });
  const row = (n) => team.locator(`[data-worker="${worker(n).id}"]`);
  const groupIds = (name) =>
    team
      .getByRole("region", { name, exact: true })
      .locator("[data-worker]")
      .evaluateAll((nodes) => nodes.map((node) => node.dataset.worker));
  assert.deepEqual(
    (await groupIds("Attention")).sort(),
    [worker(2).id, worker(3).id, worker(7).id].sort(),
  );
  assert.deepEqual(
    (await groupIds("Working")).sort(),
    [0, 1, 5, 6].map((n) => worker(n).id).sort(),
  );
  assert.equal((await groupIds("Waiting")).length, 18);
  assert.equal(await team.locator(".worker-group").getAttribute("open"), null);
  assert.equal(
    await row(39).isVisible(),
    false,
    "completed workers start collapsed",
  );
  await team.getByRole("button", { name: "Attention 3", exact: true }).click();
  assert.equal(await team.locator("[data-worker]").count(), 3);
  await search.fill("Worker 39");
  await row(39).waitFor({ state: "visible" });
  assert.equal(
    await team.locator("[data-worker]").count(),
    1,
    "search includes completed workers even from the Attention filter",
  );
  await row(39).click();
  assert.equal(await row(39).getAttribute("aria-current"), "page");
  await page.getByRole("button", { name: "Back to lead", exact: true }).click();
  assert.equal(
    await page.locator("#conversation-title").innerText(),
    "Release lead",
  );
  await search.fill("no matching worker");
  await team.getByText("No workers match your search.").waitFor();
  await search.fill("");
  assert.equal(
    await team
      .getByRole("button", { name: "Attention 3" })
      .getAttribute("aria-pressed"),
    "true",
  );
  await team.getByRole("button", { name: "Active 5", exact: true }).click();
  assert.equal(await team.locator("[data-worker]").count(), 5);
  assert.equal(await row(3).count(), 0, "interrupted workers are not active");
  await team.getByRole("button", { name: "All 40", exact: true }).click();
  await row(2).click();
  assert.equal(await row(2).getAttribute("aria-current"), "page");
  assert.equal(
    await page
      .getByRole("button", { name: "Back to lead", exact: true })
      .count(),
    1,
  );
  assert.equal(
    await page
      .getByRole("button", { name: "Subagent settings", exact: true })
      .count(),
    1,
  );
  const title = row(2).locator("strong");
  const titleLayout = await title.evaluate((node) => ({
    text: node.textContent,
    clipped: node.scrollWidth > node.clientWidth,
    height: node.getBoundingClientRect().height,
    lineHeight: parseFloat(getComputedStyle(node).lineHeight),
  }));
  assert.equal(titleLayout.text, longName);
  assert.equal(titleLayout.clipped, false);
  assert.ok(
    titleLayout.height > titleLayout.lineHeight * 2,
    "the full worker name wraps",
  );
  await page.screenshot({ path: join(root, "team-desktop.png") });
  await team.locator("#workers").evaluate((node) => {
    node.scrollTop = node.scrollHeight;
  });
  assert.equal(
    await search.isVisible(),
    true,
    "search stays outside the worker scroll area",
  );
  await search.fill("Worker 39");
  await row(39).waitFor({ state: "visible" });
  await page
    .locator("[data-chat]")
    .filter({ hasText: "Other project" })
    .click();
  assert.equal(await team.count(), 0, "another chat cannot show this team");
  await selectLead();
  assert.equal(
    await search.inputValue(),
    "",
    "changing the chat clears its worker search",
  );
  assert.equal(
    await team
      .getByRole("button", { name: "All 40" })
      .getAttribute("aria-pressed"),
    "true",
  );
  await page.setViewportSize({ width: 390, height: 844 });
  await page.locator("#team-toggle").click();
  await search.waitFor({ state: "visible" });
  await search.fill("approval");
  await row(2).waitFor();
  assert.equal(
    await row(2).evaluate((node) => node.scrollWidth <= node.clientWidth),
    true,
  );
  await page.waitForTimeout(350);
  await page.screenshot({ path: join(root, "team-mobile.png") });
  await row(2).click();
  await team.waitFor({ state: "hidden" });
  await page.getByRole("button", { name: "Back to lead", exact: true }).click();
  assert.equal(
    await page.locator("#conversation-title").innerText(),
    "Release lead",
  );
  assert.deepEqual(errors, []);
  console.log(
    JSON.stringify({
      passed: true,
      groups: true,
      filters: true,
      completedSearch: true,
      selection: true,
      chatIsolation: true,
      mobile: true,
      screenshots: root,
    }),
  );
} catch (error) {
  await page?.screenshot({ path: join(root, "failure.png") });
  console.error("Failure evidence:", root);
  throw error;
} finally {
  await browser?.close();
  proc.kill("SIGTERM");
  if (proc.exitCode === null)
    await new Promise((resolve) => proc.once("exit", resolve));
}
