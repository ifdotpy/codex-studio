// Count real renderer GETs against the isolated repository fixture backend.
import assert from "node:assert/strict";
import { mkdtemp } from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { test } from "../playwright.mjs";
import { spawnFixture as spawn } from "../playwright.mjs";

const measured = new Set([
  "/api/models",
  "/api/limits",
  "/api/accounts",
  "/api/desktop",
  "/api/costs",
  "/api/worktree-disk",
]);

test("repeated read counts across chat navigation @performance", async ({
  browser,
}) => {
  test.setTimeout(180_000);
  const repo = dirname(
    dirname(dirname(dirname(fileURLToPath(import.meta.url)))),
  );
  const state = await mkdtemp(join(tmpdir(), "repeated-read-counts-"));
  const fixture = spawn(
    "python3",
    ["-B", join(repo, "tests/simple-ui-fixture.py"), state],
    {
      stdio: ["pipe", "pipe", "pipe"],
      env: {
        ...process.env,
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
    const counts = new Map();
    const resourceEvents = [];
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
        streamOpens.push({
          at: Date.now(),
          url: `${url.pathname}${url.search}`,
        });
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
    const loadStartedAt = Date.now();
    await page.goto(`http://127.0.0.1:${port}`);
    await page.locator("#conversation-title").waitFor();
    const loadFirstContentMs = Date.now() - loadStartedAt;
    await waitForQuiet();
    const load = snapshot();
    const loadStreams = [...streamOpens];
    resourceEvents.push(
      ...(await page.evaluate(() => window.__resourceEvents)),
    );

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
    await page.locator(`[data-message="${streamMessageId}"]`).waitFor();
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
      `SEND_FLOW ${JSON.stringify({ loadStreams: streamSummary(loadStreams), otherStreams: streamSummary(otherStreams), backStreams: streamSummary(backStreams), sendStreams: streamSummary(sendStreams), sendRequests })}`,
    );

    // Verify a real resource event updates the catalog rendered by the model picker.
    await page.locator("#chat-settings-toggle").click();
    const subagentsMenu = page
      .locator(".execution-menu")
      .filter({ hasText: "Subagents" });
    await subagentsMenu.click();
    const modelPicker = page
      .locator(".execution-dropdown button:has(.model-picker-value)")
      .first();
    await modelPicker.waitFor();
    await modelPicker.click();
    await page.getByRole("option", { name: "Fixture model old" }).waitFor();
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
      `REPEATED_READ_COUNTS ${JSON.stringify({ load, toOther, back, firstContentMs: { load: loadFirstContentMs, openOther: openOtherFirstContentMs, backToLead: openLeadFirstContentMs }, readTimeline: requestTimeline, streamOpens: { load: streamSummary(loadStreams), toOther: streamSummary(otherStreams), back: streamSummary(backStreams) }, resourceEvents: resourceEvents.map(({ at, event: { reason, resources } }) => ({ at, reason, resourceCounts: resources.reduce((result, resource) => ((result[resource.kind] = (result[resource.kind] || 0) + 1), result), {}) })), sendFlow: { streamOpens: streamSummary(sendStreams), duplicateUrls: repeatedSendStreamUrls.length, readsWithin700ms: sendRequests } })}`,
    );
    assert.deepEqual(
      repeatedSendStreamUrls,
      [],
      "sending a message must not reopen a sync stream URL that was already active on this chat",
    );
    // Stage one unlocks state/drafts, stage two adds the typed resources and foreground transcript,
    // and stage three adds prefetched transcripts once the chat list is available.
    assert.ok(
      loadStreams.length <= 3,
      "startup should use at most three stream configurations",
    );
    assert.equal(
      new Set(loadStreams.map(({ url }) => url)).size,
      loadStreams.length,
      "load must not reopen an identical stream URL",
    );
    assert.ok(
      otherStreams.length <= 2,
      "opening another chat should use at most two distinct subscription stages",
    );
    assert.equal(
      new Set(otherStreams.map(({ url }) => url)).size,
      otherStreams.length,
      "chat navigation must not reopen an identical stream URL",
    );
    assert.ok(
      backStreams.length <= 2,
      "returning to the chat should use at most two distinct subscription stages",
    );
    assert.equal(
      new Set(backStreams.map(({ url }) => url)).size,
      backStreams.length,
      "return navigation must not reopen an identical stream URL",
    );
    for (const [phase, countsInPhase] of Object.entries({
      load,
      toOther,
      back,
    }))
      for (const [key, count] of Object.entries(countsInPhase))
        assert.ok(
          count <= (phase === "load" && key.startsWith("/api/costs?") ? 2 : 1),
          `${phase} repeated ${key} ${count} times`,
        );
    assert.deepEqual(
      await page
        .locator("[data-chat]")
        .filter({ hasText: "Release lead" })
        .count(),
      1,
    );
  } finally {
    await context.close();
    fixture.kill("SIGTERM");
  }
});
