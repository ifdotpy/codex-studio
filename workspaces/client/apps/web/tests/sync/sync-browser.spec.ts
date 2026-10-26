import type { FixturePullResponse, FixtureResourceChange } from "./helpers.ts";
import type { PostBody } from "../../src/apiTypes.ts";
import type { components } from "../../src/generated/api.ts";
import { fixturePostBody, fixtureResources } from "./helpers.ts";
import type {} from "./sync-browser.fixture.tsx";
import { fixtureOrigin, fixtureAssert as assert } from "./helpers.ts";
import { SYNC_PROTOCOL_VERSION } from "../../src/generated/api.ts";
import {
  apiSchemaHandshakeSse,
  protocol3SseEvent,
  API_SCHEMA_HASH_HEADER,
  readApiSchemaHash,
  test,
} from "../playwright.ts";
// Real RxDB/Dexie in Chromium, with an isolated HTTP fixture and no runtime.
import { fileURLToPath } from "node:url";
test("Sync Browser", async ({
  browser: _testBrowser,
  context: _testContext,
  page: testPage,
}) => {
  const { createFixtureViteServer: createServer } =
    await import("../playwright.ts");
  const server = await createServer({
    configFile: false,
    optimizeDeps: { noDiscovery: true },
    root: fileURLToPath(new URL("../../", import.meta.url)),
    server: { host: "127.0.0.1", port: 0 },
  });
  server.middlewares.use("/sync-check", (_req, res) => {
    res.setHeader("Content-Type", "text/html");
    res.end("<!doctype html><title>Sync contract</title>");
  });
  await server.listen();
  try {
    const page = testPage;
    const errors: string[] = [];
    page.on("pageerror", (error) => errors.push(error.message));
    await page.route("**/sync-check", (route) =>
      route.fulfill({
        contentType: "text/html",
        body: "<!doctype html><title>Sync contract</title>",
      }),
    );
    let revision = 1,
      failSend = true,
      sends: PostBody<"/api/messages">[] = [],
      content = "first",
      draftPushes: components["schemas"]["DraftPushRow"][] = [];
    await page.route("**/api/session", (route) =>
      route.fulfill({ json: { token: "fixture" } }),
    );
    await page.route("**/api/sync/identity", (route) =>
      route.fulfill({
        json: { workspaceId: "a".repeat(32) },
        headers: { [API_SCHEMA_HASH_HEADER]: readApiSchemaHash() },
      }),
    );
    await page.addInitScript((protocol) => {
      window.syncBrowserFixtureEntityStreams = [];
      const Native = window.EventSource;
      window.EventSource = class extends Native {
        constructor(url: string | URL, options?: EventSourceInit) {
          super(url, options);
          const parsed = new URL(url, location.href);
          if (parsed.searchParams.get("protocol") === String(protocol))
            window.syncBrowserFixtureEntityStreams.push({
              source: this,
              resources: JSON.parse(
                parsed.searchParams.get("resources") || "[]",
              ),
            });
        }
      };
    }, SYNC_PROTOCOL_VERSION);
    await page.route("**/api/sync/stream**", (route) => {
      const resources = fixtureResources(
        new URL(route.request().url()).searchParams.get("resources"),
      );
      route.fulfill({
        contentType: "text/event-stream",
        body: apiSchemaHandshakeSse(
          protocol3SseEvent("resources", {
            protocol: 4,
            workspaceId: "a".repeat(32),
            epoch: "fixture-epoch",
            revision,
            reason: "initial",
            resources,
            resourceVersions: resources.map((resource) => ({
              resource,
              revision,
            })),
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
          } satisfies FixturePullResponse,
        });
      if (url.searchParams.get("scope") === "state:entities:v1")
        return route.fulfill({
          json: {
            workspaceId: "a".repeat(32),
            documents:
              after < revision
                ? [
                    {
                      op: "upsert",
                      collection: "agent",
                      id: "lead",
                      value: {
                        id: "lead",
                        name: content,
                        status: "running",
                        source: "managed",
                      },
                      seq: revision,
                    },
                  ]
                : [],
            checkpoint: { seq: Math.max(after, revision) },
            maxSeq: revision,
            initialHigh: revision,
          } satisfies FixturePullResponse,
        });
      return route.fulfill({
        status: 400,
        json: { error: "Invalid sync scope" },
      });
    });
    await page.route("**/api/sync/drafts", (route) => {
      draftPushes.push(
        ...fixturePostBody<"/api/sync/drafts">(route.request().postDataJSON())
          .rows,
      );
      return route.fulfill({ json: [] });
    });
    await page.route("**/api/messages", (route) => {
      sends.push(
        fixturePostBody<"/api/messages">(route.request().postDataJSON()),
      );
      return failSend
        ? route.abort("failed")
        : route.fulfill({ json: { status: "queued", id: "intent-one" } });
    });
    const origin = fixtureOrigin(server);
    await page.goto(origin + "/sync-check");
    await page.evaluate(async () => {
      const client = await import("/src/sync/client.ts");
      window.syncBrowserFixtureValues = [];
      window.syncBrowserFixtureStopSync = client.subscribeStateProjection(
        (value) => {
          if (value)
            window.syncBrowserFixtureValues.push(value.threads[0]?.name);
        },
        (error) => {
          if (error) window.syncBrowserFixtureFailure = String(error);
          else delete window.syncBrowserFixtureFailure;
        },
      );
    });
    await page.waitForFunction(
      () =>
        window.syncBrowserFixtureValues.includes("first") ||
        window.syncBrowserFixtureFailure,
    );
    assert.equal(
      await page.evaluate(() => window.syncBrowserFixtureFailure),
      undefined,
    );
    content = "second";
    revision++;
    await page.waitForFunction(
      () => window.syncBrowserFixtureEntityStreams.length > 0,
    );
    await page.evaluate((seq) => {
      const stream = window.syncBrowserFixtureEntityStreams.at(-1);
      if (!stream) throw new Error("Missing captured entity stream");
      stream.source.dispatchEvent(
        new MessageEvent("resources", {
          data: JSON.stringify({
            protocol: 4,
            workspaceId: "a".repeat(32),
            epoch: "fixture-epoch",
            revision: seq,
            reason: "change",
            resources: stream.resources,
            resourceVersions: stream.resources.map((resource) => ({
              resource,
              revision: seq,
            })),
          } satisfies FixtureResourceChange),
        }),
      );
    }, revision);
    await page.waitForFunction(
      () => window.syncBrowserFixtureValues.includes("second"),
      null,
      {
        timeout: 10000,
      },
    );
    await page.evaluate(async () => {
      const client = await import("/src/sync/client.ts");
      window.syncBrowserFixtureStopDrafts = await client.startDraftReplication(
        (error) => {
          if (error) window.syncBrowserFixtureFailure = String(error);
          else delete window.syncBrowserFixtureFailure;
        },
      );
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
    const pushedDraft = draftPushes[0];
    if (!pushedDraft) throw new Error("Missing pushed draft");
    assert.equal(
      JSON.parse(pushedDraft.newDocumentState.payload).text,
      "draft",
    );
    assert.equal(
      "newForkState" in pushedDraft ? pushedDraft.newForkState : undefined,
      undefined,
    );
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
      const row = await db.outbox.findOne("intent-one").exec();
      if (!row) throw new Error("Missing outbox row");
      return JSON.parse(row.payload);
    });
    assert.equal(stored.status, "queued");
    failSend = false;
    await page.addScriptTag({
      type: "module",
      url: "/tests/sync/sync-browser.fixture.tsx",
    });
    await page.evaluate(() => window.syncBrowserMount1());
    for (let i = 0; i < 100 && sends.length < 2; i++)
      await new Promise((resolve) => setTimeout(resolve, 50));
    await page.waitForFunction(async () => {
      const { db } = await (await import("/src/sync/client.ts")).syncDatabase();
      return (
        JSON.parse(
          (await db.outbox.findOne("intent-one").exec())?.payload ?? "null",
        ).status === "accepted"
      );
    });
    assert.equal(sends.length, 2);
    assert.deepEqual(sends[0], sends[1]);
    await page.evaluate(async () => {
      const { db } = await (await import("/src/sync/client.ts")).syncDatabase();
      const doc = await db.outbox.findOne("intent-one").exec();
      if (!doc) throw new Error("Missing accepted outbox row");
      if (JSON.parse(doc.payload).status !== "accepted")
        throw Error("Receipt not saved");
      const handler = (await import("/src/sync/conflicts.ts"))
        .draftConflictHandler;
      const merged = await handler.resolve(
        {
          realMasterState: {
            id: "d",
            seq: 1,
            payload: JSON.stringify({ text: "desktop" }),
            _deleted: false,
          },
          newDocumentState: {
            id: "d",
            seq: 0,
            payload: JSON.stringify({ text: "phone" }),
            _deleted: false,
          },
        },
        "sync-browser-fixture",
      );
      if (!JSON.parse(merged.payload).alternatives.includes("phone"))
        throw Error("Conflict lost");
    });
    assert.deepEqual(errors, []);
    console.log("sync browser contract passed");
  } finally {
    await server.close();
  }
});
