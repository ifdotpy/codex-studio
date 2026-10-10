#!/usr/bin/env node
import {
  test,
  expect,
  spawnFixture as spawn,
  readTestState,
} from "../playwright.mjs";
// Real app-server notification handlers, SQLite, event stream, and browser. No inference.
import assert from "node:assert/strict";
import { mkdtemp } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
test("sentence stream ui", async ({ page: runnerPage }) => {
  const skill = fileURLToPath(new URL("../../../../../../", import.meta.url));
  const root = await mkdtemp(join(tmpdir(), "codex-stream-ui-"));
  const proc = spawn(
    "python3",
    [
      "-B",
      join(skill, "workspaces/runtime/apps/server/tests/simple-ui-fixture.py"),
      root,
    ],
    { stdio: ["pipe", "pipe", "pipe"] },
  );
  let log = "",
    page;
  proc.stderr.on("data", (d) => (log += d));
  const poll = async (fn, label) => {
    for (let i = 0; i < 100; i++) {
      if (await fn()) return;
      await new Promise((r) => setTimeout(r, 50));
    }
    throw Error(label + " " + log);
  };
  try {
    const port = await new Promise((resolve, reject) => {
      proc.stdout.once("data", (d) => resolve(Number(String(d).trim())));
      proc.once("exit", () => reject(Error(log)));
    });
    const origin = `http://127.0.0.1:${port}`;
    const state = async () => await readTestState(origin);
    page = runnerPage;
    await page.setViewportSize({ width: 1200, height: 900 });
    const errors = [];
    page.on("pageerror", (e) => errors.push(e.message));
    await page.goto(origin);
    await page
      .locator("[data-chat]")
      .filter({ hasText: "Other project" })
      .click();
    await page.locator("#message").fill("Exercise the live stream");
    await page.locator("#send").click();
    let agent;
    await poll(async () => {
      agent = (await state()).runtime.agents.find(
        (a) => a.name === "Other project",
      );
      return agent.status === "running" && agent.turnId;
    }, "turn starts");
    const event = (method, params) =>
      proc.stdin.write(
        JSON.stringify({
          method,
          params: { threadId: agent.threadId, turnId: agent.turnId, ...params },
        }) + "\n",
      );
    await page.locator('[data-phase="thinking"]').waitFor();
    event("item/started", { item: { id: "reason-live", type: "reasoning" } });
    event("item/started", {
      item: { id: "live-text", type: "agentMessage", text: "" },
    });
    event("item/agentMessage/delta", {
      itemId: "live-text",
      delta: "First paragraph",
    });
    await page.locator('[data-phase="writing"]').waitFor();
    await expect(
      page.locator("#messages .prose").filter({ hasText: "First paragraph" }),
    ).toHaveCount(1);
    assert.equal(
      await page
        .locator("#messages .prose")
        .filter({ hasText: "First paragraph" })
        .count(),
      1,
      "show unfinished paragraph",
    );
    event("item/agentMessage/delta", {
      itemId: "live-text",
      delta: ". Second sentence begins",
    });
    await page.getByText("First paragraph.", { exact: true }).waitFor();
    await page.getByText("First paragraph.", { exact: true }).evaluate((el) => {
      window.retainedSentence = el;
    });
    event("item/agentMessage/delta", {
      itemId: "live-text",
      delta: ".\n\nSecond paragraph",
    });
    await page.getByText("Second sentence begins.", { exact: true }).waitFor();
    assert.equal(
      await page
        .locator("#messages .prose p")
        .filter({ hasText: "First paragraph." })
        .count(),
      1,
    );
    assert.equal(
      await page
        .getByText("First paragraph.", { exact: true })
        .evaluate((el) => el === window.retainedSentence),
      true,
      "Earlier sentence keeps its DOM node",
    );
    assert.equal(
      await page
        .getByText("Second sentence begins.", { exact: true })
        .evaluate((el) => getComputedStyle(el).animationDuration),
      "0.18s",
    );
    await page.emulateMedia({ reducedMotion: "reduce" });
    assert.equal(
      await page
        .getByText("Second sentence begins.", { exact: true })
        .evaluate((el) => getComputedStyle(el).animationName),
      "none",
    );
    await page.emulateMedia({ reducedMotion: "no-preference" });
    await page
      .getByText("First paragraph.", { exact: true })
      .evaluate((el) => (el.dataset.retained = "yes"));
    assert.equal(
      await page.getByText("Second paragraph", { exact: true }).count(),
      1,
    );
    event("item/agentMessage/delta", {
      itemId: "live-text",
      delta: '.\n\n```json\n{"ok":true}\n\n',
    });
    await page.getByText("Second paragraph.", { exact: true }).waitFor();
    assert.equal(
      await page.locator("#messages .prose pre").count(),
      1,
      "show open fenced code",
    );
    event("item/agentMessage/delta", {
      itemId: "live-text",
      delta: "```\nFinal tail",
    });
    await page.locator("#messages .prose pre").waitFor();
    assert.equal(
      await page
        .getByText("First paragraph.", { exact: true })
        .getAttribute("data-retained"),
      "yes",
      "prior paragraphs stay mounted",
    );
    await page.screenshot({ path: join(root, "writing.png") });
    const full =
      'First paragraph. Second sentence begins.\n\nSecond paragraph.\n\n```json\n{"ok":true}\n\n```\nFinal tail';
    event("item/completed", {
      item: { id: "live-text", type: "agentMessage", text: full },
    });
    await page.getByText("Final tail", { exact: true }).waitFor();

    assert.equal(
      await page
        .getByText("First paragraph.", { exact: true })
        .evaluate((el) => el === window.retainedSentence),
      true,
      "Completion retains sentence nodes",
    );
    event("turn/completed", {
      turn: { id: agent.turnId, status: "completed" },
    });
    await poll(
      async () =>
        (await state()).runtime.agents.find((a) => a.id === agent.id).status ===
        "completed",
      "Turn completes",
    );
    await page.reload();
    await page
      .locator("[data-chat]")
      .filter({ hasText: "Other project" })
      .click();
    await page.getByText("Final tail", { exact: true }).waitFor();
    assert.equal(
      await page.locator(".sentence-enter").count(),
      0,
      "History has no entrance animation",
    );
    await page.setViewportSize({ width: 390, height: 844 });
    await page.screenshot({ path: join(root, "sentence-mobile.png") });
    assert.equal(await page.evaluate(() => document.body.scrollWidth), 390);
    await page.locator("#message").fill("Check interruption");
    await page.locator("#send").click();
    const oldTurn = agent.turnId;
    await poll(async () => {
      agent = (await state()).runtime.agents.find((a) => a.id === agent.id);
      return agent.turnId && agent.turnId !== oldTurn;
    }, "next turn");
    event("item/agentMessage/delta", {
      itemId: "unfinished",
      delta: "Partial text retained after stop",
    });
    await page.locator('[data-phase="writing"]').waitFor();
    await page.locator("#stop").click();
    await page
      .getByText("Partial text retained after stop", { exact: true })
      .waitFor();
    expect(errors).toEqual([]);
    console.log(
      "PASS sentence stream, stable nodes, Markdown, reduced motion, history, narrow viewport, interruption",
    );
    console.log("Browser evidence:", root);
  } catch (error) {
    try {
      await page?.screenshot({ path: join(root, "failure.png") });
    } catch (screenshotError) {
      console.error("Failure screenshot unavailable:", screenshotError);
    }
    console.error("Evidence:", root);
    throw error;
  } finally {
    // spawnFixture owns the Python server lifecycle, including timeouts.
  }
});
