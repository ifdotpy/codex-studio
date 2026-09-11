// Real RxDB projection persistence and two browser tabs, with an isolated API.
import assert from "node:assert/strict";
import { createRequire } from "node:module";
import { fileURLToPath } from "node:url";
const require = createRequire(new URL("../web/package.json", import.meta.url));
const { chromium, webkit } = require("playwright-core");
const engine = process.env.BROWSER === "webkit" ? webkit : chromium;
const { createServer } = await import(
  new URL("../web/node_modules/vite/dist/node/index.js", import.meta.url)
);
const streams = new Set();
const server = await createServer({
  configFile: false,
  root: fileURLToPath(new URL("../web", import.meta.url)),
  server: { host: "127.0.0.1", port: 0 },
  plugins: [
    {
      name: "prefetch-fixture",
      configureServer(server) {
        server.middlewares.use("/api/sync/stream", (_request, response) => {
          response.setHeader("Content-Type", "text/event-stream");
          response.flushHeaders();
          streams.add(response);
          response.on("close", () => streams.delete(response));
        });
      },
    },
  ],
});
await server.listen();
const browser = await engine.launch({
  ...(engine === chromium
    ? {
        executablePath:
          process.env.CHROME_BIN ||
          "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
      }
    : {}),
  headless: true,
});
const delay = (ms) => new Promise((resolve) => setTimeout(resolve, ms));
const workspaceId = "b".repeat(32);
const documents = new Map();
const pulls = [];
const holds = new Map();
let running = 0,
  maximum = 0;
