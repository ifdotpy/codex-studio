#!/usr/bin/env node
import {
  apiSchemaHeaders,
  readFixtureSyncContract,
  stubEntityState,
  legacySnapshotRoute,
  test,
  expect,
  spawnFixture as spawn,
  readTestState,
} from "../playwright.mjs";
// Production UI with isolated state and terminal responses. No model or PTY starts.
import { fileURLToPath } from "node:url";
import { mkdtemp } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
test("Team Motion Ui", async ({
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

  const repo = fileURLToPath(new URL("../../../../../../", import.meta.url));
  const root = await mkdtemp(join(tmpdir(), "studio-motion-audit-"));
  const proc = spawn(
    "python3",
    [
      "-B",
      join(repo, "workspaces/runtime/apps/server/tests/simple-ui-fixture.py"),
      root,
    ],
    {
      stdio: ["pipe", "pipe", "pipe"],
      env: { ...process.env, CODEX_BOARD_STATE_DIR: join(root, "board") },
    },
  );
  let log = "";
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
    const initial = await readTestState(origin);
    const lead = initial.threads.find((a) => a.name === "Release lead");
    const target = initial.threads.find(
      (a) => a.rootId === lead.id && a.name === "Worker 00",
    );
    const entitySnapshot = structuredClone(initial);
    delete entitySnapshot.token;
    delete entitySnapshot.threads;
    entitySnapshot.runtime.requests = [];
    for (const agent of entitySnapshot.runtime.agents) {
      if (agent.rootId !== lead.id || agent.isLead) continue;
      agent.status = agent.name === "Worker 01" ? "queued" : "running";
      agent.overview = {
        task: "Independent worker task",
        result: "Independent worker report",
      };
    }
    const targetAgent = entitySnapshot.runtime.agents.find(
      (agent) => agent.id === target.id,
    );
    targetAgent.status = "queued";
    targetAgent.overview = {
      task: "Verify the exact response receipt.",
      result:
        "Verified the exact response receipt. " +
        "Keep this explanation open while the worker starts. ".repeat(10),
    };
    const syncFixture = await stubEntityState(
      testPage,
      entitySnapshot,
      await readFixtureSyncContract(origin),
    );
    let created = false;
    const releases = [];
    let olderPollStarted;
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
    const page = testPage;
    const syncPullRequests = [],
      stateRequests = [];
    page.on("request", (request) => {
      const path = new URL(request.url()).pathname;
      if (path === "/api/sync/pull") syncPullRequests.push(request.url());
      if (legacySnapshotRoute.test(request.url()))
        stateRequests.push(request.url());
    });
    await page.setViewportSize({ width: 1440, height: 980 });
    await page.route("**/api/terminals", async (route) => {
      const snapshot = [...shells];
      if (holdOldList) {
        holdOldList = false;
        await new Promise((resolve) => {
          releaseOldList = resolve;
          olderPollStarted();
        });
      } else if (created)
        await new Promise((resolve) => releases.push(resolve));
      await route.fulfill({
        json: { items: snapshot },
        headers: apiSchemaHeaders(),
      });
    });
    await page.route("**/api/terminals/create", async (route) => {
      const next = shell("New stable terminal", 2);
      shells.unshift(next);
      created = true;
      await route.fulfill({ json: next, headers: apiSchemaHeaders() });
    });
    await page.route("**/api/terminals/output?*", (route) =>
      route.fulfill({
        json: { text: "", offset: 0, status: "exited" },
        headers: apiSchemaHeaders(),
      }),
    );
    await page.goto(origin);
    await page.locator("[data-chat]").first().waitFor();
    assert.ok(
      syncPullRequests.some(
        (url) => new URL(url).searchParams.get("scope") === "state:entities:v1",
      ),
    );
    assert.deepEqual(stateRequests, []);
    await page
      .locator("[data-chat]")
      .filter({ hasText: "Release lead" })
      .click();
    assert.equal(
      await page.locator("#team-toggle").getAttribute("aria-expanded"),
      "false",
    );
    await page.locator("#team-toggle").click();
    const card = page
      .locator(`[data-worker="${target.id}"]`)
      .locator("..")
      .locator("..");
    const details = card.locator(".worker-excerpt");
    await details.locator("summary").click();
    const node = await details.elementHandle();
    const before = await page.evaluate(() => ({
      searchY: document.querySelector("#worker-search").getBoundingClientRect()
        .y,
      scroll: document.querySelector("#workers").scrollTop,
    }));
    const workingSnapshot = structuredClone(entitySnapshot);
    workingSnapshot.runtime.agents.find(
      (agent) => agent.id === target.id,
    ).status = "running";
    await syncFixture.update(workingSnapshot, {
      origin,
      token: initial.token,
      resources: [{ kind: "state" }],
    });
    await page.waitForFunction(
      (id) =>
        document.querySelector(`[data-worker="${id}"] small`)?.textContent ===
        "Working",
      target.id,
    );
    const result = {
      before,
      after: await page.evaluate(() => ({
        searchY: document
          .querySelector("#worker-search")
          .getBoundingClientRect().y,
        scroll: document.querySelector("#workers").scrollTop,
      })),
      originalNodeConnected: await node.evaluate((n) => n.isConnected),
      expandedAfter: await details.evaluate((n) => n.open),
    };
    assert.equal(
      result.expandedAfter,
      true,
      "A worker report stays expanded when its status changes",
    );
    assert.ok(
      Math.abs(result.before.searchY - result.after.searchY) <= 1,
      `Team counts do not shift the worker search: ${JSON.stringify(result)}`,
    );
    // The other worker has no saved disclosure preference.
    const other = page
      .locator("[data-worker]")
      .filter({ hasText: "Worker 01" })
      .locator("..")
      .locator("..");
    assert.equal(
      await other.locator(".worker-excerpt").evaluate((node) => node.open),
      false,
    );
    await details.locator("summary").click();
    const queuedSnapshot = structuredClone(entitySnapshot);
    await syncFixture.update(queuedSnapshot, {
      origin,
      token: initial.token,
      resources: [{ kind: "state" }],
    });
    await page.waitForFunction(
      (id) =>
        document
          .querySelector(`[data-worker="${id}"] .worker-state`)
          ?.getAttribute("aria-label") === "Queued",
      target.id,
    );
    assert.equal(
      await details.evaluate((n) => n.open),
      false,
      "A closed task stays closed after regrouping",
    );
    // Hold a poll that captured the list before terminal creation.
    holdOldList = true;
    const waitingForOlderPoll = new Promise((resolve) => {
      olderPollStarted = resolve;
    });
    await page
      .getByRole("button", { name: "Show terminals", exact: true })
      .click();
    await syncFixture.update(entitySnapshot, {
      origin,
      token: initial.token,
      resources: [{ kind: "terminals" }],
    });
    await waitingForOlderPoll;
    assert.ok(releaseOldList, "The older poll is pending");
    // Delay the list refresh to inspect the optimistic placement separately.
    await page
      .getByRole("button", { name: "New terminal", exact: true })
      .click();
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
    assert.ok(
      syncPullRequests.some(
        (url) => new URL(url).searchParams.get("scope") === "state:entities:v1",
      ),
    );
    assert.deepEqual(stateRequests, []);
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
        syncPullRequests: syncPullRequests.length,
        stateRequests,
      }),
    );
  } finally {
    // spawnFixture is cleaned by the shared Playwright test fixture.
  }
});
