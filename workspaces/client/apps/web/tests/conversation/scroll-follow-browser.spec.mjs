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
} from "../playwright.mjs";
// Production renderer, isolated fixture server, and a controlled transcript
// stream. The reader scrolls with wheel steps while the agent streams.
import assert from "node:assert/strict";
import { createServer } from "node:http";
import { createHash, randomUUID } from "node:crypto";
import { mkdtemp } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { streamTranscriptFixture } from "../fixtures/streaming-transcript.mjs";

// More rows than HISTORY_WINDOW_THRESHOLD, so the transcript is windowed.
const TURNS = 100;

let stopStreamFixture = async () => {};
test.afterEach(async () => stopStreamFixture());

function history() {
  const items = [];
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
  test.setTimeout(150_000);
  page.setDefaultTimeout(15000);
  const dir = await mkdtemp(join(tmpdir(), "studio-scroll-follow-"));
  const proc = spawn(process.execPath, [rustUiFixture, dir], {
    stdio: ["ignore", "pipe", "pipe"],
  });
  let log = "";
  proc.stderr.on("data", (d) => (log += d));
  const port = await new Promise((resolve, reject) => {
    proc.stdout.once("data", (d) => resolve(Number(String(d).trim())));
    proc.once("exit", () => reject(Error(log)));
  });
  const origin = `http://127.0.0.1:${port}`;
  const state = await readTestState(origin);
  const syncContract = await readFixtureSyncContract(origin);
  const chat = state.threads.find((a) => a.name === "Other project");
  const passes = [
    { width: 1440, height: 900 },
    { width: 390, height: 844 },
  ];
  const reset = () => ({
    items: history(),
    agent: {
      ...chat,
      status: "running",
      inFlight: true,
      turnId: "live-turn",
      activity: { phase: "writing" },
    },
  });
  const transcripts = new Map([[chat.id, reset()]]);
  const transcript = (id) => {
    const { agent, items } = transcripts.get(id);
    return { agent, items, order: items.map((i) => i.id), replace: true };
  };
  let transcriptSeq = 1;
  const errors = [];
  page.on("pageerror", (e) => errors.push(e.message));
  // The transcript map owns both pulled baselines and sparse stream deltas.
  const syncStreams = new Map();
  const sessions = new Map();
  let subscriptionUpdates = 0;
  let streamRevision = 0;
  let transcriptPulls = 0;
  const streamFrame = (resources, reason, batch) => ({
    protocol: 4,
    workspaceId: syncContract.identity.workspaceId,
    epoch: "scroll-follow-transcripts",
    revision: ++streamRevision,
    reason,
    resources,
    resourceVersions: resources.map((resource) => ({
      resource,
      revision: resource.kind === "transcript" ? transcriptSeq : 0,
      ...(batch && resource.kind === "transcript"
        ? { transcriptChanges: batch }
        : {}),
    })),
  });
  const streamServer = createServer(async (request, response) => {
    if (request.method === "POST") {
      let body = "";
      for await (const chunk of request) body += chunk;
      const update = JSON.parse(body);
      const stream = sessions.get(update.sessionId);
      if (!stream) {
        response.writeHead(410).end();
        return;
      }
      subscriptionUpdates++;
      const resources = syncStreams.get(stream);
      const removed = new Set(
        update.remove.map((resource) => JSON.stringify(resource)),
      );
      const next = resources.filter(
        (resource) => !removed.has(JSON.stringify(resource)),
      );
      for (const added of update.add) {
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
          protocol3SseEvent("resources", streamFrame(update.add, "initial")),
        );
      response.writeHead(200, {
        "Content-Type": "application/json",
        "Access-Control-Allow-Origin": "*",
      });
      response.end(JSON.stringify({ gen: update.gen }));
      return;
    }
    const resources = JSON.parse(
      new URL(request.url, "http://fixture").searchParams.get("resources") ||
        "[]",
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
  await new Promise((resolve) => streamServer.listen(0, "127.0.0.1", resolve));
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
      threads: state.threads.map((a) => transcripts.get(a.id)?.agent || a),
      runtime: { ...state.runtime, requests: [] },
    },
    syncContract,
  );
  await page.route("**/api/sync/stream?*", (route) => {
    const url = new URL(route.request().url());
    url.port = String(streamServer.address().port);
    return route.continue({ url: url.href });
  });
  await page.route("**/api/sync/subscriptions", (route) => {
    const url = new URL(route.request().url());
    url.port = String(streamServer.address().port);
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
    const id = new URL(r.request().url()).searchParams.get("id");
    return r.fulfill({
      json: transcripts.has(id) ? transcript(id) : { items: [] },
    });
  });
  const publishedItems = new Map();
  const notify = async (id) => {
    const after = transcriptSeq++;
    const live = transcripts.get(id);
    const previous = publishedItems.get(id) || new Map();
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
      itemRevisions: Object.fromEntries(
        changed.map((item) => [
          item.id,
          createHash("sha256").update(current.get(item.id)).digest("hex"),
        ]),
      ),
    };
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
            streamFrame([{ kind: "transcript", agentId: id }], "change", {
              after,
              through: transcriptSeq,
              delta,
            }),
          ),
        );
    }
  };
  const root = page.locator("#conversation #messages:visible");
  const latest = page.locator("#conversation #jump-latest:visible");
  // While a stream runs, content below the reader keeps growing. Then only
  // the scroll position must settle.
  const settle = (withHeight) =>
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
          .items.map((item) => [item.id, JSON.stringify(item)]),
      ),
    );
    // One growing reply plus agent activity changes, as a running turn sends.
    let streamed = 0;
    let streaming = true;
    const live = transcripts.get(chat.id);
    const renderSentence = (sentence) =>
      page.waitForFunction(
        (sentence) =>
          [
            ...document.querySelectorAll(
              '#messages [data-message="reply-live"]',
            ),
          ].some((reply) => reply.textContent?.includes(sentence)),
        sentence,
      );
    const stream = streamTranscriptFixture({
      running: () => streaming,
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
      rendered: renderSentence,
    });
    // Observe a producer error immediately; the finally block below awaits and
    // propagates it after stopping publication.
    void stream.catch(() => {});
    // Resolves after the renderer commits a stream update sent after this call.
    const nextRender = () =>
      renderSentence(`Streamed ${width} sentence ${streamed} `);
    // The first paragraph whose top is inside the transcript viewport, else
    // the first such message.
    const readingPoint = () =>
      root.evaluate((element) => {
        const bounds = element.getBoundingClientRect();
        const inside = (node) => {
          const box = node.getBoundingClientRect();
          return (
            box.height > 0 && box.top >= bounds.top && box.top < bounds.bottom
          );
        };
        const paragraph =
          [...element.querySelectorAll("[data-message] p")].find(inside) ||
          [...element.querySelectorAll("[data-message]")].find(inside) ||
          // One tall message can cover the whole phone viewport.
          [...element.querySelectorAll("[data-message]")].find((node) => {
            const box = node.getBoundingClientRect();
            return (
              box.height > 0 && box.top < bounds.top && box.bottom > bounds.top
            );
          });
        const message = paragraph?.closest("[data-message]");
        return {
          scrollTop: element.scrollTop,
          gap: element.scrollHeight - element.scrollTop - element.clientHeight,
          message: message?.dataset.message,
          index: message
            ? [...message.querySelectorAll("p")].indexOf(paragraph)
            : -1,
          top: paragraph
            ? paragraph.getBoundingClientRect().top - bounds.top
            : 0,
        };
      });
    const anchored = async (point, label) => {
      const now = await root.evaluate((element, target) => {
        const bounds = element.getBoundingClientRect();
        const message = [...element.querySelectorAll("[data-message]")].find(
          (node) => node.dataset.message === target.message,
        );
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
    const wheelStepInGap = async (deltaY, label, growAbove = 0) => {
      const step = await root.evaluate(
        (element, { deltaY, growAbove }) =>
          new Promise((resolve, reject) => {
            const offset = (node) =>
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
              const text = reply.textContent;
              const before = element.scrollTop;
              const hold = (event) => {
                if (event.target === element) event.stopImmediatePropagation();
              };
              window.addEventListener("scroll", hold, true);
              element.dispatchEvent(new WheelEvent("wheel", { deltaY }));
              element.scrollTop = before + deltaY;
              const inside = (node) =>
                node.getBoundingClientRect().height > 0 &&
                offset(node) >= 0 &&
                offset(node) < element.clientHeight;
              const paragraph =
                [...element.querySelectorAll("[data-message] p")].find(
                  inside,
                ) ||
                [...element.querySelectorAll("[data-message]")].find(inside) ||
                // One tall message can cover the whole phone viewport.
                [...element.querySelectorAll("[data-message]")].find(
                  (node) =>
                    offset(node) < 0 &&
                    node.getBoundingClientRect().bottom >
                      element.getBoundingClientRect().top,
                );
              const grown = growAbove
                ? [...element.querySelectorAll("[data-message]")].findLast(
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
              let during = null;
              const committed = new MutationObserver(() => {
                if (during || (reply.isConnected && reply.textContent === text))
                  return;
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
            };
            attempt();
          }),
        { deltaY, growAbove },
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

    try {
      // At the bottom, stream updates keep the latest text in view.
      await nextRender();
      await nextRender();
      await settle(false);
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
      await settle(false);
      await anchored(left, "the reader stays where the wheel step left it");
      await latest.waitFor();

      // A further wheel step while scrolled up is kept the same way.
      await wheelStepInGap(
        -300,
        "a stream render does not undo a scroll step or growth above",
        240,
      );
      const further = await readingPoint();
      await nextRender();
      await settle(false);
      await anchored(further, "the reader stays after a further step");

      // Trackpad-sized wheel steps, then more stream updates.
      const box = await root.boundingBox();
      await page.mouse.move(box.x + box.width / 2, box.y + box.height / 2);
      for (let step = 0; step < 20; step++) {
        await page.mouse.wheel(0, -30);
        await page.waitForTimeout(16);
      }
      await settle(false);
      const reading = await readingPoint();
      assert.ok(
        reading.gap > 900 && reading.message,
        `${width}px: wheel steps move the reader up (${JSON.stringify(reading)})`,
      );
      for (let update = 0; update < 4; update++) await nextRender();
      await settle(false);
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
        (element) =>
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
              const text = reply.textContent;
              const top = element.getBoundingClientRect().top;
              const above =
                [...element.querySelectorAll("[data-history-row]")].find(
                  (row) => {
                    const box = row.getBoundingClientRect();
                    return box.top < top && box.bottom > top;
                  },
                ) ||
                [...element.querySelectorAll("[data-message]")].findLast(
                  (node) => node.getBoundingClientRect().bottom < top,
                );
              above.style.paddingTop = "240px";
              const observer = new MutationObserver(() => {
                if (reply.isConnected && reply.textContent === text) return;
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
            };
            attempt();
          }),
      );
      await nextRender();
      await settle(false);
      await anchored(
        reading,
        "content growth above the reader keeps it anchored",
      );

      // The explicit Latest action returns to the bottom and follows again.
      await latest.click();
      await settle(false);
      assert.ok(
        (await geometry()).gap < 2,
        `${width}px: Latest returns to the bottom`,
      );
      await nextRender();
      await nextRender();
      await settle(false);
      assert.ok(
        (await geometry()).gap < 2,
        `${width}px: the stream follows after Latest (${JSON.stringify(await geometry())})`,
      );
      await latest.waitFor({ state: "detached" });
    } finally {
      streaming = false;
      await stream;
    }
    assert.equal(
      transcriptPulls,
      baselinePulls,
      `${width}px: streaming after baseline requires zero transcript pulls`,
    );
  }
  expect(errors).toEqual([]);
});