const doc = (id, seq, deleted = false) => ({
  id: `transcript:${id}`,
  seq,
  payload: JSON.stringify({
    items: [{ id: `message-${seq}`, body: `revision ${seq}` }],
  }),
  _deleted: deleted,
});
async function until(test, message) {
  for (let n = 0; n < 150 && !test(); n++) await delay(20);
  assert.ok(test(), message);
}
try {
  const context = await browser.newContext();
  const errors = [];
  async function pageFor(name) {
    const page = await context.newPage();
    page.on("pageerror", (error) => errors.push(error.message));
    await page.route("**/check", (route) =>
      route.fulfill({ contentType: "text/html", body: "<!doctype html>" }),
    );
    await page.route("**/api/sync/identity", (route) =>
      route.fulfill({ json: { workspaceId } }),
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
          json: { error: "Temporary fixture failure" },
        });
      }
      const current = documents.get(scope);
      const result = {
        workspaceId,
        documents: current && current.seq > after ? [current] : [],
        checkpoint: { seq: current?.seq || after },
      };
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
    await page.goto(
      `http://127.0.0.1:${server.httpServer.address().port}/check`,
    );
    await page.evaluate(async () => {
      window.client = await import("/src/sync/client.ts");
      window.cache = await import("/src/sync/transcriptCache.ts");
      window.stops = [];
      window.values = [];
    });
    return page;
  }
  const page = await pageFor("first");
  documents.set("transcript:a", doc("a", 1));
  assert.equal(
    await page.evaluate(
      (workspace) => client.prefetchTranscript(workspace, "a"),
      workspaceId,
    ),
    true,
  );
  assert.equal(
    await page.evaluate(
      (workspace) => cache.peekTranscript(workspace, "a").seq,
      workspaceId,
    ),
    1,
  );
  await until(
    () => streams.size === 0,
    "Completed background work releases its socket",
  );
  const finishedPulls = pulls.length;
  await delay(3200);
  assert.equal(
    pulls.length,
    finishedPulls,
    "Completed background chats do not retain polling",
  );

  let releaseA, releaseB;
  holds.set(
    "first:transcript:a",
    new Promise((resolve) => {
      releaseA = resolve;
    }),
  );
  holds.set(
    "first:transcript:b",
    new Promise((resolve) => {
      releaseB = resolve;
    }),
  );
  documents.set("transcript:a", doc("a", 2));
  documents.set("transcript:b", doc("b", 3));
  await page.evaluate((workspace) => {
    window.jobs = [
      client.prefetchTranscript(workspace, "a"),
      client.prefetchTranscript(workspace, "b"),
    ];
  }, workspaceId);
  await until(() => running === 2, "Two background transfers can run");
  assert.equal(
    await page.evaluate(
      (workspace) => client.prefetchTranscript(workspace, "c"),
      workspaceId,
    ),
    false,
  );
  await page.evaluate(async () => {
    stops.push(
      await client.watchProjection(
        "transcript:a",
        (value) => values.push(value),
        () => {},
      ),
    );
  });
  assert.equal(running, 2, "Foreground joins the same in-flight chat transfer");
  releaseA();
  releaseB();
  assert.deepEqual(await page.evaluate(() => Promise.all(jobs)), [true, true]);
  await page.waitForFunction(
    () => values.at(-1)?.items[0].body === "revision 2",
  );
  await page.evaluate(() => stops.splice(0).forEach((stop) => stop()));
  await until(
    () => streams.size === 0,
    "Foreground release also closes its shared socket",
  );
  assert.ok(
    maximum <= 2,
    "Background transport has at most two active requests",
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
      (workspace) => client.prefetchTranscript(workspace, "a"),
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
      (workspace) => client.prefetchTranscript(workspace, "a"),
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
        await client.prefetchTranscript("c".repeat(32), "a");
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
      () => cache.peekTranscript("c".repeat(32), "a") === undefined,
    ),
    true,
  );

  // A late reply from another tab must not replace a newer persisted document.
  const second = await pageFor("second");
  let releaseOld;
  documents.set("transcript:race", doc("race", 4));
  holds.set(
    "first:transcript:race",
    new Promise((resolve) => {
      releaseOld = resolve;
    }),
  );
  await page.evaluate((workspace) => {
    window.race = client.prefetchTranscript(workspace, "race");
  }, workspaceId);
  await until(() => running === 1, "The old reply is held");
  documents.set("transcript:race", doc("race", 5));
  assert.equal(
    await second.evaluate(
      (workspace) => client.prefetchTranscript(workspace, "race"),
      workspaceId,
    ),
    true,
    "A second tab refreshes a chat independently of the leader",
  );
  releaseOld();
  await page.evaluate(() => race);
  assert.equal(
    await page.evaluate(
      (workspace) => cache.peekTranscript(workspace, "race").seq,
      workspaceId,
    ),
    5,
  );
  assert.equal(
    await second.evaluate(async () => {
      const { db } = await client.syncDatabase();
      return (await db.projections.findOne("transcript:race").exec()).seq;
    }),
    5,
    "Persisted state rejects the late older reply",
  );

  // Force a commit between the pull filter read and RxDB's downstream write.
  let releaseFiltered;
  documents.set("transcript:filtered", doc("filtered", 8));
  holds.set(
    "first:transcript:filtered",
    new Promise((resolve) => {
      releaseFiltered = resolve;
    }),
  );
  await page.evaluate((workspace) => {
    window.filtered = client.prefetchTranscript(workspace, "filtered");
  }, workspaceId);
  await until(() => running === 1, "The pre-filter reply is held");
  await page.evaluate(async () => {
    const { db } = await client.syncDatabase();
    const storage = db.projections.storageInstance;
    const original = storage.findDocumentsById.bind(storage);
    let held = false;
    window.filterRead = false;
    storage.findDocumentsById = async (...args) => {
      const result = await original(...args);
      if (!held && args[0].includes("transcript:filtered")) {
        held = true;
        window.filterRead = true;
        await new Promise((resolve) => {
          window.releaseFilter = resolve;
        });
        storage.findDocumentsById = original;
      }
      return result;
    };
  });
  releaseFiltered();
  await page.waitForFunction(() => filterRead);
  documents.set("transcript:filtered", doc("filtered", 9));
  await second.evaluate(
    (workspace) => client.prefetchTranscript(workspace, "filtered"),
    workspaceId,
  );
  await second.evaluate(async () => {
    window.filteredWrites = [];
    const { db } = await client.syncDatabase();
    window.stopFilteredWrites = db.projections.$.subscribe((event) => {
      if (event.documentData.id === "transcript:filtered")
        filteredWrites.push(event.documentData.seq);
    });
  });
  await page.evaluate(() => releaseFilter());
  await page.evaluate(() => filtered);
  assert.equal(
    await second.evaluate(async () => {
      const { db } = await client.syncDatabase();
      return (await db.projections.findOne("transcript:filtered").exec()).seq;
    }),
    9,
    "A newer commit after the pull filter still cannot roll back",
  );

  assert.equal(
    await second.evaluate(() => filteredWrites.includes(8)),
    false,
    "The late filtered reply never commits even a temporary rollback",
  );
  await second.evaluate(() => stopFilteredWrites.unsubscribe());

  documents.set("transcript:active", doc("active", 10));
  await second.evaluate(async () => {
    window.activeValues = [];
    window.stopActive = await client.watchProjection(
      "transcript:active",
      (value) => activeValues.push(value),
      () => {},
    );
  });
  await second.waitForFunction(
    () => activeValues.at(-1)?.items[0].body === "revision 10",
  );
  documents.set("transcript:active", doc("active", 11, true));
  await page.evaluate(
    (workspace) => client.prefetchTranscript(workspace, "active"),
    workspaceId,
  );
  await second.waitForFunction(() => activeValues.at(-1) === null);
  await second.evaluate(() => stopActive());

  // Preserve the deleted sequence too, including through a full page reload.
  let releaseBeforeDelete;
  documents.set("transcript:race", doc("race", 6));
  holds.set(
    "first:transcript:race",
    new Promise((resolve) => {
      releaseBeforeDelete = resolve;
    }),
  );
  await page.evaluate((workspace) => {
    window.race = client.prefetchTranscript(workspace, "race");
  }, workspaceId);
  await until(() => running === 1, "The pre-delete reply is held");
  documents.set("transcript:race", doc("race", 7, true));
  await second.evaluate(
    (workspace) => client.prefetchTranscript(workspace, "race"),
    workspaceId,
  );
  releaseBeforeDelete();
  await page.evaluate(() => race);
  assert.deepEqual(
    await page.evaluate(
      (workspace) => cache.peekTranscript(workspace, "race"),
      workspaceId,
    ),
    { payload: null, seq: 7 },
  );
  await page.reload();
  await page.evaluate(async () => {
    window.client = await import("/src/sync/client.ts");
    window.cache = await import("/src/sync/transcriptCache.ts");
    Object.defineProperty(navigator, "onLine", {
      configurable: true,
      value: false,
    });
    window.stop = await client.watchProjection(
      "transcript:race",
      () => {},
      () => {},
    );
  });
  assert.deepEqual(
    await page.evaluate(
      (workspace) => cache.peekTranscript(workspace, "race"),
      workspaceId,
    ),
    { payload: null, seq: 7 },
    "Reload preserves the tombstone while offline",
  );
  await page.evaluate(() => stop());

  assert.equal(
    await second.evaluate(async (workspace) => {
      try {
        await client.prefetchTranscript(workspace, "failure");
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
    const { db } = await client.syncDatabase();
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
        await client.prefetchTranscript(workspace, "storage-failure");
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
      (workspace) => client.prefetchTranscript(workspace, "a"),
      workspaceId,
    ),
    true,
    "Failures release their transport slots for later chats",
  );

  // The memory view is bounded; evicted chats remain in IndexedDB.
  await second.evaluate((workspace) => {
    for (let i = 0; i < 40; i++)
      cache.cacheTranscript(workspace, `memory-${i}`, {
        id: `transcript:memory-${i}`,
        seq: 100 + i,
        payload: "{}",
      });
  }, workspaceId);
  assert.equal(
    await second.evaluate(
      (workspace) => cache.peekTranscript(workspace, "memory-0") === undefined,
      workspaceId,
    ),
    true,
  );
  assert.equal(
    await second.evaluate(async () => {
      const { db } = await client.syncDatabase();
      return (await db.projections.findOne("transcript:a").exec()).seq;
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
  await browser.close();
  await server.close();
}
