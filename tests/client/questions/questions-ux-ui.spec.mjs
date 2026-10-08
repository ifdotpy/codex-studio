#!/usr/bin/env node
// Isolated HTTP fixture and headless Chrome. No model requests or user state.

import { mkdtemp } from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { test, expect, spawnFixture as spawn } from "../playwright.mjs";

test("Questions Ux Ui", async ({
  browser: _testBrowser,
  context: _testContext,
  page: testPage,
}) => {
  const assert = {
    equal: (actual, expected, message) =>
      expect(actual, message).toBe(expected),
    notEqual: (actual, expected, message) =>
      expect(actual, message).not.toBe(expected),
    deepEqual: (actual, expected, message) =>
      expect(actual, message).toEqual(expected),
    ok: (actual, message) => expect(actual, message).toBeTruthy(),
    match: (actual, expected, message) =>
      expect(actual, message).toMatch(expected),
    doesNotMatch: (actual, expected, message) =>
      expect(actual, message).not.toMatch(expected),
    fail: (message) => {
      throw new Error(message);
    },
  };

  const project = dirname(
    dirname(dirname(dirname(fileURLToPath(import.meta.url)))),
  );
  const root = await mkdtemp(join(tmpdir(), "codex-questions-ui-"));
  const proc = spawn(
    "python3",
    ["-B", join(project, "tests/simple-ui-fixture.py"), root],
    {
      stdio: ["pipe", "pipe", "pipe"],
      env: {
        ...process.env,
        CODEX_BOARD_STATE_DIR: join(root, "board"),
        QUESTIONS_UX_UI_FIXTURE: "1",
      },
    },
  );
  let log = "",
    page,
    releaseResponse;
  proc.stderr.on("data", (data) => {
    log += data;
  });
  const poll = async (check, label) => {
    for (let i = 0; i < 200; i++) {
      if (await check()) return;
      await new Promise((resolve) => setTimeout(resolve, 50));
    }
    throw Error(label + "\n" + log);
  };
  try {
    const port = await new Promise((resolve, reject) => {
      proc.stdout.once("data", (data) => resolve(Number(String(data).trim())));
      proc.once("exit", () => reject(Error(log)));
    });
    const origin = `http://127.0.0.1:${port}`;
    const description = "Review the selected file and report its test result.";
    page = testPage;
    await page.setViewportSize({ width: 1440, height: 960 });
    page.setDefaultTimeout(10000);
    const errors = [];
    page.on("pageerror", (error) => errors.push(error.message));
    await page.goto(origin);
    await page
      .locator("[data-chat]")
      .filter({ hasText: "Release lead" })
      .click();
    const card = page.locator('[data-request="async-question"]');
    await card.getByText("Which scope?", { exact: true }).waitFor();
    await card.getByText("2 questions", { exact: true }).waitFor();
    assert.equal(
      await card.getByText("2 questions", { exact: true }).count(),
      1,
    );
    assert.equal(
      await card.getByText("Agent can continue", { exact: true }).isVisible(),
      false,
    );
    assert.equal(
      await page
        .locator('[data-request="blocking-question"]')
        .getByText("Agent can continue")
        .count(),
      0,
    );
    assert.equal(
      await page
        .locator('[data-request="permission-question"]')
        .getByText("Approval required", { exact: true })
        .count(),
      1,
    );
    await page.locator('[data-answer="blocking-question"]').click();
    const modal = page
      .locator('[data-request="blocking-question"]')
      .getByRole("form");
    await modal.getByRole("button", { name: "Yes", exact: true }).waitFor();
    assert.equal(
      await page
        .locator('[data-request="blocking-question"]')
        .getByText("Blocking tool question?", { exact: true })
        .count(),
      1,
      "an open single question appears once",
    );
    await page.keyboard.press("Escape");
    await modal.waitFor({ state: "hidden" });
    const mcpCard = page.locator('[data-request="mcp-optional"]');
    await mcpCard.locator("[data-answer]").click();
    const mcpForm = mcpCard.getByRole("form", { name: "Reply to the agent" });
    const mcpSend = mcpForm.getByRole("button", { name: "Send answer" });
    assert.equal(await mcpSend.isDisabled(), true);
    assert.equal(await mcpForm.getByRole("checkbox").count(), 2);
    assert.equal(
      await mcpForm.getByRole("textbox").count(),
      2,
      "enum fields have no custom answer",
    );
    await mcpForm.getByRole("textbox", { name: "Required scope" }).fill("One");
    await poll(() => mcpSend.isEnabled(), "optional fields do not block Send");
    let mcpAnswer;
    const captureMcp = async (route) => {
      mcpAnswer = route.request().postDataJSON();
      await route.fulfill({ json: { status: "answered" } });
    };
    await page.route("**/api/answer", captureMcp);
    await mcpSend.click();
    await poll(() => Boolean(mcpAnswer), "MCP answer was sent");
    assert.deepEqual(mcpAnswer, {
      id: "mcp-optional",
      decision: "accept",
      content: { scope: "One" },
    });
    await page.unroute("**/api/answer", captureMcp);
    await page.reload();
    await page
      .locator("[data-chat]")
      .filter({ hasText: "Release lead" })
      .click();
    await card.locator("[data-answer]").click();
    const form = card.getByRole("form", { name: "Reply to the agent" });
    const send = form.getByRole("button", { name: "Send answer", exact: true });
    assert.equal(
      await card
        .locator(".request-question-label")
        .filter({ hasText: "Which scope?" })
        .count(),
      1,
      "an open multi-question card does not repeat its first question",
    );
    assert.equal(await card.locator(".request-prompt").count(), 0);
    const openLayout = await card.evaluate((element) => {
      const viewport = document
        .querySelector("#messages")
        .getBoundingClientRect();
      const heading = element
        .querySelector(".request-heading")
        .getBoundingClientRect();
      const actions = element
        .querySelector(".request-answer-actions")
        .getBoundingClientRect();
      return {
        headingVisible:
          heading.top >= viewport.top && heading.bottom <= viewport.bottom,
        actionsVisible:
          actions.top >= viewport.top && actions.bottom <= viewport.bottom,
      };
    });
    assert.equal(
      openLayout.headingVisible,
      true,
      "Answer brings the card heading into view",
    );
    assert.equal(
      openLayout.actionsVisible,
      true,
      "the answer action row stays in view",
    );

    assert.equal(
      await page.getByRole("dialog", { name: "Reply to the agent" }).count(),
      0,
    );
    assert.equal(await send.isDisabled(), true, "no default answer");
    assert.equal(await form.locator('[aria-pressed="true"]').count(), 0);
    await form.getByText(description, { exact: true }).waitFor();
    const firstOption = form.getByRole("button", {
      name: "One file",
      exact: true,
    });
    await firstOption.focus();
    await page.keyboard.press("2");
    assert.equal(
      await form
        .getByRole("button", { name: "All files", exact: true })
        .getAttribute("aria-pressed"),
      "true",
    );
    await page.keyboard.press("1");
    assert.equal(await firstOption.getAttribute("aria-pressed"), "true");
    const testsOption = form.getByRole("checkbox", {
      name: "Tests",
      exact: true,
    });
    await testsOption.focus();
    await page.keyboard.press("1");
    assert.equal(await testsOption.isChecked(), true);
    await page.keyboard.press("1");
    assert.equal(await testsOption.isChecked(), false);
    await page.locator("#message").fill("The main composer remains available.");
    assert.equal(
      await page
        .locator("#message")
        .evaluate((el) => el === document.activeElement),
      true,
    );
    await form.getByRole("button", { name: "One file", exact: false }).click();
    assert.equal(
      await send.isDisabled(),
      true,
      "all questions need an explicit answer",
    );
    await form
      .getByRole("textbox", { name: "Which evidence must the report include?" })
      .fill("Tests and the complete diff");
    await form.getByRole("checkbox", { name: "Tests" }).check();
    await form.getByRole("checkbox", { name: "Screenshots" }).check();
    assert.equal(
      await form.getByRole("checkbox", { name: "Tests" }).isChecked(),
      true,
    );
    assert.equal(await send.isEnabled(), true);
    await form
      .getByRole("textbox", { name: "Which scope?" })
      .fill("Only the request controls");
    await page.keyboard.press("2");
    assert.equal(
      await form.getByRole("textbox", { name: "Which scope?" }).inputValue(),
      "Only the request controls2",
    );
    await form
      .getByRole("textbox", { name: "Which scope?" })
      .fill("Only the request controls");
    assert.equal(
      await form.locator('[aria-pressed="true"]').count(),
      0,
      "custom text clears the option selection",
    );
    await card.getByRole("button", { name: "Hide", exact: true }).click();
    assert.equal(
      await card.getByText("Which scope?", { exact: true }).count(),
      1,
    );
    await card.locator("[data-answer]").click();
    assert.equal(
      await form.getByRole("textbox", { name: "Which scope?" }).inputValue(),
      "Only the request controls",
      "Hide preserves the answer draft",
    );
    await page
      .locator("[data-chat]")
      .filter({ hasText: "Other project" })
      .click();
    await page
      .locator("[data-chat]")
      .filter({ hasText: "Release lead" })
      .click();
    await card.locator("[data-answer]").click();
    assert.equal(
      await form.getByRole("textbox", { name: "Which scope?" }).inputValue(),
      "Only the request controls",
      "Chat navigation preserves the answer draft",
    );
    assert.equal(
      await form
        .getByRole("textbox", {
          name: "Which evidence must the report include?",
        })
        .inputValue(),
      "Tests and the complete diff",
    );
    assert.equal(
      await page.evaluate(() =>
        [localStorage, sessionStorage].some((storage) =>
          Object.keys(storage).some((key) =>
            storage.getItem(key)?.includes("Only the request controls"),
          ),
        ),
      ),
      true,
      "Non-secret answer drafts persist for application reload",
    );
    await page.reload();
    await page
      .locator("[data-chat]")
      .filter({ hasText: "Release lead" })
      .click();
    await card.locator("[data-answer]").click();
    assert.equal(
      await form.getByRole("textbox", { name: "Which scope?" }).inputValue(),
      "Only the request controls",
      "Reload restores the non-secret answer draft",
    );
    for (const [width, height, theme] of [
      [1440, 960, "light"],
      [390, 844, "light"],
      [1440, 960, "dark"],
      [390, 844, "dark"],
    ]) {
      await page.setViewportSize({ width, height });
      await page.emulateMedia({ colorScheme: theme });
      await send.click({ trial: true });
      await page.mouse.move(0, 0);
      assert.equal(await page.evaluate(() => document.body.scrollWidth), width);
      assert.equal(await page.locator("#message").isVisible(), true);
      const box = await send.boundingBox();
      assert.ok(
        box &&
          box.x >= 0 &&
          box.y >= 0 &&
          box.x + box.width <= width + 1 &&
          box.y + box.height <= height + 1,
      );
      assert.equal(
        await send.evaluate((el) => {
          const rect = el.getBoundingClientRect();
          return el.contains(
            document.elementFromPoint(
              rect.x + rect.width / 2,
              rect.y + rect.height / 2,
            ),
          );
        }),
        true,
        "Send remains accessible inside the request scroll area",
      );
      await form
        .locator(".request-question-label")
        .first()
        .scrollIntoViewIfNeeded();
      await page.screenshot({
        path: join(root, `question-${width}-${theme}.png`),
      });
    }
    await page.setViewportSize({ width: 1440, height: 960 });
    const answers = [];
    let fail = true,
      answerCommitted = false;
    await page.route("**/api/answer", async (route) => {
      answers.push(route.request().postDataJSON());
      if (fail) {
        fail = false;
        await route.abort("failed");
        return;
      }
      const response = await route.fetch();
      answerCommitted = true;
      await new Promise((resolve) => {
        releaseResponse = resolve;
      });
      await route.fulfill({ response });
    });
    await send.click();
    await page.locator("#toast").waitFor();
    await poll(
      () => send.isEnabled(),
      "failed request permits an explicit retry",
    );
    assert.equal(
      await form.getByRole("textbox", { name: "Which scope?" }).inputValue(),
      "Only the request controls",
    );
    assert.equal(answers.length, 1);
    await send.click();
    await poll(() => answerCommitted, "answer reaches the real server");
    assert.equal(await send.isDisabled(), true);
    assert.deepEqual(answers[1], {
      id: "async-question",
      answers: {
        0: { answers: ["Only the request controls"] },
        evidence: {
          answers: ["Tests", "Screenshots", "Tests and the complete diff"],
        },
      },
    });
    await page
      .locator("[data-chat]")
      .filter({ hasText: "Other project" })
      .click();
    await page.locator('[data-answer="other-question"]').click();
    const otherForm = page
      .locator('[data-request="other-question"]')
      .getByRole("form");
    await otherForm
      .getByRole("textbox", { name: "Other project question?" })
      .fill("Keep this answer");
    releaseResponse();
    await page.waitForTimeout(250);
    assert.equal(
      await otherForm
        .getByRole("textbox", { name: "Other project question?" })
        .inputValue(),
      "Keep this answer",
    );
    await otherForm
      .getByRole("button", { name: "Close reply", exact: true })
      .click();
    assert.equal(
      await page
        .locator('[data-answer="other-question"]')
        .evaluate((el) => el === document.activeElement),
      true,
    );
    assert.equal(answers.length, 2, "Hide never sends an answer");
    await page.locator('[data-answer="other-question"]').click();
    assert.equal(
      await otherForm
        .getByRole("textbox", { name: "Other project question?" })
        .inputValue(),
      "Keep this answer",
      "Hide preserves the answer draft",
    );
    assert.deepEqual(errors, []);
    console.log(
      "Question UX: PASS (inline choices, custom text, draft preservation, explicit hide, explicit submit, failed retry, exact ID, stale response, blocking input, narrow layout)",
    );
    console.log("Browser evidence:", root);
  } catch (error) {
    await page?.screenshot({ path: join(root, "failure.png") });
    console.error("Failure evidence:", root);
    throw error;
  } finally {
    releaseResponse?.();
    await page?.unrouteAll({ behavior: "wait" });
  }
});
