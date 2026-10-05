import { test } from "../playwright.mjs";
// Two browser profiles, real SQLite/RxDB replication, no live model calls.
import assert from "node:assert/strict";
import { spawnFixture as spawn } from "../playwright.mjs";
import { mkdtemp } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
test("Draft sync ui", async ({
  browser: testBrowser,
  page: runnerPage,
  context: runnerContext,
}) => {
  test.setTimeout(180_000);
  const root = fileURLToPath(new URL("../../../", import.meta.url));
  const state = await mkdtemp(join(tmpdir(), "studio-draft-sync-"));
  const fixture = spawn(
    process.env.PYTHON_BIN || "python3",
    ["-B", join(root, "tests/simple-ui-fixture.py"), state],
    {
      stdio: ["pipe", "pipe", "pipe"],
      env: { ...process.env, TOKEN_RATE_WORKER_COUNT: "1" },
    },
  );
  let log = "";
  fixture.stderr.on("data", (chunk) => {
    log += chunk;
  });
  try {
    const port = await new Promise((resolve, reject) => {
      fixture.stdout.once("data", (chunk) =>
        resolve(Number(String(chunk).trim())),
      );
      fixture.once("exit", () => reject(new Error(log)));
    });
    const origin = `http://127.0.0.1:${port}`;
    const snapshot = await (await fetch(origin + "/api/state")).json();
    const session = snapshot.runtime.agents.find(
      (agent) => agent.name === "Other project",
    ).id;
    const { workspaceId } = await (
      await fetch(origin + "/api/sync/identity")
    ).json();
    const documents = async () =>
      (
        await (
          await fetch(origin + "/api/sync/pull?scope=drafts&after=0&limit=100")
        ).json()
      ).documents;
    const push = async (value, previous) => {
      const response = await fetch(origin + "/api/sync/drafts", {
        method: "POST",
        headers: {
          "Content-Type": "application/json",
          "X-Canvas-Token": snapshot.token,
          "X-Canvas-Workspace": workspaceId,
          Origin: origin,
        },
        body: JSON.stringify({
          rows: [
            {
              newDocumentState: {
                id: value.id,
                seq: 0,
                payload: JSON.stringify(value),
              },
              ...(previous ? { assumedMasterState: previous } : {}),
            },
          ],
        }),
      });
      assert.equal(response.status, 200);
      assert.deepEqual(await response.json(), []);
    };
    const untilDraftSaved = async (text) => {
      for (let i = 0; i < 100; i++) {
        if (
          (await documents()).some(
            (doc) => JSON.parse(doc.payload).text === text,
          )
        )
          return;
        await new Promise((resolve) => setTimeout(resolve, 100));
      }
      throw new Error("The draft did not reach the server: " + text);
    };
    const desktop = runnerPage;
    await desktop.setViewportSize({ width: 1280, height: 850 });
    const phone = await testBrowser.newPage({
      viewport: { width: 390, height: 844 },
      isMobile: true,
      hasTouch: true,
    });
    const errors = [];
    for (const page of [desktop, phone])
      page.on("pageerror", (error) => errors.push(error.message));
    await desktop.goto(origin);
    await desktop
      .getByRole("button", { name: /^Other project/ })
      .first()
      .click();
    await desktop.locator("#message").fill("Draft from desktop");
    const waitText = (page, text) =>
      page.waitForFunction(
        (text) => document.querySelector("#message")?.value === text,
        text,
        { timeout: 15000 },
      );
    await phone.goto(origin);
    await phone.getByRole("button", { name: "Toggle conversations" }).click();
    await phone
      .getByRole("button", { name: /^Other project/ })
      .first()
      .click();
    await waitText(phone, "Draft from desktop");
    await phone.locator("#message").fill("Continue on phone");
    await untilDraftSaved("Continue on phone");
    await desktop.waitForTimeout(3500);
    assert.equal(
      await desktop.locator("#message").inputValue(),
      "Draft from desktop",
      "A synced remote edit must not replace text edited in this tab",
    );
    await desktop.getByRole("button", { name: /^Other drafts/ }).waitFor();
    await desktop.reload();
    await waitText(desktop, "Continue on phone");
    assert.equal(
      await desktop.getByRole("button", { name: /^Other drafts/ }).count(),
      0,
    );
    await desktop.reload();
    await waitText(desktop, "Continue on phone");
    await phone.locator("#message").fill("");
    await waitText(desktop, "");
    await desktop.reload();
    await waitText(desktop, "");
    assert.equal(
      await desktop.getByRole("button", { name: /^Other drafts/ }).count(),
      0,
      "Cleared drafts must not return as conflicts",
    );
    await phone.locator("#message").fill("Current draft");
    await waitText(desktop, "Current draft");
    // A temporary pull error must not leave an error banner after recovery.
    let failPull = false,
      failPush = false,
      pullFailures = 0,
      pushFailures = 0,
      successfulPulls = 0,
      draftPullRequests = 0,
      remoteProbePulled = false;
    const remoteProbeId = "remote-pull-probe:remote-pull-probe-session";
    const pushedDraftIds = [];
    const pushedDraftRows = [];
    await desktop.route("**/api/sync/pull?*", async (route) => {
      if (new URL(route.request().url()).searchParams.get("scope") !== "drafts")
        return route.continue();
      draftPullRequests++;
      if (failPull) {
        pullFailures++;
        return route.abort("failed");
      }
      const response = await route.fetch();
      const body = await response.json();
      if (body.documents.some((document) => document.id === remoteProbeId))
        remoteProbePulled = true;
      successfulPulls++;
      return route.fulfill({ response, body: JSON.stringify(body) });
    });
    await desktop.route("**/api/sync/drafts", (route) => {
      const rows = route.request().postDataJSON().rows;
      pushedDraftIds.push(...rows.map((row) => row.newDocumentState.id));
      pushedDraftRows.push(...rows);
      if (failPush) {
        pushFailures++;
        return route.abort("failed");
      }
      return route.continue();
    });
    const until = async (condition, label) => {
      for (let i = 0; i < 160; i++) {
        if (await condition()) return;
        await desktop.waitForTimeout(100);
      }
      throw new Error(label);
    };
    await push({
      id: remoteProbeId,
      session: "remote-pull-probe-session",
      device: "remote-pull-probe",
      text: "Remote draft must not echo back",
      updated: Date.now(),
    });
    await until(() => remoteProbePulled, "remote probe draft is pulled");
    assert.equal(
      pushedDraftIds.includes(remoteProbeId),
      false,
      "a pulled remote draft is not pushed back",
    );
    const status = desktop.locator("[data-draft-sync-status]");
    failPull = true;
    await push({
      id: "idle-probe-a:unused",
      session: "unused",
      device: "idle-probe-a",
      text: "probe",
      updated: Date.now(),
    });
    await until(() => pullFailures > 0, "pull failure observed");
    assert.equal(
      await status.count(),
      0,
      "short failure does not move the chat",
    );
    failPull = false;
    const previousPulls = successfulPulls;
    // Draft pulls are event-driven; resume supplies the next retry trigger.
    await desktop.evaluate(() => window.dispatchEvent(new Event("online")));
    await until(() => successfulPulls > previousPulls, "empty pull recovered");
    await desktop.waitForTimeout(8500);
    assert.equal(
      await status.count(),
      0,
      "recovered pull leaves no delayed warning",
    );
    failPull = true;
    await push({
      id: "idle-probe-b:unused",
      session: "unused",
      device: "idle-probe-b",
      text: "probe",
      updated: Date.now(),
    });
    await status.waitFor({ timeout: 18000 });
    assert.equal(
      await status.innerText(),
      "Draft sync paused. Retrying automatically.",
    );
    assert.ok(
      (await status.boundingBox()).height < 50,
      "failure is a compact status",
    );
    assert.equal(
      await desktop
        .getByText(/RxDB Error-Code|RC_PULL|Find out more about this error/)
        .count(),
      0,
    );
    failPush = true;
    await desktop.locator("#message").fill("Retained during sync outage");
    await until(() => pushFailures > 0, "push failure observed");
    const pullsBeforeRecovery = successfulPulls;
    const requestsBeforeRecovery = draftPullRequests;
    failPull = false;
    // Resume retries the pull while the failed push remains in its own lane.
    await desktop.evaluate(() => window.dispatchEvent(new Event("online")));
    await until(
      () => successfulPulls > pullsBeforeRecovery,
      "pull restored while push fails",
    );
    assert.equal(
      draftPullRequests - requestsBeforeRecovery,
      1,
      "resume starts one draft pull while push retries",
    );
    for (let restart = 0; restart < 2; restart++) {
      const requestsBeforeRestart = draftPullRequests;
      await desktop.evaluate(() => window.dispatchEvent(new Event("online")));
      await until(
        () => draftPullRequests > requestsBeforeRestart,
        `resume restart ${restart + 2} pulls once`,
      );
      assert.equal(
        draftPullRequests - requestsBeforeRestart,
        1,
        `resume restart ${restart + 2} makes exactly one pull request`,
      );
    }
    const pullsAfterRecovery = draftPullRequests;
    const pushesAfterRecovery = pushFailures;
    await desktop.waitForTimeout(30_000);
    assert.equal(
      draftPullRequests,
      pullsAfterRecovery,
      "draft pulls remain event-driven during a sustained push outage",
    );
    assert.ok(
      pushFailures > pushesAfterRecovery,
      "push keeps retrying during the sustained outage",
    );
    const pullRequestsTriggeredByResume =
      pullsAfterRecovery - requestsBeforeRecovery;
    console.log(
      "Draft sync outage requests:",
      JSON.stringify({
        draftPullRequests,
        pullRequestsTriggeredByResume,
        pullRequestsDuring30sOutage: draftPullRequests - pullsAfterRecovery,
        pushFailuresDuring30sOutage: pushFailures - pushesAfterRecovery,
      }),
    );
    assert.equal(
      await status.isVisible(),
      true,
      "healthy pull cannot hide failed push",
    );
    assert.equal(
      await desktop.locator("#message").inputValue(),
      "Retained during sync outage",
    );
    failPush = false;
    await phone.reload();
    await waitText(phone, "Retained during sync outage");
    await status.waitFor({ state: "hidden", timeout: 15000 });
    await desktop.reload();
    await waitText(desktop, "Retained during sync outage");
    await desktop.locator("#message").fill("Current draft");
    await waitText(phone, "Current draft");
    // Independent old branches model concurrent edits from offline profiles.
    const stamp = Date.now() - 10000;
    for (let i = 0; i < 12; i++)
      await push({
        id: `offline-${i}:${session}`,
        session,
        device: `offline-${i}`,
        text: `Alternative ${i}\n` + "Long draft text ".repeat(700),
        updated: stamp + i,
      });
    await phone
      .getByRole("button", { name: "Other drafts (12)", exact: true })
      .waitFor({ timeout: 15000 });
    assert.equal(await phone.locator("#message").inputValue(), "Current draft");
    assert.equal(await phone.locator(".sync-notices details").count(), 0);
    const before = await phone.locator("#composer").boundingBox();
    await phone
      .getByRole("button", { name: "Other drafts (12)", exact: true })
      .click();
    const panel = phone.locator(".draft-versions");
    await panel.waitFor();
    const bounds = await panel.boundingBox();
    assert.ok(
      bounds.height <= 321 && bounds.width <= 366,
      JSON.stringify(bounds),
    );
    const after = await phone.locator("#composer").boundingBox();
    assert.ok(
      Math.abs(before.y - after.y) < 1,
      "Conflict preview must not shrink the chat",
    );
    await phone.waitForTimeout(250);
    await phone.screenshot({ path: join(state, "draft-versions.png") });
    await phone
      .getByRole("button", { name: "Dismiss", exact: true })
      .first()
      .click();
    await phone
      .getByRole("button", { name: "Dismiss all", exact: true })
      .click();
    assert.equal(
      await phone.getByRole("button", { name: /^Other drafts/ }).count(),
      0,
    );
    await phone.reload();
    await waitText(phone, "Current draft");
    await phone.waitForTimeout(3500);
    assert.equal(
      await phone.getByRole("button", { name: /^Other drafts/ }).count(),
      0,
      "Dismissal must survive reload and replication",
    );
    // New content on a dismissed branch remains available.
    const previous = (await documents()).find(
      (doc) => doc.id === `offline-0:${session}`,
    );
    await push(
      {
        ...JSON.parse(previous.payload),
        text: "Changed alternative",
        updated: stamp + 100,
      },
      previous,
    );
    await phone
      .getByRole("button", { name: "Other drafts (1)", exact: true })
      .waitFor({ timeout: 15000 });
    await phone
      .getByRole("button", { name: "Other drafts (1)", exact: true })
      .click();
    await phone
      .getByRole("button", { name: "Replace text", exact: true })
      .click();
    await waitText(phone, "Changed alternative");
    await desktop.reload();
    await waitText(desktop, "Changed alternative");
    await until(
      async () =>
        (await documents()).some(
          (document) =>
            JSON.parse(document.payload).text === "Changed alternative",
        ),
      "replacement branch is committed before the concurrent edit",
    );
    assert.equal(
      await phone.getByRole("button", { name: /^Other drafts/ }).count(),
      0,
    );
    assert.equal(
      await desktop.getByRole("button", { name: /^Other drafts/ }).count(),
      0,
    );
    assert.deepEqual(errors, []);
    console.log(
      `PASS: two-device draft handoff, clear, reload, bounded mobile preview, durable dismissal, replacement, transient failure, direction-specific recovery, retained offline edits. ${state}`,
    );
  } finally {
    await Promise.all(
      testBrowser
        .contexts()
        .filter((ownedContext) => ownedContext !== runnerContext)
        .map((ownedContext) => ownedContext.close()),
    );
  }
});

