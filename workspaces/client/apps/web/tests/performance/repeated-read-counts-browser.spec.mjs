// Count real renderer GETs against the isolated repository fixture backend.
import assert from "node:assert/strict";
import { mkdtemp } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import {
  test,
  installApiSchemaFixtureHeaders,
  spawnFixture as spawn,
} from "../playwright.mjs";

const measured = new Set([
  "/api/models",
  "/api/limits",
  "/api/accounts",
  "/api/desktop",
  "/api/costs",
]);

async function repeatedReadCountsScenario(
  browser,
  { modelOnly = false, accountsOnly = false } = {},
) {
  const repo = fileURLToPath(new URL("../../../../../../", import.meta.url));
  const state = await mkdtemp(join(tmpdir(), "repeated-read-counts-"));
  const fixture = spawn(
    "python3",
    [
      "-B",
      join(repo, "workspaces/runtime/apps/server/tests/simple-ui-fixture.py"),
      state,
    ],
    {
      stdio: ["pipe", "pipe", "pipe"],
      env: {
        ...process.env,
        XDG_STATE_HOME: join(state, "xdg-state"),
        XDG_CACHE_HOME: join(state, "xdg-cache"),
        CODEX_HOME: join(state, "codex-home"),
        CODEX_AGENTS_STATE_DIR: join(state, "codex-state"),
        RESOURCE_FRESHNESS_UI_FIXTURE: "1",
      },
    },
  );
  let log = "";
  const fixtureResponses = new Map();
  const fixtureWaiters = new Map();
  let fixtureOutputBuffer = "";
  fixture.stdout.on("data", (chunk) => {
    fixtureOutputBuffer += String(chunk);
    const lines = fixtureOutputBuffer.split("\n");
    fixtureOutputBuffer = lines.pop() || "";
    for (const line of lines) {
      try {
        const response = JSON.parse(line);
        const waiter = fixtureWaiters.get(response.id);
        if (waiter) {
          clearTimeout(waiter.timeout);
          fixtureWaiters.delete(response.id);
          waiter.resolve(response);
        } else if (response.id) fixtureResponses.set(response.id, response);
      } catch {}
    }
  });
  fixture.stderr.on("data", (chunk) => (log += chunk));
  const port = await new Promise((resolve, reject) => {
    fixture.stdout.once("data", (chunk) =>
      resolve(Number(String(chunk).trim())),
    );
    fixture.once("exit", () => reject(Error(log)));
  });
  const context = await browser.newContext(); // clean profile for each run
  try {
    const page = await context.newPage();
    page.setDefaultTimeout(10000);
    installApiSchemaFixtureHeaders(page);
    const counts = new Map();
    const resourceEvents = [];
    let observedEventCount = 0;
    const streamOpens = [];
    const requestTimeline = [];
    await page.addInitScript(() => {
      const OriginalEventSource = window.EventSource;
      window.EventSource = class extends OriginalEventSource {
        constructor(...args) {
          super(...args);
          this.addEventListener("resources", (event) => {
            window.__resourceEvents.push({
              at: Date.now(),
              event: JSON.parse(event.data),
            });
          });
        }
      };
      window.__resourceEvents = [];
    });
    page.on("request", (request) => {
      const url = new URL(request.url());
      if (url.pathname === "/api/sync/stream") {
        const attempt = {
          at: Date.now(),
          url: `${url.pathname}${url.search}`,
          request,
          status: null,
          networkError: false,
          failure: null,
        };
        streamOpens.push(attempt);
      }
      if (measured.has(url.pathname))
        requestTimeline.push({
          at: Date.now(),
          method: request.method(),
          key: `${url.pathname}${url.search}`,
        });
      if (request.method() !== "GET" || !measured.has(url.pathname)) return;
      const key = `${url.pathname}${url.search}`;
      counts.set(key, (counts.get(key) || 0) + 1);
    });
    page.on("response", (response) => {
      const url = new URL(response.url());
      if (url.pathname !== "/api/sync/stream") return;
      const attempt = streamOpens.find(
        (candidate) => candidate.request === response.request(),
      );
      if (attempt) attempt.status = response.status();
    });
    page.on("requestfailed", (request) => {
      const url = new URL(request.url());
      if (url.pathname !== "/api/sync/stream") return;
      const attempt = streamOpens.find(
        (candidate) => candidate.request === request,
      );
      if (attempt && attempt.status === null) {
        attempt.networkError = true;
        attempt.failure = request.failure()?.errorText || "network error";
      }
    });
    const streamAccounting = (attempts) => {
      const latestByUrl = new Map();
      const logical = [];
      const retries = [];
      const failed = [];
      for (const attempt of attempts) {
        const previous = latestByUrl.get(attempt.url);
        const retry =
          previous &&
          (previous.networkError ||
            (previous.status !== null && previous.status >= 500));
        if (retry) retries.push(attempt);
        else logical.push(attempt);
        if (
          attempt.networkError ||
          (attempt.status !== null && attempt.status >= 500)
        )
          failed.push(attempt);
        latestByUrl.set(attempt.url, attempt);
      }
      return { logical, retries, failed };
    };
    const snapshot = () => Object.fromEntries([...counts].sort());
    const streamSummary = (entries) =>
      entries.map(({ at, url }) => {
        const parsed = new URL(url, "http://studio.test");
        const resources = JSON.parse(parsed.searchParams.get("resources"));
        return {
          at,
          resources: resources.reduce((result, resource) => {
            result[resource.kind] = (result[resource.kind] || 0) + 1;
            return result;
          }, {}),
        };
      });
    const waitForQuiet = async () => {
      let previous = "";
      let stable = 0;
      await page.waitForFunction(() => !!document.querySelector("#message"));
      await page.waitForFunction(() => {
        const text = document.querySelector("#message");
        return !!text && !text.disabled;
      });
      while (stable < 4) {
        await page.waitForTimeout(250);
        const current = JSON.stringify(snapshot());
        stable = current === previous ? stable + 1 : 0;
        previous = current;
      }
    };
    const captureNewResourceEvents = async () => {
      const events = await page.evaluate(() => window.__resourceEvents);
      resourceEvents.push(...events.slice(observedEventCount));
      observedEventCount = events.length;
    };
    const loadStartedAt = Date.now();
    await page.goto(`http://127.0.0.1:${port}`);
    await page.locator("#conversation-title").waitFor();
    const loadFirstContentMs = Date.now() - loadStartedAt;
    await waitForQuiet();
    const load = snapshot();
    const loadStreams = [...streamOpens];
    const loadSettledAt = Date.now();
    await captureNewResourceEvents();
    if (accountsOnly) {
      const count = load["/api/accounts"] || 0;
      const invalidations = resourceEvents.filter(
        ({ at, event }) =>
          at >= loadStartedAt &&
          at <= loadSettledAt &&
          event.reason === "change" &&
          event.resources.some((resource) => resource.kind === "accounts"),
      ).length;
      assert.ok(
        count <= 1 + invalidations,
        `page load read /api/accounts ${count} times with ${invalidations} matching resource changes`,
      );
      return [];
    }

    counts.clear();
    const otherChat = page
      .locator("[data-chat]")
      .filter({ hasText: "Other project" });
    const otherChatId = await otherChat.getAttribute("data-chat");
    const openOtherStartedAt = Date.now();
    await otherChat.click();
    await page.waitForFunction(
      (id) =>
        document
          .querySelector(".sidebar-row.selected [data-chat]")
          ?.getAttribute("data-chat") === id,
      otherChatId,
    );
    const openOtherFirstContentMs = Date.now() - openOtherStartedAt;
    const streamedText = `Fixture streamed answer ${Date.now()}`;
    const streamMessageId = `fixture-stream-${Date.now()}`;
    const streamRequestId = `stream-transcript-${Date.now()}`;
    const streamAck = fixtureResponses.has(streamRequestId)
      ? Promise.resolve(fixtureResponses.get(streamRequestId))
      : new Promise((resolve, reject) => {
          const timeout = setTimeout(
            () => reject(new Error(`fixture stream write timed out: ${log}`)),
            5000,
          );
          fixtureWaiters.set(streamRequestId, { resolve, reject, timeout });
        });
    const streamStartedAt = Date.now();
    fixture.stdin.write(
      `${JSON.stringify({
        id: streamRequestId,
        method: "fixture/stream-transcript",
        params: { agent: otherChatId, id: streamMessageId, text: streamedText },
      })}\n`,
    );
    const streamReply = await streamAck;
    assert.equal(streamReply.ok, true);
    await page
      .locator(`[data-message="${otherChatId}:${streamMessageId}"]`)
      .waitFor();
    const openedChatStreamMs = Date.now() - streamStartedAt;
    const transcriptEvent = await page.evaluate(
      ({ agentId, startedAt }) =>
        window.__resourceEvents.find(
          ({ at, event }) =>
            at >= startedAt &&
            event.resources.some(
              (resource) =>
                resource.kind === "transcript" && resource.agentId === agentId,
            ),
        ),
      { agentId: otherChatId, startedAt: streamStartedAt },
    );
    const transcriptEventToTextMs = transcriptEvent
      ? Date.now() - transcriptEvent.at
      : null;
    await waitForQuiet();
    const toOther = snapshot();
    const otherSettledAt = Date.now();
    await captureNewResourceEvents();
    const otherStreams = streamOpens.slice(loadStreams.length);

    counts.clear();
    const leadChat = page
      .locator("[data-chat]")
      .filter({ hasText: "Release lead" });
    const leadChatId = await leadChat.getAttribute("data-chat");
    const openLeadStartedAt = Date.now();
    await leadChat.click();
    await page.waitForFunction(
      (id) =>
        document
          .querySelector(".sidebar-row.selected [data-chat]")
          ?.getAttribute("data-chat") === id,
      leadChatId,
    );
    const openLeadFirstContentMs = Date.now() - openLeadStartedAt;
    await waitForQuiet();
    const back = snapshot();
    const backSettledAt = Date.now();
    await captureNewResourceEvents();
    const backStreams = streamOpens.slice(
      loadStreams.length + otherStreams.length,
    );
    const sendStart = Date.now();
    const streamStart = streamOpens.length;
    const requestStart = requestTimeline.length;
    await page.locator("#message").fill("Request count stream churn probe");
    await page.getByRole("button", { name: "Send message" }).click();
    await page.waitForResponse(
      (response) =>
        response.request().method() === "POST" &&
        new URL(response.url()).pathname === "/api/messages",
    );
    await page.waitForTimeout(900);
    const sendStreams = streamOpens.slice(streamStart);
    const sendRequests = requestTimeline
      .slice(requestStart)
      .filter((request) => request.at - sendStart <= 700);
    const priorStreamUrls = new Set(
      streamOpens.slice(0, streamStart).map(({ url }) => url),
    );
    const repeatedSendStreamUrls = sendStreams
      .map(({ url }) => url)
      .filter((url) => priorStreamUrls.has(url));
    console.log(
      `SEND_FLOW ${JSON.stringify({ loadStreams: streamSummary(loadStreams), loadStreamAccounting: Object.fromEntries(Object.entries(streamAccounting(loadStreams)).map(([kind, attempts]) => [kind, streamSummary(attempts)])), otherStreams: streamSummary(otherStreams), otherStreamAccounting: Object.fromEntries(Object.entries(streamAccounting(otherStreams)).map(([kind, attempts]) => [kind, streamSummary(attempts)])), backStreams: streamSummary(backStreams), backStreamAccounting: Object.fromEntries(Object.entries(streamAccounting(backStreams)).map(([kind, attempts]) => [kind, streamSummary(attempts)])), sendStreams: streamSummary(sendStreams), failedStreamAttemptCount: streamOpens.filter((attempt) => attempt.networkError || (attempt.status !== null && attempt.status >= 500)).length, failedStreamAttempts: streamOpens.filter((attempt) => attempt.networkError || (attempt.status !== null && attempt.status >= 500)).map(({ at, url, status, networkError, failure }) => ({ at, url, status, networkError, failure })), sendRequests })}`,
    );

    // Verify a real resource event updates the catalog rendered by the model picker.
    counts.clear();
    const settingsStartedAt = Date.now();
    await page.locator("#chat-settings-toggle").click();
    const subagentsMenu = page.getByRole("button", {
      name: "Subagent defaults",
      exact: true,
    });
    await subagentsMenu.click();
    const settingsDialog = page.getByRole("dialog", {
      name: "Subagent defaults",
    });
    await settingsDialog.waitFor({ state: "visible" });
    const modelPicker = settingsDialog.getByLabel("Default subagent model", {
      exact: true,
    });
    await modelPicker.waitFor();
    await modelPicker.click();
    await page.getByRole("option", { name: "Fixture model old" }).waitFor();
    await waitForQuiet();
    const settingsCounts = snapshot();
    const settingsSettledAt = Date.now();
    const freshnessId = `freshness-${Date.now()}`;
    const fixtureAck = fixtureResponses.has(freshnessId)
      ? Promise.resolve(fixtureResponses.get(freshnessId))
      : new Promise((resolve, reject) => {
          const timeout = setTimeout(
            () => reject(new Error(`fixture event timed out: ${log}`)),
            5000,
          );
          fixtureWaiters.set(freshnessId, { resolve, reject, timeout });
        });
    fixture.stdin.write(
      `${JSON.stringify({
        id: freshnessId,
        method: "fixture/model-catalog",
        params: {
          models: [
            { model: "gpt-5.6-luna", displayName: "Fixture worker model" },
            { model: "fixture-model-new", displayName: "Fixture model new" },
          ],
        },
      })}\n`,
    );
    await fixtureAck;
    await page.getByRole("option", { name: "Fixture model new" }).waitFor();
    await page.getByRole("option", { name: "Fixture model new" }).click();
    assert.match(await modelPicker.innerText(), /Fixture model new/);
    console.log(
      `STREAM_NAVIGATION_LATENCY ${JSON.stringify({ firstStreamOpenMs: loadStreams[0]?.at - loadStartedAt, reconfigureAfterOpeningChatMs: otherStreams[0]?.at - openOtherStartedAt, streamedMessageToVisibleMs: openedChatStreamMs, transcriptEventToTextMs, firstContentMs: { load: loadFirstContentMs, openOther: openOtherFirstContentMs, back: openLeadFirstContentMs } })}`,
    );
    console.log(
      `REPEATED_READ_COUNTS ${JSON.stringify({ load, toOther, back, settings: settingsCounts, firstContentMs: { load: loadFirstContentMs, openOther: openOtherFirstContentMs, backToLead: openLeadFirstContentMs }, readTimeline: requestTimeline, streamOpens: { load: streamSummary(loadStreams), toOther: streamSummary(otherStreams), back: streamSummary(backStreams) }, resourceEvents: resourceEvents.map(({ at, event: { reason, resources } }) => ({ at, reason, resourceCounts: resources.reduce((result, resource) => ((result[resource.kind] = (result[resource.kind] || 0) + 1), result), {}) })), sendFlow: { streamOpens: streamSummary(sendStreams), duplicateUrls: repeatedSendStreamUrls.length, readsWithin700ms: sendRequests } })}`,
    );
    if (!modelOnly)
      assert.deepEqual(
        repeatedSendStreamUrls,
        [],
        "sending a message must not reopen a sync stream URL that was already active on this chat",
      );
    // Stage one unlocks state/drafts, stage two adds typed resources (including terminals) and the foreground transcript,
    // and stage three adds prefetched transcripts once the chat list is available.
    if (!modelOnly) {
      const loadAccounting = streamAccounting(loadStreams);
      const otherAccounting = streamAccounting(otherStreams);
      const backAccounting = streamAccounting(backStreams);
      const loadConfigurations = loadAccounting.logical.map(({ url }) => {
        const resources = JSON.parse(
          new URL(url, "http://studio.test").searchParams.get("resources"),
        );
        return resources.map((resource) => resource.kind).sort();
      });
      const stateOnlyConfigurations = loadConfigurations.filter(
        (kinds) => kinds.length === 1 && kinds[0] === "state",
      );
      const mainClientConfigurations = loadConfigurations.filter(
        (kinds) => !(kinds.length === 1 && kinds[0] === "state"),
      );
      assert.ok(
        stateOnlyConfigurations.length === 1,
        `shell startup should use exactly one state-only stream and no second connector; configurations: ${JSON.stringify(loadConfigurations)}`,
      );
      assert.ok(
        mainClientConfigurations.length <= 3,
        `shell startup should use at most three main-client staged configurations; configurations: ${JSON.stringify(loadConfigurations)}`,
      );
      assert.equal(
        new Set(loadAccounting.logical.map(({ url }) => url)).size,
        loadAccounting.logical.length,
        "load must not reopen an identical healthy stream URL",
      );
      assert.ok(
        otherAccounting.logical.length <= 2,
        "opening another chat should use at most two distinct subscription stages",
      );
      assert.equal(
        new Set(otherAccounting.logical.map(({ url }) => url)).size,
        otherAccounting.logical.length,
        "chat navigation must not reopen an identical healthy stream URL",
      );
      assert.ok(
        backAccounting.logical.length <= 2,
        "returning to the chat should use at most two distinct subscription stages",
      );
      assert.equal(
        new Set(backAccounting.logical.map(({ url }) => url)).size,
        backAccounting.logical.length,
        "return navigation must not reopen an identical healthy stream URL",
      );
    }
    const routeKind = (key) =>
      key.startsWith("/api/models?")
        ? "models"
        : key.startsWith("/api/limits")
          ? "limits"
          : key.startsWith("/api/accounts")
            ? "accounts"
            : key.startsWith("/api/desktop")
              ? "desktop"
              : key.startsWith("/api/costs?")
                ? "costs"
                : undefined;
    const phaseBounds = {
      load: [loadStartedAt, loadSettledAt],
      toOther: [openOtherStartedAt, otherSettledAt],
      back: [openLeadStartedAt, backSettledAt],
      settings: [settingsStartedAt, settingsSettledAt],
    };
    const modelChecks = [];
    for (const [phase, countsInPhase] of Object.entries({
      load,
      toOther,
      back,
      settings: settingsCounts,
    }))
      for (const [key, count] of Object.entries(countsInPhase)) {
        const kind = routeKind(key);
        const [startedAt, settledAt] = phaseBounds[phase];
        const invalidations = kind
          ? resourceEvents.filter(
              ({ at, event }) =>
                at >= startedAt &&
                at <= settledAt &&
                event.reason === "change" &&
                event.resources.some((resource) => resource.kind === kind),
            ).length
          : 0;
        const check = { phase, key, count, invalidations };
        if (kind === "models") modelChecks.push(check);
        else if (!modelOnly)
          assert.ok(
            count <= 1 + invalidations,
            `${phase} repeated ${key} ${count} times with ${invalidations} matching resource changes`,
          );
      }
    if (!modelOnly)
      assert.deepEqual(
        await page
          .locator("[data-chat]")
          .filter({ hasText: "Release lead" })
          .count(),
        1,
      );
    return modelChecks;
  } finally {
    await context.close();
    fixture.kill("SIGTERM");
  }
}

test("repeated read counts across chat navigation @performance", async ({
  browser,
}) => {
  test.setTimeout(180_000);
  await repeatedReadCountsScenario(browser);
});

test("model catalog reads stay within invalidation counts @performance", async ({
  browser,
}) => {
  test.setTimeout(180_000);
  const modelChecks = await repeatedReadCountsScenario(browser, {
    modelOnly: true,
  });
  assert.ok(modelChecks.length > 0, "model catalog reads were measured");
  for (const check of modelChecks) {
    assert.ok(
      check.count <= 1 + check.invalidations,
      `model catalog reads exceeded 1 + matching invalidations: ${JSON.stringify(check)}`,
    );
  }
});

test("account catalog reads stay within invalidation counts @performance", async ({
  browser,
}) => {
  test.fixme(
    true,
    "intermittent duplicate /api/accounts read during initial load; see workspaces/client/apps/web/tests/KNOWN-FAILURES.md",
  );
  test.setTimeout(180_000);
  await repeatedReadCountsScenario(browser, { accountsOnly: true });
});
