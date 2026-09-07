#!/usr/bin/env node
// Real isolated HTTP runtime, no model or user state.
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
const root = await mkdtemp(join(tmpdir(), "studio-question-history-"));
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
  const state = await (await fetch(origin + "/api/state")).json();
  const lead = state.runtime.agents.find(
    (agent) => agent.name === "Release lead",
  );
  browser = await chromium.launch({
    headless: true,
    executablePath:
      process.env.CHROME_BIN ||
      "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
  });
  page = await browser.newPage({ viewport: { width: 1280, height: 900 } });
  page.setDefaultTimeout(10000);
  const errors = [];
  page.on("pageerror", (error) => errors.push(error.message));
  await page.goto(origin);
  const selectLead = () =>
    page.locator("[data-chat]").filter({ hasText: "Release lead" }).click();
  await selectLead();
  const card = page.locator('[data-request="async-question"]');
  await card.getByRole("button", { name: "Defer", exact: true }).click();
  const deferred = page.locator(".request-deferred");
  await deferred.locator("summary").waitFor();
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
  await card.getByRole("button", { name: "Send answer", exact: true }).click();
  await card.waitFor({ state: "hidden" });
  const historyDisclosure = page
    .locator(".request-history")
    .filter({ has: page.locator("summary", { hasText: "Question history" }) });
  await historyDisclosure.locator("summary").click();
  await historyDisclosure
    .getByText("Only the runtime folder", { exact: true })
    .waitFor();
  await historyDisclosure
    .getByText("Answered by you", { exact: true })
    .waitFor();
  history = await (
    await fetch(origin + "/api/questions?agent=" + lead.id)
  ).json();
  assert.equal(
    history.items[0].answerHistory[0].answer[0],
    "Only the runtime folder",
  );
  assert.equal(history.items[0].answeredBy, "user");
  assert.equal(history.items[0].deferred, false);
  // A slow refresh must not replace confirmed answers with a loading row.
  const answer = historyDisclosure.locator(".request-history-answer");
  const historyHeight = (await historyDisclosure.boundingBox()).height;
  let delayedHistory;
  await page.route("**/api/questions?*", (route) => {
    delayedHistory = route;
  });
  await historyDisclosure.locator("summary").click();
  await historyDisclosure.locator("summary").click();
  await page.waitForFunction(() =>
    document.querySelector('.request-history-items[aria-busy="true"]'),
  );
  assert.equal(await answer.textContent(), "Only the runtime folder");
  assert.equal(
    (await historyDisclosure.boundingBox()).height,
    historyHeight,
    "a pending refresh preserves history height",
  );
  assert.equal(
    await historyDisclosure.getByText("Loading decisions…").count(),
    0,
  );
  for (let attempt = 0; attempt < 100 && !delayedHistory; attempt++)
    await new Promise((resolve) => setTimeout(resolve, 10));
  assert.ok(delayedHistory, "history request is delayed");
  await delayedHistory.fulfill({
    status: 503,
    json: { error: "Fixture history unavailable" },
  });
  await historyDisclosure.getByRole("alert").waitFor();
  assert.equal(
    await answer.textContent(),
    "Only the runtime folder",
    "failed refresh preserves decisions",
  );
  delayedHistory = undefined;
  await historyDisclosure.locator("summary").click();
  await historyDisclosure.locator("summary").click();
  await page.waitForFunction(() =>
    document.querySelector('.request-history-items[aria-busy="true"]'),
  );
  assert.equal(await answer.textContent(), "Only the runtime folder");
  for (let attempt = 0; attempt < 100 && !delayedHistory; attempt++)
    await new Promise((resolve) => setTimeout(resolve, 10));
  assert.ok(delayedHistory, "history request is delayed");
  await delayedHistory.fulfill({ json: history });
  await page.unroute("**/api/questions?*");
  await page.waitForFunction(() =>
    document.querySelector('.request-history-items[aria-busy="false"]'),
  );
  assert.equal(await historyDisclosure.getByRole("alert").count(), 0);
  for (const width of [1280, 390]) {
    await page.setViewportSize({ width, height: 900 });
    await page.screenshot({ path: join(root, `history-${width}.png`) });
    const box = await historyDisclosure.boundingBox();
    assert.ok(
      box && box.x >= 0 && box.x + box.width <= width + 1,
      "history fits viewport",
    );
  }
  await page.setViewportSize({ width: 1280, height: 900 });
  await page
    .locator("[data-chat]")
    .filter({ hasText: "Other project" })
    .click();
  await page
    .locator(".request-history > summary")
    .filter({ hasText: "Question history" })
    .click();
  await page
    .getByText("No past answers in this chat.", { exact: true })
    .waitFor();
  assert.equal(
    await page.getByText("Only the runtime folder", { exact: true }).count(),
    0,
  );
  await selectLead();
  await page.reload();
  await selectLead();
  await page
    .locator(".request-history > summary")
    .filter({ hasText: "Question history" })
    .click();
  await page
    .locator(".request-history-answer")
    .getByText("Only the runtime folder", { exact: true })
    .waitFor();
  assert.deepEqual(errors, []);
  console.log(
    "Question history UI: PASS (durable defer, restore, answer history, reload, chat isolation, delayed refresh height, failure retention, 390/1280px)",
  );
  console.log("Evidence:", root);
} catch (error) {
  await page?.screenshot({ path: join(root, "failure.png") });
  console.error("Evidence:", root, log);
  throw error;
} finally {
  await browser?.close();
  proc.kill("SIGTERM");
  if (proc.exitCode === null)
    await new Promise((resolve) => proc.once("exit", resolve));
}
