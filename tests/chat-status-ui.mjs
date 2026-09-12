#!/usr/bin/env node
// Production renderer with isolated HTTP fixtures. No model calls or user state.
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
const browserType = process.env.BROWSER === "webkit" ? webkit : chromium;
const root = await mkdtemp(join(tmpdir(), "studio-chat-status-ui-"));
const proc = spawn(
  process.env.PYTHON || "/opt/homebrew/bin/python3",
  ["-B", join(repo, "tests/simple-ui-fixture.py"), root],
  {
    stdio: ["pipe", "pipe", "pipe"],
    env: {
      ...process.env,
      CODEX_BOARD_STATE_DIR: join(root, "board"),
      EXECUTION_SETTINGS_CATALOG: JSON.stringify(
        ["gpt-6-astra", "gpt-5.6-luna"].map((model) => ({
          model,
          defaultReasoningEffort: "medium",
          supportedReasoningEfforts: [
            "low",
            "medium",
            "high",
            "max",
            "ultra",
          ].map((reasoningEffort) => ({ reasoningEffort })),
          serviceTiers: [{ id: "priority" }],
        })),
      ),
    },
  },
);
let browser,
  page,
  log = "";
proc.stderr.on("data", (data) => {
  log += data;
});
try {
  const port = await new Promise((resolve, reject) => {
    const timer = setTimeout(
      () => reject(Error("Fixture startup timed out: " + log)),
      30000,
    );
    proc.stdout.once("data", (data) => {
      clearTimeout(timer);
      resolve(Number(String(data).trim()));
    });
    proc.once("exit", () => {
      clearTimeout(timer);
      reject(Error(log || "Fixture exited"));
    });
  });
  const origin = `http://127.0.0.1:${port}`;
  const state = await (await fetch(origin + "/api/state")).json();
  const lead = state.threads.find((agent) => agent.name === "Release lead");
  const other = state.threads.find((agent) => agent.name === "Other project");
  const worker = (index) =>
    state.threads.find(
      (agent) => agent.name === `Worker ${String(index).padStart(2, "0")}`,
    );
  for (const agent of state.threads)
    Object.assign(agent, {
      status: "idle",
      inFlight: false,
      autoWake: true,
      error: null,
      threadId: `fixture-thread-${agent.id}`,
      lastCompletedTurn: null,
      lastCompletedTurnStatus: null,
      readState: null,
      readStateSupported: true,
    });
  const complete = (agent) =>
    Object.assign(agent, {
      status: "completed",
      lastCompletedTurn: `fixture-turn-${agent.id}`,
      lastCompletedTurnStatus: "completed",
    });
  complete(lead);
  complete(other);
  complete(worker(1));
  worker(0).status = "running";
  worker(0).inFlight = true;
  worker(3).status = "failed";
  worker(3).error = "Fixture failure";
  worker(4).status = "paused";
  worker(4).autoWake = false;
  state.runtime.agents = state.threads;
  state.runtime.complaints = [];
  state.runtime.userTasks = [];
  state.runtime.tasks = [];
  state.runtime.requests = [
    {
      id: "status-question",
      agent: worker(0).id,
      epoch: worker(0).epoch,
      status: "pending",
      method: "agent/asyncQuestion",
      params: {
        questions: [{ id: "scope", question: "Which scope should I use?" }],
      },
    },
  ];
  state.runtime.monitors = [
    {
      id: "status-monitor",
      agent: worker(2).id,
      status: "running",
      command: "fixture monitor",
      created: 1,
    },
  ];
  browser = await browserType.launch({
    headless: true,
    ...(browserType === chromium
      ? {
          executablePath:
            process.env.CHROME_BIN ||
            "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
        }
      : {}),
  });
  page = await browser.newPage({
    viewport: { width: 1440, height: 980 },
  });
  page.setDefaultTimeout(10000);
  const errors = [],
    writes = [];
  page.on("pageerror", (error) => errors.push(error.message));
  await page.route("**/api/sync/**", (route) =>
    route.fulfill({
      status: 404,
      json: { error: "Fixture uses HTTP snapshots" },
    }),
  );
  await page.route("**/api/state", (route) => route.fulfill({ json: state }));
  await page.route("**/api/transcript/stream?*", (route) =>
    route.fulfill({
      status: 404,
      json: { error: "Fixture uses transcript polling" },
    }),
  );
  await page.route("**/api/transcript?*", async (route) => {
    const agent = state.threads.find(
      (value) =>
        value.id === new URL(route.request().url()).searchParams.get("id"),
    );
    const data = { truncated: false };
    if (agent)
      Object.assign(data, {
        agent,
        items: agent.lastCompletedTurn
          ? [
              {
                id: `result-${agent.id}`,
                role: "assistant",
                text: "The fixture result is complete.",
                phase: "final_answer",
                turnId: agent.lastCompletedTurn,
                created: 1,
              },
            ]
          : [],
      });
    await route.fulfill({ json: data });
  });
  await page.route("**/api/organization", async (route) => {
    const body = route.request().postDataJSON();
    if (!body.read_state) return route.continue();
    writes.push(body);
    const agent = state.threads.find((value) => value.id === body.id),
      desired = body.read_state;
    assert.equal(desired.thread_id, agent.threadId);
    assert.equal(desired.turn_id, agent.lastCompletedTurn);
    assert.equal(desired.expected_revision, agent.readState?.revision || 0);
    agent.readState = {
      threadId: desired.thread_id,
      turnId: desired.turn_id,
      read: desired.read,
      revision: desired.expected_revision + 1,
    };
    await route.fulfill({ json: agent });
  });
  await page.addInitScript(
    ({ stateDir, id }) => {
      localStorage.setItem(
        `codex-desktop-opened:${stateDir}`,
        JSON.stringify(id),
      );
      localStorage.setItem("codex-mobile-opened", JSON.stringify(id));
    },
    { stateDir: state.stateDir, id: lead.id },
  );
  await page.goto(origin);
  const row = (agent) => page.locator(`[data-chat="${agent.id}"]`);
  const status = (locator, kind) =>
    locator.locator(`[data-chat-status="${kind}"]`).waitFor();
  await row(lead).click();
  await status(row(lead), "answer");
  await status(page.locator("#conversation-title"), "answer");
  const pulse = page.locator("#conversation-title .chat-status-dot");
  assert.equal(
    await pulse.evaluate((node) => getComputedStyle(node).animationName),
    "chat-status-breathe",
  );
  await page.locator("#team-toggle").click();
  const card = (agent) => page.locator(`[data-worker="${agent.id}"]`);
  await status(card(worker(0)), "answer");
  await page.locator(".worker-group > summary").click();
  await status(card(worker(1)), "unread");
  await status(card(worker(2)), "working");
  await status(card(worker(3)), "error");
  await status(card(worker(4)), "paused");
  assert.equal(
    await card(worker(2))
      .locator('[data-chat-status="working"]')
      .getAttribute("aria-label"),
    "Waiting for a monitor",
  );
  assert.equal(
    await card(worker(2))
      .locator(".chat-status-working svg")
      .evaluate((node) => getComputedStyle(node).animationName),
    "chat-status-spin",
  );
  console.log(
    "PASS sidebar, header, worker statuses; question overrides active work; monitor spinner and question pulse",
  );
  state.runtime.requests = [];
  await page.evaluate(() => window.dispatchEvent(new Event("online")));
  await status(row(lead), "working");
  await status(page.locator("#conversation-title"), "working");
  await status(card(worker(0)), "working");
  await page.locator("#team-toggle").click();
  await status(row(other), "unread");
  await row(other).click();
  await page
    .locator("#messages [data-message]")
    .filter({ hasText: "The fixture result is complete." })
    .waitFor();
  await page.waitForFunction(
    () =>
      document.querySelector("#conversation-title [data-chat-status]") === null,
  );
  assert.equal(other.readState.read, true);
  await page.locator("#mark-unread").click();
  await status(page.locator("#conversation-title"), "unread");
  await status(row(other), "unread");
  for (let i = 0; i < 2; i++) {
    await page.evaluate(() => {
      window.dispatchEvent(new Event("online"));
      window.dispatchEvent(new Event("focus"));
    });
    await page.waitForTimeout(150);
  }
  assert.equal(other.readState.read, false);
  assert.deepEqual(
    writes
      .filter((write) => write.id === other.id)
      .map((write) => write.read_state.read),
    [true, false],
  );
  assert.equal(
    await page.locator("#conversation-title").innerText(),
    "Other project",
  );
  console.log(
    "PASS current chat acknowledges visible result; explicit Unread persists while open and after snapshots",
  );
  await page.screenshot({
    path: join(root, "desktop-status.png"),
    animations: "disabled",
  });
  await page.setViewportSize({ width: 390, height: 844 });
  await status(page.locator("#conversation-title"), "working");
  await page.locator("#sidebar-toggle").click();
  await row(other).click();
  await page.waitForFunction(
    () =>
      document.querySelector("#conversation-title")?.textContent ===
        "Other project" &&
      !document.querySelector("#conversation-title [data-chat-status]"),
  );
  await page.locator("#mark-unread").click();
  await status(page.locator("#conversation-title"), "unread");
  await page.waitForTimeout(150);
  assert.equal(
    await page.evaluate(
      () => document.documentElement.scrollWidth <= window.innerWidth,
    ),
    true,
    "The mobile page has no horizontal overflow",
  );
  assert.equal(
    await page
      .locator("header")
      .first()
      .evaluate((node) => node.scrollWidth <= node.clientWidth + 1),
    true,
    "The mobile header has no horizontal overflow",
  );
  await page.screenshot({
    path: join(root, "mobile-status.png"),
    animations: "disabled",
  });
  console.log("PASS 390px layout without horizontal overflow");
  assert.deepEqual(errors, []);
  console.log(
    JSON.stringify({
      browser: browserType.name(),
      evidence: root,
      writes: writes.length,
    }),
  );
} catch (error) {
  console.error(error);
  await page?.screenshot({ path: join(root, "failure.png") }).catch(() => {});
  throw error;
} finally {
  await page?.unrouteAll({ behavior: "ignoreErrors" });
  await browser?.close();
  if (proc.exitCode === null) {
    const exit = once(proc, "exit");
    proc.kill("SIGTERM");
    await exit;
  }
}
