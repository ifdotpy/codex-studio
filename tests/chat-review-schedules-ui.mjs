import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { once } from "node:events";
import { mkdtemp } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { createRequire } from "node:module";

const repo = join(import.meta.dirname, "..");
const { chromium, webkit } = createRequire(join(repo, "web/package.json"))(
  "playwright-core",
);
const engine = process.env.BROWSER === "webkit" ? webkit : chromium;
const root = await mkdtemp(join(tmpdir(), "studio-review-schedules-"));
const server = spawn(
  process.env.PYTHON || "/opt/homebrew/bin/python3",
  ["-B", join(repo, "tests/simple-ui-fixture.py"), root],
  {
    env: { ...process.env, CODEX_BOARD_STATE_DIR: join(root, "board") },
    stdio: ["pipe", "pipe", "pipe"],
  },
);
let log = "",
  browser;
server.stderr.on("data", (chunk) => {
  log += chunk;
});
try {
  const port = await new Promise((resolve, reject) => {
    const timer = setTimeout(
      () => reject(Error(log || "Fixture timeout")),
      30000,
    );
    server.stdout.once("data", (chunk) => {
      clearTimeout(timer);
      resolve(Number(String(chunk).trim()));
    });
    server.once("exit", () => {
      clearTimeout(timer);
      reject(Error(log || "Fixture exited"));
    });
  });
  const origin = `http://127.0.0.1:${port}`;
  const snapshot = async () =>
    (await fetch(origin + "/api/state?view=chat")).json();
  const state = await snapshot();
  const target = state.threads.find((agent) => agent.name === "Release lead");
  const reviewer = state.threads.find((agent) => agent.name === "Worker 38");
  assert(target && reviewer);
  browser = await engine.launch({
    headless: true,
    ...(engine === chromium
      ? {
          executablePath:
            process.env.CHROME_BIN ||
            "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
        }
      : {}),
  });
  const page = await browser.newPage({
    viewport: { width: 1280, height: 900 },
    // Keep lost-response injection observable after the reload.
    serviceWorkers: "block",
  });
  page.setDefaultTimeout(15000);
  const errors = [];
  page.on("pageerror", (error) => errors.push(error.message));
  await page.addInitScript(
    ({ stateDir, id }) => {
      localStorage.setItem(
        `codex-desktop-opened:${stateDir}`,
        JSON.stringify(id),
      );
      localStorage.setItem("codex-mobile-opened", JSON.stringify(id));
    },
    { stateDir: state.stateDir, id: reviewer.id },
  );
  const writes = [];
  let expectedTarget = target.id;
  let loseNextReply = false;
  await page.route("**/api/organization", async (route) => {
    const body = route.request().postDataJSON();
    if (!body.review_schedule) return route.continue();
    assert.equal(body.id, expectedTarget, "The selected chat is the review target");
    assert.equal(
      body.review_schedule.reviewer_id,
      reviewer.id,
      "The current chat receives the scheduled review",
    );
    writes.push(body);
    if (loseNextReply) {
      loseNextReply = false;
      const response = await route.fetch();
      assert.equal(response.status(), 200);
      return route.abort("failed");
    }
    return route.continue();
  });
  await page.goto(origin);
  const settings = async () => {
    await page
      .getByRole("button", { name: "Chat settings", exact: true })
      .click();
    await page.getByRole("region", { name: "Chat reviews" }).waitFor();
  };
  await settings();
  await page.getByText("Review other chats", { exact: true }).waitFor();
  const region = page.getByRole("region", { name: "Chat reviews" });
  assert.equal(
    await region
      .getByLabel("Review interval (minutes)", { exact: true })
      .inputValue(),
    "30",
  );
  await region
    .getByLabel("Chat to review", { exact: true })
    .fill("Other project");
  assert.equal(
    await page.getByRole("option", { name: /Other project/ }).count(),
    1,
  );
  await region
    .getByLabel("Chat to review", { exact: true })
    .fill("Release lead");
  await page.getByRole("option", { name: /Release lead/ }).click();
  await region.getByRole("button", { name: "Add chat", exact: true }).click();
  const row = region.locator(`[data-review-target="${target.id}"]`);
  await row.waitFor();
  let saved = (await snapshot()).threads.find((agent) => agent.id === target.id)
    .reviewSchedules[0];
  assert.equal(saved.reviewerId, reviewer.id);
  assert.equal(
    (await snapshot()).threads.find((agent) => agent.id === reviewer.id)
      .reviewSchedules?.length || 0,
    0,
    "The reviewer does not receive a reversed target schedule",
  );
  assert.equal(saved.intervalMinutes, 30);
  assert.equal(saved.enabled, true);
  assert.equal(saved.lastRunAt, null);
  const firstNext = saved.nextAt;
  await row.getByLabel(/Interval for/).fill("45");
  await row.getByRole("button", { name: "Save interval" }).click();
  await page.waitForFunction(
    (id) =>
      document.querySelector(`[data-review-target="${id}"] input`)?.value ===
      "45",
    target.id,
  );
  await row.getByRole("button", { name: "Pause", exact: true }).click();
  await row.getByRole("button", { name: "Resume", exact: true }).waitFor();
  await row.getByRole("button", { name: "Resume", exact: true }).click();
  await row.getByRole("button", { name: "Pause", exact: true }).waitFor();
  await row.getByRole("button", { name: "Discussion", exact: true }).click();
  await page.getByRole("tab", { name: "Team", exact: true }).waitFor();
  assert.equal(
    await page
      .getByRole("tab", { name: "Team", exact: true })
      .getAttribute("aria-selected"),
    "true",
  );
  await page.locator('[data-message-group="reviews"]').waitFor();
  await page.screenshot({ path: join(root, "discussion.png") });
  console.log(
    "PASS defaults, assign, interval, pause/resume, reviewer-owned assignment, same-team discussion and cross-team target selection",
  );

  await page.reload();
  await settings();
  await row.waitFor();
  assert.equal(await row.getByLabel(/Interval for/).inputValue(), "45");
  await row.getByRole("button", { name: "Remove", exact: true }).click();
  await row.waitFor({ state: "detached" });
  await region
    .getByLabel("Chat to review", { exact: true })
    .fill("Other project");
  assert.equal(
    await page.getByRole("option", { name: /Other project/ }).count(),
    1,
  );
  await region
    .getByLabel("Chat to review", { exact: true })
    .fill("Release lead");
  await page.getByRole("option", { name: /Release lead/ }).click();
  loseNextReply = true;
  await region.getByRole("button", { name: "Add chat", exact: true }).click();
  await region.getByRole("alert").waitFor();
  assert.equal(loseNextReply, false);
  await row.waitFor();
  saved = (await snapshot()).threads
    .find((agent) => agent.id === target.id)
    .reviewSchedules.find((item) => item.reviewerId === reviewer.id);
  assert.equal(saved.removed, false);
  assert.equal(saved.enabled, true);
  assert(saved.nextAt >= firstNext);
  assert.equal(
    (await snapshot()).threads.find((agent) => agent.id === target.id)
      .reviewSchedules.length,
    1,
  );
  console.log(
    "PASS reload, remove/re-add, lost save response without duplicate schedule",
  );

  const foreign = state.threads.find((agent) => agent.name === "Other project");
  assert(foreign);
  expectedTarget = foreign.id;
  await region.getByLabel("Chat to review", { exact: true }).fill("Other project");
  await page.getByRole("option", { name: /Other project/ }).click();
  await region.getByRole("button", { name: "Add chat", exact: true }).click();
  await page.locator(`[data-review-target="${foreign.id}"]`).waitFor();
  const foreignSchedule = (await snapshot()).threads.find((agent) => agent.id === foreign.id)
    .reviewSchedules.find((item) => item.reviewerId === reviewer.id);
  assert.equal(foreignSchedule.enabled, true);
  assert.deepEqual(foreignSchedule.authorizedRoots, {
    [foreign.id]: foreign.rootId, [reviewer.id]: reviewer.rootId,
  });
  console.log("PASS cross-team assignment saves the exact reviewer and target");

  await page.setViewportSize({ width: 390, height: 844 });
  await page.screenshot({ path: join(root, "mobile-settings.png") });
  assert(
    await region.evaluate((node) => node.scrollWidth <= node.clientWidth + 1),
  );
  assert(
    await page.evaluate(
      () => document.documentElement.scrollWidth <= innerWidth,
    ),
  );
  assert.deepEqual(errors, []);
  console.log(
    JSON.stringify({
      browser: engine.name(),
      evidence: root,
      writes: writes.length,
    }),
  );
} finally {
  await browser?.close();
  server.kill("SIGTERM");
  await once(server, "exit").catch(() => {});
}
