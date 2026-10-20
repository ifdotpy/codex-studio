import {
  test,
  expect,
  spawnFixture as spawn,
  installApiSchemaFixtureHeaders,
  entityPullFixtureForRequest,
  apiSchemaHeaders,
  readTestState,
  readFixtureSyncContract,
  stubEntityState,
} from "../playwright.mjs";
// Production renderer, isolated fixture server, and a controlled transcript
// stream. The reader scrolls with wheel steps while the agent streams.
import assert from "node:assert/strict";
import { mkdtemp } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";

// More rows than HISTORY_WINDOW_THRESHOLD, so the transcript is windowed.
const TURNS = 100;

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
  const repo = fileURLToPath(new URL("../../../../../../", import.meta.url));
  const dir = await mkdtemp(join(tmpdir(), "studio-scroll-follow-"));
  const proc = spawn(
    "python3",
    [
      "-B",
      join(repo, "workspaces/runtime/apps/server/tests/simple-ui-fixture.py"),
      dir,
    ],
    { stdio: ["ignore", "pipe", "pipe"] },
  );
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
  installApiSchemaFixtureHeaders(page);
  // WebKit, the macOS client engine, applies a wheel step to scrollTop
  // before it dispatches the scroll event, and it dispatches that event
  // before the next animation frame callbacks. This hook lets a step
  // release its held scroll event at that point.
  await page.addInitScript(() => {
    const native = window.requestAnimationFrame.bind(window);
    let beforeFrame = null;
    let armedAt = 0;
    let frameTime = 0;
    const run = (time) => {
      frameTime = time;
      if (!beforeFrame || time <= armedAt) return;
      const task = beforeFrame;
      beforeFrame = null;
      task();
    };
    window.beforeNextFrame = (task) => {
      beforeFrame = task;
      armedAt = Math.max(frameTime, 0);
      native(run);
    };
    window.requestAnimationFrame = (callback) =>
      native((time) => {
        run(time);
        callback(time);
      });
  });
  await stubEntityState(
    page,
    {
      ...state,
      threads: state.threads.map((a) => transcripts.get(a.id)?.agent || a),
      runtime: { ...state.runtime, requests: [] },
    },
    syncContract,
  );
  await page.route("**/api/sync/pull?*", (route) => {
    const requestUrl = route.request().url();
    const scope = new URL(requestUrl).searchParams.get("scope") || "";
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
  const notify = async (id) => {
    transcriptSeq++;
    const response = await fetch(new URL("/api/sync/notify", origin), {
      method: "POST",
      headers: {
        Origin: origin,
        "Content-Type": "application/json",
        "X-Canvas-Token": state.token,
        ...apiSchemaHeaders(),
      },
      body: JSON.stringify({
        requestId: crypto.randomUUID(),
        resources: [{ kind: "transcript", agentId: id }],
      }),
    });
    assert.equal(response.status, 200, await response.text());
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
    if (width < 700) await page.locator("#sidebar-toggle").click();
    await page.locator(`[data-chat="${chat.id}"]`).click();
    await root.locator('[data-message="prompt-live"]').waitFor();
    assert.ok(
      await root.evaluate((e) => !!e.querySelector("[data-history-window]")),
      `${width}px: the long transcript uses the windowed history`,
    );
    await settle(true);
    assert.ok(
      (await geometry()).gap < 2,
      `${width}px: an opened chat starts at the latest message`,
    );

    // One growing reply plus agent activity changes, as a running turn sends.
    let streamed = 0;
    let streaming = true;
    const live = transcripts.get(chat.id);
    const stream = (async () => {
      while (streaming) {
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
        await new Promise((resolve) => setTimeout(resolve, 50));
      }
    })();
    // Resolves after the renderer commits a stream update sent after this call.
    const nextRender = () =>
      page.waitForFunction(
        (sentence) =>
          [
            ...document.querySelectorAll(
              '#messages [data-message="reply-live"]',
            ),
          ].some((reply) => reply.textContent?.includes(sentence)),
        `Streamed ${width} sentence ${streamed} `,
      );
    // The first paragraph whose top is inside the transcript viewport.
    const readingPoint = () =>
      root.evaluate((element) => {
        const bounds = element.getBoundingClientRect();
        const paragraph = [
          ...element.querySelectorAll("[data-message] p"),
        ].find((p) => {
          const box = p.getBoundingClientRect();
          return (
            box.height > 0 && box.top >= bounds.top && box.top < bounds.bottom
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
        const paragraph = message?.querySelectorAll("p")[target.index];
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
    // One wheel step whose scroll event waits until the next frame, as in
    // WebKit. An attempt counts when a stream commit lands in that gap;
    // otherwise the step is undone and tried again. Rows that the window
    // mounts above the reader may correct scrollTop by their measured
    // height, so the check is the visible paragraph and the distance from
    // the latest message.
    const wheelStepInGap = async (deltaY, label) => {
      const step = await root.evaluate(
        (element, deltaY) =>
          new Promise((resolve, reject) => {
            const offset = (node) =>
              node.getBoundingClientRect().top -
              element.getBoundingClientRect().top;
            const scrolled = () => element.dispatchEvent(new Event("scroll"));
            let attempts = 0;
            const attempt = () => {
              if (++attempts > 300) {
                reject(new Error("No stream commit landed in a frame gap."));
                return;
              }
              const reply = element.querySelector(
                '[data-message="reply-live"]',
              );
              if (!reply) {
                reject(new Error("The streamed reply is not mounted."));
                return;
              }
              const text = reply.textContent;
              const before = element.scrollTop;
              const atBottom =
                element.scrollHeight - before - element.clientHeight < 2;
              const hold = (event) => {
                if (event.target === element) event.stopImmediatePropagation();
              };
              window.addEventListener("scroll", hold, true);
              element.dispatchEvent(new WheelEvent("wheel", { deltaY }));
              element.scrollTop = before + deltaY;
              const paragraph = [
                ...element.querySelectorAll("[data-message] p"),
              ].find(
                (p) => p.getBoundingClientRect().height > 0 && offset(p) >= 0,
              );
              const moved = {
                top: element.scrollTop,
                paragraph: paragraph ? offset(paragraph) : null,
              };
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
              });
              committed.observe(element, {
                childList: true,
                subtree: true,
                characterData: true,
              });
              window.beforeNextFrame(() => {
                committed.disconnect();
                window.removeEventListener("scroll", hold, true);
                scrolled();
                if (during) {
                  resolve({ attempts, before, moved, during });
                  return;
                }
                // No commit in this gap: return to the start and retry.
                element.scrollTop = atBottom ? element.scrollHeight : before;
                scrolled();
                requestAnimationFrame(() => requestAnimationFrame(attempt));
              });
            };
            attempt();
          }),
        deltaY,
      );
      assert.ok(
        step.before - step.moved.top > Math.abs(deltaY) * 0.75 &&
          step.moved.paragraph !== null,
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
      await wheelStepInGap(-300, "a stream render does not undo a scroll step");
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
      await root.evaluate((element, target) => {
        const bounds = element.getBoundingClientRect();
        const above = [...element.querySelectorAll("[data-message]")]
          .filter((node) => node.dataset.message !== target.message)
          .findLast((node) => node.getBoundingClientRect().bottom < bounds.top);
        above.style.paddingTop = "240px";
      }, reading);
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
  }
  expect(errors).toEqual([]);
});
