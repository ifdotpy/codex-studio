// Real RxDB/Dexie in Chromium, with an isolated HTTP fixture and no runtime.
import assert from "node:assert/strict";
import { createRequire } from "node:module";
import { fileURLToPath } from "node:url";
const require = createRequire(new URL("../web/package.json", import.meta.url));
const { chromium } = require("playwright-core");
const { createServer } = await import(
  new URL("../web/node_modules/vite/dist/node/index.js", import.meta.url)
);
const server = await createServer({
  configFile: false,
  root: fileURLToPath(new URL("../web", import.meta.url)),
  server: { host: "127.0.0.1", port: 0 },
});
server.middlewares.use("/sync-check", (_req, res) => {
  res.setHeader("Content-Type", "text/html");
  res.end("<!doctype html><title>Sync contract</title>");
});
await server.listen();
const browser = await chromium.launch({
  executablePath:
    process.env.CHROME_BIN ||
    "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
  headless: true,
});
try {
  const page = await browser.newPage();
  const errors = [];
  page.on("pageerror", (error) => errors.push(error.message));
  await page.route("**/sync-check", (route) =>
    route.fulfill({
      contentType: "text/html",
      body: "<!doctype html><title>Sync contract</title>",
    }),
  );
  let revision = 1,
    draftGeneration = 0,
    generationPolls = 0,
    statePulls = 0;
  let failSend = true,
    sends = [],
    content = "first",
    draftPushes = [];
  await page.route("**/api/state", (route) =>
    route.fulfill({ json: { token: "fixture" } }),
  );
  await page.route("**/api/session", (route) =>
    route.fulfill({ json: { token: "fixture" } }),
  );
  await page.route("**/api/sync/identity", (route) =>
    route.fulfill({ json: { workspaceId: "a".repeat(32) } }),
  );
  await page.route("**/api/sync/stream*", (route) =>
    route.fulfill({
      contentType: "text/event-stream",
      body: `retry: 3600000\n\ndata: ${JSON.stringify({
        protocol: 2,
        workspaceId: "a".repeat(32),
        generations: { drafts: 0, state: revision, transcripts: 0 },
      })}\n\n`,
    }),
  );
  await page.route("**/api/sync/generations", (route) => {
    generationPolls++;
    return route.fulfill({
      json: {
        protocol: 2,
        workspaceId: "a".repeat(32),
        generations: {
          drafts: draftGeneration,
          state: revision,
          transcripts: 0,
        },
      },
    });
  });
  await page.route("**/api/sync/pull?**", (route) => {
    const url = new URL(route.request().url()),
      after = Number(url.searchParams.get("after"));
    if (url.searchParams.get("scope") === "drafts")
      return route.fulfill({
        json: {
          workspaceId: "a".repeat(32),
          documents: [],
          checkpoint: { seq: 0 },
        },
      });
    statePulls++;
    route.fulfill({
      json: {
        workspaceId: "a".repeat(32),
        documents:
          after < revision
            ? [
                {
                  id: "state:chat",
                  payload: JSON.stringify({ text: content }),
                  seq: revision,
                  _deleted: false,
                },
              ]
            : [],
        checkpoint: { seq: Math.max(after, revision) },
      },
    });
  });
  await page.route("**/api/sync/drafts", (route) => {
    draftPushes.push(...route.request().postDataJSON().rows);
    return route.fulfill({ json: [] });
  });
  await page.route("**/api/messages", (route) => {
    sends.push(route.request().postDataJSON());
    return failSend
      ? route.abort("failed")
      : route.fulfill({ json: { status: "queued", id: "intent-one" } });
  });
  const origin = `http://127.0.0.1:${server.httpServer.address().port}`;
  await page.goto(origin + "/sync-check");
  await page.evaluate(async () => {
    const client = await import("/src/sync/client.ts");
    window.values = [];
    window.stopSync = await client.watchProjection(
      "state",
      (value) => {
        if (value) window.values.push(value.text);
      },
      (error) => {
        if (error) window.failure = String(error);
        else delete window.failure;
      },
    );
  });
  await page.waitForFunction(
    () => window.values.includes("first") || window.failure,
  );
  assert.equal(await page.evaluate(() => window.failure), undefined);
  for (let i = 0; i < 90 && generationPolls < 1; i++)
    await new Promise((resolve) => setTimeout(resolve, 50));
  assert.ok(
    generationPolls >= 1,
    "The scoped generation fallback was not used",
  );
  const pullsBeforeDraftOnlyChange = statePulls;
  const pollsBeforeDraftOnlyChange = generationPolls;
  draftGeneration++;
  for (let i = 0; i < 90 && generationPolls === pollsBeforeDraftOnlyChange; i++)
    await new Promise((resolve) => setTimeout(resolve, 50));
  assert.equal(
    statePulls,
    pullsBeforeDraftOnlyChange,
    "Draft-only changes must not refresh chat state",
  );
  content = "second";
  revision++;
  await page.waitForFunction(() => window.values.includes("second"), null, {
    timeout: 10000,
  });
  await page.evaluate(async () => {
    const client = await import("/src/sync/client.ts");
    window.stopDrafts = await client.startDraftReplication((error) => {
      if (error) window.failure = String(error);
      else delete window.failure;
    });
    const { db } = await client.syncDatabase();
    await db.drafts.insert({
      id: "device:lead",
      seq: 0,
      payload: JSON.stringify({
        text: "draft",
        session: "lead",
        device: "device",
        updated: 1,
      }),
    });
  });
  for (let i = 0; i < 100 && !draftPushes.length; i++)
    await new Promise((resolve) => setTimeout(resolve, 50));
  assert.equal(draftPushes.length, 1);
  assert.equal(
    JSON.parse(draftPushes[0].newDocumentState.payload).text,
    "draft",
  );
  assert.equal(draftPushes[0].newForkState, undefined);
  const result = await page.evaluate(async () =>
    (await import("/src/sync/send.ts")).durableSend({
      id: "intent-one",
      room: "lead",
      text: "Exact text",
      assets: [],
      delivery: "queue",
    }),
  );
  assert.equal(result.queued, true);
  await page.reload();
  const stored = await page.evaluate(async () => {
    const { db } = await (await import("/src/sync/client.ts")).syncDatabase();
    return JSON.parse((await db.outbox.findOne("intent-one").exec()).payload);
  });
  assert.equal(stored.status, "queued");
  failSend = false;
  await page.evaluate(async () => {
    const { useOutbox } = await import("/src/sync/send.ts");
    const reactModule = await import("/node_modules/.vite/deps/react.js");
    const React = reactModule.default || reactModule;
    const domModule =
      await import("/node_modules/.vite/deps/react-dom_client.js");
    const { createRoot } = domModule.default || domModule;
    const root = document.createElement("div");
    document.body.appendChild(root);
    createRoot(root).render(
      React.createElement(function Harness() {
        useOutbox();
        return null;
      }),
    );
  });
  for (let i = 0; i < 100 && sends.length < 2; i++)
    await new Promise((resolve) => setTimeout(resolve, 50));
  await page.waitForFunction(async () => {
    const { db } = await (await import("/src/sync/client.ts")).syncDatabase();
    return (
      JSON.parse((await db.outbox.findOne("intent-one").exec()).payload)
        .status === "accepted"
    );
  });
  assert.equal(sends.length, 2);
  assert.deepEqual(sends[0], sends[1]);
  await page.evaluate(async () => {
    const { db } = await (await import("/src/sync/client.ts")).syncDatabase();
    const doc = await db.outbox.findOne("intent-one").exec();
    if (JSON.parse(doc.payload).status !== "accepted")
      throw Error("Receipt not saved");
    const handler = (await import("/src/sync/conflicts.ts"))
      .draftConflictHandler;
    const merged = await handler.resolve({
      realMasterState: {
        id: "d",
        seq: 1,
        payload: JSON.stringify({ text: "desktop" }),
      },
      newDocumentState: {
        id: "d",
        seq: 0,
        payload: JSON.stringify({ text: "phone" }),
      },
    });
    if (!JSON.parse(merged.payload).alternatives.includes("phone"))
      throw Error("Conflict lost");
  });
  assert.deepEqual(errors, []);
  const debouncePage = await browser.newPage();
  await debouncePage.goto(origin + "/sync-check");
  const scopedCallbacks = await debouncePage.evaluate(async () => {
    const streams = [];
    class FakeEventSource {
      onmessage;
      onopen;
      constructor() {
        streams.push(this);
      }
      close() {}
      emit(generations, workspaceId = "b".repeat(32)) {
        this.onmessage?.({
          data: JSON.stringify({
            protocol: 2,
            workspaceId,
            generations,
          }),
        });
      }
    }
    window.EventSource = FakeEventSource;
    const { watchSyncInvalidations } = await import("/src/sync/client.ts");
    const counts = { state: 0, transcripts: 0 };
    watchSyncInvalidations("state", () => counts.state++);
    watchSyncInvalidations("transcripts", () => counts.transcripts++);
    const base = { drafts: 0, state: 1, transcripts: 0 };
    streams[0].onopen?.();
    streams[0].emit(base);
    await new Promise((resolve) => setTimeout(resolve, 80));
    counts.state = 0;
    counts.transcripts = 0;
    streams[0].emit({ ...base, state: 2 });
    streams[0].emit({ ...base, state: 2, transcripts: 1 });
    await new Promise((resolve) => setTimeout(resolve, 80));
    const rapid = { ...counts };
    counts.state = 0;
    counts.transcripts = 0;
    streams[0].emit({ ...base, state: 2, transcripts: 1 }, "c".repeat(32));
    await new Promise((resolve) => setTimeout(resolve, 80));
    const workspaceReset = { ...counts };
    counts.state = 0;
    counts.transcripts = 0;
    streams[0].onopen?.();
    await new Promise((resolve) => setTimeout(resolve, 80));
    const streamReconnect = { ...counts };
    counts.state = 0;
    counts.transcripts = 0;
    window.dispatchEvent(new Event("pageshow"));
    await new Promise((resolve) => setTimeout(resolve, 140));
    return {
      rapid,
      workspaceReset,
      streamReconnect,
      resumeReconnect: { ...counts },
      streams: streams.length,
    };
  });
  assert.deepEqual(
    scopedCallbacks.rapid,
    { state: 1, transcripts: 1 },
    "Rapid events for different scopes must both survive debounce",
  );
  assert.deepEqual(
    scopedCallbacks.workspaceReset,
    { state: 1, transcripts: 1 },
    "Workspace reset must conservatively refresh every scope",
  );
  assert.deepEqual(
    scopedCallbacks.streamReconnect,
    { state: 1, transcripts: 1 },
    "A reconnect open with unchanged counters must refresh every scope",
  );
  assert.deepEqual(
    scopedCallbacks.resumeReconnect,
    { state: 1, transcripts: 1 },
    "Resume/reconnect must conservatively refresh every scope",
  );
  assert.equal(scopedCallbacks.streams, 2);
  await debouncePage.close();
  console.log("sync browser contract passed");
} finally {
  await browser.close();
  await server.close();
}
