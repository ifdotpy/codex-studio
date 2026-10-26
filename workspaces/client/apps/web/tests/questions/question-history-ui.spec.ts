import { responseJson, required } from "../messages/helpers.ts";
import type { GetResult } from "../../src/apiTypes.ts";
import type { Page } from "@playwright/test";
import {
  readTestState,
  test,
  spawnFixture as spawn,
  rustUiFixture,
} from "../playwright.ts";
import assert from "node:assert/strict";
import { mkdtemp } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
test("Question history", async ({ context }) => {
  test.setTimeout(180_000);
  // Real isolated HTTP runtime, no model or user state.
  const root = await mkdtemp(join(tmpdir(), "studio-question-history-"));
  const proc = spawn(process.execPath, [rustUiFixture, root], {
    stdio: ["pipe", "pipe", "pipe"],
    env: {
      ...process.env,
      CODEX_BOARD_STATE_DIR: join(root, "board"),
      FIXTURE_SEED: "report,question",
      FIXTURE_HOLD: "lead",
    },
  });
  let log = "",
    page: Page | undefined;
  proc.stderr?.on("data", (data) => {
    log += data;
  });
  try {
    const port = await new Promise<number>((resolve, reject) => {
      proc.stdout?.once("data", (data) => resolve(Number(String(data).trim())));
      proc.once("exit", () => reject(Error(log)));
    });
    const origin = `http://127.0.0.1:${port}`;
    const state = await readTestState(origin);
    const lead = state.runtime.agents.find(
      (agent) => agent.name === "Release lead",
    );
    assert(lead, "Fixture lead exists");
    page = await context.newPage();
    await page.setViewportSize({ width: 1280, height: 900 });
    page.setDefaultTimeout(10000);
    const errors: string[] = [];
    page.on("pageerror", (error) => errors.push(error.message));
    await page.goto(origin);
    await page.locator("#message").waitFor({ timeout: 60000 });
    const selectLead = () =>
      required(page)
        .locator("[data-chat]")
        .filter({ hasText: "Release lead" })
        .click();
    await selectLead();
    const assistant = page.locator(".message.assistant").first();
    await assistant.waitFor();
    const transcriptLayout = await assistant.evaluate((element) => {
      const content = element.closest(".message-content");
      if (!content) throw new Error("Missing message content");
      const column = content.getBoundingClientRect();
      const answer = element.getBoundingClientRect();
      return {
        width: column.width,
        answerWidth: answer.width,
        left: answer.left - column.left,
        border: getComputedStyle(element).borderTopWidth,
      };
    });
    assert.ok(
      transcriptLayout.width <= 760,
      "the transcript uses a bounded text column",
    );
    assert.equal(transcriptLayout.answerWidth, transcriptLayout.width);
    assert.equal(transcriptLayout.left, 0);
    assert.equal(
      transcriptLayout.border,
      "1px",
      "assistant answers retain their card border",
    );
    const copy = assistant.getByRole("button", {
      name: "Copy message",
      exact: true,
    });
    await page.mouse.move(0, 0);
    await page.waitForFunction(() => {
      const button = document.querySelector(".message.assistant .copy-message");
      if (!button) throw new Error("Missing copy button");
      return getComputedStyle(button).opacity === "0";
    });
    await assistant.hover();
    await page.waitForFunction(() => {
      const button = document.querySelector(".message.assistant .copy-message");
      if (!button) throw new Error("Missing copy button");
      return getComputedStyle(button).opacity === "1";
    });
    await page.mouse.move(0, 0);
    await copy.focus();
    await page.waitForFunction(() => {
      const button = document.querySelector(".message.assistant .copy-message");
      if (!button) throw new Error("Missing copy button");
      return getComputedStyle(button).opacity === "1";
    });
    const card = page.locator('[data-request$=":question:async-question"]');
    assert.equal(
      await card
        .getByRole("button", { name: "Defer", exact: true })
        .innerText(),
      "Answer later",
    );
    await card.getByRole("button", { name: "Defer", exact: true }).click();
    const deferred = page.locator(".request-deferred");
    await deferred.locator("summary").waitFor();
    assert.match(await deferred.locator("summary").innerText(), /Later\s+1/);
    assert.equal(
      await card.isVisible(),
      false,
      "deferral removes the reminder card",
    );
    let history = await responseJson<GetResult<"/api/questions">>(
      await fetch(origin + "/api/questions?agent=" + lead.id),
    );
    assert.equal(required(history.items[0]).status, "pending");
    assert.equal(required(history.items[0]).deferred, true);
    await page.reload();
    await selectLead();
    await deferred.locator("summary").click();
    const deferredStatus = card.getByText("Deferred · Agent can continue", {
      exact: true,
    });
    await deferredStatus.waitFor({ state: "attached" });
    assert.equal(
      await deferredStatus.isVisible(),
      false,
      "Question status stays hidden in deferred history",
    );
    await card.getByRole("button", { name: "Restore", exact: true }).click();
    await deferred.waitFor({ state: "hidden" });
    await card.getByRole("button", { name: "Answer", exact: true }).click();
    await card
      .getByRole("textbox", { name: "Which scope?" })
      .fill("Only the runtime folder");
    await card
      .getByRole("button", { name: "Send answer", exact: true })
      .click();
    await card.waitFor({ state: "hidden" });
    const absent = async () => {
      assert.equal(
        await required(page)
          .getByText("Question history", { exact: true })
          .count(),
        0,
      );
      assert.equal(
        await required(page)
          .locator('.agent-phase[data-phase="completed"]')
          .count(),
        0,
      );
    };
    await absent();
    history = await responseJson<GetResult<"/api/questions">>(
      await fetch(origin + "/api/questions?agent=" + lead.id),
    );
    const answer = required(
      required(history.items[0]).answerHistory?.[0],
    ).answer;
    assert(Array.isArray(answer), "Question answer is a list");
    assert.equal(answer[0], "Only the runtime folder");
    assert.equal(required(history.items[0]).answeredBy, "user");
    assert.equal(required(history.items[0]).deferred, false);
    await page
      .locator("[data-chat]")
      .filter({ hasText: "Other project" })
      .click();
    await absent();
    await selectLead();
    await page.reload();
    await selectLead();
    await absent();
    for (const width of [1280, 390]) {
      await page.setViewportSize({ width, height: 900 });
      await page.screenshot({ path: join(root, `quiet-chat-${width}.png`) });
      await absent();
    }
    assert.deepEqual(errors, []);
    console.log(
      "PASS: question history removed; defer, restore and answer receipts retained; reload and chat isolation.",
    );
    console.log("Evidence:", root);
  } catch (error) {
    await page?.screenshot({ path: join(root, "failure.png") });
    console.error("Evidence:", root, log);
    throw error;
  }
});
