#!/usr/bin/env node
// Production UI with isolated state and terminal responses. No model or PTY starts.
import assert from "node:assert/strict";
import { fileURLToPath } from "node:url";
import { spawn } from "node:child_process";
import { mkdtemp } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, dirname } from "node:path";
import { createRequire } from "node:module";
const repo = dirname(dirname(fileURLToPath(import.meta.url)));
const { chromium } = createRequire(join(repo, "web/package.json"))(
  "playwright-core",
);
const root = await mkdtemp(join(tmpdir(), "studio-motion-audit-"));
const proc = spawn(
  "python3",
  ["-B", join(repo, "tests/simple-ui-fixture.py"), root],
  {
    stdio: ["pipe", "pipe", "pipe"],
    env: { ...process.env, CODEX_BOARD_STATE_DIR: join(root, "board") },
  },
);
let log = "",
  browser;
proc.stderr.on("data", (d) => (log += d));
try {
  const port = await new Promise((resolve, reject) => {
    const timer = setTimeout(
      () => reject(Error("Fixture timeout: " + log)),
      30000,
    );
    proc.stdout.once("data", (d) => {
      clearTimeout(timer);
      resolve(Number(String(d).trim()));
    });
    proc.once("exit", () => {
      clearTimeout(timer);
      reject(Error(log));
    });
  });
  const origin = `http://127.0.0.1:${port}`;
  const initial = await (await fetch(origin + "/api/state")).json();
  const lead = initial.threads.find((a) => a.name === "Release lead");
  const target = initial.threads.find(
    (a) => a.rootId === lead.id && a.name === "Worker 00",
  );
  let phase = 0;
  let created = false;
  const releases = [];
  let holdOldList = false,
    releaseOldList;
  const shell = (id, created) => ({
    id,
    agent: lead.id,
    title: id,
    cwd: lead.cwd,
    status: "exited",
    created,
  });
  const shells = [shell("Existing terminal", 1)];
  browser = await chromium.launch({
    headless: true,
    executablePath:
      process.env.CHROME_BIN ||
      "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
  });
  const page = await browser.newPage({
    viewport: { width: 1440, height: 980 },
  });
  // Keep the status fixture on HTTP snapshots; sync has separate coverage.
  await page.route("**/api/sync/**", (route) =>
    route.fulfill({ status: 503, body: "Fixture uses HTTP snapshots" }),
  );
  await page.route("**/api/state", async (route) => {
    const response = await route.fetch();
    const data = await response.json();
    data.runtime.requests = [];
    for (const a of data.threads)
      if (a.rootId === lead.id && !a.isLead) {
        a.status = "running";
        a.overview = { task: "Independent worker task", result: "" };
        if (a.id === target.id) {
          a.status = phase === 0 ? "queued" : "running";
          a.overview = {
            task:
              "Verify the exact response receipt. " +
              "Keep this explanation open while the worker starts. ".repeat(10),
            result: "",
          };
        }
      }
    await route.fulfill({ response, json: data });
  });
  await page.route("**/api/terminals", async (route) => {
    const snapshot = [...shells];
    if (holdOldList) {
      holdOldList = false;
      await new Promise((resolve) => (releaseOldList = resolve));
    } else if (created) await new Promise((resolve) => releases.push(resolve));
    await route.fulfill({ json: { items: snapshot } });
  });
  await page.route("**/api/terminals/create", async (route) => {
    const next = shell("New stable terminal", 2);
    shells.unshift(next);
    created = true;
    await route.fulfill({ json: next });
  });
  await page.route("**/api/terminals/output?*", (route) =>
    route.fulfill({ json: { text: "", offset: 0, status: "exited" } }),
  );
  await page.goto(origin);
  await page.locator("[data-chat]").filter({ hasText: "Release lead" }).click();
  assert.equal(
    await page.locator("#team-toggle").getAttribute("aria-expanded"),
    "false",
  );
  await page.locator("#team-toggle").click();
  const card = page.locator(`[data-worker="${target.id}"]`).locator("..");
  const details = card.locator(".worker-excerpt");
  await details.locator("summary").click();
  const node = await details.elementHandle();
  const before = await page.evaluate(() => ({
    searchY: document.querySelector("#worker-search").getBoundingClientRect().y,
    scroll: document.querySelector("#workers").scrollTop,
  }));
  phase = 1;
  await page.waitForFunction(
    (id) =>
      document.querySelector(`[data-worker="${id}"] small`)?.textContent ===
      "Working",
    target.id,
  );
  const result = {
    before,
    after: await page.evaluate(() => ({
      searchY: document.querySelector("#worker-search").getBoundingClientRect()
        .y,
      scroll: document.querySelector("#workers").scrollTop,
    })),
    originalNodeConnected: await node.evaluate((n) => n.isConnected),
    expandedAfter: await details.evaluate((n) => n.open),
  };
  assert.equal(
    result.expandedAfter,
    true,
    "A worker task stays expanded when its status changes",
  );
  assert.ok(
    Math.abs(result.before.searchY - result.after.searchY) <= 1,
    "Team counts do not shift the worker search",
  );
  // The other worker has no saved disclosure preference.
  const other = page
    .locator("[data-worker]")
    .filter({ hasText: "Worker 01" })
    .locator("..");
  assert.equal(
    await other.locator(".worker-excerpt").evaluate((node) => node.open),
    false,
  );
  await details.locator("summary").click();
  phase = 0;
  await page.waitForFunction(
    (id) =>
      document.querySelector(`[data-worker="${id}"] small`)?.textContent ===
      "Queued",
    target.id,
  );
  assert.equal(
    await details.evaluate((n) => n.open),
    false,
    "A closed task stays closed after regrouping",
  );
  // Hold a poll that captured the list before terminal creation.
  holdOldList = true;
  await page
    .getByRole("button", { name: "Show terminals", exact: true })
    .click();
  for (let attempt = 0; !releaseOldList && attempt < 100; attempt++)
    await new Promise((resolve) => setTimeout(resolve, 10));
  assert.ok(releaseOldList, "The older poll is pending");
  // Delay the list refresh to inspect the optimistic placement separately.
  await page.getByRole("button", { name: "New terminal", exact: true }).click();
  await page.waitForFunction(
    () =>
      document.querySelector(".terminal-session.is-selected strong")
        ?.textContent === "New stable terminal",
  );
  const local = await page
    .locator(".terminal-session.is-selected")
    .boundingBox();
  assert.equal(
    await page
      .locator(".terminal-session")
      .first()
      .locator("strong")
      .innerText(),
    "New stable terminal",
  );
  await page.waitForFunction(() => document.querySelector(".terminal-xterm"));
  assert.ok(releases.length, "A list refresh is pending after creation");
  created = false;
  for (const release of releases) release();
  await page.waitForFunction(
    () =>
      !Array.from(document.querySelectorAll("button")).find((button) =>
        button.textContent.includes("New terminal"),
      )?.disabled,
  );
  const confirmed = await page
    .locator(".terminal-session.is-selected")
    .boundingBox();
  assert.ok(
    Math.abs(local.y - confirmed.y) <= 1,
    "Server confirmation does not move the new terminal",
  );
  const oldResponse = page.waitForResponse(async (response) => {
    if (new URL(response.url()).pathname !== "/api/terminals") return false;
    return (await response.json()).items.length === 1;
  });
  releaseOldList();
  await oldResponse;
  await page.waitForTimeout(100);
  assert.equal(
    await page.locator(".terminal-session.is-selected strong").count(),
    1,
    "An older poll cannot remove the newly confirmed terminal",
  );
  assert.equal(
    await page.locator(".terminal-session.is-selected strong").innerText(),
    "New stable terminal",
    "An older poll cannot remove the newly confirmed terminal",
  );
  assert.equal(await page.locator(".terminal-session").count(), 2);
  await page.screenshot({
    path: join(root, "team-motion.png"),
    animations: "disabled",
  });
  console.log(
    JSON.stringify({
      passed: true,
      evidence: root,
      summaryShift: result.after.searchY - result.before.searchY,
      terminalShift: confirmed.y - local.y,
    }),
  );
} finally {
  await browser?.close();
  proc.kill("SIGTERM");
}
