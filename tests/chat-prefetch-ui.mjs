#!/usr/bin/env node
// Production App and real RxDB. Only the isolated fixture receives requests.
import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { mkdtemp } from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { createRequire } from "node:module";

const repo = dirname(dirname(fileURLToPath(import.meta.url)));
const { chromium, webkit } = createRequire(join(repo, "web/package.json"))(
  "playwright-core",
);
const browserType = process.env.BROWSER === "webkit" ? webkit : chromium;
const legacySync = process.env.LEGACY_SYNC === "1";
const dir = await mkdtemp(join(tmpdir(), "studio-chat-prefetch-"));
const fixture = spawn(
  "python3",
  ["-B", join(repo, "tests/simple-ui-fixture.py"), dir],
  {
    stdio: ["ignore", "pipe", "pipe"],
    env: { ...process.env, CODEX_BOARD_STATE_DIR: join(dir, "board") },
  },
);
let log = "",
  browser,
  page;
fixture.stderr.on("data", (chunk) => {
  log += chunk;
});
const until = async (predicate, label, timeout = 12000) => {
  const deadline = Date.now() + timeout;
  while (Date.now() < deadline) {
    if (await predicate()) return;
    await new Promise((resolve) => setTimeout(resolve, 25));
  }
  throw new Error(label);
};
try {
  const port = await new Promise((resolve, reject) => {
    fixture.stdout.once("data", (chunk) =>
      resolve(Number(String(chunk).trim())),
    );
    fixture.once("exit", () => reject(new Error(log)));
  });
  const origin = `http://127.0.0.1:${port}`;
  const original = await (await fetch(origin + "/api/state")).json();
  const identity = await (await fetch(origin + "/api/sync/identity")).json();
  let workspaceId = identity.workspaceId;
  const a = original.threads.find((agent) => agent.name === "Other project");
  const b = original.threads.find((agent) => agent.name === "Release lead");
  const agents = [a, b].map((agent) => ({
    ...agent,
    status: "completed",
    inFlight: false,
    turnId: null,
  }));
  const state = {
    ...original,
    threads: agents,
    nodes: agents,
    chats: [],
    edges: [],
    runtime: {
      ...original.runtime,
      agents,
      rooms: [],
      requests: [],
      complaints: [],
      monitors: [],
      tasks: [],
      userTasks: [],
      rules: [],
      events: [],
      work: [],
    },
  };
  let stateSeq = 100;
  const payload = (agent, tag) => ({
    agent: { ...agent, status: "completed", inFlight: false, turnId: null },
    items: Array.from({ length: 40 }, (_, index) => ({
      id: `${tag}-${index}`,
      role: "assistant",
      at: index,
      text: `${tag} exact message ${index}. ${"Saved conversation text. ".repeat(18)}`,
    })),
    historyVersion: "fixture-history",
    nextCursor: agent.id === a.id ? "older-a" : null,
    replace: true,
  });
  const values = new Map([
    [a.id, { seq: 200, data: payload(a, "A-current") }],
    [b.id, { seq: 300, data: payload(b, "B-first") }],
  ]);
  const reads = [],
    streams = [],
    held = [];
  let holdNetwork = false,
    holdWorkspaceB = false,
    staleB = false;
  let staleReplies = 0,
    delayedA = false,
    rawA,
    releaseA;
  browser = await browserType.launch({
    headless: true,
    executablePath:
      browserType === chromium
        ? process.env.CHROME_BIN ||
          "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome"
        : undefined,
  });
  const context = await browser.newContext({
    viewport: { width: 1440, height: 960 },
    // This test controls HTTP replies. A worker can bypass Playwright routes
    // after reload; the separate loading suite tests the production worker.
    serviceWorkers: "block",
  });
  page = await context.newPage();
  page.setDefaultTimeout(12000);
  const errors = [];
  page.on("pageerror", (error) => errors.push(error.message));
  await page.addInitScript(
    ({ stateDir, id }) => {
      localStorage.setItem(
        `codex-desktop-opened:${stateDir}`,
        JSON.stringify(id),
      );
    },
    { stateDir: original.stateDir, id: a.id },
  );

  const handle = async (route) => {
    const url = new URL(route.request().url());
    if (holdNetwork) {
      held.push(route);
      return;
    }
    if (url.pathname === "/api/sync/identity")
      return route.fulfill({
        json: { workspaceId, ...(legacySync ? {} : { chatState: true }) },
      });
    if (url.pathname === "/api/state") return route.fulfill({ json: state });
    if (url.pathname === "/api/transcript/stream") {
      streams.push(url.searchParams.get("id"));
      return route.fulfill({ status: 204, body: "" });
    }
    if (url.pathname === "/api/search")
      return route.fulfill({
        json: {
          results: [
            {
              kind: "message",
              id: "A-focused-20",
              agent: a.id,
              text: "Find the focused history",
            },
          ],
        },
      });
    if (url.pathname === "/api/search/item")
      return route.fulfill({
        json: {
          id: "A-focused-20",
          room: a.id,
          text: "Find the focused history",
        },
      });
    if (url.pathname === "/api/transcript/page") {
      assert.equal(url.searchParams.get("id"), a.id);
      const focused = !!url.searchParams.get("around");
      const older = payload(a, focused ? "A-focused" : "A-older");
      older.items = older.items.map((item, index) => ({
        ...item,
        at: index - 100,
      }));
      return route.fulfill({
        json: {
          ...older,
          nextCursor: null,
          nextAfterCursor: focused ? "focused-next" : null,
        },
      });
    }
    if (url.pathname === "/api/transcript") {
      const id = url.searchParams.get("id");
      if (id === a.id && !rawA) {
        rawA = route;
        return;
      }
      if (holdWorkspaceB && id === b.id) {
        held.push(route);
        return;
      }
      return route.fulfill({
        json: payload(id === a.id ? a : b, "raw-obsolete"),
      });
    }
    if (url.pathname !== "/api/sync/pull") return route.fallback();
    const scope = url.searchParams.get("scope");
    const after = Number(url.searchParams.get("after") || 0);
    if (scope === "drafts")
      return route.fulfill({
        json: { workspaceId, documents: [], checkpoint: { seq: after } },
      });
    const isState = scope === "state" || scope === "state:chat";
    if (!isState && !scope?.startsWith("transcript:")) return route.fallback();
    const id = isState ? null : scope.slice(11);
    if (holdWorkspaceB && id === b.id) {
      held.push(route);
      return;
    }
    if (id === a.id && !delayedA) {
      delayedA = true;
      await new Promise((resolve) => {
        releaseA = resolve;
      });
    }
    let value = isState ? { seq: stateSeq, data: state } : values.get(id);
    if (!value)
      return route.fulfill({
        status: 400,
        json: { error: "Unknown fixture chat" },
      });
    const forcedStale = staleB && id === b.id;
    if (forcedStale) {
      staleB = false;
      staleReplies++;
      value = { seq: 300, data: payload(b, "B-obsolete") };
    }
    if (id) reads.push({ id, seq: value.seq, at: Date.now(), after });
    await route.fulfill({
      json: {
        workspaceId,
        documents:
          after < value.seq || forcedStale
            ? [
                {
                  id: scope,
                  payload: JSON.stringify(value.data),
                  seq: value.seq,
                  _deleted: false,
                },
              ]
            : [],
        checkpoint: { seq: Math.max(after, value.seq) },
      },
    });
  };
  await page.route("**/api/**", handle);
  const invalidate = async (name) => {
    const response = await fetch(origin + "/api/rename", {
      method: "POST",
      headers: {
        "Content-Type": "application/json",
        "X-Canvas-Token": original.token,
      },
      body: JSON.stringify({ id: b.id, name }),
    });
    assert.ok(response.ok, await response.text());
  };
  const selected = () => page.locator("#conversation-title").innerText();
  const marker = (tag) => page.locator(`[data-message="${tag}-39"]`);
  await page.goto(origin);
  await until(() => !!releaseA, "The first foreground request must start");
  await page.waitForTimeout(1250);
  assert.equal(
    reads.some((read) => read.id === b.id),
    false,
    "Background requests wait for the first foreground transcript",
  );
  assert.equal(streams.includes(b.id), false);
  releaseA();
  await marker("A-current").waitFor();
  await until(
    () => !!rawA,
    "The delayed initial projection exercised raw HTTP",
  );
  await rawA.fulfill({ json: payload(a, "raw-obsolete") });
  await page.waitForTimeout(100);
  assert.equal(
    await marker("raw-obsolete").count(),
    0,
    "Late raw HTTP cannot replace a projection",
  );
  await until(
    () => reads.some((read) => read.id === b.id && read.seq === 300),
    "Never-opened B must load in the background",
  );
  assert.equal(await selected(), a.name);
  assert.equal(
    streams.includes(b.id),
    false,
    "Background B must not open a transcript SSE stream",
  );

  // A real shared SSE invalidation announces a newer B while the reader stays in A.
  values.set(b.id, { seq: 301, data: payload(b, "B-newest") });
  agents.find((agent) => agent.id === b.id).updated = Date.now();
  stateSeq++;
  await invalidate("Prefetch background update");
  await until(
    () => reads.some((read) => read.id === b.id && read.seq === 301),
    "B must refresh in the background after the shared invalidation",
  );
  assert.equal(await selected(), a.name);
  await page.waitForTimeout(200); // Let the real RxDB transaction finish before the network stops.
  await page.locator("#messages").evaluate((element) => {
    element.scrollTop = 900;
    element.dispatchEvent(new Event("scroll"));
  });
  const savedA = await page
    .locator("#messages")
    .evaluate((element) => element.scrollTop);
  holdNetwork = true;
  const switchCached = async (
    id,
    title,
    tag,
    forbidden,
    expectedScroll = null,
  ) => {
    await page.evaluate(
      ({ title, tag, forbidden }) => {
        window.switchFrames = [];
        const capture = (window.captureSwitch =
          (window.captureSwitch || 0) + 1);
        const frame = () => {
          if (window.captureSwitch !== capture) return;
          if (
            document.querySelector("#conversation-title")?.textContent === title
          ) {
            const ids = [
              ...document.querySelectorAll("#messages [data-message]"),
            ].map((node) => node.getAttribute("data-message"));
            window.switchFrames.push({
              time: performance.now(),
              fresh: ids.includes(`${tag}-39`),
              wrong: ids.some((value) => value.startsWith(forbidden)),
              count: ids.length,
              scrollTop: document.querySelector("#messages")?.scrollTop,
            });
          }
          requestAnimationFrame(frame);
        };
        requestAnimationFrame(frame);
      },
      { title, tag, forbidden },
    );
    const start = Date.now();
    await page.locator(`[data-chat="${id}"]`).click();
    await marker(tag).waitFor();
    const elapsed = Date.now() - start;
    assert.ok(elapsed < 500, `Cached ${tag} switch took ${elapsed} ms`);
    await page.waitForFunction(() => window.switchFrames.length >= 3);
    const frames = await page.evaluate(() => {
      window.captureSwitch++;
      return window.switchFrames;
    });
    assert.ok(
      frames.every((frame) => frame.fresh && !frame.wrong && frame.count > 0),
      `Cached switch exposed an empty, old, or wrong-chat frame: ${JSON.stringify(frames)}`,
    );
    if (expectedScroll !== null)
      assert.ok(
        frames.every((frame) => Math.abs(frame.scrollTop - expectedScroll) < 2),
        `The first cached frame must restore scroll ${expectedScroll}: ${JSON.stringify(frames)}`,
      );
    return elapsed;
  };
  const coldCached = await switchCached(b.id, b.name, "B-newest", "A-");
  assert.equal(await marker("B-first").count(), 0);
  await page.locator("#messages").evaluate((element) => {
    element.scrollTop = 1100;
    element.dispatchEvent(new Event("scroll"));
  });
  const savedB = await page
    .locator("#messages")
    .evaluate((element) => element.scrollTop);
  const revisitA = await switchCached(a.id, a.name, "A-current", "B-");
  assert.ok(
    Math.abs(
      (await page
        .locator("#messages")
        .evaluate((element) => element.scrollTop)) - savedA,
    ) < 2,
  );
  const revisitB = await switchCached(b.id, b.name, "B-newest", "A-");
  assert.ok(
    Math.abs(
      (await page
        .locator("#messages")
        .evaluate((element) => element.scrollTop)) - savedB,
    ) < 2,
  );
  assert.equal(
    streams.includes(b.id),
    false,
    "A prefetched switch does not create a per-chat SSE stream",
  );

  holdNetwork = false;
  staleB = true;
  for (const route of held.splice(0)) await handle(route).catch(() => {});
  await invalidate("Prefetch stale response test");
  await until(
    () => staleReplies > 0,
    "The fixture must deliver a lower-sequence B projection",
  );
  await page.waitForTimeout(250);
  assert.equal(
    await marker("B-obsolete").count(),
    0,
    "A late lower-sequence RxDB reply cannot roll history back",
  );
  assert.equal(await marker("B-newest").count(), 1);

  // Both appended and focused history must survive a return before any HTTP reply.
  await page.locator(`[data-chat="${a.id}"]`).click();
  await marker("A-current").waitFor();
  await page.locator("#earlier-messages").click();
  await marker("A-older").waitFor();
  const checkPageReturn = async (tag, offset) => {
    await page.locator("#messages").evaluate((element, top) => {
      element.scrollTop = top;
      element.dispatchEvent(new Event("scroll"));
    }, offset);
    const saved = await page
      .locator("#messages")
      .evaluate((element) => element.scrollTop);
    assert.ok(
      saved > 0,
      "The historical page has a measurable scroll position",
    );
    holdNetwork = true;
    await switchCached(b.id, b.name, "B-newest", "A-");
    const elapsed = await switchCached(a.id, a.name, tag, "B-", saved);
    holdNetwork = false;
    for (const route of held.splice(0)) await handle(route).catch(() => {});
    return elapsed;
  };
  const olderReturn = await checkPageReturn("A-older", 650);
  await page.getByRole("button", { name: "Search chats", exact: true }).click();
  const drawer = page.locator(".mantine-Drawer-content:visible");
  await drawer
    .getByRole("textbox", { name: "Search all conversations" })
    .fill("Find focused history");
  await drawer
    .getByRole("button", { name: "Search", exact: true })
    .last()
    .click();
  await drawer.getByText("Find the focused history", { exact: true }).click();
  await page
    .locator(".mantine-Modal-content:visible")
    .getByRole("button", { name: "Open chat", exact: true })
    .click();
  await drawer.waitFor({ state: "hidden" });
  await marker("A-focused").waitFor();
  await page
    .getByRole("button", { name: "Return to latest messages", exact: true })
    .waitFor();
  assert.equal(
    await marker("A-current").count(),
    0,
    "Focused history excludes the recent tail",
  );
  const focusedReturn = await checkPageReturn("A-focused", 750);
  assert.equal(await marker("A-current").count(), 0);
  await page
    .getByRole("button", { name: "Return to latest messages", exact: true })
    .waitFor();

  // A different workspace at the same origin reuses chat IDs but must use another database.
  workspaceId = "f".repeat(32);
  stateSeq = 10;
  values.set(a.id, { seq: 20, data: payload(a, "Workspace2-A") });
  values.set(b.id, { seq: 30, data: payload(b, "Workspace2-B") });
  holdWorkspaceB = true;
  await page.reload();
  await marker("Workspace2-A").waitFor();
  await page.locator(`[data-chat="${b.id}"]`).click();
  await page.waitForTimeout(200);
  assert.equal(
    await marker("B-newest").count(),
    0,
    "An old workspace cannot supply a same-ID chat",
  );
  assert.equal(await marker("A-current").count(), 0);
  assert.equal(
    await marker("Workspace2-A").count(),
    0,
    "The previous chat stays scoped during a cold switch",
  );
  holdWorkspaceB = false;
  for (const route of held.splice(0)) await handle(route).catch(() => {});
  await marker("Workspace2-B").waitFor();
  assert.deepEqual(errors, []);
  await page.screenshot({ path: join(dir, "workspace-separated.png") });
  console.log(
    JSON.stringify({
      result: "PASS",
      browser: browserType === webkit ? "WebKit" : "Chromium",
      legacySync,
      neverOpenedSwitchMs: coldCached,
      revisitA,
      revisitB,
      olderReturn,
      focusedReturn,
      evidence: dir,
      checks: [
        "foreground before background",
        "background freshness",
        "paged and focused first-frame scroll",
        "no wrong or empty frame",
        "scroll restore",
        "late HTTP and RxDB guards",
        "workspace isolation",
      ],
    }),
  );
} catch (error) {
  await page?.screenshot({ path: join(dir, "failure.png") }).catch(() => {});
  console.error("Evidence:", dir, log);
  throw error;
} finally {
  await browser?.close();
  fixture.kill("SIGTERM");
  if (fixture.exitCode === null)
    await new Promise((resolve) => fixture.once("exit", resolve));
}
