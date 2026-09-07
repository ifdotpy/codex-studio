#!/usr/bin/env node
// Production UI and isolated HTTP fixture. Requests deliberately finish out of order.
import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { mkdtemp, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { createRequire } from "node:module";
const repo = dirname(dirname(fileURLToPath(import.meta.url)));
const { chromium } = createRequire(join(repo, "web/package.json"))(
  "playwright-core",
);
const evidence = await mkdtemp(join(tmpdir(), "studio-snapshot-order-"));
const fixture = spawn(
  "python3",
  ["-B", join(repo, "tests/simple-ui-fixture.py"), evidence],
  {
    stdio: ["ignore", "pipe", "pipe"],
    env: { ...process.env, CODEX_BOARD_STATE_DIR: join(evidence, "board") },
  },
);
let log = "",
  browser;
fixture.stderr.on("data", (data) => (log += data));
const waitFor = async (condition, label) => {
  for (let index = 0; index < 120; index++) {
    if (condition()) return;
    await new Promise((resolve) => setTimeout(resolve, 50));
  }
  throw Error(label);
};
try {
  const port = await new Promise((resolve, reject) => {
    fixture.stdout.once("data", (data) => resolve(Number(String(data).trim())));
    fixture.once("exit", () => reject(Error(log)));
  });
  const origin = `http://127.0.0.1:${port}`;
  const initial = await (await fetch(`${origin}/api/state`)).json();
  const lead = initial.threads.find((agent) => agent.name === "Release lead");
  const worker = initial.threads.find(
    (agent) => agent.rootId === lead.id && agent.name === "Worker 00",
  );
  browser = await chromium.launch({
    executablePath:
      process.env.CHROME_BIN ||
      "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
    headless: true,
  });
  const page = await browser.newPage({
    viewport: { width: 1440, height: 960 },
  });
  page.setDefaultTimeout(12000);
  const errors = [],
    requests = [];
  page.on("pageerror", (error) => errors.push(error.message));
  let revision = 0,
    holdNext = false,
    oldRequest,
    currentFailure = false,
    pendingAnswer,
    answered = false;
  const snapshot = () => {
    const data = structuredClone(initial);
    if (answered) data.runtime.requests = [];
    for (const list of [data.threads, data.runtime.agents])
      for (const agent of list)
        if (agent.id === worker.id) {
          agent.name = `Worker revision ${revision}`;
          agent.status = ["queued", "running", "failed"][revision];
        }
    return data;
  };
  // This fixture tests the legacy API used by servers before mobile sync.
  await page.route("**/api/sync/identity", (route) =>
    route.fulfill({ status: 404, json: { error: "Not found" } }),
  );
  await page.route("**/api/state", async (route) => {
    const data = snapshot();
    requests.push({ revision, held: holdNext, failure: currentFailure });
    if (holdNext) {
      holdNext = false;
      oldRequest = { route, data };
      return;
    }
    await route.fulfill(
      currentFailure
        ? { status: 503, json: { error: "Current snapshot unavailable" } }
        : { json: data },
    );
  });
  await page.route("**/api/messages", (route) =>
    route.fulfill({ json: { status: "sent" } }),
  );
  await page.route("**/api/answer", (route) => {
    pendingAnswer = route;
  });
  await page.goto(origin);
  await page.locator(`[data-chat="${lead.id}"]`).click();
  const workerButton = page.locator(`[data-worker="${worker.id}"]`);
  await workerButton.getByText("Worker revision 0", { exact: true }).waitFor();

  // The real Mantine loading state must not translate the button label.
  const card = page.locator('[data-request="async-question"]');
  await card.getByRole("button", { name: "Answer", exact: true }).click();
  await card
    .getByRole("textbox", { name: "Which scope?" })
    .fill("Only this fixture");
  const answer = card.getByRole("button", { name: "Send answer", exact: true });
  const label = answer.locator(".mantine-Button-inner");
  // Finish the browser scroll before measuring the loading transition.
  await answer.scrollIntoViewIfNeeded();
  const before = await label.boundingBox();
  await answer.click();
  await waitFor(() => pendingAnswer, "Answer request reaches the held route");
  await page.waitForFunction(() =>
    document.querySelector(
      '.request-answer-actions button[data-loading="true"]',
    ),
  );
  assert.equal(
    await label.evaluate((node) => getComputedStyle(node).transform),
    "none",
    "Loading keeps the inner button transform unset",
  );
  const during = await label.boundingBox();

  for (const key of ["x", "y", "width", "height"])
    assert.ok(
      Math.abs(before[key] - during[key]) <= 1,
      `Loading label ${key} stays stable`,
    );
  answered = true;
  await pendingAnswer.continue();
  await card.waitFor({ state: "hidden" });

  for (const oldFailure of [false, true]) {
    oldRequest = null;
    holdNext = true;
    await waitFor(() => oldRequest, "Background snapshot poll is held");
    const oldRevision = revision;
    revision++;
    await page.locator("#message").fill(`Snapshot refresh ${revision}`);
    await page.locator("#send").click();
    await workerButton
      .getByText(`Worker revision ${revision}`, { exact: true })
      .waitFor();
    const expectedStatus = revision === 1 ? "Working" : "Failed";
    assert.equal(
      await workerButton.locator("small").innerText(),
      expectedStatus,
    );
    const freshNode = await workerButton.elementHandle();
    assert.equal(
      oldRequest.data.threads.find((agent) => agent.id === worker.id).name,
      `Worker revision ${oldRevision}`,
      "Held response contains the previous snapshot",
    );
    await oldRequest.route.fulfill(
      oldFailure
        ? { status: 503, json: { error: "Obsolete snapshot failure" } }
        : { json: oldRequest.data },
    );
    // Two paints settle the response before the next scheduled 1.6s poll can hide a regression.
    await page.waitForTimeout(150);
    await page.evaluate(
      () =>
        new Promise((resolve) =>
          requestAnimationFrame(() => requestAnimationFrame(resolve)),
        ),
    );
    assert.equal(
      await workerButton.locator("strong").innerText(),
      `Worker revision ${revision}`,
      "Old responses cannot replace the confirmed worker",
    );
    assert.equal(
      await workerButton.locator("small").innerText(),
      expectedStatus,
      "Old responses cannot revert worker status",
    );
    assert.equal(
      await freshNode.evaluate((node) => node.isConnected),
      true,
      "Obsolete responses do not regroup the confirmed worker",
    );
    assert.equal(
      await page.locator("#error").count(),
      0,
      "An obsolete error does not replace a successful snapshot",
    );
  }

  // A current failure still appears and the next current success clears it.
  currentFailure = true;
  await page
    .locator("#error")
    .getByText("Current snapshot unavailable", { exact: true })
    .waitFor();
  currentFailure = false;
  await page.locator("#error").waitFor({ state: "hidden" });
  assert.deepEqual(errors, []);
  await page.screenshot({
    path: join(evidence, "snapshot-order.png"),
    fullPage: true,
  });
  await writeFile(
    join(evidence, "requests.json"),
    JSON.stringify({ requests, button: { before, during } }, null, 2),
  );
  console.log(
    `PASS: delayed snapshot success and error cannot revert newer data; current errors remain visible; loading button label stays fixed. ${evidence}`,
  );
} finally {
  await browser?.close();
  fixture.kill("SIGTERM");
}
