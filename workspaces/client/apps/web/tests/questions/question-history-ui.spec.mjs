import { readTestState, test, spawnFixture as spawn } from "../playwright.mjs";
import assert from "node:assert/strict";
import { fileURLToPath } from "node:url";
import { mkdtemp } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
test("Question history", async ({ context }) => {
  test.setTimeout(180_000);
  const testRepo = fileURLToPath(
    new URL("../../../../../../", import.meta.url),
  );
  // Real isolated HTTP runtime, no model or user state.
  const project = testRepo;
  const root = await mkdtemp(join(tmpdir(), "studio-question-history-"));
  const proc = spawn(
    "python3",
    [
      "-B",
      join(
        project,
        "workspaces/runtime/apps/server/tests/simple-ui-fixture.py",
      ),
      root,
    ],
    {
      stdio: ["pipe", "pipe", "pipe"],
      env: { ...process.env, CODEX_BOARD_STATE_DIR: join(root, "board") },
    },
  );
  let log = "",
    page;
  proc.stderr.on("data", (data) => {
    log += data;
  });
  try {
    const port = await new Promise((resolve, reject) => {
      proc.stdout.once("data", (data) => resolve(Number(String(data).trim())));
      proc.once("exit", () => reject(Error(log)));
    });
    const origin = `http://127.0.0.1:${port}`;
    const state = await readTestState(origin);
    const lead = state.runtime.agents.find(
      (agent) => agent.name === "Release lead",
    );
    page = await context.newPage();
    await page.setViewportSize({ width: 1280, height: 900 });
    page.setDefaultTimeout(10000);
    const errors = [];
    page.on("pageerror", (error) => errors.push(error.message));
    await page.goto(origin);
    await page.locator("#message").waitFor({ timeout: 60000 });
    const selectLead = () =>
      page.locator("[data-chat]").filter({ hasText: "Release lead" }).click();
    await selectLead();
    const assistant = page.locator(".message.assistant").first();
    await assistant.waitFor();
    const transcriptLayout = await assistant.evaluate((element) => {
      const column = element
        .closest(".message-content")
        .getBoundingClientRect();
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
      "0px",
      "assistant answers have no card border",
    );
    const copy = assistant.getByRole("button", {
      name: "Copy message",
      exact: true,
    });
    await page.mouse.move(0, 0);
    await page.waitForFunction(
      () =>
        getComputedStyle(
          document.querySelector(".message.assistant .copy-message"),
        ).opacity === "0",
    );
    await assistant.hover();
    await page.waitForFunction(
      () =>
        getComputedStyle(
          document.querySelector(".message.assistant .copy-message"),
        ).opacity === "1",
    );
    await page.mouse.move(0, 0);
    await copy.focus();
    await page.waitForFunction(
      () =>
        getComputedStyle(
          document.querySelector(".message.assistant .copy-message"),
        ).opacity === "1",
    );
    const card = page.locator('[data-request="async-question"]');
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
    let history = await (
      await fetch(origin + "/api/questions?agent=" + lead.id)
    ).json();
    assert.equal(history.items[0].status, "pending");
    assert.equal(history.items[0].deferred, true);
    await page.reload();
    await selectLead();
    await deferred.locator("summary").click();
    await card
      .getByText("Deferred · Agent can continue", { exact: true })
      .waitFor();
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
        await page.getByText("Question history", { exact: true }).count(),
        0,
      );
      assert.equal(
        await page.locator('.agent-phase[data-phase="completed"]').count(),
        0,
      );
    };
    await absent();
    history = await (
      await fetch(origin + "/api/questions?agent=" + lead.id)
    ).json();
    assert.equal(
      history.items[0].answerHistory[0].answer[0],
      "Only the runtime folder",
    );
    assert.equal(history.items[0].answeredBy, "user");
    assert.equal(history.items[0].deferred, false);
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
