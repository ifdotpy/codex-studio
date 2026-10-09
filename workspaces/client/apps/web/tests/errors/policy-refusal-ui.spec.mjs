#!/usr/bin/env node
import { test, spawnFixture, readTestState } from "../playwright.mjs";
import assert from "node:assert/strict";
import { mkdtemp } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";

test("cyber policy refusal settles and leaves a warning before send", async ({
  page,
}) => {
  const skill = fileURLToPath(new URL("../../../../../../", import.meta.url));
  const root = await mkdtemp(join(tmpdir(), "codex-policy-ui-"));
  const proc = spawnFixture(
    "python3",
    [
      "-B",
      join(skill, "workspaces/runtime/apps/server/tests/simple-ui-fixture.py"),
      root,
    ],
    { stdio: ["pipe", "pipe", "pipe"] },
  );
  let log = "";
  proc.stderr.on("data", (data) => (log += data));
  const poll = async (fn, label) => {
    for (let index = 0; index < 100; index++) {
      if (await fn()) return;
      await new Promise((resolve) => setTimeout(resolve, 50));
    }
    throw Error(`${label}: ${log}`);
  };
  try {
    const port = await new Promise((resolve, reject) => {
      proc.stdout.once("data", (data) => resolve(Number(String(data).trim())));
      proc.once("exit", () => reject(Error(log)));
    });
    const origin = `http://127.0.0.1:${port}`;
    const state = () => readTestState(origin);
    await page.goto(origin);
    await page
      .locator("[data-chat]")
      .filter({ hasText: "Other project" })
      .click();
    await page.locator("#message").fill("Try a bounded security review");
    await page.locator("#send").click();
    let agent;
    await poll(async () => {
      agent = (await state()).runtime.agents.find(
        (row) => row.name === "Other project",
      );
      return agent?.status === "running" && agent.turnId;
    }, "turn starts");
    const event = (method, params) =>
      proc.stdin.write(
        JSON.stringify({
          method,
          params: { threadId: agent.threadId, turnId: agent.turnId, ...params },
        }) + "\n",
      );
    event("error", {
      willRetry: true,
      error: {
        message: "Sensitive native refusal details",
        codexErrorInfo: "cyberPolicy",
      },
    });
    await poll(async () => {
      agent = (await state()).runtime.agents.find((row) => row.id === agent.id);
      return agent?.error?.codexErrorInfo === "cyberPolicy";
    }, "policy refusal diagnostics");
    event("turn/completed", {
      turn: { id: agent.turnId, status: "interrupted", error: null },
    });
    await poll(async () => {
      agent = (await state()).runtime.agents.find((row) => row.id === agent.id);
      return agent?.status === "failed" && agent.inFlight === false;
    }, "terminal settlement");
    await page
      .locator(".native-error")
      .getByText("Codex refused this turn under its cybersecurity policy.", {
        exact: true,
      })
      .waitFor();
    await page
      .locator(".native-error")
      .getByText(
        "Start a new chat with a narrower or rephrased task, or use another provider or model.",
        { exact: true },
      )
      .waitFor();
    await page
      .locator(".native-error")
      .getByText("Cyber access program: standard", { exact: true })
      .waitFor();
    await page
      .locator(".native-policy-composer-warning")
      .getByText(
        "Continuing this chat sends the same history and can be refused again.",
        { exact: true },
      )
      .waitFor();
    await page.locator("#message").fill("Use a narrower request");
    assert.equal(await page.locator("#message").isDisabled(), false);
    assert.equal(await page.locator("#send").isDisabled(), false);
    assert.equal(agent.error.codexErrorInfo, "cyberPolicy");
    assert.equal(agent.status, "failed");
    assert.equal(agent.inFlight, false);
  } finally {
    proc.kill();
  }
});