test("Draft sync resume preserves a pending conflict", async ({ page }) => {
  test.setTimeout(90_000);
  const root = fileURLToPath(new URL("../../../", import.meta.url));
  const state = await mkdtemp(join(tmpdir(), "studio-draft-conflict-"));
  const fixture = spawn(
    process.env.PYTHON_BIN || "python3",
    ["-B", join(root, "tests/simple-ui-fixture.py"), state],
    {
      stdio: ["pipe", "pipe", "pipe"],
      env: { ...process.env, TOKEN_RATE_WORKER_COUNT: "1" },
    },
  );
  let log = "";
  fixture.stderr.on("data", (chunk) => {
    log += chunk;
  });
  try {
    const port = await new Promise((resolve, reject) => {
      fixture.stdout.once("data", (chunk) =>
        resolve(Number(String(chunk).trim())),
      );
      fixture.once("exit", (code) =>
        reject(new Error(`fixture exited ${code}: ${log}`)),
      );
    });
    const origin = `http://127.0.0.1:${port}`;
    const snapshot = await (await fetch(`${origin}/api/state`)).json();
    const { workspaceId } = await (
      await fetch(`${origin}/api/sync/identity`)
    ).json();
    const documents = async () =>
      (
        await (
          await fetch(`${origin}/api/sync/pull?scope=drafts&after=0&limit=100`)
        ).json()
      ).documents;
    const sendRemote = async (value, previous) => {
      const response = await fetch(`${origin}/api/sync/drafts`, {
        method: "POST",
        headers: {
          "Content-Type": "application/json",
          "X-Canvas-Token": snapshot.token,
          "X-Canvas-Workspace": workspaceId,
          Origin: origin,
        },
        body: JSON.stringify({
          rows: [
            {
              newDocumentState: {
                id: value.id,
                seq: 0,
                payload: JSON.stringify(value),
              },
              assumedMasterState: previous,
            },
          ],
        }),
      });
      assert.equal(response.status, 200);
    };
    const until = async (condition, label, timeout = 20_000) => {
      const deadline = Date.now() + timeout;
      while (Date.now() < deadline) {
        if (await condition()) return;
        await page.waitForTimeout(100);
      }
      throw new Error(label);
    };
    const attempts = [];
    let pushFailures = 0;
    let failPush = false;
    let remotePulled = false;
    let targetId = "";
    let remoteText = "";
    const pageErrors = [];
    page.on("pageerror", (error) => pageErrors.push(error.message));
    await page.route("**/api/sync/drafts", async (route) => {
      const rows = route.request().postDataJSON().rows;
      attempts.push(...rows);
      if (failPush) {
        pushFailures++;
        return route.abort("failed");
      }
      return route.continue();
    });
    await page.route("**/api/sync/pull?*", async (route) => {
      if (new URL(route.request().url()).searchParams.get("scope") !== "drafts")
        return route.continue();
      const response = await route.fetch();
      const body = await response.json();
      if (
        body.documents.some(
          (document) =>
            document.id === targetId &&
            JSON.parse(document.payload).text === remoteText,
        )
      )
        remotePulled = true;
      return route.fulfill({ response, body: JSON.stringify(body) });
    });
    await page.goto(origin);
    await page
      .getByRole("button", { name: /^Other project/ })
      .first()
      .click();
    await page.locator("#message").fill("Conflict baseline");
    await until(async () => {
      const post = [...attempts]
        .reverse()
        .find(
          (row) =>
            JSON.parse(row.newDocumentState.payload).text ===
            "Conflict baseline",
        );
      return (
        post &&
        (await documents()).some(
          (document) => document.id === post.newDocumentState.id,
        )
      );
    }, "conflict baseline reaches the server");
    const baselinePost = [...attempts]
      .reverse()
      .find(
        (row) =>
          JSON.parse(row.newDocumentState.payload).text === "Conflict baseline",
      );
    const baseline = (await documents()).find(
      (document) => document.id === baselinePost.newDocumentState.id,
    );
    assert.ok(baseline);
    targetId = baseline.id;
    remoteText = "Remote winner after resume";
    failPush = true;
    const failuresBeforeEdit = pushFailures;
    await page.locator("#message").fill("Pending local conflict");
    await until(
      () =>
        pushFailures > failuresBeforeEdit &&
        attempts.some(
          (row) =>
            row.newDocumentState.id === targetId &&
            JSON.parse(row.newDocumentState.payload).text ===
              "Pending local conflict",
        ),
      "pending local edit is rejected by the push route",
    );
    await sendRemote(
      {
        ...JSON.parse(baseline.payload),
        text: remoteText,
        updated: Date.now() + 60_000,
      },
      baseline,
    );
    await page.evaluate(() => window.dispatchEvent(new Event("online")));
    await until(
      () => remotePulled,
      "resume pulls the remote edit while push retries",
    );
    const status = page.locator("[data-draft-sync-status]");
    await status.waitFor({ timeout: 15_000 });
    assert.equal(
      await status.innerText(),
      "Draft sync paused. Retrying automatically.",
      "a healthy pull cannot hide the failed push",
    );
    failPush = false;
    await until(async () => {
      const current = (await documents()).find(
        (document) => document.id === targetId,
      );
      if (!current) return false;
      const version = JSON.parse(current.payload);
      return (
        version.text === remoteText &&
        version.alternatives.includes("Pending local conflict")
      );
    }, "remote master wins without discarding the local conflict");
    assert.ok(
      attempts.some(
        (row) =>
          row.newDocumentState.id === targetId &&
          row.assumedMasterState &&
          JSON.parse(row.assumedMasterState.payload).text === remoteText &&
          JSON.parse(row.newDocumentState.payload).alternatives.includes(
            "Pending local conflict",
          ),
      ),
      "recovery push uses the pulled remote document as assumed master",
    );
    assert.deepEqual(pageErrors, []);
  } finally {
    fixture.kill("SIGTERM");
  }
});
