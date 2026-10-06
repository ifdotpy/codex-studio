import {
  apiSchemaHandshakeSse,
  protocol3SseEvent,
  test,
  expect,
} from "../playwright.mjs";
// Real RxDB/Dexie in Chromium, with an isolated HTTP fixture and no runtime.
import { fileURLToPath } from "node:url";
test("Sync Browser", async ({
  browser: _testBrowser,
  context: _testContext,
  page: testPage,
}) => {
  const assert = {
    equal: (actual, expected, message) =>
      expect(actual, message).toBe(expected),
    notEqual: (actual, expected, message) =>
      expect(actual, message).not.toBe(expected),
    deepEqual: (actual, expected, message) =>
      expect(actual, message).toEqual(expected),
    ok: (actual, message) => expect(actual, message).toBeTruthy(),
    match: (actual, expected, message) =>
      expect(actual, message).toMatch(expected),
    doesNotMatch: (actual, expected, message) =>
      expect(actual, message).not.toMatch(expected),
    fail: (message) => {
      throw new Error(message);
    },
  };

  const { createServer } = await import(
    new URL(
      "../../../web/node_modules/vite/dist/node/index.js",
      import.meta.url,
    )
  );
  const server = await createServer({
    configFile: false,
    root: fileURLToPath(new URL("../../../web", import.meta.url)),
    server: { host: "127.0.0.1", port: 0 },
  });
  server.middlewares.use("/sync-check", (_req, res) => {
    res.setHeader("Content-Type", "text/html");
    res.end("<!doctype html><title>Sync contract</title>");
  });
  await server.listen();
  try {
    const page = testPage;
    const errors = [];
    page.on("pageerror", (error) => errors.push(error.message));
    await page.route("**/sync-check", (route) =>
      route.fulfill({
        contentType: "text/html",
        body: "<!doctype html><title>Sync contract</title>",
      }),
    );
    let revision = 1,
      failSend = true,
      sends = [],
      content = "first",
      draftPushes = [];
    await page.route("**/api/session", (route) =>
      route.fulfill({ json: { token: "fixture" } }),
    );
    await page.route("**/api/sync/identity", (route) =>
      route.fulfill({ json: { workspaceId: "a".repeat(32) } }),
    );
    await page.addInitScript(() => {
      window.entityStreams = [];
      const Native = window.EventSource;
      window.EventSource = class extends Native {
        constructor(url, options) {
          super(url, options);
          const parsed = new URL(url, location.href);
          if (parsed.searchParams.get("protocol") === "3")
            window.entityStreams.push({
              source: this,
              resources: JSON.parse(
                parsed.searchParams.get("resources") || "[]",
              ),
            });
        }
      };
    });
    await page.route("**/api/sync/stream**", (route) => {
      const resources = JSON.parse(
        new URL(route.request().url()).searchParams.get("resources") || "[]",
      );
      route.fulfill({
        contentType: "text/event-stream",
        body: apiSchemaHandshakeSse(
          protocol3SseEvent("resources", {
            protocol: 3,
            workspaceId: "a".repeat(32),
            epoch: "fixture-epoch",
            revision,
            reason: "initial",
            resources,
          }),
        ),
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
      if (url.searchParams.get("scope") === "state:entities:v1")
        return route.fulfill({
          json: {
            workspaceId: "a".repeat(32),
            documents:
              after < revision
                ? [
                    {
                      id: "entity:agent:lead",
                      payload: JSON.stringify({
                        collection: "agent",
                        id: "lead",
                        value: {
                          id: "lead",
                          name: content,
                          status: "running",
                          source: "managed",
                        },
                      }),
                      seq: revision,
                      _deleted: false,
                    },
                  ]
                : [],
            checkpoint: { seq: Math.max(after, revision) },
            maxSeq: revision,
            initialHigh: revision,
          },
        });
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
      window.stopSync = client.subscribeStateProjection(
        (value) => {
          if (value) window.values.push(value.threads[0]?.name);
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
    content = "second";
    revision++;
    await page.waitForFunction(() => window.entityStreams.length > 0);
    await page.evaluate((seq) => {
      const stream = window.entityStreams.at(-1);
      stream.source.dispatchEvent(
        new MessageEvent("resources", {
          data: JSON.stringify({
            protocol: 3,
            workspaceId: "a".repeat(32),
            epoch: "fixture-epoch",
            revision: seq,
            reason: "change",
            resources: stream.resources,
          }),
        }),
      );
    }, revision);
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
    console.log("sync browser contract passed");
  } finally {
    await server.close();
  }
});
