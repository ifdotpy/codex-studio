import assert from "node:assert/strict";
import { ENTITY_PROJECTION_CACHE_VERSION } from "../../src/sync/entityCacheStorage.ts";
import { createHash, randomUUID } from "node:crypto";
import { existsSync } from "node:fs";
import { mkdtemp, readFile, readdir } from "node:fs/promises";
import { tmpdir } from "node:os";
import { extname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { createServer } from "node:http";
import { readResourceBaselines } from "../fixtures/resource-baselines.mjs";

import {
  API_SCHEMA_HASH_HEADER,
  apiSchemaHandshakeSse,
  protocol3SseEvent,
  readApiSchemaHash,
  spawnFixture,
  rustUiFixture,
  test,
  expect,
} from "../playwright.mjs";

const leadChat = (page) =>
  page.locator('[data-chat][title="Release lead"]').first();

test.describe("legacy schema mismatch", () => {
  test.use({ apiSchemaFixtureHeaders: false });

  test("a real built renderer uses a new projection cache and preserves drafts", async ({
    browser,
  }) => {
    test.setTimeout(120_000);
    const root = fileURLToPath(new URL("../../../../../../", import.meta.url));
    const oldHash = readApiSchemaHash();
    const nextHash = "f".repeat(64);
    assert.notEqual(nextHash, oldHash);
    const distDirectory =
      process.env.P6_DIST_DIR || join(root, "workspaces/client/apps/web/dist");
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
      createHash("sha256")
        .update(JSON.stringify(distAssetsBefore))
        .digest("hex");
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
    let backendOrigin = "";
    const pageSession = await context.newCDPSession(page);
    await pageSession.send("Network.enable");
    await pageSession.send("Network.setCacheDisabled", { cacheDisabled: true });
    const fixture = spawnFixture(process.execPath, [rustUiFixture, state], {
      stdio: ["pipe", "pipe", "pipe"],
      env: { ...process.env },
    });
    let fixtureLog = "";
    fixture.stderr.on("data", (chunk) => {
      fixtureLog += chunk;
    });
    let activeHash = oldHash;
    let pendingEntitySequence;
    let pendingEntityChanges;
    let newPage;
    let allowPendingChangeEventResolve;
    let allowPendingChangeEventPromise;
    let allowPendingChangeEvent = false;
    let firstPullFails = false;
    let pullFailureCount = 0;
    const rendererHashes = [];
    const apiRequests = [];
    const statePulls = [];
    let validProjectionDatabaseNames = [];
    const entityChangePages = new WeakSet();
    const confirmedResources = new Set();
    const transcriptRows = [];
    const errors = [];
    const streamFrames = new Map();
    const openStreams = new Set();
    // Inline baselines are persisted asynchronously. A fulfilled finite body
    // immediately disconnects and cancels those writes, unlike a real stream.
    const streamServer = createServer((request, response) => {
      const frame = streamFrames.get(request.url);
      streamFrames.delete(request.url);
      if (!frame) {
        response.writeHead(404).end();
        return;
      }
      response.writeHead(200, {
        "content-type": "text/event-stream",
        "cache-control": "no-cache",
        [API_SCHEMA_HASH_HEADER]: frame.hash,
      });
      response.write(frame.body);
      openStreams.add(response);
      response.on("close", () => openStreams.delete(response));
    });
    await new Promise((resolve) =>
      streamServer.listen(0, "127.0.0.1", resolve),
    );
    const streamOrigin = `http://127.0.0.1:${streamServer.address().port}`;
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
          query: Object.fromEntries(requestUrl.searchParams),
        });
      }
      if (requestUrl.pathname === "/api/sync/stream") {
        const streamHash =
          rendererHash || requestUrl.searchParams.get("apiSchema") || "";
        const resources = JSON.parse(
          requestUrl.searchParams.get("resources") || "[]",
        );
        const initialResources = resources.filter((resource) => {
          // Detail can be cancelled when selection changes before its first
          // frame is persisted; every reconnect must replay that view's baseline.
          if (resource.kind === "agentDetail") return true;
          const key = `${streamHash}:${JSON.stringify(resource)}`;
          if (confirmedResources.has(key)) return false;
          confirmedResources.add(key);
          return true;
        });
        let body = "";
        let workspaceId;
        if (initialResources.length > 0) {
          const identity = await fetch(
            new URL("/api/sync/identity", backendOrigin),
          );
          ({ workspaceId } = await identity.json());
          const details = initialResources.filter(
            (resource) => resource.kind === "agentDetail",
          );
          const detailVersions = await readResourceBaselines(
            backendOrigin,
            details,
          );
          const resourceVersions = initialResources.map(
            (resource) =>
              detailVersions.find(
                (entry) =>
                  entry.resource?.kind === resource.kind &&
                  entry.resource?.agentId === resource.agentId,
              ) || { resource, revision: 1 },
          );
          body = protocol3SseEvent("resources", {
            protocol: 4,
            workspaceId,
            epoch: "schema-cache-test",
            revision: 1,
            reason: "initial",
            resources: initialResources,
            resourceVersions,
          });
        }
        if (
          streamHash === nextHash &&
          newPage &&
          route.request().frame().page() === newPage &&
          resources.some((resource) => resource.kind === "state") &&
          pendingEntitySequence !== undefined &&
          pendingEntityChanges !== undefined
        ) {
          const entityChanges = pendingEntityChanges;
          const entitySequence = pendingEntitySequence;
          if (!allowPendingChangeEvent) await allowPendingChangeEventPromise;
          if (pendingEntitySequence === entitySequence) {
            if (!workspaceId) {
              const identity = await fetch(
                new URL("/api/sync/identity", backendOrigin),
              );
              ({ workspaceId } = await identity.json());
            }
            body += protocol3SseEvent("resources", {
              protocol: 4,
              workspaceId,
              epoch: "schema-cache-test",
              revision: 2,
              reason: "change",
              resources: [{ kind: "state" }],
              resourceVersions: [
                {
                  resource: { kind: "state" },
                  revision: entityChanges.through,
                  entityChanges,
                },
              ],
            });
            entityChangePages.add(newPage);
          }
        }
        const streamPath = `/stream/${randomUUID()}`;
        streamFrames.set(streamPath, {
          hash: activeHash,
          body: apiSchemaHandshakeSse(body, { hash: activeHash }),
        });
        return route.continue({ url: `${streamOrigin}${streamPath}` });
      }
      if (
        requestUrl.pathname === "/api/sync/pull" &&
        requestUrl.searchParams.get("reset") === "1" &&
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
        if (requestHeaders.origin) requestHeaders.origin = backendOrigin;
        requestHeaders.referer = backendOrigin + "/";
        const response = await route.fetch({
          url: backendOrigin + requestUrl.pathname + requestUrl.search,
          headers: requestHeaders,
        });
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
          body = Buffer.from(
            body.toString("utf8").replaceAll(oldHash, nextHash),
          );
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
      backendOrigin = origin;
      appOrigin = streamOrigin;
      await page.goto(appOrigin);
      await expect(page.locator(".startup")).toBeVisible({ timeout: 10_000 });
      await expect(leadChat(page)).toBeVisible({ timeout: 60_000 });
      await leadChat(page).click();
      await expect(page.locator("#message")).toBeVisible({ timeout: 60_000 });
      await page
        .locator("#message")
        .fill("Draft kept across the schema update");
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
      await expect(leadChat(page)).toBeVisible({ timeout: 30_000 });
      await leadChat(page).click();
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
      const oldBuildNames = [
        `rxdb-dexie-studio-entity-projection-${workspaceId}-${oldHash}--${ENTITY_PROJECTION_CACHE_VERSION}--projections`,
        `rxdb-dexie-studio-entity-projection-${workspaceId}-${oldHash}--0--_rxdb_internal`,
        `rxdb-dexie-studio-entity-projection-${workspaceId}-${oldHash}--0--rx-replication-meta-${"a".repeat(64)}`,
      ];
      const reclaimableHash = "c".repeat(64);
      const staleNames = [
        `rxdb-dexie-studio-entity-projection-${workspaceId}-${reclaimableHash}--0--projections`,
        `rxdb-dexie-studio-entity-projection-${workspaceId}-${reclaimableHash}--0--_rxdb_internal`,
        `rxdb-dexie-studio-entity-projection-${workspaceId}-${reclaimableHash}--0--rx-replication-meta-${"a".repeat(64)}`,
      ];
      const legacyName = `rxdb-dexie-studio${workspaceId}--0--projections`;
      await page.evaluate(
        async ({ oldBuildNames, staleNames, legacyName }) => {
          const open = (name) =>
            new Promise((resolve, reject) => {
              const request = indexedDB.open(name);
              request.onsuccess = () => {
                request.result.close();
                resolve();
              };
              request.onerror = () => reject(request.error);
            });
          await Promise.all(
            [...oldBuildNames, ...staleNames, legacyName].map(open),
          );
        },
        { oldBuildNames, staleNames, legacyName },
      );

      activeHash = nextHash;
      for (const stream of openStreams)
        stream.write(apiSchemaHandshakeSse("", { hash: activeHash }));
      await expect(
        page.locator('[data-modal-content="true"]').filter({
          hasText: "Studio has been updated. Update this tab",
        }),
      ).toBeVisible({ timeout: 20_000 });
      const oldPage = page;
      newPage = await context.newPage();
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
      await newPageSession.send("Page.enable");
      await newPageSession.send("Network.setCacheDisabled", {
        cacheDisabled: true,
      });
      const newPageResetPull = newPage.waitForResponse((response) => {
        const url = new URL(response.url());
        return (
          url.pathname === "/api/sync/pull" &&
          url.searchParams.get("scope") === "state:entities:v1" &&
          url.searchParams.get("reset") === "1" &&
          url.searchParams.get("after") === "0" &&
          response.status() === 200
        );
      });
      await newPage.goto(appOrigin);
      await newPageResetPull;
      await expect(newPage.locator(".startup")).toBeVisible({
        timeout: 10_000,
      });
      await expect(leadChat(newPage)).toBeVisible({ timeout: 60_000 });
      await leadChat(newPage).click();
      await expect(newPage.locator("#message")).toBeVisible({
        timeout: 60_000,
      });
      await expect(newPage.locator("#message")).toHaveValue(
        "Draft kept across the schema update",
      );
      validProjectionDatabaseNames = await newPage.evaluate(
        async ({ workspaceId, schemaHash, cacheVersion }) =>
          (await indexedDB.databases())
            .map(({ name }) => name)
            .filter(
              (name) =>
                name?.includes(
                  `entity-projection-${workspaceId}-${schemaHash}`,
                ) && name.endsWith(`--${cacheVersion}--projections`),
            ),
        {
          workspaceId,
          schemaHash: nextHash,
          cacheVersion: ENTITY_PROJECTION_CACHE_VERSION,
        },
      );
      assert.ok(
        validProjectionDatabaseNames.length > 0,
        "the valid new-build tab has a populated projection cache",
      );
      const readNewPageStateCheckpoint = () =>
        newPage.evaluate(async (names) => {
          const name = names.find((databaseName) =>
            databaseName.endsWith("--projections"),
          );
          if (!name) return {};
          const db = await new Promise((resolve, reject) => {
            const request = indexedDB.open(name);
            request.onsuccess = () => resolve(request.result);
            request.onerror = () => reject(request.error);
          });
          const read = (id) =>
            new Promise((resolve, reject) => {
              const request = db
                .transaction("docs", "readonly")
                .objectStore("docs")
                .get(id);
              request.onsuccess = () => resolve(request.result);
              request.onerror = () => reject(request.error);
            });
          const [ready, checkpoint, complete] = await Promise.all([
            read("state:entities:ready"),
            read("state:entities:checkpoint"),
            read("state:entities:complete"),
          ]);
          db.close();
          return {
            ready: ready?.payload,
            checkpoint: checkpoint?.seq,
            complete: complete?.payload === "complete",
          };
        }, validProjectionDatabaseNames);
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
        await newPage.evaluate(async (names) => {
          const existing = new Set(
            (await indexedDB.databases()).map(({ name }) => name),
          );
          return names.every((name) => existing.has(name));
        }, oldBuildNames),
        "cleanup preserves the stale-hash database held by the old-build tab",
      );
      // Before the projection cache moved to its own database (2026-10-07), the
      // workspace database held a projections collection. Its pull checkpoint
      // and ready marker were rows of that collection, and the only RxDB
      // replication of that database was for drafts. So deleting it gives an
      // old build an empty cache that pulls in full. No current build opens it,
      // and an old build cannot sync with this server (see the old-build tab
      // below). Cleanup deletes it and keeps the drafts and outbox.
      await expect
        .poll(() =>
          newPage.evaluate(
            async (name) =>
              (await indexedDB.databases()).some(
                (database) => database.name === name,
              ),
            legacyName,
          ),
        )
        .toBe(false);
      assert.ok(
        await newPage.evaluate(
          async (names) => {
            const existing = new Set(
              (await indexedDB.databases()).map(({ name }) => name),
            );
            return names.every((name) => existing.has(name));
          },
          [
            `rxdb-dexie-studio${workspaceId}--0--drafts`,
            `rxdb-dexie-studio${workspaceId}--0--outbox`,
          ],
        ),
        "cleanup keeps the drafts and outbox of the workspace database",
      );

      // Reopen the old build after the server hash changes. It is now rejected;
      // its startup cleanup must not remove the live new-build cache.
      await oldPage.reload();
      await expect(
        oldPage.locator('[data-modal-content="true"]').filter({
          hasText: "Studio has been updated. Update this tab",
        }),
      ).toBeVisible({ timeout: 20_000 });
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
      await expect
        .poll(readNewPageStateCheckpoint)
        .toMatchObject({ ready: "ready", complete: true });
      const newPageStateCheckpoint = await readNewPageStateCheckpoint();
      assert.ok(
        Number.isSafeInteger(newPageStateCheckpoint.checkpoint) &&
          newPageStateCheckpoint.checkpoint > 0,
        `the valid tab has a durable nonzero checkpoint: ${JSON.stringify(newPageStateCheckpoint)}`,
      );

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
      const entityChangeDocuments = [];
      let entityChangeCursor = newPageStateCheckpoint.checkpoint;
      let mutationEntity;
      while (!mutationEntity) {
        const pageResponse = await fetch(
          `${origin}/api/sync/pull?scope=state%3Aentities%3Av1&after=${entityChangeCursor}&limit=500`,
          {
            headers: {
              "X-Canvas-Token": token,
              [API_SCHEMA_HASH_HEADER]: oldHash,
            },
          },
        );
        assert.equal(pageResponse.status, 200);
        const entityPage = await pageResponse.json();
        assert.notEqual(entityPage.reset, true);
        entityChangeDocuments.push(...entityPage.documents);
        assert.ok(entityPage.checkpoint.seq > entityChangeCursor);
        entityChangeCursor = entityPage.checkpoint.seq;
        mutationEntity = entityChangeDocuments.find(
          (document) =>
            document.op === "upsert" &&
            document.collection === "agentSummary" &&
            document.id === createdId,
        );
      }
      assert.ok(Number.isSafeInteger(Number(mutationEntity.seq)));
      assert.ok(Number(mutationEntity.seq) > newPageStateCheckpoint.checkpoint);
      pendingEntitySequence = Number(mutationEntity.seq);
      const orderedEntityChangeDocuments = entityChangeDocuments.filter(
        (document) => Number(document.seq) <= pendingEntitySequence,
      );
      pendingEntityChanges = {
        after: newPageStateCheckpoint.checkpoint,
        through: pendingEntitySequence,
        documents: orderedEntityChangeDocuments,
      };
      allowPendingChangeEvent = false;
      allowPendingChangeEventPromise = new Promise((resolve) => {
        allowPendingChangeEventResolve = resolve;
      });
      await newPage.reload();
      await expect(leadChat(newPage)).toBeVisible();
      await expect(newPage.locator("#message")).toBeVisible();
      await expect
        .poll(readNewPageStateCheckpoint)
        .toMatchObject({ ready: "ready", complete: true });
      // Prove the reload kept the checkpoint while its entity-change stream
      // response is still held. Once released, the client may apply it before
      // the next Playwright poll runs.
      await expect
        .poll(readNewPageStateCheckpoint)
        .toMatchObject(newPageStateCheckpoint);
      allowPendingChangeEvent = true;
      allowPendingChangeEventResolve?.();
      allowPendingChangeEventResolve = undefined;
      // Other source owners may publish summaries after the mutation's agent
      // row. The applied checkpoint must cover that row and may advance further.
      await expect
        .poll(async () => (await readNewPageStateCheckpoint()).checkpoint)
        .toBeGreaterThanOrEqual(Number(mutationEntity.seq));
      pendingEntitySequence = undefined;
      pendingEntityChanges = undefined;
      assert.ok(
        entityChangePages.has(newPage),
        "the valid tab applied the fixture's entity change event",
      );
      assert.deepEqual(
        errors,
        [],
        "the valid tab applies the entity change without console errors",
      );
      await expect(newPage.locator(`[data-chat="${createdId}"]`)).toBeVisible({
        timeout: 30_000,
      });

      const resetBootstrapPull = statePulls.find(
        (pull) => pull.page === newPage && pull.query.reset === "1",
      );
      console.log("Schema variant observed:", {
        sameHashCachedReloadMs: Math.round(cachedReloadMs),
        rendererHashes: [...new Set(rendererHashes)],
        resetBootstrapQuery: resetBootstrapPull?.query,
        distAssetsSha256: distAssetsDigest(),
      });
      assert.ok(resetBootstrapPull, "the new schema starts a reset bootstrap");
      assert.equal(
        resetBootstrapPull.after,
        "0",
        "the new schema cache's main-client bootstrap starts at zero",
      );
    } finally {
      fixture.kill();
      await context.close();
      streamServer.closeAllConnections();
      await new Promise((resolve) => streamServer.close(resolve));
      const distAssetsAfter = await hashAssets();
      assert.deepEqual(
        distAssetsAfter,
        distAssetsBefore,
        "workspaces/client/apps/web/dist/assets remains byte-identical; the spec only serves rewritten response bodies",
      );
      console.log(
        "workspaces/client/apps/web/dist/assets unchanged sha256:",
        distAssetsDigest(),
      );
    }
  });
});

test("different build collection sets share only drafts and outbox", async ({
  browser,
}) => {
  // This cache belongs to the retired build, whose broad agent projection
  // included answers. Keep its size independent of today's narrowed server view.
  const legacyCacheRows = 256;
  const workspaceId = randomUUID().replaceAll("-", "");
  const legacyRows = Array.from({ length: legacyCacheRows }, (_, index) => ({
    id: `entity:agent:legacy-${index}`,
    seq: index + 1,
    payload: JSON.stringify({
      collection: "agent",
      id: `legacy-${index}`,
      value: {
        id: `legacy-${index}`,
        name: `Cached chat ${index}`,
        lastAnswer: "Saved report from the previous build. ".repeat(32),
      },
    }),
    _deleted: false,
  }));
  const { createServer } = await import(
    new URL("../../node_modules/vite/dist/node/index.js", import.meta.url)
  );
  const server = await createServer({
    configFile: false,
    root: fileURLToPath(new URL("../../", import.meta.url)),
    server: {
      host: "127.0.0.1",
      port: 0,
      fs: {
        allow: [
          fileURLToPath(new URL("../../", import.meta.url)),
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
      entityRows: legacyRows.length,
    });
  } finally {
    await context.close();
    await server.close();
  }
});
