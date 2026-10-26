import type {
  LegacyPullFixture,
  LegacyIdentityFixture,
} from "./prefetch-storage.fixture.ts";
import { fixtureOrigin } from "./browser.ts";
import type { ServerResponse } from "node:http";
import { SYNC_PROTOCOL_VERSION } from "../../src/generated/api.ts";
import type { components } from "../../src/generated/api.ts";
import type {} from "./chat-prefetch-browser.fixture.ts";
import {
  apiSchemaHandshakeSse,
  createFixtureViteServer,
  test,
} from "../playwright.ts";
// Real RxDB projection persistence and two browser tabs, with an isolated API.
import assert from "node:assert/strict";
import { fileURLToPath } from "node:url";
test("Chat prefetch browser", async ({
  browser: testBrowser,
  context: runnerContext,
}) => {
  const streams = new Set<ServerResponse>();
  const server = await createFixtureViteServer({
    configFile: false,
    root: fileURLToPath(new URL("../../", import.meta.url)),
    server: { host: "127.0.0.1", port: 0 },
    plugins: [
      {
        name: "prefetch-fixture",
        configureServer(server) {
          server.middlewares.use(
            "/fixture/resource-event",
            (request, response) => {
              const url = new URL(request.url ?? "/", "http://127.0.0.1");
              const workspace = url.searchParams.get("workspace");
              const agentId = url.searchParams.get("agent");
              const revision = Number(url.searchParams.get("revision"));
              if (workspace === null || agentId === null)
                throw new Error("Incomplete resource fixture request");
              const resource = {
                kind: "transcript",
                agentId,
              } satisfies components["schemas"]["ResourceRef"];
              for (const stream of streams)
                stream.write(
                  `event: resources\ndata: ${JSON.stringify({
                    protocol: SYNC_PROTOCOL_VERSION,
                    workspaceId: workspace,
                    epoch: "fixture-epoch",
                    revision,
                    reason: "change",
                    resources: [resource],
                    resourceVersions: [{ resource, revision }],
                  } satisfies components["schemas"]["ResourceChangeEvent"])}\n\n`,
                );
              response.end("ok");
            },
          );
          server.middlewares.use("/api/sync/stream", (_request, response) => {
            response.setHeader("Content-Type", "text/event-stream");
            response.write(apiSchemaHandshakeSse());
            response.flushHeaders();
            streams.add(response);
            response.on("close", () => streams.delete(response));
          });
        },
      },
    ],
  });
  await server.listen();
  const delay = (ms: number) =>
    new Promise<void>((resolve) => setTimeout(resolve, ms));
  const workspaceId = "b".repeat(32);
  const documents = new Map<
    string | null,
    components["schemas"]["SyncDocument"]
  >();
  const pulls: { name: string; scope: string | null; after: number }[] = [];
  const holds = new Map<string, Promise<void>>();
  let running = 0,
    maximum = 0;
  const doc = (id: string, seq: number, deleted = false) =>
    ({
      id: `transcript:${id}`,
      seq,
      payload: JSON.stringify({
        items: [{ id: `message-${seq}`, body: `revision ${seq}` }],
      }),
      _deleted: deleted,
    }) satisfies components["schemas"]["SyncDocument"];
  async function until(test: () => boolean, message: string) {
    for (let n = 0; n < 150 && !test(); n++) await delay(20);
    assert.ok(test(), message);
  }
  try {
    const context = runnerContext;
    const errors: string[] = [];
    async function pageFor(name: string) {
      const page = await context.newPage();
      page.on("pageerror", (error) => errors.push(error.message));
      await page.route("**/check", (route) =>
        route.fulfill({
          contentType: "text/html",
          body: '<!doctype html><script type="module" src="/tests/chat/chat-prefetch-browser.fixture.ts"></script>',
        }),
      );
      await page.route("**/api/sync/identity", (route) =>
        route.fulfill({
          json: { workspaceId } satisfies LegacyIdentityFixture,
        }),
      );
      await page.route("**/api/sync/pull?*", async (route) => {
        const url = new URL(route.request().url());
        const scope = url.searchParams.get("scope");
        const after = Number(url.searchParams.get("after"));
        pulls.push({ name, scope, after });
        running++;
        maximum = Math.max(maximum, running);
        if (scope === "transcript:failure") {
          running--;
          return route.fulfill({
            status: 500,
            json: {
              error: "Temporary fixture failure",
            } satisfies components["schemas"]["ErrorResponse"],
          });
        }
        const current = documents.get(scope);
        const result = {
          workspaceId,
          documents: current && current.seq > after ? [current] : [],
          checkpoint: { seq: current?.seq || after },
        } satisfies LegacyPullFixture;
        const hold = holds.get(`${name}:${scope}`);
        if (hold) {
          holds.delete(`${name}:${scope}`);
          await hold;
        }
        try {
          await route.fulfill({ json: result });
        } finally {
          running--;
        }
      });
      await page.clock.install();
      await page.goto(`${fixtureOrigin(server)}/check`);
      await page.waitForFunction(() => !!window.prefetchBrowser);
      return page;
    }
    const page = await pageFor("first");
    // A scoped chat refresh must not scan unrelated durable cache entries.
    await page.evaluate(async () => {
      const { db } = await window.prefetchBrowser.client.syncDatabase();
      const payload = JSON.stringify({
        items: [],
        padding: "unrelated transcript",
      });
      await db.projections.bulkInsert(
        Array.from({ length: 64 }, (_, n) => ({
          id: `transcript:cold-${n}`,
          seq: n + 1000,
          payload,
        })),
      );
      window.prefetchBrowser.coldReads = [];
      const storage = db.projections.storageInstance;
      const original = storage.query.bind(storage);
      storage.query = async (...args) => {
        const result = await original(...args);
        window.prefetchBrowser.coldReads.push(
          ...result.documents
            .filter((row) => row.id.startsWith("transcript:cold-"))
            .map((row) => row.id),
        );
        return result;
      };
    });
    documents.set("transcript:a", doc("a", 1));
    assert.equal(
      await page.evaluate(
        (workspace) =>
          window.prefetchBrowser.client.prefetchTranscript(workspace, "a"),
        workspaceId,
      ),
      true,
    );
    assert.equal(
      await page.evaluate(
        (workspace) =>
          window.prefetchBrowser.cache.peekTranscript(workspace, "a")?.seq,
        workspaceId,
      ),
      1,
    );
    await until(
      () => streams.size === 0,
      "Completed background work releases its socket",
    );
    const finishedPulls = pulls.length;
    await page.clock.fastForward(3200);
    assert.equal(
      pulls.length,
      finishedPulls,
      "Completed background chats do not retain polling",
    );

    let releaseA: (() => void) | undefined, releaseB: (() => void) | undefined;
    holds.set(
      "first:transcript:a",
      new Promise<void>((resolve) => {
        releaseA = resolve;
      }),
    );
    holds.set(
      "first:transcript:b",
      new Promise<void>((resolve) => {
        releaseB = resolve;
      }),
    );
    documents.set("transcript:a", doc("a", 2));
    documents.set("transcript:b", doc("b", 3));
    await page.evaluate((workspace) => {
      window.prefetchBrowser.jobs = [
        window.prefetchBrowser.client.prefetchTranscript(workspace, "a"),
        window.prefetchBrowser.client.prefetchTranscript(workspace, "b"),
      ];
    }, workspaceId);
    await until(() => running === 1, "One background transfer can run");
    assert.equal(streams.size, 0, "Background prefetch holds no live stream");
    assert.equal(
      await page.evaluate(
        (workspace) =>
          window.prefetchBrowser.client.prefetchTranscript(workspace, "c"),
        workspaceId,
      ),
      false,
    );
    await page.evaluate(async () => {
      window.prefetchBrowser.stops.push(
        window.prefetchBrowser.client.subscribeTranscriptProjection(
          "transcript:a",
          (value) => window.prefetchBrowser.values.push(value),
          () => {},
        ),
      );
    });
    assert.equal(
      running,
      1,
      "Foreground joins the same in-flight chat transfer",
    );
    await until(
      () => streams.size === 1,
      "Foreground opens one transcript stream",
    );
    assert.ok(releaseA);
    releaseA();
    assert.ok(releaseB);
    releaseB();
    assert.deepEqual(
      await page.evaluate(() => Promise.all(window.prefetchBrowser.jobs)),
      [true, false],
    );
    await page.waitForFunction(() => {
      const item = window.prefetchBrowser.values.at(-1)?.items[0];
      return item && "body" in item && item.body === "revision 2";
    });
    await page.evaluate(() =>
      window.prefetchBrowser.stops.splice(0).forEach((stop) => stop()),
    );
    await until(
      () => streams.size === 0,
      "Foreground release also closes its shared socket",
    );
    assert.ok(
      maximum <= 1,
      "Background transport has at most one active request per renderer",
    );

    assert.deepEqual(
      await page.evaluate(() => window.prefetchBrowser.coldReads),
      [],
      "Foreground and background refresh never read unrelated transcripts",
    );
    assert.equal(
      await page.evaluate(
        async () =>
          (await indexedDB.databases()).filter((db) =>
            db.name?.includes("rx-replication-meta"),
          ).length,
      ),
      0,
      "Pull-only chats do not create bidirectional replication metadata",
    );

    const beforePause = pulls.length;
    await page.evaluate(() => {
      Object.defineProperty(document, "hidden", {
        configurable: true,
        value: true,
      });
    });
    assert.equal(
      await page.evaluate(
        (workspace) =>
          window.prefetchBrowser.client.prefetchTranscript(workspace, "a"),
        workspaceId,
      ),
      false,
    );
    await page.evaluate(() => {
      Object.defineProperty(document, "hidden", {
        configurable: true,
        value: false,
      });
      Object.defineProperty(navigator, "onLine", {
        configurable: true,
        value: false,
      });
    });
    assert.equal(
      await page.evaluate(
        (workspace) =>
          window.prefetchBrowser.client.prefetchTranscript(workspace, "a"),
        workspaceId,
      ),
      false,
    );
    assert.equal(
      pulls.length,
      beforePause,
      "Hidden and offline calls do not send requests",
    );
    await page.evaluate(() => {
      Object.defineProperty(navigator, "onLine", {
        configurable: true,
        value: true,
      });
    });
    assert.equal(
      await page.evaluate(async () => {
        try {
          await window.prefetchBrowser.client.prefetchTranscript(
            "c".repeat(32),
            "a",
          );
          return false;
        } catch {
          return true;
        }
      }),
      true,
      "Mismatched workspace is rejected",
    );
    assert.equal(
      pulls.length,
      beforePause,
      "Wrong-workspace requests never reach the server",
    );
    assert.equal(
      await page.evaluate(
        () =>
          window.prefetchBrowser.cache.peekTranscript("c".repeat(32), "a") ===
          undefined,
      ),
      true,
    );

    // A late reply from another tab must not replace a newer persisted document.
    const second = await pageFor("second");
    let releaseOld: (() => void) | undefined;
    documents.set("transcript:race", doc("race", 4));
    holds.set(
      "first:transcript:race",
      new Promise<void>((resolve) => {
        releaseOld = resolve;
      }),
    );
    await page.evaluate((workspace) => {
      window.prefetchBrowser.race =
        window.prefetchBrowser.client.prefetchTranscript(workspace, "race");
    }, workspaceId);
    await until(() => running === 1, "The old reply is held");
    documents.set("transcript:race", doc("race", 5));
    assert.equal(
      await second.evaluate(
        (workspace) =>
          window.prefetchBrowser.client.prefetchTranscript(workspace, "race"),
        workspaceId,
      ),
      true,
      "A second tab refreshes a chat independently of the leader",
    );
    assert.ok(releaseOld);
    releaseOld();
    await page.evaluate(() => window.prefetchBrowser.race);
    assert.equal(
      await page.evaluate(
        (workspace) =>
          window.prefetchBrowser.cache.peekTranscript(workspace, "race")?.seq,
        workspaceId,
      ),
      5,
    );
    assert.equal(
      await second.evaluate(async () => {
        const { db } = await window.prefetchBrowser.client.syncDatabase();
        return (await db.projections.findOne("transcript:race").exec())?.seq;
      }),
      5,
      "Persisted state rejects the late older reply",
    );

    // Force a commit between the pull filter read and RxDB's downstream write.
    let releaseFiltered: (() => void) | undefined;
    documents.set("transcript:filtered", doc("filtered", 8));
    holds.set(
      "first:transcript:filtered",
      new Promise<void>((resolve) => {
        releaseFiltered = resolve;
      }),
    );
    await page.evaluate((workspace) => {
      window.prefetchBrowser.filtered = window.prefetchBrowser.client
        .prefetchTranscript(workspace, "filtered")
        .then((result) => {
          window.prefetchBrowser.filteredResult = result;
          return result;
        });
    }, workspaceId);
    await until(
      () => pulls.some((pull) => pull.scope === "transcript:filtered"),
      `The filtered pull starts; pulls=${JSON.stringify(pulls)}; pageErrors=${JSON.stringify(errors)}`,
    );
    assert.equal(running, 1, "The pre-filter reply is held");
    await page.evaluate(async () => {
      const { db } = await window.prefetchBrowser.client.syncDatabase();
      const storage = db.projections.storageInstance;
      const original = storage.findDocumentsById.bind(storage);
      let held = false;
      window.prefetchBrowser.filterRead = false;
      storage.findDocumentsById = async (...args) => {
        const result = await original(...args);
        if (!held && args[0].includes("transcript:filtered")) {
          held = true;
          window.prefetchBrowser.filterRead = true;
          await new Promise<void>((resolve) => {
            window.prefetchBrowser.releaseFilter = resolve;
          });
          storage.findDocumentsById = original;
        }
        return result;
      };
    });
    assert.ok(releaseFiltered);
    releaseFiltered();
    await page.waitForFunction(
      () => window.prefetchBrowser.filterRead,
      undefined,
      { timeout: 5000 },
    );
    documents.set("transcript:filtered", doc("filtered", 9));
    await second.evaluate(
      (workspace) =>
        window.prefetchBrowser.client.prefetchTranscript(workspace, "filtered"),
      workspaceId,
    );
    await second.evaluate(async () => {
      window.prefetchBrowser.filteredWrites = [];
      const { db } = await window.prefetchBrowser.client.syncDatabase();
      window.prefetchBrowser.stopFilteredWrites = db.projections.$.subscribe(
        (event) => {
          if (event.documentData.id === "transcript:filtered")
            window.prefetchBrowser.filteredWrites.push(event.documentData.seq);
        },
      );
    });
    await page.evaluate(() => window.prefetchBrowser.releaseFilter());
    await page.evaluate(() => window.prefetchBrowser.filtered);
    assert.equal(
      await second.evaluate(async () => {
        const { db } = await window.prefetchBrowser.client.syncDatabase();
        return (await db.projections.findOne("transcript:filtered").exec())
          ?.seq;
      }),
      9,
      "A newer commit after the pull filter still cannot roll back",
    );

    assert.equal(
      await second.evaluate(() =>
        window.prefetchBrowser.filteredWrites.includes(8),
      ),
      false,
      "The late filtered reply never commits even a temporary rollback",
    );
    await second.evaluate(() => {
      const stop = window.prefetchBrowser.stopFilteredWrites;
      if (!stop) throw new Error("Missing filtered write observer");
      stop.unsubscribe();
    });

    // A tombstone can appear after an absent read but before the stale insert.
    await page.evaluate(async () => {
      const { db } = await window.prefetchBrowser.client.syncDatabase();
      const storage = db.projections.storageInstance;
      const original = storage.findDocumentsById.bind(storage);
      window.prefetchBrowser.absentRead = false;
      storage.findDocumentsById = async (...args) => {
        const rows = await original(...args);
        if (args[0].includes("transcript:insert-race")) {
          storage.findDocumentsById = original;
          window.prefetchBrowser.absentRead = rows.length === 0;
          await new Promise<void>((resolve) => {
            window.prefetchBrowser.releaseAbsent = resolve;
          });
        }
        return rows;
      };
      window.prefetchBrowser.staleInsert =
        window.prefetchBrowser.client.persistProjection(db.projections, {
          id: "transcript:insert-race",
          seq: 30,
          payload: "{}",
          _deleted: false,
        });
    });
    await page.waitForFunction(() => window.prefetchBrowser.absentRead);
    await second.evaluate(async () => {
      const { db } = await window.prefetchBrowser.client.syncDatabase();
      await window.prefetchBrowser.client.persistProjection(db.projections, {
        id: "transcript:insert-race",
        seq: 31,
        payload: "{}",
        _deleted: true,
      });
    });
    await page.evaluate(async () => {
      window.prefetchBrowser.releaseAbsent();
      await window.prefetchBrowser.staleInsert;
    });
    assert.deepEqual(
      await second.evaluate(async () => {
        const { db } = await window.prefetchBrowser.client.syncDatabase();
        const [row] = await db.projections.storageInstance.findDocumentsById(
          ["transcript:insert-race"],
          true,
        );
        if (!row) throw new Error("Missing insert race projection");
        return { seq: row.seq, deleted: row._deleted };
      }),
      { seq: 31, deleted: true },
      "A stale insert cannot auto-revive a newer tombstone",
    );

    documents.set("transcript:active", doc("active", 10));
    assert.equal(
      await second.evaluate(
        (workspace) =>
          window.prefetchBrowser.client.prefetchTranscript(workspace, "active"),
        workspaceId,
      ),
      true,
      "The active transcript is persisted before subscribing",
    );
    await second.evaluate(async () => {
      window.prefetchBrowser.activeValues = [];
      window.prefetchBrowser.stopActive =
        window.prefetchBrowser.client.subscribeTranscriptProjection(
          "transcript:active",
          (value) => window.prefetchBrowser.activeValues.push(value),
          () => {},
        );
    });
    await second.waitForFunction(() => {
      const item = window.prefetchBrowser.activeValues.at(-1)?.items[0];
      return item && "body" in item && item.body === "revision 10";
    });
    documents.set("transcript:active", doc("active", 11, true));
    await page.evaluate(
      (workspace) =>
        window.prefetchBrowser.client.prefetchTranscript(workspace, "active"),
      workspaceId,
    );
    await second.waitForFunction(
      () => window.prefetchBrowser.activeValues.at(-1) === null,
    );
    await second.evaluate(() => window.prefetchBrowser.stopActive());

    // Preserve the deleted sequence too, including through a full page reload.
    let releaseBeforeDelete: (() => void) | undefined;
    documents.set("transcript:race", doc("race", 6));
    holds.set(
      "first:transcript:race",
      new Promise<void>((resolve) => {
        releaseBeforeDelete = resolve;
      }),
    );
    await page.evaluate((workspace) => {
      window.prefetchBrowser.race =
        window.prefetchBrowser.client.prefetchTranscript(workspace, "race");
    }, workspaceId);
    await until(() => running === 1, "The pre-delete reply is held");
    documents.set("transcript:race", doc("race", 7, true));
    await second.evaluate(
      (workspace) =>
        window.prefetchBrowser.client.prefetchTranscript(workspace, "race"),
      workspaceId,
    );
    assert.ok(releaseBeforeDelete);
    releaseBeforeDelete();
    await page.evaluate(() => window.prefetchBrowser.race);
    assert.deepEqual(
      await page.evaluate(
        (workspace) =>
          window.prefetchBrowser.cache.peekTranscript(workspace, "race"),
        workspaceId,
      ),
      { payload: null, seq: 7 },
    );
    await page.reload();
    const reloadedTombstone = await page.evaluate(async (workspace) => {
      Object.defineProperty(navigator, "onLine", {
        configurable: true,
        value: false,
      });
      return await new Promise((resolve, reject) => {
        let stop = () => {};
        const timeout = setTimeout(() => {
          stop();
          reject(new Error("Durable transcript hydration did not complete."));
        }, 5000);
        stop = window.prefetchBrowser.client.subscribeTranscriptProjection(
          "transcript:race",
          () => {
            const hydrated = window.prefetchBrowser.cache.peekTranscript(
              workspace,
              "race",
            );
            if (hydrated?.seq !== 7) return;
            clearTimeout(timeout);
            stop();
            resolve(hydrated);
          },
          (error) => {
            clearTimeout(timeout);
            stop();
            reject(error);
          },
        );
        window.prefetchBrowser.stop = stop;
      });
    }, workspaceId);
    assert.deepEqual(
      reloadedTombstone,
      { payload: null, seq: 7 },
      "The subscription emits the durable tombstone after asynchronous hydration",
    );
    assert.deepEqual(
      await page.evaluate(
        (workspace) =>
          window.prefetchBrowser.cache.peekTranscript(workspace, "race"),
        workspaceId,
      ),
      { payload: null, seq: 7 },
      "Reload preserves the tombstone while offline",
    );
    await page.evaluate(() => window.prefetchBrowser.stop());

    assert.equal(
      await second.evaluate(async (workspace) => {
        try {
          await window.prefetchBrowser.client.prefetchTranscript(
            workspace,
            "failure",
          );
          return false;
        } catch {
          return true;
        }
      }, workspaceId),
      true,
      "A background API failure is reported",
    );
    await until(
      () => streams.size === 0,
      "A failed background refresh releases the socket",
    );
    await second.evaluate(async () => {
      const { db } = await window.prefetchBrowser.client.syncDatabase();
      const storage = db.projections.storageInstance;
      const original = storage.findDocumentsById.bind(storage);
      storage.findDocumentsById = async (...args) => {
        if (args[0].includes("transcript:storage-failure")) {
          storage.findDocumentsById = original;
          throw new Error("Fixture read failure");
        }
        return original(...args);
      };
    });
    assert.equal(
      await second.evaluate(async (workspace) => {
        try {
          await window.prefetchBrowser.client.prefetchTranscript(
            workspace,
            "storage-failure",
          );
          return false;
        } catch {
          return true;
        }
      }, workspaceId),
      true,
      "An initial cache read failure is reported",
    );
    await until(
      () => streams.size === 0,
      "Initial cache read failure releases the acquired scope",
    );
    assert.equal(
      await second.evaluate(
        (workspace) =>
          window.prefetchBrowser.client.prefetchTranscript(workspace, "a"),
        workspaceId,
      ),
      true,
      "Failures release their transport slots for later chats",
    );

    // The typed resource coordinator carries transcript invalidations through
    // the live resource stream.
    await second.evaluate(() => {
      window.prefetchBrowser.revisionHints = [];
      window.prefetchBrowser.stopHints =
        window.prefetchBrowser.client.watchSyncInvalidations(
          (version) => window.prefetchBrowser.revisionHints.push(version),
          "transcript:a",
        );
    });
    await until(
      () => streams.size === 1,
      "Transcript invalidations reuse one workspace stream",
    );
    const hint = async (
      workspace: string,
      agentId: string,
      revision: number,
    ) => {
      await second.evaluate(
        async ({ workspace, agentId, revision }) =>
          fetch(
            `/fixture/resource-event?workspace=${encodeURIComponent(workspace)}` +
              `&agent=${encodeURIComponent(agentId)}&revision=${revision}`,
          ),
        { workspace, agentId, revision },
      );
    };
    await hint(workspaceId, "b", 7);
    assert.deepEqual(
      await second.evaluate(() => window.prefetchBrowser.revisionHints),
      [],
      "Unwatched transcript resources are ignored",
    );
    await hint(workspaceId, "a", 8);
    await second.waitForFunction(
      () => window.prefetchBrowser.revisionHints.length === 1,
    );
    assert.equal(
      await second.evaluate(
        () => window.prefetchBrowser.revisionHints.at(-1)?.revision,
      ),
      8,
      "A same-workspace watched transcript invalidation is delivered",
    );
    await second.evaluate(() => window.prefetchBrowser.stopHints());
    await until(
      () => streams.size === 0,
      "Invalidation watcher disposal releases its shared stream",
    );

    // The memory view is bounded; evicted chats remain in IndexedDB.
    await second.evaluate((workspace) => {
      for (let i = 0; i < 64; i++)
        window.prefetchBrowser.cache.cacheTranscriptValue(
          workspace,
          `memory-${i}`,
          { items: [] },
          100 + i,
        );
    }, workspaceId);
    assert.equal(
      await second.evaluate(
        (workspace) =>
          !!window.prefetchBrowser.cache.peekTranscript(workspace, "memory-0"),
        workspaceId,
      ),
      true,
      "Warming a full team retains its first chat for immediate switches",
    );
    await second.evaluate((workspace) => {
      for (let i = 64; i < 320; i++)
        window.prefetchBrowser.cache.cacheTranscriptValue(
          workspace,
          `memory-${i}`,
          { items: [] },
          100 + i,
        );
    }, workspaceId);
    assert.equal(
      await second.evaluate(
        (workspace) =>
          window.prefetchBrowser.cache.peekTranscript(workspace, "memory-0") ===
          undefined,
        workspaceId,
      ),
      true,
    );
    assert.equal(
      await second.evaluate(async () => {
        const { db } = await window.prefetchBrowser.client.syncDatabase();
        return (await db.projections.findOne("transcript:a").exec())?.seq;
      }),
      2,
      "Memory eviction retains persisted chat content",
    );
    assert.deepEqual(errors, []);
    console.log(
      `chat prefetch browser (${process.env.BROWSER || "chromium"}): PASS`,
    );
  } finally {
    for (const stream of streams) stream.end();
    await Promise.all(
      testBrowser
        .contexts()
        .filter((ownedContext) => ownedContext !== runnerContext)
        .map((ownedContext) => ownedContext.close()),
    );
    await server.close();
  }
});
