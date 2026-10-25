import {
  readTestState,
  test,
  spawnFixture as spawn,
  rustUiFixture,
  expect,
} from "../playwright.mjs";
// Two browser profiles, real SQLite/RxDB replication, no live model calls.
import assert from "node:assert/strict";
import { mkdtemp } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { DRAFT_SYNC_TIMING_MS } from "../../src/sync/draftSyncTiming.mjs";

test("Draft sync ui", async ({
  browser: testBrowser,
  page: runnerPage,
  context: runnerContext,
}) => {
  test.setTimeout(45_000);
  const state = await mkdtemp(join(tmpdir(), "studio-draft-sync-"));
  const fixture = spawn(process.execPath, [rustUiFixture, state], {
    stdio: ["pipe", "pipe", "pipe"],
    env: { ...process.env, FIXTURE_WORKER_COUNT: "1" },
  });
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
    const snapshot = await readTestState(origin);
    const session = snapshot.runtime.agents.find(
      (agent) => agent.name === "Other project",
    ).id;
    const { workspaceId } = await (
      await fetch(origin + "/api/sync/identity")
    ).json();
    const retiredDraftMapKey = `codex-drafts:${workspaceId}`;
    await runnerPage.addInitScript(
      ({ workspaceId, session, key }) => {
        localStorage.setItem(
          "codex-sync-workspace",
          JSON.stringify(workspaceId),
        );
        const originalSetItem = Storage.prototype.setItem;
        const originalGetItem = Storage.prototype.getItem;
        const originalRemoveItem = Storage.prototype.removeItem;
        originalSetItem.call(
          localStorage,
          key,
          JSON.stringify({ [session]: "Retired aggregate-map draft" }),
        );
        window.__retiredDraftMap = {
          key,
          original: JSON.stringify({
            [session]: "Retired aggregate-map draft",
          }),
          accesses: 0,
          read: () => originalGetItem.call(localStorage, key),
        };
        Storage.prototype.getItem = function (candidate) {
          if (candidate === key) window.__retiredDraftMap.accesses++;
          return originalGetItem.call(this, candidate);
        };
        Storage.prototype.setItem = function (candidate, value) {
          if (candidate === key) window.__retiredDraftMap.accesses++;
          return originalSetItem.call(this, candidate, value);
        };
        Storage.prototype.removeItem = function (candidate) {
          if (candidate === key) window.__retiredDraftMap.accesses++;
          return originalRemoveItem.call(this, candidate);
        };
      },
      { workspaceId, session, key: retiredDraftMapKey },
    );
    const documents = async () =>
      (
        await (
          await fetch(origin + "/api/sync/pull?scope=drafts&after=0&limit=100")
        ).json()
      ).documents;
    const push = async (value, previous) => {
      const toApiDocument = (document) => {
        const { _deleted, ...state } = document;
        return { ...state, deleted: _deleted === true };
      };
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
              newDocumentState: toApiDocument({
                id: value.id,
                seq: 0,
                payload: JSON.stringify(value),
                _deleted: false,
              }),
              ...(previous
                ? { assumedMasterState: toApiDocument(previous) }
                : {}),
            },
          ],
        }),
      });
      assert.equal(response.status, 200);
      assert.deepEqual(await response.json(), []);
    };
    const editDraft = async (page, text) => {
      const saved = page.waitForResponse(
        (response) =>
          new URL(response.url()).pathname === "/api/sync/drafts" &&
          response.request().method() === "POST" &&
          response.ok() &&
          response
            .request()
            .postDataJSON()
            .rows.some(
              (row) => JSON.parse(row.newDocumentState.payload).text === text,
            ),
      );
      await page.locator("#message").fill(text);
      assert.deepEqual(await (await saved).json(), []);
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
    await desktop.waitForFunction(() => window.__retiredDraftMap);
    assert.equal(
      await desktop.evaluate(() => window.__retiredDraftMap.accesses),
      0,
      "App startup must not access the retired aggregate draft map",
    );
    await desktop
      .getByRole("button", { name: /^Other project/ })
      .first()
      .click();
    assert.notEqual(
      await desktop.locator("#message").inputValue(),
      "Retired aggregate-map draft",
      "A draft from the retired aggregate map must not appear",
    );
    assert.equal(
      await desktop.evaluate(() => window.__retiredDraftMap.read()),
      JSON.stringify({ [session]: "Retired aggregate-map draft" }),
      "The aggregate map must remain byte-identical",
    );
    await desktop.locator("#message").fill("Draft from desktop");
    const waitText = (page, text) =>
      expect(page.locator("#message")).toHaveValue(text, { timeout: 15_000 });
    await phone.goto(origin);
    await phone
      .getByRole("button", { name: /^Other project/ })
      .first()
      .click();
    await waitText(phone, "Draft from desktop");
    await editDraft(phone, "Continue on phone");
    await desktop.getByRole("button", { name: /^Other drafts/ }).waitFor();
    assert.equal(
      await desktop.locator("#message").inputValue(),
      "Draft from desktop",
      "A synced remote edit must not replace text edited in this tab",
    );
    await desktop.reload();
    await waitText(desktop, "Continue on phone");
    assert.equal(
      await desktop.getByRole("button", { name: /^Other drafts/ }).count(),
      0,
    );
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
    assert.equal(
      await desktop.evaluate(() => window.__retiredDraftMap.accesses),
      0,
      "Current draft edits and synchronization never access the retired aggregate map",
    );
    assert.equal(
      await desktop.evaluate(() => window.__retiredDraftMap.read()),
      JSON.stringify({ [session]: "Retired aggregate-map draft" }),
    );
    // Failure/retry scheduling is exercised with virtual time by
    // sync-draft-pull-gate and draft-restart-races; keep this witness on the
    // real two-profile handoff and mobile conflict picker.
    // Independent old branches model concurrent edits from offline profiles.
    const stamp = Date.now() - 10000;
    await Promise.all(
      Array.from({ length: 12 }, (_, i) =>
        push({
          id: `offline-${i}:${session}`,
          session,
          device: `offline-${i}`,
          text: `Alternative ${i}\n` + "Long draft text ".repeat(700),
          updated: stamp + i,
        }),
      ),
    );
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
    await phone.locator(`[data-chat="${session}"]`).click();
    await waitText(phone, "Current draft");
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
      `PASS: two-device draft handoff, clear, reload, bounded mobile preview, durable dismissal and replacement. ${state}`,
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
  const state = await mkdtemp(join(tmpdir(), "studio-draft-conflict-"));
  const fixture = spawn(process.execPath, [rustUiFixture, state], {
    stdio: ["pipe", "pipe", "pipe"],
    env: { ...process.env, FIXTURE_WORKER_COUNT: "1" },
  });
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
    const snapshot = await readTestState(origin);
    const { workspaceId } = await (
      await fetch(`${origin}/api/sync/identity`)
    ).json();
    const documents = async () =>
      (
        await (
          await fetch(`${origin}/api/sync/pull?scope=drafts&after=0&limit=100`)
        ).json()
      ).documents;
    // Observe the actual Dexie transaction commit used by the mounted app.
    // A successful HTTP response alone does not prove the remote row is local.
    await page.addInitScript(() => {
      const committed = new Map();
      const changes = new EventTarget();
      const put = IDBObjectStore.prototype.put;
      IDBObjectStore.prototype.put = function (value, ...args) {
        const request = put.call(this, value, ...args);
        if (this.transaction.db.name.endsWith("--0--drafts")) {
          const payload = JSON.parse(value.payload);
          this.transaction.addEventListener("complete", () => {
            committed.set(value.id, payload);
            changes.dispatchEvent(new Event("commit"));
          });
        }
        return request;
      };
      window.waitForDraftText = (id, text) =>
        new Promise((resolve) => {
          const check = () => {
            if (committed.get(id)?.text !== text) return;
            changes.removeEventListener("commit", check);
            resolve();
          };
          changes.addEventListener("commit", check);
          check();
        });
    });
    const sendRemote = async (value, previous) => {
      const toApiDocument = (document) => {
        const { _deleted, ...state } = document;
        return { ...state, deleted: _deleted === true };
      };
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
              newDocumentState: toApiDocument({
                id: value.id,
                seq: 0,
                payload: JSON.stringify(value),
                _deleted: false,
              }),
              assumedMasterState: toApiDocument(previous),
            },
          ],
        }),
      });
      assert.equal(response.status, 200);
      assert.deepEqual(await response.json(), []);
    };
    const hasText = (request, text) =>
      new URL(request.url()).pathname === "/api/sync/drafts" &&
      request.method() === "POST" &&
      request
        .postDataJSON()
        .rows.some(
          (row) => JSON.parse(row.newDocumentState.payload).text === text,
        );
    const savedText = (text) =>
      page.waitForResponse(
        (response) => response.ok() && hasText(response.request(), text),
      );
    const attempts = [];
    let failPush = false;
    let remotePulled = false;
    let completeRemotePull;
    const remotePullCompleted = new Promise((resolve) => {
      completeRemotePull = resolve;
    });
    let targetId = "";
    let remoteText = "";
    const pageErrors = [];
    page.on("pageerror", (error) => pageErrors.push(error.message));
    await page.route("**/api/sync/drafts", async (route) => {
      const rows = route.request().postDataJSON().rows;
      attempts.push(...rows);
      if (failPush) {
        return route.abort("failed");
      }
      return route.continue();
    });
    await page.route("**/api/sync/pull?*", async (route) => {
      if (new URL(route.request().url()).searchParams.get("scope") !== "drafts")
        return route.continue();
      const response = await route.fetch();
      const body = await response.json();
      const containsRemoteWinner = body.documents.some(
        (document) =>
          document.id === targetId &&
          JSON.parse(document.payload).text === remoteText,
      );
      await route.fulfill({ response, body: JSON.stringify(body) });
      if (containsRemoteWinner) {
        remotePulled = true;
        completeRemotePull();
      }
    });
    await page.clock.install();
    await page.goto(origin);
    await page
      .getByRole("button", { name: /^Other project/ })
      .first()
      .click();
    const baselineSaved = savedText("Conflict baseline");
    await page.locator("#message").fill("Conflict baseline");
    const baselineResponse = await baselineSaved;
    assert.deepEqual(await baselineResponse.json(), []);
    const baselinePost = baselineResponse.request().postDataJSON().rows[0];
    const baseline = (await documents()).find(
      (document) => document.id === baselinePost.newDocumentState.id,
    );
    assert.ok(baseline);
    targetId = baseline.id;
    const pulledRemoteText = "Remote master applied before local edit";
    await sendRemote(
      {
        ...JSON.parse(baseline.payload),
        text: pulledRemoteText,
        updated: Date.now() + 30_000,
      },
      baseline,
    );
    await page.evaluate(() => window.dispatchEvent(new Event("online")));
    await page.evaluate(({ id, text }) => window.waitForDraftText(id, text), {
      id: targetId,
      text: pulledRemoteText,
    });
    const pulledRemoteMaster = (await documents()).find(
      (document) => document.id === targetId,
    );
    assert.ok(pulledRemoteMaster);
    remoteText = "Remote winner after resume";
    failPush = true;
    const rejected = page.waitForEvent("requestfailed", {
      predicate: (request) => hasText(request, "Pending local conflict"),
    });
    await page.locator("#message").fill("Pending local conflict");
    await rejected;
    assert.ok(
      attempts.some(
        (row) =>
          row.newDocumentState.id === targetId &&
          JSON.parse(row.newDocumentState.payload).text ===
            "Pending local conflict" &&
          row.assumedMasterState &&
          JSON.parse(row.assumedMasterState.payload).text === pulledRemoteText,
      ),
      "push uses the master state written by the earlier pull",
    );
    await sendRemote(
      {
        ...JSON.parse(baseline.payload),
        text: remoteText,
        updated: Date.now() + 60_000,
      },
      pulledRemoteMaster,
    );
    await page.evaluate(() => window.dispatchEvent(new Event("online")));
    await remotePullCompleted;
    assert.equal(remotePulled, true);
    const status = page.locator("[data-draft-sync-status]");
    await page.clock.fastForward(DRAFT_SYNC_TIMING_MS.noticeDelay);
    await status.waitFor();
    assert.equal(
      await status.innerText(),
      "Draft sync paused. Retrying automatically.",
      "a healthy pull cannot hide the failed push",
    );
    assert.ok(
      (await status.boundingBox()).height < 50,
      "failure is a compact status",
    );
    const recovered = savedText(remoteText);
    failPush = false;
    await page.clock.runFor(
      DRAFT_SYNC_TIMING_MS.pushRetry + DRAFT_SYNC_TIMING_MS.pushQuietWait,
    );
    assert.deepEqual(await (await recovered).json(), []);
    const current = (await documents()).find(
      (document) => document.id === targetId,
    );
    const version = JSON.parse(current.payload);
    assert.equal(version.text, remoteText);
    assert.ok(version.alternatives.includes("Pending local conflict"));
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
      "conflict retry uses the latest remote master as assumed master",
    );
    await page.clock.fastForward(DRAFT_SYNC_TIMING_MS.noticeDelay);
    await expect(status).toBeHidden();
    assert.deepEqual(pageErrors, []);
  } finally {
    fixture.kill("SIGTERM");
  }
});
