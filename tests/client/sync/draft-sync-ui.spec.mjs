import { readTestState, test, spawnFixture as spawn } from "../playwright.mjs";
// Two browser profiles, real SQLite/RxDB replication, no live model calls.
import assert from "node:assert/strict";
import { mkdtemp } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { DRAFT_SYNC_TIMING_MS } from "../../../web/src/sync/draftSyncTiming.mjs";

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
        window.__draftReplicationCreations = 0;
        window.__codexDraftReplicationTestOnly = {
          onCreate: () => window.__draftReplicationCreations++,
        };
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
    const restartRequestSkewMs = 100;
    const restartRequestBoundMs = 900;
    const phone = await testBrowser.newPage({
      viewport: { width: 390, height: 844 },
      isMobile: true,
      hasTouch: true,
    });
    const errors = [];
    for (const page of [desktop, phone])
      page.on("pageerror", (error) => errors.push(error.message));
    await desktop.goto(origin);
    const draftTiming = DRAFT_SYNC_TIMING_MS;
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
    assert.equal(
      await desktop.evaluate(() => window.__retiredDraftMap.accesses),
      0,
      "Current draft edits and synchronization never access the retired aggregate map",
    );
    assert.equal(
      await desktop.evaluate(() => window.__retiredDraftMap.read()),
      JSON.stringify({ [session]: "Retired aggregate-map draft" }),
    );
    // A temporary pull error must not leave an error banner after recovery.
    let failPull = false,
      failPush = false,
      pullFailures = 0,
      pushFailures = 0,
      successfulDraftPosts = 0,
      successfulPulls = 0,
      draftPullRequests = 0,
      remoteProbePulled = false;
    const draftPullRequestTimes = [];
    const draftPullCompletedTimes = [];
    let nextPullGate;
    const remoteProbeId = "remote-pull-probe:remote-pull-probe-session";
    const pushedDraftIds = [];
    const pushedDraftRows = [];
    const pushFailureTimes = [];
    await desktop.route("**/api/sync/pull?*", async (route) => {
      if (new URL(route.request().url()).searchParams.get("scope") !== "drafts")
        return route.continue();
      draftPullRequests++;
      draftPullRequestTimes.push(Date.now());
      if (failPull) {
        pullFailures++;
        draftPullCompletedTimes.push(Date.now());
        return route.abort("failed");
      }
      const response = await route.fetch();
      const body = await response.json();
      const gate = nextPullGate;
      if (gate) {
        nextPullGate = undefined;
        gate.snapshotReadyAt = Date.now();
        gate.body = body;
        gate.response = response;
        gate.startedResolve(gate.snapshotReadyAt);
        await gate.releasePromise;
      }
      if (body.documents.some((document) => document.id === remoteProbeId))
        remoteProbePulled = true;
      successfulPulls++;
      draftPullCompletedTimes.push(Date.now());
      return route.fulfill({ response, body: JSON.stringify(body) });
    });
    await desktop.route("**/api/sync/drafts", (route) => {
      const rows = route.request().postDataJSON().rows;
      pushedDraftIds.push(...rows.map((row) => row.newDocumentState.id));
      pushedDraftRows.push(...rows);
      if (failPush) {
        pushFailures++;
        pushFailureTimes.push(Date.now());
        return route.abort("failed");
      }
      successfulDraftPosts++;
      return route.continue();
    });
    const until = async (condition, label) => {
      for (let i = 0; i < 160; i++) {
        if (await condition()) return;
        await desktop.waitForTimeout(100);
      }
      throw new Error(label);
    };
    const localDraft = (id) =>
      desktop.evaluate(
        async ({ workspaceId, id }) => {
          const databases = await indexedDB.databases();
          const name = databases
            .map((database) => database.name)
            .find(
              (candidate) =>
                candidate?.endsWith(`--0--drafts`) &&
                candidate.includes(workspaceId),
            );
          if (!name) return null;
          const database = await new Promise((resolve, reject) => {
            const request = indexedDB.open(name);
            request.onsuccess = () => resolve(request.result);
            request.onerror = () => reject(request.error);
          });
          try {
            return await new Promise((resolve, reject) => {
              const request = database
                .transaction("docs", "readonly")
                .objectStore("docs")
                .get(id);
              request.onsuccess = () => resolve(request.result || null);
              request.onerror = () => reject(request.error);
            });
          } finally {
            database.close();
          }
        },
        { workspaceId, id },
      );
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
    // This single resume event must yield exactly one pull. The storm phase
    // below covers bursts while restart throttling is active.
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
    assert.equal(
      await status.isVisible(),
      true,
      "a healthy pull cannot hide the failed push",
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

    const stormStartedAt = Date.now();
    const stormPullsAtStart = draftPullRequests;
    const stormCreationsAtStart = await desktop.evaluate(
      () => window.__draftReplicationCreations,
    );
    const stormPushesAtStart = pushFailures;
    const stormFailureTimesAtStart = pushFailureTimes.length;
    const pushProbeUpdate = async (text) => {
      const previous = (await documents()).find(
        (document) => document.id === remoteProbeId,
      );
      assert.ok(previous, "the existing pull probe remains on the server");
      await push(
        { ...JSON.parse(previous.payload), text, updated: Date.now() },
        previous,
      );
    };
    for (let index = 0; index < 60; index++) {
      await pushProbeUpdate(`restart trigger ${index}`);
      await desktop.evaluate(() => window.dispatchEvent(new Event("online")));
      await desktop.waitForTimeout(100);
    }
    const stormDuration = Date.now() - stormStartedAt;
    const stormPulls = draftPullRequests - stormPullsAtStart;
    const stormRestarts =
      (await desktop.evaluate(() => window.__draftReplicationCreations)) -
      stormCreationsAtStart;
    const stormPushFailures = pushFailures - stormPushesAtStart;
    const stormPushOffsets = pushFailureTimes
      .slice(stormFailureTimesAtStart)
      .map((time) => time - stormStartedAt);
    const maxRestartsAtMinimumInterval =
      Math.floor(stormDuration / draftTiming.restartMinInterval) + 1;
    const minExpectedRestarts = Math.max(
      1,
      Math.floor(stormDuration / draftTiming.restartMinInterval) - 1,
    );
    assert.ok(
      stormRestarts >= minExpectedRestarts,
      `100 ms invalidations made only ${stormRestarts} restarts in ${stormDuration} ms; the minimum interval is ${draftTiming.restartMinInterval} ms`,
    );
    assert.ok(
      stormRestarts <= maxRestartsAtMinimumInterval,
      `100 ms invalidations made ${stormRestarts} restarts in ${stormDuration} ms; max at the ${draftTiming.restartMinInterval} ms restart interval is ${maxRestartsAtMinimumInterval}`,
    );
    assert.equal(
      stormPulls,
      stormRestarts,
      "each replacement state makes one initial draft pull",
    );
    assert.ok(
      stormPulls <= maxRestartsAtMinimumInterval,
      `100 ms invalidations made ${stormPulls} draft pulls in ${stormDuration} ms; max at the ${draftTiming.restartMinInterval} ms restart interval is ${maxRestartsAtMinimumInterval}`,
    );
    await until(async () => {
      const local = await localDraft(remoteProbeId);
      return local && JSON.parse(local.payload).text === "restart trigger 59";
    }, "the last remote draft written during the storm is pulled");
    const baseRetryAttempts =
      Math.floor(stormDuration / draftTiming.pushRetry) + 1;
    const maxPushAttempts = baseRetryAttempts + stormRestarts;
    assert.ok(
      stormPushFailures <= maxPushAttempts,
      `100 ms invalidations made ${stormPushFailures} failed pushes at offsets ${JSON.stringify(stormPushOffsets)} in ${stormDuration} ms; the ${baseRetryAttempts} base retry slots plus ${stormRestarts} restart attempts allow ${maxPushAttempts}`,
    );
    console.log(
      "Draft restart storm requests:",
      JSON.stringify({
        stormDuration,
        stormPulls,
        stormPushFailures,
        stormPushOffsets,
      }),
    );
    // Queue several events while a replacement is starting, then let its first
    // push recover. No queued restart may survive into a later outage.
    const failedBurstStart = pushFailures;
    for (let index = 0; index < 6; index++) {
      await pushProbeUpdate(`recovery trigger ${index}`);
    }
    await until(
      () => pushFailures > failedBurstStart,
      "a replacement push fails during the trigger burst",
    );
    const postsBeforeRecovery = successfulDraftPosts;
    failPush = false;
    await desktop.locator("#message").fill("Pending local draft after restart");
    await until(
      () =>
        successfulDraftPosts > postsBeforeRecovery &&
        pushedDraftRows.some(
          (row) =>
            JSON.parse(row.newDocumentState.payload).text ===
            "Pending local draft after restart",
        ),
      "the pending local draft is pushed after replacement recovery",
    );
    const postsBeforeRestore = successfulDraftPosts;
    await desktop.locator("#message").fill("Retained during sync outage");
    await until(
      () => successfulDraftPosts > postsBeforeRestore,
      "the retained composer draft is pushed after recovery",
    );
    await desktop.waitForTimeout(
      draftTiming.pushRetry + draftTiming.pushQuietWait + 200,
    );
    failPush = true;
    await desktop.locator("#message").fill("Second push outage");
    const failuresBeforeNextOutage = pushFailures;
    await until(
      () => pushFailures > failuresBeforeNextOutage,
      "second push outage is observed",
    );

    // Trigger A starts immediately once the restart interval has elapsed.
    const pullsBeforeA = draftPullRequests;
    await desktop.evaluate(() => window.dispatchEvent(new Event("online")));
    await until(
      () => draftPullRequests > pullsBeforeA,
      "trigger A starts its draft pull during the push outage",
    );
    assert.equal(
      draftPullRequests - pullsBeforeA,
      1,
      "trigger A starts exactly one pull",
    );
    const pullAIndex = draftPullRequestTimes.length - 1;
    await until(
      () => draftPullCompletedTimes.length > pullAIndex,
      "trigger A pull completes",
    );
    const pullAStartedAt = draftPullRequestTimes[pullAIndex];
    const pullACompletedAt = draftPullCompletedTimes[pullAIndex];

    // Trigger B arrives 500 ms after A's pull completes. The restart interval
    // retains it and runs one replacement at the next allowed time.
    await pushProbeUpdate("Latest remote draft after deferred trigger B");
    await desktop.waitForTimeout(500);
    const triggerBDispatchedAt = Date.now();
    assert.ok(
      triggerBDispatchedAt - pullACompletedAt >= 500 &&
        triggerBDispatchedAt - pullACompletedAt <= 1_000,
      "trigger B is dispatched about 500 ms after trigger A's pull completes",
    );
    const pullCountBeforeB = draftPullRequests;
    await desktop.evaluate(() => window.dispatchEvent(new Event("online")));
    await until(
      () => draftPullRequests > pullCountBeforeB,
      "trigger B is deferred until the restart interval",
    );
    const pullBIndex = draftPullRequestTimes.length - 1;
    const pullBStartedAt = draftPullRequestTimes[pullBIndex];
    assert.ok(
      pullBStartedAt >=
        pullAStartedAt + draftTiming.restartMinInterval - restartRequestSkewMs,
      `trigger B pull started earlier than the restart interval minus ${restartRequestSkewMs} ms request-log skew: A=${pullAStartedAt}, B=${pullBStartedAt}`,
    );
    assert.ok(
      pullBStartedAt <=
        pullAStartedAt + draftTiming.restartMinInterval + restartRequestBoundMs,
      `trigger B pull exceeded the restart interval plus ${restartRequestBoundMs} ms: A=${pullAStartedAt}, B=${pullBStartedAt}`,
    );
    await until(async () => {
      const local = await localDraft(remoteProbeId);
      return (
        local &&
        JSON.parse(local.payload).text ===
          "Latest remote draft after deferred trigger B"
      );
    }, "deferred trigger B applies the latest remote draft to IndexedDB");
    await until(
      () => draftPullCompletedTimes.length > pullBIndex,
      "trigger B pull completes",
    );
    await desktop.waitForTimeout(
      draftTiming.pushRetry + draftTiming.pushQuietWait + 100,
    );
    assert.equal(
      draftPullRequests - pullCountBeforeB,
      1,
      "trigger B produces one deferred pull and no quiet-period polling",
    );

    // Hold A's initial response after its server snapshot is captured. B is
    // dispatched while A is still in progress, so the latest server row is
    // intentionally absent from A and needs one queued replacement pull.
    let releaseHeldPull;
    let signalHeldPullStarted;
    const heldPullStarted = new Promise((resolve) => {
      signalHeldPullStarted = resolve;
    });
    const heldPullReleased = new Promise((resolve) => {
      releaseHeldPull = resolve;
    });
    nextPullGate = {
      startedResolve: signalHeldPullStarted,
      releasePromise: heldPullReleased,
    };
    const pullsBeforeInFlightA = draftPullRequests;
    await desktop.evaluate(() => window.dispatchEvent(new Event("online")));
    await heldPullStarted;
    const inFlightAStartedAt = draftPullRequestTimes.at(-1);
    assert.equal(
      draftPullRequests - pullsBeforeInFlightA,
      1,
      "in-flight trigger A starts one held pull",
    );
    await pushProbeUpdate("Latest remote draft after in-flight trigger B");
    const inFlightPullCountBeforeB = draftPullRequests;
    await desktop.evaluate(() => window.dispatchEvent(new Event("online")));
    // onResume debounces lifecycle events by 50 ms; leave A held long enough
    // for B's callback to queue its deferred restart before releasing A.
    await desktop.waitForTimeout(100);
    releaseHeldPull();
    await until(
      () => draftPullRequests > inFlightPullCountBeforeB,
      "trigger B during A's restart gets one deferred pull",
    );
    const inFlightBIndex = draftPullRequestTimes.length - 1;
    const inFlightBStartedAt = draftPullRequestTimes[inFlightBIndex];
    assert.ok(
      inFlightBStartedAt >=
        inFlightAStartedAt +
          draftTiming.restartMinInterval -
          restartRequestSkewMs,
      `in-flight trigger B pull started earlier than the restart interval minus ${restartRequestSkewMs} ms request-log skew: A=${inFlightAStartedAt}, B=${inFlightBStartedAt}`,
    );
    assert.ok(
      inFlightBStartedAt <=
        inFlightAStartedAt +
          draftTiming.restartMinInterval +
          restartRequestBoundMs,
      `in-flight trigger B pull exceeded the interval plus ${restartRequestBoundMs} ms: A=${inFlightAStartedAt}, B=${inFlightBStartedAt}`,
    );
    await until(async () => {
      const local = await localDraft(remoteProbeId);
      return (
        local &&
        JSON.parse(local.payload).text ===
          "Latest remote draft after in-flight trigger B"
      );
    }, "in-flight trigger B applies the latest remote draft to IndexedDB");
    await until(
      () => draftPullCompletedTimes.length > inFlightBIndex,
      "in-flight trigger B pull completes",
    );
    await desktop.waitForTimeout(
      draftTiming.pushRetry + draftTiming.pushQuietWait + 100,
    );
    assert.equal(
      draftPullRequests - inFlightPullCountBeforeB,
      1,
      "trigger B during a restart gets one later pull and no polling",
    );

    assert.equal(
      await desktop.locator("#message").inputValue(),
      "Second push outage",
    );
    const postsBeforeFinalRecovery = successfulDraftPosts;
    failPush = false;
    await desktop.locator("#message").fill("Retained during sync outage");
    await until(
      () => successfulDraftPosts > postsBeforeFinalRecovery,
      "the retained draft recovers after the second outage",
    );
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
    const localDraft = (id) =>
      page.evaluate(
        async ({ workspaceId, id }) => {
          const databases = await indexedDB.databases();
          const name = databases
            .map((database) => database.name)
            .find(
              (candidate) =>
                candidate?.endsWith(`--0--drafts`) &&
                candidate.includes(workspaceId),
            );
          if (!name) return null;
          const database = await new Promise((resolve, reject) => {
            const request = indexedDB.open(name);
            request.onsuccess = () => resolve(request.result);
            request.onerror = () => reject(request.error);
          });
          try {
            return await new Promise((resolve, reject) => {
              const request = database
                .transaction("docs", "readonly")
                .objectStore("docs")
                .get(id);
              request.onsuccess = () => resolve(request.result || null);
              request.onerror = () => reject(request.error);
            });
          } finally {
            database.close();
          }
        },
        { workspaceId, id },
      );
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
      assert.deepEqual(await response.json(), []);
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
    await until(async () => {
      const local = await localDraft(targetId);
      return local && JSON.parse(local.payload).text === pulledRemoteText;
    }, "an unopposed remote draft update is applied to the local RxDB row");
    const pulledRemoteMaster = (await documents()).find(
      (document) => document.id === targetId,
    );
    assert.ok(pulledRemoteMaster);
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
      "conflict retry uses the latest remote master as assumed master",
    );
    assert.deepEqual(pageErrors, []);
  } finally {
    fixture.kill("SIGTERM");
  }
});
