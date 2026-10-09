#!/usr/bin/env node
import assert from "node:assert/strict";
import { test, spawnFixture as spawn, readTestState } from "../playwright.mjs";
// Verify the visible task drawer follows task entity updates without a reload.
import { mkdtemp } from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

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
    const initial = await readTestState(origin);
    const lead = initial.runtime.agents.find(
      (agent) => agent.name === "Release lead",
    );
    const otherChat = initial.runtime.agents.find(
      (agent) => agent.name === "Other project",
    );
    assert.ok(lead && otherChat, "fixture exposes both chats");
    const now = Date.now() / 1000;
    const task = (id, agent, command, created) => ({
      id,
      agent,
      status: "running",
      kind: "command",
      type: "commandExecution",
      name: "commandExecution",
      command,
      cwd: stateDir,
      processId: "5173",
      created,
    });
    const older = task(
      "feed-ui-older",
      lead.id,
      "fixture command --older",
      now,
    );
    const newest = task(
      "feed-ui-newest",
      lead.id,
      "fixture command --newest",
      now + 1,
    );
    const other = task(
      "feed-ui-other-chat",
      otherChat.id,
      "fixture command --other-chat",
      now + 2,
    );
    for (const item of [older, newest, other])
      proc.stdin.write(
        JSON.stringify({ method: "fixture/task", params: item }) + "\n",
      );

    const page = runnerPage;
    await page.setViewportSize({ width: 1280, height: 900 });
    let mainDocumentNavigations = 0;
    page.on("framenavigated", (frame) => {
      if (frame === page.mainFrame()) mainDocumentNavigations++;
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
    await page.getByRole("heading", { name: "Current activity" }).waitFor();
    await poll(
      async () =>
        (await page.locator('[data-task="feed-ui-newest"]').count()) === 1 &&
        (await page.locator('[data-task="feed-ui-older"]').count()) === 1,
      "selected chat's active tasks arrive through entity sync",
    );
    assert.equal(
      await page.locator('[data-task="feed-ui-other-chat"]').count(),
      0,
      "tasks from another chat are excluded",
    );
    assert.equal(
      await page.getByText("2 active", { exact: true }).count(),
      1,
      "activity count includes only the selected chat's two active commands",
    );
    assert.deepEqual(
      await page
        .locator(".tasks-rows [data-task]")
        .evaluateAll((rows) =>
          rows.map((row) => row.getAttribute("data-task")),
        ),
      ["feed-ui-newest", "feed-ui-older"],
      "active commands are ordered newest first",
    );

    const navigationsBeforeUpdate = mainDocumentNavigations;
    proc.stdin.write(
      JSON.stringify({
        method: "fixture/task",
        params: {
          ...newest,
          status: "completed",
          finished: newest.created + 1,
        },
      }) + "\n",
    );
    await poll(
      async () =>
        (await page.locator('[data-task="feed-ui-newest"]').count()) === 0 &&
        (await page.locator('[data-task="feed-ui-older"]').count()) === 1 &&
        (await page.getByText("1 active", { exact: true }).count()) === 1,
      "completed command is removed and active count updates from the entity stream",
    );
    assert.equal(
      mainDocumentNavigations,
      navigationsBeforeUpdate,
      "task completion updates the drawer without a document reload",
    );
    console.log(
      "PASS workspace task feed UI: selected-chat scope, active count, newest-first order, and completion update via entity sync without reload",
    );
  } catch (error) {
    await runnerPage
      .screenshot({ path: join(stateDir, "task-feed-failure.png") })
      .catch(() => {});
    console.error("Evidence:", stateDir);
    throw error;
  } finally {
    proc.stdin.end();
  }
});
