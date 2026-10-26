import { waitForFixturePort } from "../playwright.ts";
import type { Page } from "@playwright/test";
import type { ServerResponse } from "node:http";
import type { components } from "../../src/generated/api.ts";
import type {
  ResourceRef,
  ResourceChangeEvent,
  TranscriptPageResponse,
} from "../playwright.ts";
import { fixturePostBody, fixtureResourceRefs } from "../chat/http.ts";

declare global {
  interface Window {
    publishScrollUpdate: () => Promise<unknown>;
  }
}

import { SYNC_TRANSCRIPT_INITIAL_TAIL_ITEMS } from "../../src/generated/sharedValues.ts";
import {
  test,
  expect,
  spawnFixture as spawn,
  rustUiFixture,
  installApiSchemaFixtureHeaders,
  entityPullFixtureForRequest,
  apiSchemaHandshakeSse,
  protocol3SseEvent,
  readTestState,
  readFixtureSyncContract,
  stubEntityState,
} from "../playwright.ts";
// Production renderer, isolated fixture server, and a controlled transcript
// stream. The reader scrolls with wheel steps while the agent streams.
import assert from "node:assert/strict";
import { createServer } from "node:http";
import { randomUUID } from "node:crypto";
import { mkdtemp } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { streamTranscriptFixture } from "../fixtures/streaming-transcript.ts";

// More rows than HISTORY_WINDOW_THRESHOLD, so the transcript is windowed.
const TURNS = 100;

let stopStreamFixture = async () => {};
test.afterEach(async () => stopStreamFixture());

function history() {
  const items: TranscriptPageResponse["items"] = [];
  for (let turn = 0; turn < TURNS; turn++) {
    const turnId = `turn-${turn}`;
    items.push(
      {
        id: `prompt-${turn}`,
        role: "user",
        text: `Question ${turn}: check the module.`,
        turnId,
        turnStatus: "completed",
      },
      {
        id: `reply-${turn}`,
        role: "assistant",
        text: `Answer ${turn}.\n\n${"A paragraph that the reader wants to read. ".repeat(6)}\n\n${"A second paragraph with more words. ".repeat(4)}`,
        turnId,
        turnStatus: "completed",
      },
    );
  }
  items.push({
    id: "prompt-live",
    role: "user",
    text: "Continue with the live task.",
    turnId: "live-turn",
  });
  return items;
}

test("a scrolled-up reader keeps its place while the agent streams", async ({
  page,
}) => {
  await scrollFollow(page, false);
});

test("scroll-follow desktop and phone wheel, trackpad and content-growth races", async ({
  page,
}) => {
  await scrollFollow(page, true);
});

