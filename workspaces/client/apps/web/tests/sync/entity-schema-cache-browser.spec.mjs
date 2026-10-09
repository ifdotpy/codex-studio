import assert from "node:assert/strict";
import { createHash, randomUUID } from "node:crypto";
import { existsSync } from "node:fs";
import { mkdtemp, readFile, readdir } from "node:fs/promises";
import { tmpdir } from "node:os";
import { extname, join } from "node:path";
import { fileURLToPath } from "node:url";

import {
  API_SCHEMA_HASH_HEADER,
  apiSchemaHandshakeSse,
  protocol3SseEvent,
  readApiSchemaHash,
  spawnFixture,
  test,
  expect,
} from "../playwright.mjs";

test("a real built renderer uses a new projection cache and preserves drafts", async ({
  browser,
}) => {
  test.setTimeout(120_000);
  const root = fileURLToPath(new URL("../../../", import.meta.url));
  const oldHash = readApiSchemaHash();
  const nextHash = "f".repeat(64);
  assert.notEqual(nextHash, oldHash);
  const distDirectory = process.env.P6_DIST_DIR || join(root, "web/dist");
  const assetDirectory = join(distDirectory, "assets");
  const assetNames = await readdir(assetDirectory);
  const hashAssets = async () =>
    Object.fromEntries(
      await Promise.all(
        (await readdir(assetDirectory)).sort().map(async (name) => [
          name,
          createHash("sha256")
            .update(await readFile(join(assetDirectory, name)))
            .digest("hex"),
        ]),
      ),
    );
  const distAssetsBefore = await hashAssets();
  const distAssetsDigest = () =>
    createHash("sha256").update(JSON.stringify(distAssetsBefore)).digest("hex");
  assert.ok(
    assetNames.some((name) => name.endsWith(".js")),
    "built app assets are present",
  );
  const state = await mkdtemp(join(tmpdir(), "studio-real-schema-cache-"));
  const context = await browser.newContext({ serviceWorkers: "block" });
  const page = await context.newPage();
  const variantPages = new WeakSet();
  const oldBuildPages = new WeakSet([page]);
  let appOrigin = "";
  const pageSession = await context.newCDPSession(page);
  await pageSession.send("Network.enable");
  await pageSession.send("Network.setCacheDisabled", { cacheDisabled: true });
  const fixture = spawnFixture(
    process.env.PYTHON_BIN || "python3",
    ["-B", join(root, "tests/simple-ui-fixture.py"), state],
    { stdio: ["pipe", "pipe", "pipe"], env: { ...process.env } },
  );
  let fixtureLog = "";
  fixture.stderr.on("data", (chunk) => {
    fixtureLog += chunk;
  });
  let activeHash = oldHash;
  let pendingEntitySequence;
  let firstPullFails = false;
  let pullFailureCount = 0;
  const rendererHashes = [];
  const apiRequests = [];
  const statePulls = [];
  const confirmedResources = new Set();
  const transcriptRows = [];
  const errors = [];
  await context.route("**/api/**", async (route) => {
    const requestUrl = new URL(route.request().url());
    apiRequests.push(
      `${route.request().method()} ${requestUrl.pathname}${requestUrl.search}`,
    );
    const rendererHash = route.request().headers()[
      API_SCHEMA_HASH_HEADER.toLowerCase()
    ];
    if (rendererHash) rendererHashes.push(rendererHash);
    if (
      oldBuildPages.has(route.request().frame().page()) &&
      rendererHash === oldHash &&
      activeHash !== oldHash
    )
      return route.fulfill({
        status: 426,
        headers: { [API_SCHEMA_HASH_HEADER]: activeHash },
        json: { error: "Renderer schema mismatch" },
      });
    if (
      requestUrl.pathname === "/api/sync/pull" &&
      requestUrl.searchParams.get("scope") === "state:entities:v1"
    ) {
      statePulls.push({
        page: route.request().frame().page(),
        after: requestUrl.searchParams.get("after"),
      });
    }
    if (requestUrl.pathname === "/api/sync/stream") {
      const streamHash =
        rendererHash || requestUrl.searchParams.get("apiSchema") || "";
      const resources = JSON.parse(
        requestUrl.searchParams.get("resources") || "[]",
      );
      const initialResources = resources.filter((resource) => {
        const key = `${streamHash}:${JSON.stringify(resource)}`;
        if (confirmedResources.has(key)) return false;
        confirmedResources.add(key);
        return true;
      });
      let body = "";
      if (initialResources.length > 0) {
        const identity = await fetch(new URL("/api/sync/identity", requestUrl));
        const { workspaceId } = await identity.json();
        body = protocol3SseEvent("resources", {
          protocol: 3,
          workspaceId,
          epoch: "schema-cache-test",
          revision: 1,
          reason: "initial",
          resources: initialResources,
        });
        if (streamHash === nextHash && pendingEntitySequence !== undefined) {
          body += protocol3SseEvent("resources", {
            protocol: 3,
            workspaceId,
            epoch: "schema-cache-test",
            revision: 2,
            reason: "change",
            resources: [{ kind: "state" }],
            resourceVersions: [
              {
                resource: { kind: "state" },
                revision: 2,
                entitySequences: [pendingEntitySequence],
              },
            ],
          });
          pendingEntitySequence = undefined;
        }
      }
      return route.fulfill({
        status: 200,
        headers: { "content-type": "text/event-stream" },
        body: apiSchemaHandshakeSse(body, { hash: activeHash }),
      });
    }
    if (
      requestUrl.pathname === "/api/sync/pull" &&
      requestUrl.searchParams.get("after") === "0" &&
      firstPullFails
    ) {
      firstPullFails = false;
      pullFailureCount++;
      await new Promise((resolve) => setTimeout(resolve, 400));
      return route.fulfill({
        status: 503,
        headers: { [API_SCHEMA_HASH_HEADER]: activeHash },
        json: { error: "temporary first pull failure" },
      });
    }
    try {
      const requestHeaders = { ...route.request().headers() };
      requestHeaders[API_SCHEMA_HASH_HEADER.toLowerCase()] = oldHash;
      const response = await route.fetch({ headers: requestHeaders });
      if (
        requestUrl.pathname === "/api/sync/pull" &&
        requestUrl.searchParams.get("scope")?.startsWith("transcript:")
      ) {
        const body = await response.body();
        const transcriptPage = JSON.parse(body.toString("utf8"));
        transcriptRows.push(...(transcriptPage.documents || []));
      }
      const headers = { ...response.headers() };
      headers[API_SCHEMA_HASH_HEADER] = activeHash;
      return route.fulfill({ response, headers });
    } catch (error) {
      if (fixture.exitCode !== null)
        throw new Error(`Fixture exited: ${fixtureLog}`);
      return route.fulfill({
        status: 503,
        headers: { [API_SCHEMA_HASH_HEADER]: activeHash },
        json: { error: `Fixture request failed: ${String(error)}` },
      });
    }
  });
  await context.route(
    (url) =>
      !!appOrigin &&
      url.origin === appOrigin &&
      !url.pathname.startsWith("/api/"),
    async (route) => {
      const url = new URL(route.request().url());
      const requestedPath =
        url.pathname === "/"
          ? join(distDirectory, "index.html")
          : join(distDirectory, decodeURIComponent(url.pathname.slice(1)));
      const filePath = existsSync(requestedPath)
        ? requestedPath
        : join(distDirectory, "index.html");
      const extension = extname(filePath);
      const contentType =
        extension === ".js"
          ? "text/javascript"
          : extension === ".css"
            ? "text/css"
            : extension === ".html"
              ? "text/html"
              : extension === ".svg"
                ? "image/svg+xml"
                : "application/octet-stream";
      let body = await readFile(filePath);
      if (
        extension === ".js" &&
        variantPages.has(route.request().frame().page())
      )
        body = Buffer.from(body.toString("utf8").replaceAll(oldHash, nextHash));
      return route.fulfill({
        status: 200,
        contentType,
        headers: { "cache-control": "no-store" },
        body,
      });
    },
  );
  let port;
  try {
    port = await new Promise((resolve, reject) => {
      fixture.stdout.once("data", (chunk) =>
        resolve(Number(String(chunk).trim())),
      );
      fixture.once("exit", () => reject(new Error(fixtureLog)));
    });
    const origin = `http://127.0.0.1:${port}`;
    appOrigin = origin;
    await page.goto(origin);
    await expect(page.locator(".startup")).toBeVisible({ timeout: 10_000 });
    await expect(
      page.getByRole("button", { name: /^Release lead/ }).first(),
    ).toBeVisible({ timeout: 60_000 });
    await page
      .getByRole("button", { name: /^Release lead/ })
      .first()
      .click();
    await expect(page.locator("#message")).toBeVisible({ timeout: 60_000 });
    await page.locator("#message").fill("Draft kept across the schema update");
    await expect
      .poll(() =>
        page.evaluate(() =>
          Object.keys(localStorage).some((key) =>
            localStorage
              .getItem(key)
              ?.includes("Draft kept across the schema update"),
          ),
        ),
      )
      .toBe(true);

    const statePullCount = () =>
      apiRequests.filter(
        (request) =>
          request.includes("/api/sync/pull?") &&
          request.includes("scope=state%3Aentities%3Av1"),
      ).length;
    const initialStatePullCount = statePullCount();
    await page.reload();
    await expect(
      page.getByRole("button", { name: /^Release lead/ }).first(),
    ).toBeVisible({ timeout: 30_000 });
    await page
      .getByRole("button", { name: /^Release lead/ })
      .first()
      .click();
    await expect(page.locator("#message")).toHaveValue(
      "Draft kept across the schema update",
    );
    const cachedReloadMs = await page.evaluate(() => performance.now());
    console.log(
      "Same-hash cached reload startup ms:",
      Math.round(cachedReloadMs),
    );
    assert.equal(
      statePullCount(),
      initialStatePullCount,
      "same-hash reload keeps the checkpoint and does not pull entities",
    );

    const { workspaceId } = await fetch(`${origin}/api/sync/identity`).then(
      (response) => response.json(),
    );
    const cachedRows = [];
    let after = 0;
    let fresh = true;
    for (;;) {
      const pullUrl = new URL("/api/sync/pull", origin);
      pullUrl.searchParams.set("scope", "state:entities:v1");
      pullUrl.searchParams.set("after", String(after));
      pullUrl.searchParams.set("limit", "500");
      if (fresh) pullUrl.searchParams.set("fresh", "1");
      const pull = await fetch(pullUrl).then((response) => response.json());
      if (pull.reset) {
        after = 0;
        fresh = true;
        continue;
      }
      fresh = false;
      cachedRows.push(...pull.documents);
      if (pull.checkpoint.seq <= after || pull.checkpoint.seq >= pull.maxSeq)
        break;
      after = pull.checkpoint.seq;
    }
    console.log("Legacy projection fixture estimate:", {
      rows: cachedRows.length + transcriptRows.length,
      bytes: new TextEncoder().encode(
        JSON.stringify([...cachedRows, ...transcriptRows]),
      ).length,
      transcripts: transcriptRows.length,
    });
    const staleNames = [
      `rxdb-dexie-studio-entity-projection-${workspaceId}-${oldHash}--0--projections`,
      `rxdb-dexie-studio-entity-projection-${workspaceId}-${oldHash}--0--_rxdb_internal`,
      `rxdb-dexie-studio-entity-projection-${workspaceId}-${oldHash}--0--rx-replication-meta-${"a".repeat(64)}`,
    ];
    const legacyName = `rxdb-dexie-studio${workspaceId}--0--projections`;
    await page.evaluate(
      async ({ staleNames, legacyName }) => {
        const open = (name) =>
          new Promise((resolve, reject) => {
            const request = indexedDB.open(name);
            request.onsuccess = () => {
              request.result.close();
              resolve();
            };
            request.onerror = () => reject(request.error);
          });
        await Promise.all([...staleNames, legacyName].map(open));
      },
      { staleNames, legacyName },
    );

    activeHash = nextHash;
    await expect(
      page.locator('[data-modal-content="true"]').filter({
        hasText: "Studio has been updated. Update this tab",
      }),
    ).toBeVisible({ timeout: 20_000 });
    const oldPage = page;
    const newPage = await context.newPage();
    variantPages.add(newPage);
    newPage.on("pageerror", (error) => errors.push(error.message));
    newPage.on("console", (message) => {
      if (
        message.type() === "error" &&
        !message.text().includes("503 (Service Unavailable)")
      )
        errors.push(message.text());
    });
    firstPullFails = true;
    const newPageSession = await context.newCDPSession(newPage);
    await newPageSession.send("Network.enable");
    await newPageSession.send("Network.setCacheDisabled", {
      cacheDisabled: true,
    });
    await newPage.goto(origin);
    await expect(newPage.locator(".startup")).toBeVisible({ timeout: 10_000 });
    await expect(
      newPage.getByRole("button", { name: /^Release lead/ }).first(),
    ).toBeVisible({ timeout: 60_000 });
    await newPage
      .getByRole("button", { name: /^Release lead/ })
      .first()
      .click();
    await expect(newPage.locator("#message")).toBeVisible({ timeout: 60_000 });
    await expect(newPage.locator("#message")).toHaveValue(
      "Draft kept across the schema update",
    );
    const validProjectionDatabaseNames = await newPage.evaluate(
      async ({ workspaceId, schemaHash }) =>
        (await indexedDB.databases())
          .map(({ name }) => name)
          .filter(
            (name) =>
              name?.includes(
                `entity-projection-${workspaceId}-${schemaHash}`,
              ) && name.endsWith("--0--projections"),
          ),
      { workspaceId, schemaHash: nextHash },
    );
    assert.ok(
      validProjectionDatabaseNames.length > 0,
      "the valid new-build tab has a populated projection cache",
    );
    assert.deepEqual(
      errors,
      [],
      "the first full pull renders without console errors",
    );
    assert.equal(
      pullFailureCount,
      1,
      "the reset's first pull failed and retried",
    );
    await expect
      .poll(() =>
        newPage.evaluate(async (names) => {
          const existing = new Set(
            (await indexedDB.databases()).map(({ name }) => name),
          );
          return names.filter((name) => existing.has(name));
        }, staleNames),
      )
      .toEqual([]);
    assert.ok(
      await newPage.evaluate(
        async (name) =>
          (await indexedDB.databases()).some(
            (database) => database.name === name,
          ),
        legacyName,
      ),
      "the legacy stable projections collection is preserved",
    );

    // Reopen the old build after the server hash changes. It is now rejected;
    // its startup cleanup must not remove the live new-build cache.
    await oldPage.reload();
    await expect(
      oldPage.locator('[data-modal-content="true"]').filter({
        hasText: "Studio has been updated. Update this tab",
      }),
    ).toBeVisible({ timeout: 20_000 });
    await oldPage.waitForTimeout(1_500);
    await expect
      .poll(() =>
        newPage.evaluate(async (expected) => {
          const names = new Set(
            (await indexedDB.databases()).map(({ name }) => name),
          );
          return expected.filter((name) => names.has(name));
        }, validProjectionDatabaseNames),
      )
      .toEqual(validProjectionDatabaseNames);

    const { token } = await fetch(`${origin}/api/session`).then((r) =>
      r.json(),
    );
    const createdId = randomUUID();
    const mutation = await fetch(`${origin}/api/leads`, {
      method: "POST",
      headers: {
        "Content-Type": "application/json",
        "X-Canvas-Token": token,
        Origin: origin,
        [API_SCHEMA_HASH_HEADER]: oldHash,
      },
      body: JSON.stringify({ id: createdId, cwd: "/" }),
    });
    assert.equal(mutation.status, 200);
    const mutationBody = await mutation.json();
    const mutationEntity = mutationBody._syncEntities?.find((row) =>
      row.id.startsWith("entity:"),
    );
    assert.ok(mutationEntity, "fixture mutation returns a sequenced entity");
    const pullsBeforeMutationEvent = statePulls.filter(
      (pull) => pull.page === newPage,
    ).length;
    pendingEntitySequence = mutationEntity.seq;
    confirmedResources.clear();
    await newPage.reload();
    await expect
      .poll(() => statePulls.filter((pull) => pull.page === newPage).length)
      .toBeGreaterThan(pullsBeforeMutationEvent);
    const checkpointPulls = statePulls.filter((pull) => pull.page === newPage);
    assert.ok(
      checkpointPulls.some((pull) => Number(pull.after) > 0),
      "the valid tab pulls the fixture change from its retained checkpoint",
    );
    await expect(newPage.locator(`[data-chat="${createdId}"]`)).toBeVisible({
      timeout: 30_000,
    });

    const pulls = await newPage.evaluate(() =>
      performance
        .getEntriesByType("resource")
        .filter((entry) => entry.name.includes("/api/sync/pull"))
        .map((entry) => new URL(entry.name))
        .filter((url) => url.searchParams.get("scope") === "state:entities:v1")
        .map((url) => url.searchParams.get("after")),
    );
    console.log("Schema variant observed:", {
      sameHashCachedReloadMs: Math.round(cachedReloadMs),
      rendererHashes: [...new Set(rendererHashes)],
      pulls,
      distAssetsSha256: distAssetsDigest(),
    });
    assert.ok(pulls.length > 0);
    assert.equal(
      statePulls.find((pull) => pull.page === newPage)?.after,
      "0",
      "the new schema cache's first pull starts at zero",
    );
  } finally {
    fixture.kill();
    await context.close();
    const distAssetsAfter = await hashAssets();
    assert.deepEqual(
      distAssetsAfter,
      distAssetsBefore,
      "web/dist/assets remains byte-identical; the spec only serves rewritten response bodies",
    );
    console.log("web/dist/assets unchanged sha256:", distAssetsDigest());
  }
});

