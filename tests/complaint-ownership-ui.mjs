#!/usr/bin/env node
// Isolated UI contract. HTTP fixtures exercise retry, version conflict, and legacy behavior.
import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { mkdtemp } from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { createRequire } from "node:module";
const repo = dirname(dirname(fileURLToPath(import.meta.url)));
const require = createRequire(join(repo, "web/package.json"));
const { chromium } = require("playwright-core");
const root = await mkdtemp(join(tmpdir(), "codex-complaint-owners-"));
const proc = spawn(
  "python3",
  ["-B", join(repo, "tests/simple-ui-fixture.py"), root],
  { stdio: ["pipe", "pipe", "pipe"] },
);
let browser,
  page,
  log = "";
proc.stderr.on("data", (data) => (log += data));
try {
  const port = await new Promise((resolve, reject) => {
    proc.stdout.once("data", (data) => resolve(Number(String(data).trim())));
    proc.once("exit", () => reject(Error(log)));
  });
  const origin = `http://127.0.0.1:${port}`;
  const state = await (await fetch(origin + "/api/state")).json();
  const lead = state.threads.find((a) => a.name === "Release lead");
  const worker = state.threads.find((a) => a.parentId === lead.id);
  assert.ok(worker);
  const makeComplaint = (id, author, recipient, text) => ({
    id,
    leadId: lead.id,
    author,
    recipient,
    version: 1,
    text,
    title: text,
    status: "open",
    created: Date.now() / 1000,
    updated: Date.now() / 1000,
    readAt: null,
    responses: [],
    authorName: author === lead.id ? lead.name : worker.name,
    leadName: lead.name,
    needsResponse: true,
  });
  const user = makeComplaint(
    "owner-problem",
    lead.id,
    "user",
    "The test account needs access to the private repository.",
  );
  user.responses.push({
    id: "old-self-response",
    author: lead.id,
    text: "Previous lead response before ownership split.",
    status: "resolved",
    at: Date.now() / 1000,
  });
  const assigned = makeComplaint(
    "worker-problem",
    worker.id,
    "lead",
    "The worker needs a decision from its orchestrator.",
  );
  const legacy = makeComplaint(
    "legacy-problem",
    lead.id,
    undefined,
    "Historical complaint on an older server.",
  );
  delete legacy.version;
  legacy.needsResponse = false;
  legacy.status = "resolved";
  legacy.readAt = Date.now() / 1000;
  legacy.responses.push({
    id: "legacy-self-response",
    author: lead.id,
    text: "The lead previously closed its own complaint.",
    status: "resolved",
    at: legacy.readAt,
  });
  const records = [user, assigned, legacy];
  browser = await chromium.launch({
    executablePath:
      process.env.CHROME_BIN ||
      "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
    headless: true,
  });
  page = await browser.newPage({ viewport: { width: 1200, height: 950 } });
  const errors = [];
  page.on("pageerror", (error) => errors.push(error.message));
  await page.route("**/api/state", async (route) => {
    const response = await route.fetch();
    const data = await response.json();
    data.runtime.complaints = records;
    await route.fulfill({ response, json: data });
  });
  await page.route("**/api/complaint?*", (route) =>
    route.fulfill({
      json: records.find(
        (c) => c.id === new URL(route.request().url()).searchParams.get("id"),
      ),
    }),
  );
  let attempts = [],
    mode = "lost",
    completed = 0,
    deferredResponse;
  await page.route("**/api/complaints", async (route) => {
    const payload = route.request().postDataJSON();
    attempts.push(payload);
    if (mode === "deferred") {
      deferredResponse = route;
      return;
    }
    if (mode === "lost") {
      mode = "retry";
      completed++;
      user.version++;
      user.responses.push({
        id: payload.id,
        author: "user",
        text: payload.text,
        status: payload.status,
        at: Date.now() / 1000,
      });
      user.status = payload.status;
      user.needsResponse = false;
      user.readAt = Date.now() / 1000;
      await route.abort("failed");
    } else if (mode === "retry") {
      assert.deepEqual(payload, attempts[0]);
      await route.fulfill({ json: user });
      mode = "conflict";
    } else if (mode === "conflict") {
      user.version++;
      user.responses.push({
        id: "other-tab",
        author: "user",
        text: "Another window updated this complaint.",
        status: "in_progress",
        at: Date.now() / 1000,
      });
      await route.fulfill({
        status: 409,
        json: { error: "Complaint version changed" },
      });
      mode = "success";
    } else {
      assert.equal(payload.version, user.version);
      assert.notEqual(payload.id, attempts.at(-2).id);
      completed++;
      user.version++;
      user.responses.push({
        id: payload.id,
        author: "user",
        text: payload.text,
        status: payload.status,
        at: Date.now() / 1000,
      });
      user.status = payload.status;
      await route.fulfill({ json: user });
    }
  });
  await page.goto(origin);
  await page.locator("#open-complaints").click();
  assert.equal(
    await page
      .getByRole("tab", { name: /^For you/ })
      .getAttribute("aria-selected"),
    "true",
  );
  assert.equal(await page.locator("[data-complaint]").count(), 2);
  const legacyCard = page.locator('[data-complaint="legacy-problem"]');
  assert.match(
    await legacyCard.textContent(),
    /Awaiting response/i,
    "legacy self-closed complaint stays in the user pending inbox",
  );
  assert.match(await legacyCard.textContent(), /Awaiting your response/);
  assert.match(
    await page.getByRole("tab", { name: /^For you/ }).textContent(),
    /2/,
  );
  assert.match(
    await page.getByRole("tab", { name: /^For orchestrator/ }).textContent(),
    /1/,
  );
  await page.evaluate(async () => {
    await Promise.all(
      document
        .getAnimations()
        .filter((a) => a.effect?.getTiming().iterations !== Infinity)
        .map((a) => a.finished.catch(() => {})),
    );
  });
  await page.screenshot({ path: join(root, "complaint-inbox.png") });
  await page.locator('[data-complaint="owner-problem"]').click();
  const dialog = page.getByRole("dialog", { name: "Complaint", exact: true });
  await dialog.getByRole("heading", { name: "Your response" }).waitFor();
  assert.match(
    await dialog.locator(".complaint-response strong").first().textContent(),
    /^Release lead/,
  );
  assert.equal(user.readAt, null, "opening a complaint does not mark it read");
  assert.equal(
    await dialog.getByRole("button", { name: "Send response" }).isDisabled(),
    true,
  );
  await dialog
    .getByLabel("Action or reason")
    .fill("I will grant the account access.");
  await page.evaluate(async () => {
    await Promise.all(
      document
        .getAnimations()
        .filter((a) => a.effect?.getTiming().iterations !== Infinity)
        .map((a) => a.finished.catch(() => {})),
    );
  });
  await page.screenshot({ path: join(root, "complaint-response.png") });
  await dialog.getByRole("button", { name: "Send response" }).click();
  await dialog.getByRole("button", { name: "Retry response" }).waitFor();
  assert.equal(await dialog.getByLabel("Action or reason").isDisabled(), true);
  await dialog.getByRole("button", { name: "Close", exact: true }).click();
  await page.locator("#complaint-filter").selectOption("all");
  await page.locator('[data-complaint="owner-problem"]').click();
  await dialog.getByRole("button", { name: "Retry response" }).click();
  await dialog.getByRole("button", { name: "Send response" }).waitFor();
  assert.equal(
    completed,
    1,
    "uncertain retry does not create another response",
  );
  await dialog.getByLabel("Action or reason").fill("Access is now available.");
  await dialog.getByLabel("Action", { exact: true }).selectOption("resolved");
  await dialog.getByRole("button", { name: "Send response" }).click();
  await dialog
    .getByRole("alert")
    .filter({ hasText: "This complaint changed" })
    .waitFor();
  await dialog
    .getByText("Another window updated this complaint.", { exact: true })
    .waitFor();
  assert.equal(attempts.length, 3, "conflict does not auto-resubmit");
  await dialog.getByRole("button", { name: "Send response" }).click();
  await dialog
    .locator(".complaint-response")
    .filter({ hasText: "Access is now available." })
    .waitFor();
  assert.equal(completed, 2);
  await dialog.getByRole("button", { name: "Close", exact: true }).click();
  await page.getByRole("tab", { name: /^For orchestrator/ }).click();
  await page.locator('[data-complaint="worker-problem"]').click();
  await dialog
    .getByText("A response from the orchestrator is required.")
    .waitFor();
  assert.equal(await dialog.locator(".complaint-reply").count(), 0);
  await dialog.getByRole("button", { name: "Close", exact: true }).click();
  await page.getByRole("tab", { name: /^For you/ }).click();
  await page.locator('[data-complaint="legacy-problem"]').click();
  await dialog.getByText("Update the server to respond here.").waitFor();
  assert.match(
    await dialog.locator(".complaint-response strong").first().textContent(),
    /^Release lead/,
  );
  assert.equal(
    legacy.status,
    "resolved",
    "summary normalization preserves historical detail",
  );
  assert.equal(await dialog.locator(".complaint-reply").count(), 0);
  await dialog.getByRole("button", { name: "Close", exact: true }).click();
  // A response from a closed dialog must not replace the complaint now open.
  for (const status of [200, 409]) {
    await page.locator('[data-complaint="owner-problem"]').click();
    await dialog
      .getByLabel("Action or reason")
      .fill(`Deferred response ${status}`);
    mode = "deferred";
    deferredResponse = null;
    await dialog.getByRole("button", { name: "Send response" }).click();
    for (let attempt = 0; !deferredResponse && attempt < 100; attempt++) {
      await new Promise((resolve) => setTimeout(resolve, 10));
    }
    assert.ok(deferredResponse, "the response request is pending");
    await dialog.getByRole("button", { name: "Close", exact: true }).click();
    await page.locator('[data-complaint="legacy-problem"]').click();
    await dialog.getByText("Update the server to respond here.").waitFor();
    await page.evaluate((forbidden) => {
      window.complaintRaceObserved = false;
      window.complaintRaceObserver = new MutationObserver(() => {
        if (
          document
            .querySelector('[role="dialog"]')
            ?.textContent.includes(forbidden)
        )
          window.complaintRaceObserved = true;
      });
      window.complaintRaceObserver.observe(document.body, {
        childList: true,
        subtree: true,
        characterData: true,
      });
    }, user.text);
    const settled = page.waitForResponse((response) =>
      response.url().endsWith("/api/complaints"),
    );
    const conflictDetail =
      status === 409
        ? page.waitForResponse((response) =>
            response.url().includes("/api/complaint?id=owner-problem"),
          )
        : null;
    await deferredResponse.fulfill({
      status,
      json: status === 200 ? user : { error: "Complaint version changed" },
    });
    await settled;
    // The conflict handler refreshes the state and fetches the old complaint again.
    if (status === 409) {
      await conflictDetail;
    }
    await page.evaluate(async () => {
      await new Promise(requestAnimationFrame);
      await new Promise(requestAnimationFrame);
    });
    assert.equal(
      await page.evaluate(() => window.complaintRaceObserved),
      false,
      "late response never replaces another complaint",
    );
    await dialog.getByText(legacy.text, { exact: true }).waitFor();
    assert.equal(await dialog.locator(".complaint-reply").count(), 0);
    await page.evaluate(() => window.complaintRaceObserver.disconnect());
    await dialog.getByRole("button", { name: "Close", exact: true }).click();
  }
  await page.setViewportSize({ width: 390, height: 844 });
  await page.evaluate(async () => {
    await Promise.all(
      document
        .getAnimations()
        .filter((a) => a.effect?.getTiming().iterations !== Infinity)
        .map((a) => a.finished.catch(() => {})),
    );
  });
  await page.screenshot({ path: join(root, "complaint-mobile.png") });
  assert.ok(
    await page.evaluate(
      () => document.documentElement.scrollWidth <= innerWidth,
    ),
    "no horizontal overflow",
  );
  assert.deepEqual(errors, []);
  console.log(
    "Complaint ownership UI: PASS (routing, exact retry, conflict, late response, legacy, mobile)",
  );
  console.log("Screenshots:", root);
} catch (error) {
  await page?.screenshot({ path: join(root, "failure.png") });
  console.error("Evidence:", root);
  throw error;
} finally {
  await browser?.close();
  proc.kill("SIGTERM");
  if (proc.exitCode === null)
    await new Promise((resolve) => proc.once("exit", resolve));
}
