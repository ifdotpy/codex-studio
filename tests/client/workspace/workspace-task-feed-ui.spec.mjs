#!/usr/bin/env node
// Verify the visible task drawer follows the cursor feed while it is open.
import { mkdtemp } from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { test, spawnFixture as spawn } from "../playwright.mjs";

test("workspace task feed ui", async ({ page: runnerPage }) => {
  const repo = dirname(
    dirname(dirname(dirname(fileURLToPath(import.meta.url)))),
  );
  const stateDir = await mkdtemp(join(tmpdir(), "codex-task-feed-ui-"));
  const proc = spawn(
    "python3",
    [join(repo, "tests/simple-ui-fixture.py"), stateDir],
    { stdio: ["pipe", "pipe", "pipe"] },
  );
  let log = "";
  proc.stderr.on("data", (data) => (log += data));
  const poll = async (check, description, seconds = 9) => {
    for (let n = 0; n < seconds * 20; n++) {
      if (await check()) return;
      await new Promise((resolve) => setTimeout(resolve, 50));
    }
    throw new Error(description + "\n" + log);
  };

  try {
    const port = await new Promise((resolve, reject) => {
      proc.stdout.once("data", (data) => resolve(Number(String(data).trim())));
      proc.once("exit", () => reject(new Error(log)));
    });
    const origin = `http://127.0.0.1:${port}`;
    const get = async (path) => (await fetch(origin + path)).json();
    const initial = await get("/api/state");
    const lead = initial.runtime.agents.find(
      (agent) => agent.name === "Release lead",
    );
    const task = {
      id: "feed-ui-command",
      agent: lead.id,
      status: "running",
      kind: "command",
      type: "commandExecution",
      name: "commandExecution",
      command: "fixture command --task-feed-ui",
      cwd: stateDir,
      processId: "5173",
      created: Date.now() / 1000,
    };
    proc.stdin.write(
      JSON.stringify({ method: "fixture/task", params: task }) + "\n",
    );
    const page = runnerPage;
    await page.setViewportSize({ width: 1280, height: 900 });
    const feeds = [];
    page.on("response", (response) => {
      if (new URL(response.url()).pathname === "/api/workspace/tasks")
        feeds.push(response.status());
    });
    await page.goto(origin);
    await page
      .locator("[data-chat]")
      .filter({ hasText: "Release lead" })
      .click();
    await page
      .getByRole("button", { name: "Chat actions", exact: true })
      .click();
    await page.locator("#tasks-toggle").click();
    await poll(() => feeds.length > 0, "task feed request arrives");
    await page
      .getByRole("heading", { name: task.command, exact: true })
      .waitFor();

    proc.stdin.write(
      JSON.stringify({
        method: "fixture/task",
        params: { ...task, status: "completed", finished: task.created + 1 },
      }) + "\n",
    );
    await poll(
      async () =>
        feeds.filter((status) => status === 200).length > 1 &&
        (await page.locator('[data-task="feed-ui-command"]').count()) === 0,
      "completed task disappears within the five-second feed refresh",
      8,
    );
    console.log(
      "PASS workspace task feed UI: initial task and completion refreshed within five seconds",
    );
  } catch (error) {
    if (browser) {
      const page = browser.contexts().flatMap((context) => context.pages())[0];
      if (page)
        await page
          .screenshot({ path: join(stateDir, "task-feed-failure.png") })
          .catch(() => {});
    }
    console.error("Evidence:", stateDir);
    throw error;
  } finally {
    proc.stdin.end();
  }
});
