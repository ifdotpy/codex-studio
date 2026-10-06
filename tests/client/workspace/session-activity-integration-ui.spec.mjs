import {
  readTestState,
  entityPullFixture,
  test,
  browserExecutablePath,
  spawnFixture as spawn,
} from "../playwright.mjs";
import assert from "node:assert/strict";
import { fileURLToPath } from "node:url";
import { mkdtemp } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { createRequire } from "node:module";
test("Session activity integration", async () => {
  test.setTimeout(180_000);
  const testRepo = fileURLToPath(new URL("../../../", import.meta.url));
  const repo = testRepo;
  const { chromium, webkit } = createRequire(join(repo, "web/package.json"))(
    "playwright-core",
  );
  const engine = process.env.BROWSER === "webkit" ? webkit : chromium;
  const root = await mkdtemp(
    join(tmpdir(), "studio-session-activity-integration-"),
  );
  const proc = spawn(
    process.env.PYTHON || "python3",
    ["-B", join(repo, "tests/simple-ui-fixture.py"), root],
    {
      stdio: ["pipe", "pipe", "pipe"],
      env: { ...process.env, CODEX_BOARD_STATE_DIR: join(root, "board") },
    },
  );
  let browser,
    log = "";
  proc.stderr.on("data", (v) => (log += v));
  try {
    const port = await new Promise((resolve, reject) => {
      proc.stdout.once("data", (v) => resolve(Number(String(v).trim())));
      proc.once("exit", () => reject(Error(log)));
    });
    const origin = `http://127.0.0.1:${port}`;
    const state = await readTestState(origin);
    const lead = state.threads.find((a) => a.name === "Release lead");
    const other = state.threads.find((a) => a.name === "Other project");
    const child = state.threads.find((a) => a.rootId === lead.id && !a.isLead);
    const agents = state.threads.map((agent) => ({
      ...agent,
      status: agent.id === child.id ? "paused" : "completed",
      inFlight: false,
      turnId: null,
      autoWake: agent.id !== child.id,
      lastCompletedTurn: null,
      ...(agent.id === child.id ? { name: "webcrypto-globals" } : {}),
    }));
    const task = {
      id: "old-live-command",
      agent: child.id,
      status: "running",
      kind: "command",
      name: "commandExecution",
      command:
        "find /Users/igor -path */node_modules/@babel/parser/package.json -print",
      created: Date.now() / 1000 - 14 * 3600,
      processId: "fixture-session",
    };
    const entityState = {
      threads: agents,
      chats: state.chats,
      runtime: {
        ...state.runtime,
        agents,
        tasks: [
          task,
          {
            ...task,
            id: "newer-command",
            command: "second active command",
            created: Date.now() / 1000,
          },
        ],
        monitors: [],
        requests: [],
      },
    };
    browser = await engine.launch({
      headless: true,
      ...(engine === chromium
        ? {
            executablePath: browserExecutablePath,
          }
        : {}),
    });
    const page = await browser.newPage({
      viewport: { width: 1280, height: 900 },
      serviceWorkers: "block",
    });
    page.setDefaultTimeout(12000);
    const errors = [],
      detailReads = [];
    page.on("pageerror", (e) => errors.push(e.message));
    const workspaceId = "session-activity-fixture";
    await page.route("**/api/sync/identity", (r) =>
      r.fulfill({ json: { workspaceId } }),
    );
    await page.route("**/api/sync/pull?*", (r) => {
      const after = Number(
        new URL(r.request().url()).searchParams.get("after") || 0,
      );
      return r.fulfill({
        json: { workspaceId, ...entityPullFixture(entityState, after) },
      });
    });
    await page.route("**/api/transcript/stream?*", (r) =>
      r.fulfill({ status: 404, json: { error: "Polling fixture" } }),
    );
    await page.route("**/api/transcript?*", (r) => {
      const id = new URL(r.request().url()).searchParams.get("id");
      return r.fulfill({
        json: { agent: state.threads.find((a) => a.id === id), items: [] },
      });
    });
    await page.route("**/api/task?*", (r) => {
      const id = new URL(r.request().url()).searchParams.get("id");
      detailReads.push(id);
      return r.fulfill({
        json: {
          ...state.runtime.tasks.find((t) => t.id === id),
          tail: "The command remains active.",
        },
      });
    });
    let historyTasks = [
      task,
      { ...task, id: "newer-command", created: Date.now() / 1000 },
    ];
    let taskFeedReads = 0;
    await page.route("**/api/workspace/tasks?*", (r) => {
      taskFeedReads++;
      return r.fulfill({ json: { tasks: historyTasks, monitors: [] } });
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
    const strip = page.getByRole("region", { name: "Session activity" });
    const row = strip.locator('[data-activity-id="old-live-command"]');
    await row.waitFor();
    assert.match(await row.innerText(), /webcrypto-globals/);
    assert.match(await row.innerText(), /14h/);
    assert.equal(await row.locator("code").count(), 0);
    assert(
      await strip.evaluate((node) => node.getBoundingClientRect().height <= 34),
    );
    assert.equal(
      await page.locator("#conversation-status").innerText(),
      "Working",
    );
    await page
      .locator('#conversation-title [data-chat-status="working"]')
      .waitFor();
    for (const width of [1280, 390]) {
      await page.setViewportSize({ width, height: 900 });
      assert(
        await page.evaluate(
          () => document.documentElement.scrollWidth <= innerWidth,
        ),
      );
      await page.screenshot({ path: join(root, `activity-${width}.png`) });
      const before = detailReads.length;
      await row.click();
      await page.getByRole("dialog", { name: /Current activity/ }).waitFor();
      await page.waitForFunction(
        () =>
          document.querySelector(".tasks-content.show-task-detail") !== null,
      );
      for (let n = 0; n < 100 && detailReads.length === before; n++)
        await new Promise((r) => setTimeout(r, 30));
      assert.equal(
        detailReads[before],
        task.id,
        "Open the exact older task, not the newest default",
      );
      await page.keyboard.press("Escape");
      await page
        .getByRole("dialog", { name: /Current activity/ })
        .waitFor({ state: "hidden" });
    }
    await page.setViewportSize({ width: 1280, height: 900 });
    historyTasks = [
      { ...task, id: "newer-command", command: "second active command" },
    ];
    const beforeMissingTaskFeed = taskFeedReads;
    await row.click();
    await page.getByRole("dialog", { name: /Current activity/ }).waitFor();
    for (let n = 0; n < 120 && taskFeedReads === beforeMissingTaskFeed; n++)
      await new Promise((r) => setTimeout(r, 50));
    assert.ok(taskFeedReads > beforeMissingTaskFeed);
    state.runtime.tasks = state.runtime.tasks.filter(
      (item) => item.id !== task.id,
    );
    await page
      .getByText("The selected task is no longer active in this chat.", {
        exact: true,
      })
      .waitFor();
    assert(
      !detailReads.includes("newer-command"),
      "Missing explicit selection must not open another task",
    );
    historyTasks = [task];
    const beforeLateHistory = taskFeedReads;
    for (let n = 0; n < 120 && taskFeedReads === beforeLateHistory; n++)
      await new Promise((r) => setTimeout(r, 50));
    assert.ok(taskFeedReads > beforeLateHistory);
    await page
      .locator(`[data-task="${task.id}"][aria-pressed="true"]`)
      .waitFor();
    assert(
      !detailReads.includes("newer-command"),
      "Late history restores the original selection",
    );
    await page.keyboard.press("Escape");
    await page
      .getByRole("dialog", { name: /Current activity/ })
      .waitFor({ state: "hidden" });
    await page.locator(`[data-chat="${other.id}"]`).click();
    await strip.waitFor({ state: "detached" });
    assert.deepEqual(errors, []);
    console.log(
      JSON.stringify({ browser: engine.name(), evidence: root, detailReads }),
    );
    console.log(
      "PASS completed chat explains live child command, exact task opens on desktop/mobile, other chats stay scoped",
    );
  } finally {
    await browser?.close();
    // spawnFixture owns bounded child cleanup, including test timeouts.
  }
});
