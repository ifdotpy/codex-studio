#!/usr/bin/env node
// Real renderer and isolated server. Delayed streams must not delay navigation.
import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { mkdtemp } from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { createRequire } from "node:module";
const repo = dirname(dirname(fileURLToPath(import.meta.url)));
const { chromium } = createRequire(join(repo, "web/package.json"))(
  "playwright-core",
);
const dir = await mkdtemp(join(tmpdir(), "studio-chat-switch-"));
const proc = spawn(
  "python3",
  ["-B", join(repo, "tests/simple-ui-fixture.py"), dir],
  { stdio: ["ignore", "pipe", "pipe"] },
);
let log = "",
  browser,
  page;
proc.stderr.on("data", (d) => (log += d));
try {
  const port = await new Promise((resolve, reject) => {
    proc.stdout.once("data", (d) => resolve(Number(String(d).trim())));
    proc.once("exit", () => reject(Error(log)));
  });
  const origin = `http://127.0.0.1:${port}`;
  const state = await (await fetch(origin + "/api/state")).json();
  const a = state.threads.find((x) => x.name === "Other project");
  const b = state.threads.find((x) => x.name === "Release lead");
  browser = await chromium.launch({
    headless: true,
    executablePath:
      process.env.CHROME_BIN ||
      "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
  });
  page = await browser.newPage({ viewport: { width: 1440, height: 960 } });
  page.setDefaultTimeout(10000);
  const errors = [];
  page.on("pageerror", (e) => errors.push(e.message));
  await page.addInitScript(() => {
    window.__chatSwitchStartedAt = undefined;
    window.__chatSwitchPaintAt = undefined;
    window.testStreams = [];
    window.createdStreams = [];
    window.EventSource = class extends EventTarget {
      constructor(url) {
        super();
        this.url = url;
        window.testStreams.push(this);
        window.createdStreams.push(url);
      }
      close() {
        window.testStreams = window.testStreams.filter((x) => x !== this);
      }
    };
    window.emitTranscript = (id, data) =>
      window.testStreams.forEach((s) => {
        if (new URL(s.url, location.href).searchParams.get("id") === id)
          s.onmessage?.({ data: JSON.stringify(data) });
      });
  });
  await page.route("**/api/sync/**", (r) =>
    r.fulfill({
      status: 404,
      json: { error: "Fixture uses transcript transport" },
    }),
  );
  const snapshot = {
    ...state,
    threads: state.threads.map((x) => ({
      ...x,
      status: "completed",
      inFlight: false,
      turnId: null,
    })),
    runtime: { ...state.runtime, requests: [] },
  };
  await page.route(/\/api\/state(?:\?.*)?$/, (r) =>
    r.fulfill({ json: snapshot }),
  );
  const payload = (agent, tag) => ({
    agent: { ...agent, status: "completed", inFlight: false, turnId: null },
    items: Array.from({ length: 30 }, (_, i) => ({
      id: `${tag}-${i}`,
      role: "assistant",
      text: `History ${tag} ${i}. ${"Visible saved text. ".repeat(15)}`,
    })),
    replace: true,
  });
  const requests = [];
  let hold = false;
  let resolveNextA;
  let resolveHeldB;
  await page.route("**/api/transcript?*", async (r) => {
    const id = new URL(r.request().url()).searchParams.get("id");
    requests.push({ id, at: Date.now(), route: r });
    if (id === a.id && resolveNextA) {
      resolveNextA(r);
      resolveNextA = undefined;
    }
    if (hold && id === b.id && resolveHeldB) {
      resolveHeldB(r);
      resolveHeldB = undefined;
      return;
    }
    if (!hold)
      await r.fulfill({
        json: payload(id === a.id ? a : b, id === a.id ? "a" : "b"),
      });
  });
  await page.goto(origin);
  const open = async (id, message) => {
    const chat = page.locator(`[data-chat="${id}"]`);
    const selector = `[data-message="${message}"]`;
    await chat.evaluate((element, target) => {
      window.__chatSwitchStartedAt = undefined;
      window.__chatSwitchPaintAt = undefined;
      element.addEventListener(
        "pointerdown",
        () => {
          window.__chatSwitchStartedAt = performance.now();
          const recordVisibleTarget = () => {
            const node = document.querySelector(target);
            if (node?.isConnected && node.getClientRects().length) {
              window.__chatSwitchPaintAt = performance.now();
              return;
            }
            requestAnimationFrame(recordVisibleTarget);
          };
          requestAnimationFrame(recordVisibleTarget);
        },
        { once: true },
      );
    }, selector);
    await chat.click();
    await page.locator(selector).waitFor({ state: "visible" });
    await page.waitForFunction(
      () => typeof window.__chatSwitchPaintAt === "number",
    );
    return page.evaluate(
      () => window.__chatSwitchPaintAt - window.__chatSwitchStartedAt,
    );
  };
  const cold = await open(a.id, "a-29");
  assert.ok(
    cold < 1500,
    `Cold history appears without the old three-second delay: ${cold}ms`,
  );
  await page.locator("#messages").evaluate((e) => {
    e.scrollTop = 950;
    e.dispatchEvent(new Event("scroll"));
  });
  const savedTop = await page.locator("#messages").evaluate((e) => e.scrollTop);
  await open(b.id, "b-29");
  hold = true;
  const previousReads = requests.filter((x) => x.id === a.id).length;
  const refresh = new Promise((resolve) => (resolveNextA = resolve));
  const cached = await open(a.id, "a-29");
  assert.ok(
    cached < 500,
    `Saved history appears before a blocked refresh: ${cached}ms`,
  );
  assert.equal(
    await page.locator('[data-message="b-29"]').count(),
    0,
    "No other chat history leaks",
  );
  assert.ok(
    Math.abs(
      (await page.locator("#messages").evaluate((e) => e.scrollTop)) - savedTop,
    ) < 2,
    "Saved scroll position survives instant history restore",
  );
  const pending = await refresh;
  assert.ok(
    requests.filter((x) => x.id === a.id).length > previousReads,
    "Revisit still refreshes",
  );
  const fresh = payload(a, "fresh");
  fresh.order = fresh.items.map((x) => x.id);
  await page.evaluate(({ id, data }) => window.emitTranscript(id, data), {
    id: a.id,
    data: fresh,
  });
  await page.locator('[data-message="fresh-29"]').waitFor();
  await pending.fulfill({ json: payload(a, "obsolete") });
  await page.evaluate(
    () =>
      new Promise((resolve) =>
        requestAnimationFrame(() => requestAnimationFrame(resolve)),
      ),
  );
  assert.equal(
    await page.locator('[data-message="obsolete-29"]').count(),
    0,
    "Delayed HTTP cannot replace a newer stream",
  );
  const heldB = new Promise((resolve) => (resolveHeldB = resolve));
  const lateRequest = page.waitForRequest((request) => {
    const url = new URL(request.url());
    return (
      url.pathname === "/api/transcript" && url.searchParams.get("id") === b.id
    );
  });
  await open(b.id, "b-29");
  await lateRequest;
  const lateOther = await heldB;
  const refreshed = await open(a.id, "fresh-29");
  assert.ok(refreshed < 500, "Cache retains the newest stream");
  await lateOther.fulfill({ json: payload(b, "late-other") });
  await page.evaluate(
    () =>
      new Promise((resolve) =>
        requestAnimationFrame(() => requestAnimationFrame(resolve)),
      ),
  );
  assert.equal(
    await page.locator('[data-message="late-other-29"]').count(),
    0,
    "Response from a previous chat cannot replace current history",
  );
  // Exercise the real RxDB projection path, including a slow pull on revisit.
  await page.unroute("**/api/sync/**");
  const identity = await (await fetch(origin + "/api/sync/identity")).json();
  let projectionDelay = 0;
  let syncPulls = 0;
  const waitForProjection = async (id, sequence) =>
    page.waitForFunction(
      async ({ databaseName, documentId, sequence }) => {
        const request = indexedDB.open(databaseName);
        const db = await new Promise((resolve, reject) => {
          request.onsuccess = () => resolve(request.result);
          request.onerror = () => reject(request.error);
        });
        const matches = await Promise.all(
          [...db.objectStoreNames].map(
            (store) =>
              new Promise((resolve) => {
                const query = db
                  .transaction(store, "readonly")
                  .objectStore(store)
                  .getAll();
                query.onsuccess = () =>
                  resolve(
                    query.result.some(
                      (row) => row.id === documentId && row.seq === sequence,
                    ),
                  );
                query.onerror = () => resolve(false);
              }),
          ),
        );
        db.close();
        return matches.some(Boolean);
      },
      {
        databaseName: `rxdb-dexie-studio${identity.workspaceId}--0--projections`,
        documentId: `transcript:${id}`,
        sequence,
      },
    );
  await page.route("**/api/sync/pull?*", async (r) => {
    const url = new URL(r.request().url());
    const scope = url.searchParams.get("scope");
    if (!scope?.startsWith("transcript:")) return r.fallback();
    syncPulls++;
    if (projectionDelay)
      await new Promise((resolve) => setTimeout(resolve, projectionDelay));
    const selected = scope.slice("transcript:".length) === a.id ? a : b;
    const data = payload(selected, selected.id === a.id ? "sync-a" : "sync-b");
    data.agent = {
      ...data.agent,
      status: "running",
      inFlight: true,
      turnId: "fixture-live-turn",
    };
    const seq = selected.id === a.id ? 100 : 101;
    const after = Number(url.searchParams.get("after") || 0);
    await r.fulfill({
      json: {
        ...identity,
        documents:
          after < seq
            ? [
                {
                  id: scope,
                  payload: JSON.stringify(data),
                  seq,
                  _deleted: false,
                },
              ]
            : [],
        checkpoint: { seq: Math.max(after, seq) },
      },
    });
  });
  // Preserve the old server's byte comparison in this fixture. New UI writes
  // must clear drafts safely before a busy backend can be upgraded.
  let legacyDraftConflicts = 0;
  await page.route("**/api/sync/drafts", async (r) => {
    const old = await (
      await fetch(origin + "/api/sync/pull?scope=drafts")
    ).json();
    const conflicts = [];
    for (const row of r.request().postDataJSON().rows) {
      const next = row.newDocumentState;
      const previous = old.documents.find((x) => x.id === next.id);
      const assumed = row.assumedMasterState;
      if (
        previous &&
        (!assumed ||
          assumed.payload !== previous.payload ||
          !!assumed._deleted !== !!previous._deleted) &&
        (next.payload !== previous.payload ||
          !!next._deleted !== !!previous._deleted)
      )
        conflicts.push(previous);
    }
    if (conflicts.length) {
      legacyDraftConflicts += conflicts.length;
      return r.fulfill({ json: conflicts });
    }
    return r.fallback();
  });
  hold = false;
  await page.reload();
  await open(a.id, "sync-a-29");
  const streamCount = () =>
    page.evaluate(
      (id) =>
        window.createdStreams.filter(
          (url) => new URL(url, location.href).searchParams.get("id") === id,
        ).length,
      a.id,
    );
  const beforeQuickReturn = await streamCount();
  projectionDelay = 1000;
  await page.locator(`[data-chat="${b.id}"]`).click();
  await page.locator('[data-message="sync-b-29"]').waitFor();
  await waitForProjection(a.id, 100);
  await waitForProjection(b.id, 101);
  await open(a.id, "sync-a-29");
  assert.equal(
    await streamCount(),
    beforeQuickReturn,
    "A prefetched return uses the shared projection transport without another chat stream",
  );
  projectionDelay = 0;
  await open(b.id, "sync-b-29");
  projectionDelay = 1000;
  hold = true;
  const syncCached = await open(a.id, "sync-a-29");
  assert.ok(
    syncCached < 500,
    `Revisit does not wait for RxDB pull: ${syncCached}ms`,
  );
  assert.ok(
    syncPulls >= 2,
    "Real RxDB replication loaded both chat projections",
  );
  assert.equal(
    await page.locator('[data-message="sync-b-29"]').count(),
    0,
    "Replicated chat history stays scoped",
  );
  const sends = [];
  await page.route("**/api/messages", async (r) => {
    sends.push(r.request().postDataJSON());
    assert.equal(
      r.request().headers()["x-canvas-workspace"],
      identity.workspaceId,
      "Send uses the synchronized workspace",
    );
    await r.fulfill({
      json: {
        id: r.request().postDataJSON().id,
        status: "queued",
        deliveries: { [a.id]: "queued" },
      },
    });
  });
  await page.locator("#stop").waitFor();
  for (const [action, delivery] of [
    ["button", "after_tool"],
    ["Enter", "after_tool"],
    ["queue", "queue"],
  ]) {
    await page.locator("#message").fill(`Sync shortcut ${action}`);
    if (action === "button") await page.locator("#send").click();
    else if (action === "queue")
      await page
        .getByRole("button", { name: "Queue after turn", exact: true })
        .click();
    else await page.locator("#message").press(action);
    await page
      .waitForFunction(() => document.querySelector("#message").value === "")
      .catch(async (error) => {
        console.error("Shortcut failure", action, JSON.stringify(sends));
        throw error;
      });
    const expectedCount = action === "button" ? 1 : action === "Enter" ? 2 : 3;
    const deadline = Date.now() + 12000;
    while (sends.length < expectedCount && Date.now() < deadline)
      await new Promise((resolve) => setTimeout(resolve, 40));
    assert.equal(
      sends.length,
      expectedCount,
      "The outbox submits each message once",
    );
    assert.equal(sends.at(-1).delivery, delivery);
    assert.equal(sends.at(-1).room, a.id);
  }
  assert.equal(sends.length, 3, "Each shortcut sends exactly one message");
  assert.equal(
    new Set(sends.map((x) => x.id)).size,
    3,
    "Distinct intentions retain distinct message identities",
  );
  assert.equal(
    legacyDraftConflicts,
    0,
    "Sequential edit and clear do not create a false conflict on the old server",
  );
  assert.deepEqual(errors, []);
  console.log(
    `Chat switch latency: PASS (cold ${cold}ms, cached ${cached}ms, refreshed ${refreshed}ms, RxDB revisit ${syncCached}ms; scope isolation, scroll restore, late HTTP guard, real RxDB Send/Enter/Queue).`,
  );
} catch (e) {
  await page?.screenshot({ path: join(dir, "failure.png") });
  console.error("Evidence:", dir);
  throw e;
} finally {
  await browser?.close();
  proc.kill("SIGTERM");
  if (proc.exitCode === null) await new Promise((r) => proc.once("exit", r));
}