async function scrollFollow(page: Page, full: boolean) {
  test.setTimeout(150_000);
  page.setDefaultTimeout(15000);
  const dir = await mkdtemp(join(tmpdir(), "studio-scroll-follow-"));
  const proc = spawn(process.execPath, [rustUiFixture, dir], {
    stdio: ["ignore", "pipe", "pipe"],
    env: { FIXTURE_WORKER_COUNT: "0" },
  });
  let log = "";
  assert.ok(proc.stderr && proc.stdout);
  proc.stderr.on("data", (d) => (log += d));
  const port = await waitForFixturePort(proc, { diagnostics: () => log });
  const origin = `http://127.0.0.1:${port}`;
  const state = await readTestState(origin);
  const syncContract = await readFixtureSyncContract(origin);
  const chat = state.threads.find((a) => a.name === "Other project");
  assert.ok(chat, "fixture chat exists");
  const passes = full
    ? [
        { width: 1440, height: 900 },
        { width: 390, height: 844 },
      ]
    : [{ width: 390, height: 844 }];
  const reset = () => ({
    items: history(),
    agent: {
      id: chat.id,
      status: "running",
      inFlight: true,
      activity: { phase: "writing" },
    },
  });
  const transcripts = new Map([[chat.id, reset()]]);
  const transcript = (
    id: string,
    limit: number = SYNC_TRANSCRIPT_INITIAL_TAIL_ITEMS,
  ): TranscriptPageResponse => {
    const value = transcripts.get(id);
    assert.ok(value, "fixture transcript exists");
    const { agent, items } = value;
    const selected = items.slice(-limit);
    return {
      agent,
      items: selected,
      truncated: selected.length < items.length,
      nextCursor:
        selected.length < items.length ? (selected[0]?.id ?? null) : null,
      nextAfterCursor: null,
      historyVersion: "scroll-follow",
    };
  };
  let transcriptSeq = 1;
  const errors: string[] = [];
  page.on("pageerror", (e) => errors.push(e.message));
  // The transcript map owns both pulled baselines and sparse stream deltas.
  const syncStreams = new Map<ServerResponse, ResourceRef[]>();
  const sessions = new Map<string, ServerResponse>();
  let subscriptionUpdates = 0;
  let streamRevision = 0;
  let transcriptPulls = 0;
  const streamFrame = (
    resources: ResourceRef[],
    reason: ResourceChangeEvent["reason"],
    batch?: components["schemas"]["TranscriptChangeBatch"],
  ): ResourceChangeEvent => ({
    protocol: 4,
    workspaceId: syncContract.identity.workspaceId,
    epoch: "scroll-follow-transcripts",
    revision: ++streamRevision,
    reason,
    resources,
    resourceVersions: resources.map((resource) => ({
      resource,
      revision: resource.kind === "transcript" ? transcriptSeq : 0,
      ...(resource.kind === "transcript" && transcripts.has(resource.agentId)
        ? {
            cursor: {
              workspaceId: syncContract.identity.workspaceId,
              seq: transcriptSeq,
            },
            replyKind: batch ? "delta" : "snapshot",
            ...(batch
              ? { transcriptChanges: batch }
              : {
                  transcriptSnapshot: transcript(
                    resource.agentId,
                    resource.tail,
                  ),
                }),
          }
        : {}),
    })),
  });
  const streamServer = createServer(async (request, response) => {
    if (request.method === "POST") {
      let body = "";
      for await (const chunk of request) body += chunk;
      const update = fixturePostBody(
        "/api/sync/subscriptions",
        JSON.parse(body),
      );
      const stream = sessions.get(update.sessionId);
      if (!stream) {
        response.writeHead(410).end();
        return;
      }
      subscriptionUpdates++;
      const resources = syncStreams.get(stream);
      assert.ok(resources, "subscription stream exists");
      const removed = new Set(
        update.remove.map((resource) => JSON.stringify(resource)),
      );
      const next = resources.filter(
        (resource) => !removed.has(JSON.stringify(resource)),
      );
      const addedResources = update.add.map(({ resource }) => resource);
      for (const added of addedResources) {
        if (
          !next.some(
            (resource) => JSON.stringify(resource) === JSON.stringify(added),
          )
        )
          next.push(added);
      }
      resources.splice(0, resources.length, ...next);
      stream.write(protocol3SseEvent("subscriptions", { gen: update.gen }));
      if (update.add.length)
        stream.write(
          protocol3SseEvent(
            "resources",
            streamFrame(addedResources, "initial"),
          ),
        );
      response.writeHead(200, {
        "Content-Type": "application/json",
        "Access-Control-Allow-Origin": "*",
      });
      response.end(JSON.stringify({ gen: update.gen }));
      return;
    }
    const resources = fixtureResourceRefs(
      new URL(request.url ?? "/", "http://fixture").searchParams.get(
        "resources",
      ) || "[]",
    );
    response.writeHead(200, {
      "Content-Type": "text/event-stream",
      "Cache-Control": "no-cache",
      "Access-Control-Allow-Origin": "*",
    });
    const sessionId = randomUUID();
    sessions.set(sessionId, response);
    response.write(protocol3SseEvent("session", { sessionId }));
    response.write(apiSchemaHandshakeSse());
    response.write(
      protocol3SseEvent("resources", streamFrame(resources, "initial")),
    );
    response.write(
      protocol3SseEvent("token-rates", {
        protocol: 4,
        workspaceId: syncContract.identity.workspaceId,
        epoch: "scroll-follow-transcripts",
        revision: streamRevision,
        snapshot: true,
        removed: [],
        records: [],
      }),
    );
    syncStreams.set(response, resources);
    response.on("close", () => {
      syncStreams.delete(response);
      sessions.delete(sessionId);
    });
  });
  await new Promise<void>((resolve) =>
    streamServer.listen(0, "127.0.0.1", resolve),
  );
  const streamAddress = streamServer.address();
  assert.ok(
    streamAddress && typeof streamAddress !== "string",
    "stream TCP address exists",
  );
  stopStreamFixture = async () => {
    for (const response of syncStreams.keys()) response.end();
    await new Promise((resolve) => streamServer.close(resolve));
  };
  installApiSchemaFixtureHeaders(page);
  // The race witness below holds the scroll event until the next observed
  // stream commit, so its ordering does not depend on an animation-frame gap.
  await stubEntityState(
    page,
    {
      ...state,
      runtime: { ...state.runtime, requests: [] },
    },
    syncContract,
  );
  await page.route("**/api/sync/stream?*", (route) => {
    const url = new URL(route.request().url());
    url.port = String(streamAddress.port);
    return route.continue({ url: url.href });
  });
  await page.route("**/api/sync/subscriptions", (route) => {
    const url = new URL(route.request().url());
    url.port = String(streamAddress.port);
    return route.continue({ url: url.href });
  });
  await page.route("**/api/sync/pull?*", (route) => {
    const requestUrl = route.request().url();
    const scope = new URL(requestUrl).searchParams.get("scope") || "";
    if (scope.startsWith("transcript:")) transcriptPulls++;
    const id = scope.startsWith("transcript:")
      ? scope.slice("transcript:".length)
      : "";
    return route.fulfill({
      json: {
        workspaceId: syncContract.identity.workspaceId,
        ...entityPullFixtureForRequest(state, requestUrl, {
          transcriptSeq,
          ...(transcripts.has(id) ? { transcript: transcript(id) } : {}),
        }),
      },
    });
  });
  await page.route("**/api/transcript?*", (r) => {
    const id = new URL(r.request().url()).searchParams.get("id") ?? "";
    return r.fulfill({
      json: transcripts.has(id) ? transcript(id) : { items: [] },
    });
  });
  await page.route("**/api/transcript/page?*", (route) => {
    const query = new URL(route.request().url()).searchParams;
    const live = transcripts.get(query.get("id") ?? "");
    if (!live) return route.continue();
    const end = live.items.findIndex((item) => item.id === query.get("before"));
    const start = Math.max(0, end - Number(query.get("limit")));
    return route.fulfill({
      json: {
        items: live.items.slice(start, end),
        truncated: start > 0,
        nextCursor: start > 0 ? (live.items[start]?.id ?? null) : null,
        nextAfterCursor: live.items[end - 1]?.id || null,
        historyVersion: "scroll-follow",
      },
    });
  });
  const publishedItems = new Map<string, Map<string, string>>();
  const notify = async (id: string) => {
    const after = transcriptSeq++;
    const live = transcripts.get(id);
    assert.ok(live, "fixture transcript exists");
    const previous = publishedItems.get(id) || new Map<string, string>();
    const current = new Map(
      live.items.map((item) => [item.id, JSON.stringify(item)]),
    );
    const changed = live.items.filter(
      (item) => previous.get(item.id) !== current.get(item.id),
    );
    const delta = {
      delta: true,
      items: changed,
      removed: [...previous.keys()].filter((key) => !current.has(key)),
      order: live.items.map((item) => item.id),
      truncated: false,
      agent: live.agent,
    } satisfies components["schemas"]["TranscriptStreamDelta"];
    publishedItems.set(id, current);
    for (const [response, resources] of syncStreams) {
      if (
        resources.some(
          (resource) =>
            resource.kind === "transcript" && resource.agentId === id,
        )
      )
        response.write(
          protocol3SseEvent(
            "resources",
            streamFrame(
              resources.filter(
                (resource) =>
                  resource.kind === "transcript" && resource.agentId === id,
              ),
              "change",
              {
                after,
                through: transcriptSeq,
                delta,
              },
            ),
          ),
        );
    }
  };
  const root = page.locator("#conversation #messages:visible");
  const latest = page.locator("#conversation #jump-latest:visible");
  // While a stream runs, content below the reader keeps growing. Then only
  // the scroll position must settle.
  const settle = (withHeight: boolean) =>
    root.evaluate(
      (element, withHeight) =>
        new Promise((resolve) => {
          let previous = "";
          let stable = 0;
          const sample = () => {
            const value = withHeight
              ? `${element.scrollTop}:${element.scrollHeight}`
              : `${element.scrollTop}`;
            stable = value === previous ? stable + 1 : 0;
            previous = value;
            if (stable >= 3) resolve(undefined);
            else requestAnimationFrame(sample);
          };
          requestAnimationFrame(sample);
        }),
      withHeight,
    );
  const geometry = () =>
    root.evaluate((e) => ({
      top: e.scrollTop,
      gap: e.scrollHeight - e.scrollTop - e.clientHeight,
    }));

  let publishUpdate: () => Promise<unknown> = async () => {
    throw new Error("Stream publisher not initialized");
  };
  await page.exposeFunction("publishScrollUpdate", () => publishUpdate());

  for (const viewport of passes) {
    const width = viewport.width;
    if (viewport !== passes[0]) {
      // Each pass opens the chat fresh: no saved scroll position or anchor.
      await page.evaluate(() =>
        Object.keys(localStorage)
          .filter((key) => /^studio-(chat-scroll|turns):/.test(key))
          .forEach((key) => localStorage.removeItem(key)),
      );
      transcripts.set(chat.id, reset());
    }
    await page.setViewportSize(viewport);
    await page.goto(origin);
    const chatRow = page.locator(`[data-chat="${chat.id}"]`);
    await expect(chatRow).toBeVisible();
    await chatRow.click();
    await root.locator('[data-message="prompt-live"]').waitFor();
    while (await page.locator("#earlier-messages").count()) {
      const response = page.waitForResponse(
        (response) =>
          new URL(response.url()).pathname === "/api/transcript/page",
      );
      await page.locator("#earlier-messages").click();
      await response;
      await expect
        .poll(async () => {
          const earlier = page.locator("#earlier-messages");
          return (
            !(await earlier.count()) ||
            (await earlier.getAttribute("data-loading")) !== "true"
          );
        })
        .toBe(true);
    }
    await latest.click();
    assert.ok(
      await root.evaluate((e) => !!e.querySelector("[data-history-window]")),
      `${width}px: the long transcript uses the windowed history`,
    );
    await expect
      .poll(
        () =>
          [...syncStreams.values()].some((resources) =>
            resources.some(
              (resource) =>
                resource.kind === "transcript" && resource.agentId === chat.id,
            ),
          ),
        { timeout: 15000, message: "The chat has an open transcript stream" },
      )
      .toBe(true);
    assert.ok(
      subscriptionUpdates > 0,
      "The v4 stream acknowledges live transcript subscriptions",
    );
    // Loading can take longer than a few stable frames on a busy machine.
    await root.evaluate(
      (element) =>
        new Promise((resolve, reject) => {
          const deadline = performance.now() + 15000;
          const sample = () => {
            if (
              element.scrollHeight - element.scrollTop - element.clientHeight <
              2
            )
              resolve(undefined);
            else if (performance.now() > deadline)
              reject(
                new Error("The opened chat did not reach the latest message."),
              );
            else requestAnimationFrame(sample);
          };
          sample();
        }),
    );
    await settle(true);
    assert.ok(
      (await geometry()).gap < 2,
      `${width}px: an opened chat starts at the latest message`,
    );

    const baselinePulls = transcriptPulls;
    publishedItems.set(
      chat.id,
      new Map(
        transcripts
          .get(chat.id)
          ?.items.map((item) => [item.id, JSON.stringify(item)]),
      ),
    );
    // One growing reply plus agent activity changes, as a running turn sends.
    let streamed = 0;
    const live = transcripts.get(chat.id);
    assert.ok(live, "live transcript exists");
    const renderSentence = (sentence: string) =>
      page.waitForFunction(
        (sentence) =>
          [
            ...document.querySelectorAll(
              '#messages [data-message="reply-live"]',
            ),
          ].some((reply) => reply.textContent?.includes(sentence)),
        sentence,
      );
    const nextRender = streamTranscriptFixture({
      publish: async () => {
        streamed++;
        live.items = [
          ...live.items.filter((item) => item.id !== "reply-live"),
          {
            id: "reply-live",
            role: "assistant",
            text: Array.from(
              { length: streamed },
              (_, n) => `Streamed ${width} sentence ${n} with extra words.`,
            ).join(" "),
            streaming: true,
            turnId: "live-turn",
          },
        ];
        live.agent = {
          ...live.agent,
          activity: { phase: streamed % 2 ? "writing" : "thinking" },
        };
        await notify(chat.id);
        return `Streamed ${width} sentence ${streamed - 1} `;
      },
      rendered: async (sentence) => {
        await renderSentence(sentence);
      },
    });
    publishUpdate = nextRender;
    // The first paragraph whose top is inside the transcript viewport, else
    // the first such message.
    const readingPoint = () =>
      root.evaluate((element) => {
        const bounds = element.getBoundingClientRect();
        const inside = (node: Element) => {
          const box = node.getBoundingClientRect();
          return (
            box.height > 0 && box.top >= bounds.top && box.top < bounds.bottom
          );
        };
        const paragraph =
          [...element.querySelectorAll("[data-message] p")].find(inside) ||
          [...element.querySelectorAll<HTMLElement>("[data-message]")].find(
            inside,
          ) ||
          // One tall message can cover the whole phone viewport.
          [...element.querySelectorAll<HTMLElement>("[data-message]")].find(
            (node) => {
              const box = node.getBoundingClientRect();
              return (
                box.height > 0 &&
                box.top < bounds.top &&
                box.bottom > bounds.top
              );
            },
          );
        const message = paragraph?.closest<HTMLElement>("[data-message]");
        return {
          scrollTop: element.scrollTop,
          gap: element.scrollHeight - element.scrollTop - element.clientHeight,
          message: message?.dataset.message,
          index: message
            ? [...message.querySelectorAll("p")].findIndex(
                (node) => node === paragraph,
              )
            : -1,
          top: paragraph
            ? paragraph.getBoundingClientRect().top - bounds.top
            : 0,
        };
      });
    const anchored = async (
      point: Awaited<ReturnType<typeof readingPoint>>,
      label: string,
    ) => {
      const now = await root.evaluate((element, target) => {
        const bounds = element.getBoundingClientRect();
        const message = [
          ...element.querySelectorAll<HTMLElement>("[data-message]"),
        ].find((node) => node.dataset.message === target.message);
        const paragraph =
          target.index < 0
            ? message
            : message?.querySelectorAll("p")[target.index];
        return {
          scrollTop: element.scrollTop,
          top: paragraph
            ? paragraph.getBoundingClientRect().top - bounds.top
            : null,
        };
      }, point);
      assert.ok(
        point.message && now.top !== null && Math.abs(now.top - point.top) < 2,
        `${width}px: ${label} (${JSON.stringify({ point, now })})`,
      );
      return now;
    };
    // Hold one wheel step's scroll event until a stream commit is observed.
    // This deterministically exercises scrollTop changing before its event,
    // including content growth above the reader, without retrying frame races.
    const wheelStepInGap = async (
      deltaY: number,
      label: string,
      growAbove = 0,
    ) => {
      await settle(true);
      const sentence = `Streamed ${width} sentence ${streamed} `;
      const step = await root.evaluate(
        (element, { deltaY, growAbove, sentence }) =>
          new Promise<{
            attempts: number;
            before: number;
            moved: { top: number; paragraph: number | null };
            during: { top: number; gap: number; paragraph: number | null };
            grown: boolean;
          }>((resolve, reject) => {
            const offset = (node: Element) =>
              node.getBoundingClientRect().top -
              element.getBoundingClientRect().top;
            const scrolled = () => element.dispatchEvent(new Event("scroll"));
            let attempts = 0;
            const attempt = () => {
              attempts++;
              const reply = element.querySelector(
                '[data-message="reply-live"]',
              );
              if (!reply) {
                reject(new Error("The streamed reply is not mounted."));
                return;
              }
              const before = element.scrollTop;
              const hold = (event: Event) => {
                if (event.target === element) event.stopImmediatePropagation();
              };
              window.addEventListener("scroll", hold, true);
              element.dispatchEvent(new WheelEvent("wheel", { deltaY }));
              element.scrollTop = before + deltaY;
              const inside = (node: Element) =>
                node.getBoundingClientRect().height > 0 &&
                offset(node) >= 0 &&
                offset(node) < element.clientHeight;
              const paragraph =
                [...element.querySelectorAll("[data-message] p")].find(
                  inside,
                ) ||
                [
                  ...element.querySelectorAll<HTMLElement>("[data-message]"),
                ].find(inside) ||
                // One tall message can cover the whole phone viewport.
                [
                  ...element.querySelectorAll<HTMLElement>("[data-message]"),
                ].find(
                  (node) =>
                    offset(node) < 0 &&
                    node.getBoundingClientRect().bottom >
                      element.getBoundingClientRect().top,
                );
              const grown = growAbove
                ? [
                    ...element.querySelectorAll<HTMLElement>("[data-message]"),
                  ].findLast(
                    (node) =>
                      node.getBoundingClientRect().bottom <
                      element.getBoundingClientRect().top,
                  )
                : null;
              const moved = {
                top: element.scrollTop,
                paragraph: paragraph ? offset(paragraph) : null,
              };
              if (grown) grown.style.paddingBottom = `${growAbove}px`;
              let during: {
                top: number;
                gap: number;
                paragraph: number | null;
              } | null = null;
              const committed = new MutationObserver(() => {
                const current = element.querySelector(
                  '[data-message="reply-live"]',
                );
                if (during || !current?.textContent?.includes(sentence)) return;
                during = {
                  top: element.scrollTop,
                  gap:
                    element.scrollHeight -
                    element.scrollTop -
                    element.clientHeight,
                  paragraph: paragraph?.isConnected ? offset(paragraph) : null,
                };
                clearTimeout(deadline);
                committed.disconnect();
                window.removeEventListener("scroll", hold, true);
                scrolled();
                resolve({ attempts, before, moved, during, grown: !!grown });
              });
              const deadline = setTimeout(() => {
                committed.disconnect();
                window.removeEventListener("scroll", hold, true);
                scrolled();
                reject(
                  new Error(
                    "The stream did not commit while the scroll event was held.",
                  ),
                );
              }, 15000);
              committed.observe(element, {
                childList: true,
                subtree: true,
                characterData: true,
              });
              void window.publishScrollUpdate().catch(reject);
            };
            attempt();
          }),
        { deltaY, growAbove, sentence },
      );
      assert.ok(
        step.before - step.moved.top > Math.abs(deltaY) * 0.75 &&
          step.moved.paragraph !== null &&
          (!growAbove || step.grown),
        `${width}px: the wheel step moves the reader up (${JSON.stringify(step)})`,
      );
      assert.ok(
        step.during.gap > Math.abs(deltaY) * 0.75 &&
          step.during.paragraph !== null &&
          Math.abs(step.during.paragraph - step.moved.paragraph) < 2,
        `${width}px: ${label} (${JSON.stringify(step)})`,
      );
    };

    {
      // At the bottom, stream updates keep the latest text in view.
      await nextRender();
      await nextRender();
      await settle(true);
      assert.ok(
        (await geometry()).gap < 2,
        `${width}px: the reader at the bottom follows the stream (${JSON.stringify(await geometry())})`,
      );

      // A wheel step away from the bottom is kept by a render in the gap.
      await wheelStepInGap(
        -400,
        "a stream render does not return the reader to the latest message",
      );
      const left = await readingPoint();
      await nextRender();
      await settle(true);
      await anchored(left, "the reader stays where the wheel step left it");
      await latest.waitFor();

      if (full) {
        // A further wheel step while scrolled up is kept the same way.
        await wheelStepInGap(
          -300,
          "a stream render does not undo a scroll step or growth above",
          240,
        );
        const further = await readingPoint();
        await nextRender();
        await settle(true);
        await anchored(further, "the reader stays after a further step");

        // Trackpad-sized wheel steps, then more stream updates.
        const box = await root.boundingBox();
        assert.ok(box, "transcript bounds exist");
        await page.mouse.move(box.x + box.width / 2, box.y + box.height / 2);
        for (let step = 0; step < 20; step++) {
          await page.mouse.wheel(0, -30);
          await page.evaluate(() => new Promise(requestAnimationFrame));
        }
        await settle(true);
        const reading = await readingPoint();
        assert.ok(
          reading.gap > 900 && reading.message,
          `${width}px: wheel steps move the reader up (${JSON.stringify(reading)})`,
        );
        for (let update = 0; update < 4; update++) await nextRender();
        await settle(true);
        // Rows that the window mounts above the reader can change scrollTop
        // by their measured height. The visible text must not move.
        await anchored(
          reading,
          "the reading paragraph stays in place during the stream",
        );

        // Content above the reader grows, as a late image or an expanded row.
        // It grows at the start of the window row that crosses the viewport
        // top, which is also the window's own anchor, and a stream commit
        // is observed before this witness releases the growth operation.
        await root.evaluate(
          (element, sentence) =>
            new Promise((resolve, reject) => {
              let attempts = 0;
              const attempt = () => {
                attempts++;
                const reply = element.querySelector(
                  '[data-message="reply-live"]',
                );
                if (!reply) {
                  reject(new Error("The streamed reply is not mounted."));
                  return;
                }
                const top = element.getBoundingClientRect().top;
                const above =
                  [
                    ...element.querySelectorAll<HTMLElement>(
                      "[data-history-row]",
                    ),
                  ].find((row) => {
                    const box = row.getBoundingClientRect();
                    return box.top < top && box.bottom > top;
                  }) ||
                  [
                    ...element.querySelectorAll<HTMLElement>("[data-message]"),
                  ].findLast(
                    (node) => node.getBoundingClientRect().bottom < top,
                  );
                if (!above) throw new Error("No row above reader");
                above.style.paddingTop = "240px";
                const observer = new MutationObserver(() => {
                  const current = element.querySelector(
                    '[data-message="reply-live"]',
                  );
                  if (!current?.textContent.includes(sentence)) return;
                  clearTimeout(deadline);
                  observer.disconnect();
                  resolve(attempts);
                });
                const deadline = setTimeout(() => {
                  observer.disconnect();
                  reject(
                    new Error(
                      "The stream did not commit after growth above the reader.",
                    ),
                  );
                }, 15000);
                observer.observe(element, {
                  childList: true,
                  subtree: true,
                  characterData: true,
                });
                void window.publishScrollUpdate().catch(reject);
              };
              attempt();
            }),
          `Streamed ${width} sentence ${streamed} `,
        );
        await nextRender();
        await settle(true);
        await anchored(
          reading,
          "content growth above the reader keeps it anchored",
        );
      }

      // The explicit Latest action returns to the bottom and follows again.
      await latest.click();
      await settle(true);
      assert.ok(
        (await geometry()).gap < 2,
        `${width}px: Latest returns to the bottom`,
      );
      await nextRender();
      await nextRender();
      await settle(true);
      assert.ok(
        (await geometry()).gap < 2,
        `${width}px: the stream follows after Latest (${JSON.stringify(await geometry())})`,
      );
      await latest.waitFor({ state: "detached" });
    }
    assert.equal(
      transcriptPulls,
      baselinePulls,
      `${width}px: streaming after baseline requires zero transcript pulls`,
    );
  }
  expect(errors).toEqual([]);
}
