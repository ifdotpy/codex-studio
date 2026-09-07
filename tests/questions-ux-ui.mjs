#!/usr/bin/env node
// Isolated HTTP fixture and headless Chrome. No model requests or user state.
import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { mkdtemp } from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { createRequire } from "node:module";

const project = dirname(dirname(fileURLToPath(import.meta.url)));
const { chromium } = createRequire(join(project, "web/package.json"))(
  "playwright-core",
);
const root = await mkdtemp(join(tmpdir(), "codex-questions-ui-"));
const proc = spawn(
  "python3",
  ["-B", join(project, "tests/simple-ui-fixture.py"), root],
  {
    stdio: ["pipe", "pipe", "pipe"],
    env: { ...process.env, CODEX_BOARD_STATE_DIR: join(root, "board") },
  },
);
let log = "",
  browser,
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
  const initial = await (await fetch(origin + "/api/state")).json();
  const lead = initial.runtime.agents.find(
    (agent) => agent.name === "Release lead",
  );
  const other = initial.runtime.agents.find(
    (agent) => agent.name === "Other project",
  );
  const description = "Review the selected file and report its test result.";
  browser = await chromium.launch({
    headless: true,
    executablePath:
      process.env.CHROME_BIN ||
      "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
  });
  page = await browser.newPage({ viewport: { width: 1440, height: 960 } });
  page.setDefaultTimeout(10000);
  const errors = [];
  page.on("pageerror", (error) => errors.push(error.message));
  let extraRequests = true;
  await page.route("**/api/state", async (route) => {
    const response = await route.fetch(),
      state = await response.json();
    state.runtime.requests = state.runtime.requests.map((request) =>
      request.id !== "async-question"
        ? request
        : {
            ...request,
            params: {
              questions: [
                {
                  id: "0",
                  question: "Which scope?",
                  options: [
                    { label: "One file", description },
                    { label: "All files" },
                  ],
                },
                {
                  id: "evidence",
                  question: "Which evidence must the report include?",
                  options: [],
                },
              ],
            },
          },
    );
    state.runtime.requests.push({
      id: "other-question",
      agent: other.id,
      method: "agent/asyncQuestion",
      params: {
        questions: [
          {
            id: "0",
            question: "Other project question?",
            options: [{ label: "Continue" }],
          },
        ],
      },
    });
    if (extraRequests)
      state.runtime.requests.push(
        {
          id: "blocking-question",
          agent: lead.id,
          method: "item/tool/requestUserInput",
          params: {
            questions: [
              {
                id: "blocking",
                question: "Blocking tool question?",
                options: [{ label: "Yes" }],
              },
            ],
          },
        },
        {
          id: "permission-question",
          agent: lead.id,
          method: "item/permissions/requestApproval",
          params: {
            reason: "Access to the fixture folder",
            permissions: { read: "folder/".repeat(150) },
          },
        },
      );
    await route.fulfill({ response, json: state });
  });
  await page.goto(origin);
  await page.locator("[data-chat]").filter({ hasText: "Release lead" }).click();
  const card = page.locator('[data-request="async-question"]');
  await card.getByText("Which scope?", { exact: true }).waitFor();
  assert.equal(await card.getByText("2 questions", { exact: true }).count(), 1);
  assert.equal(
    await card.getByText("Agent can continue", { exact: true }).count(),
    1,
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
  const modal = page.getByRole("dialog", {
    name: "Reply to the agent",
    exact: true,
  });
  await modal
    .getByRole("textbox", { name: "Blocking tool question?" })
    .waitFor();
  await page.keyboard.press("Escape");
  await modal.waitFor({ state: "hidden" });
  extraRequests = false;
  await page.reload();
  await page.locator("[data-chat]").filter({ hasText: "Release lead" }).click();
  await card.locator("[data-answer]").click();
  const form = card.getByRole("form", { name: "Reply to the agent" });
  const send = form.getByRole("button", { name: "Send answer", exact: true });
  assert.equal(
    await page.getByRole("dialog", { name: "Reply to the agent" }).count(),
    0,
  );
  assert.equal(await send.isDisabled(), true, "no default answer");
  assert.equal(await form.locator('[aria-pressed="true"]').count(), 0);
  await form.getByText(description, { exact: true }).waitFor();
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
  assert.equal(await send.isEnabled(), true);
  await form
    .getByRole("textbox", { name: "Which scope?" })
    .fill("Only the request controls");
  assert.equal(
    await form.locator('[aria-pressed="true"]').count(),
    0,
    "custom text clears the option selection",
  );
  for (const [width, height] of [
    [1440, 960],
    [320, 640],
    [1920, 1080],
  ]) {
    await page.setViewportSize({ width, height });
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
    await page.screenshot({ path: join(root, `question-${width}.png`) });
  }
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
      evidence: { answers: ["Tests and the complete diff"] },
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
  await otherForm.getByRole("button", { name: "Cancel", exact: true }).click();
  assert.equal(
    await page
      .locator('[data-answer="other-question"]')
      .evaluate((el) => el === document.activeElement),
    true,
  );
  assert.equal(answers.length, 2, "Cancel never sends an answer");
  assert.deepEqual(errors, []);
  console.log(
    "Question UX: PASS (inline choices, custom text, explicit submit, failed retry, exact ID, stale response, blocking input, narrow layout)",
  );
  console.log("Browser evidence:", root);
} catch (error) {
  await page?.screenshot({ path: join(root, "failure.png") });
  console.error("Failure evidence:", root);
  throw error;
} finally {
  releaseResponse?.();
  await browser?.close();
  proc.kill("SIGTERM");
  if (proc.exitCode === null)
    await new Promise((resolve) => proc.once("exit", resolve));
}