test("different build collection sets share only drafts and outbox", async ({
  browser,
}) => {
  const root = fileURLToPath(new URL("../../../", import.meta.url));
  const state = await mkdtemp(join(tmpdir(), "studio-legacy-projection-size-"));
  const fixture = spawnFixture(
    process.env.PYTHON_BIN || "python3",
    ["-B", join(root, "tests/simple-ui-fixture.py"), state],
    { stdio: ["pipe", "pipe", "pipe"], env: { ...process.env } },
  );
  let fixtureLog = "";
  fixture.stderr.on("data", (chunk) => {
    fixtureLog += chunk;
  });
  const fixturePort = await new Promise((resolve, reject) => {
    fixture.stdout.once("data", (chunk) =>
      resolve(Number(String(chunk).trim())),
    );
    fixture.once("exit", () => reject(new Error(fixtureLog)));
  });
  const fixtureOrigin = `http://127.0.0.1:${fixturePort}`;
  const { workspaceId } = await fetch(
    `${fixtureOrigin}/api/sync/identity`,
  ).then((response) => response.json());
  const stateUrl = new URL("/api/sync/pull", fixtureOrigin);
  stateUrl.searchParams.set("scope", "state:entities:v1");
  stateUrl.searchParams.set("after", "0");
  stateUrl.searchParams.set("limit", "500");
  stateUrl.searchParams.set("fresh", "1");
  const statePull = await fetch(stateUrl).then((response) => response.json());
  const lead = statePull.documents
    .map(({ payload }) => JSON.parse(payload))
    .find(
      (row) => row.collection === "agent" && row.value.name === "Release lead",
    );
  const legacyRows = statePull.documents.filter(({ _deleted }) => !_deleted);
  let legacyTranscriptRows = 0;
  if (lead) {
    const transcriptUrl = new URL("/api/sync/pull", fixtureOrigin);
    transcriptUrl.searchParams.set("scope", `transcript:${lead.id}`);
    transcriptUrl.searchParams.set("after", "0");
    transcriptUrl.searchParams.set("limit", "100");
    transcriptUrl.searchParams.set("fresh", "1");
    const transcript = await fetch(transcriptUrl).then((response) =>
      response.json(),
    );
    const liveTranscriptRows = (transcript.documents || []).filter(
      ({ _deleted }) => !_deleted,
    );
    legacyTranscriptRows = liveTranscriptRows.length;
    legacyRows.push(...liveTranscriptRows);
  }
  assert.ok(
    legacyRows.length > 200,
    "fixture has a representative legacy cache",
  );
  const { createServer } = await import(
    new URL(
      "../../../web/node_modules/vite/dist/node/index.js",
      import.meta.url,
    )
  );
  const server = await createServer({
    configFile: false,
    root: fileURLToPath(new URL("../../../web", import.meta.url)),
    server: {
      host: "127.0.0.1",
      port: 0,
      fs: {
        allow: [
          fileURLToPath(new URL("../../../web", import.meta.url)),
          fileURLToPath(new URL("../../..", import.meta.url)),
        ],
      },
    },
  });
  server.middlewares.stack.unshift({
    route: "",
    handle(request, response, next) {
      if (
        new URL(request.url || "/", "http://localhost").pathname !==
        "/sync-check"
      )
        return next();
      response.setHeader("Content-Type", "text/html");
      response.end("<!doctype html><title>Cache collection sets</title>");
    },
  });
  await server.listen();
  const context = await browser.newContext();
  const oldPage = await context.newPage();
  const newPage = await context.newPage();
  const fixtureModule = `/@fs${fileURLToPath(new URL("./fixtures/schemaCacheBrowserFixture.ts", import.meta.url))}`;
  const databaseName = `studio${workspaceId}`;
  const origin = `http://127.0.0.1:${server.httpServer.address().port}`;
  try {
    await Promise.all([
      oldPage.goto(`${origin}/sync-check`),
      newPage.goto(`${origin}/sync-check`),
    ]);
    await oldPage.evaluate(
      async ({ name, fixtureModule, rows }) => {
        const fixture = await import(fixtureModule);
        const db = await fixture.openBuildDatabase(name, true);
        await db.projections.bulkInsert(rows);
        await db.drafts.insert({
          id: "draft-from-old",
          payload: "draft",
          seq: 1,
        });
        await db.outbox.insert({
          id: "message-from-old",
          payload: "queued",
          seq: 1,
        });
        window.oldBuildDatabase = db;
      },
      { name: databaseName, fixtureModule, rows: legacyRows },
    );
    await newPage.evaluate(
      async ({ name, fixtureModule }) => {
        const fixture = await import(fixtureModule);
        const db = await fixture.openBuildDatabase(name, false);
        if (db.collections.projections)
          throw new Error(
            "The new stable database opened the legacy projection collection",
          );
        const draft = await db.drafts.findOne("draft-from-old").exec();
        const message = await db.outbox.findOne("message-from-old").exec();
        if (draft?.payload !== "draft" || message?.payload !== "queued")
          throw new Error("The new build could not read old user data");
        await db.drafts.insert({
          id: "draft-from-new",
          payload: "new draft",
          seq: 2,
        });
        await db.outbox.insert({
          id: "message-from-new",
          payload: "new queued",
          seq: 2,
        });
        const projections = await fixture.openProjectionDatabase(
          `${name}-entities-hash-a`,
        );
        await projections.projections.insert({
          id: "entity:new-build",
          payload: "new-shape",
          seq: 1,
        });
        const otherHash = await fixture.openProjectionDatabase(
          `${name}-entities-hash-b`,
        );
        if (await otherHash.projections.findOne("entity:new-build").exec())
          throw new Error("Different build caches were shared");
        window.newBuildDatabase = db;
      },
      { name: databaseName, fixtureModule },
    );
    const oldData = await oldPage.evaluate(
      async (legacyRowId) => ({
        draft: (
          await window.oldBuildDatabase.drafts.findOne("draft-from-new").exec()
        )?.payload,
        message: (
          await window.oldBuildDatabase.outbox
            .findOne("message-from-new")
            .exec()
        )?.payload,
        projection: (
          await window.oldBuildDatabase.projections.findOne(legacyRowId).exec()
        )?.payload,
        legacyProjectionStats: await (async () => {
          const rows = await window.oldBuildDatabase.projections.find().exec();
          const serialized = rows.map((row) => row.toJSON());
          return {
            rows: serialized.length,
            bytes: new TextEncoder().encode(JSON.stringify(serialized)).length,
          };
        })(),
      }),
      legacyRows[0].id,
    );
    const { legacyProjectionStats, ...sharedData } = oldData;
    assert.deepEqual(sharedData, {
      draft: "new draft",
      message: "new queued",
      projection: legacyRows[0].payload,
    });
    assert.equal(legacyProjectionStats.rows, legacyRows.length);
    assert.ok(legacyProjectionStats.bytes > 200_000);
    assert.ok(legacyProjectionStats.rows > 200);
    console.log("Legacy projections fixture:", {
      ...legacyProjectionStats,
      entityRows: legacyRows.length - legacyTranscriptRows,
      transcriptRows: legacyTranscriptRows,
    });
  } finally {
    await context.close();
    await server.close();
    fixture.kill();
  }
});
