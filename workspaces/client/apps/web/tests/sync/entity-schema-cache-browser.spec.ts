import type { Page } from "@playwright/test";
import type { ServerResponse } from "node:http";
import type { components } from "../../src/generated/api.ts";
import type { GetResult } from "../../src/apiTypes.ts";
import { fixtureResources, fixturePostBody } from "./helpers.ts";
type ResourceRevisionEntry = components["schemas"]["ResourceRevisionEntry"];
type EntityChangeBatch = components["schemas"]["EntityChangeBatch"];
type ResourceCursor = components["schemas"]["SubscriptionCursor"];
import assert from "node:assert/strict";
import { createHash, randomUUID } from "node:crypto";
import { existsSync } from "node:fs";
import { mkdtemp, readFile, readdir } from "node:fs/promises";
import { tmpdir } from "node:os";
import { extname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { createServer } from "node:http";
import { readResourceBaselines } from "../fixtures/resource-baselines.ts";

import {
  waitForFixturePort,
  API_SCHEMA_HASH_HEADER,
  apiSchemaHandshakeSse,
  protocol3SseEvent,
  readApiSchemaHash,
  spawnFixture,
  rustUiFixture,
  readCacheKeys,
  readCachedRows,
  remoteOrigin,
  test,
  expect,
} from "../playwright.ts";

const leadChat = (page: Page) =>
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
    fixture.stderr!.on("data", (chunk) => {
      fixtureLog += chunk;
    });
    let activeHash = oldHash;
    let pendingEntitySequence: number | undefined;
    let pendingEntityChanges: EntityChangeBatch | undefined;
    let newPage: Page | undefined;
    let allowPendingChangeEventResolve: (() => void) | undefined;
    let allowPendingChangeEventPromise: Promise<void> | undefined;
    const pendingChangeStreamOpened = Promise.withResolvers<
      ResourceCursor | undefined
    >();
    let firstPullFails = false;
    let pullFailureCount = 0;
    const resetPullResponseHeld = Promise.withResolvers<void>();
    const allowResetPullResponse = Promise.withResolvers<void>();
    const rendererHashes: string[] = [];
    const apiRequests: string[] = [];
    const streamDiagnostics: unknown[] = [];
    page.on("console", (message) => {
      if (message.type() === "error")
        streamDiagnostics.push({ error: message.text() });
    });
    const statePulls: {
      page: Page;
      after: string | null;
      query: Record<string, string>;
    }[] = [];
    let validCacheKeys: string[] = [];
    const entityChangePages = new WeakSet();
    const confirmedResources = new Set();
    const transcriptRows: components["schemas"]["SyncPullResponse"]["documents"] =
      [];
    const errors: string[] = [];
    type PendingChange = {
      body: string;
      sequence: number;
      page: Page;
      cursor: ResourceCursor | undefined;
      release: Promise<void>;
    };
    const streamFrames = new Map<
      string,
      {
        hash: string;
        sessionId: string;
        body: string;
        pendingChange: PendingChange | undefined;
      }
    >();
    const openStreams = new Set<ServerResponse>();
    const sessionStreams = new Map<string, ServerResponse>();
    // Inline baselines are persisted asynchronously. A fulfilled finite body
    // immediately disconnects and cancels those writes, unlike a real stream.
    const streamServer = createServer((request, response) => {
      const frame = streamFrames.get(request.url!);
      streamFrames.delete(request.url!);
      if (!frame) {
        response.writeHead(404).end();
        return;
      }
      response.writeHead(200, {
        "content-type": "text/event-stream",
        "cache-control": "no-cache",
        [API_SCHEMA_HASH_HEADER]: frame.hash,
      });
      sessionStreams.set(frame.sessionId, response);
      response.write(
        apiSchemaHandshakeSse(
          protocol3SseEvent("session", {
            sessionId: frame.sessionId,
          } satisfies components["schemas"]["SyncSessionEvent"]) + frame.body,
          { hash: frame.hash },
        ),
      );
      openStreams.add(response);
      response.on("close", () => {
        openStreams.delete(response);
        sessionStreams.delete(frame.sessionId);
      });
      if (frame.pendingChange) {
        const { body, sequence, page, release } = frame.pendingChange;
        pendingChangeStreamOpened.resolve(frame.pendingChange.cursor);
        void release.then(() => {
          if (
            !response.destroyed &&
            !response.writableEnded &&
            pendingEntitySequence === sequence
          ) {
            response.write(body);
            entityChangePages.add(page);
          }
        });
      }
    });
    await new Promise<void>((resolve) =>
      streamServer.listen(0, "127.0.0.1", resolve),
    );
    const streamAddress =
      streamServer.address() as import("node:net").AddressInfo;

    const streamOrigin = `http://127.0.0.1:${streamAddress!.port}`;
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
          json: {
            error: "Renderer schema mismatch",
          } satisfies components["schemas"]["ErrorResponse"],
        });
      if (
        requestUrl.pathname === "/api/sync/pull" &&
        requestUrl.searchParams.get("scope") === "state:entities:v1"
      ) {
        streamDiagnostics.push({
          pull: requestUrl.search,
          page: oldBuildPages.has(route.request().frame().page())
            ? "old"
            : "new",
        });
        statePulls.push({
          page: route.request().frame().page(),
          after: requestUrl.searchParams.get("after"),
          query: Object.fromEntries(requestUrl.searchParams),
        });
      }
      if (requestUrl.pathname === "/api/sync/subscriptions") {
        // Keep subscription changes on the same stream, as the server does.
        // Reconnecting here would cancel the baseline this test is measuring.
        const change = fixturePostBody<"/api/sync/subscriptions">(
          route.request().postDataJSON(),
        );
        const stream = sessionStreams.get(change.sessionId);
        assert.ok(stream, "subscription update has a live fixture session");
        const resources = change.add.map(({ resource }) => resource);
        const typed = resources.filter(
          (resource) =>
            resource.kind === "agentDetail" || resource.kind === "state",
        );
        const baselines = await readResourceBaselines(backendOrigin, typed);
        const { workspaceId }: GetResult<"/api/sync/identity"> = await fetch(
          new URL("/api/sync/identity", backendOrigin),
        ).then((response) => response.json());
        stream.write(
          protocol3SseEvent("subscriptions", {
            gen: change.gen,
          } satisfies components["schemas"]["SyncSubscriptionAck"]),
        );
        if (resources.length)
          stream.write(
            protocol3SseEvent("resources", {
              protocol: 4,
              workspaceId: workspaceId!,
              epoch: "schema-cache-test",
              revision: 1,
              reason: "initial",
              resources,
              resourceVersions: resources.map(
                (resource) =>
                  baselines.find((entry) =>
                    fixtureResourceMatches(entry.resource, resource),
                  ) || { resource, revision: 1 },
              ),
            } satisfies components["schemas"]["ResourceChangeEvent"]),
          );
        return route.fulfill({
          status: 200,
          headers: { [API_SCHEMA_HASH_HEADER]: activeHash },
          json: { ok: true },
        });
      }
      if (requestUrl.pathname === "/api/sync/stream") {
        const streamHash =
          rendererHash || requestUrl.searchParams.get("apiSchema") || "";
        const resources = fixtureResources(
          requestUrl.searchParams.get("resources"),
        );
        const since = fixtureCursors(requestUrl.searchParams.get("since"));
        const stateCursor = since.find(
          ({ resource }) => resource.kind === "state",
        )?.since;
        streamDiagnostics.push({
          resources,
          since: requestUrl.searchParams.get("since"),
        });
        const initialResources = resources.filter((resource) => {
          // Detail can be cancelled when selection changes before its first
          // frame is persisted; every reconnect must replay that view's baseline.
          if (resource.kind === "agentDetail") return true;
          // Every connection needs a state baseline. A cached cursor proves
          // an empty applied delta for either build; only a new-hash tab
          // without a cursor intentionally receives a recovery hint.
          if (resource.kind === "state") return true;
          const key = `${streamHash}:${JSON.stringify(resource)}`;
          if (confirmedResources.has(key)) return false;
          confirmedResources.add(key);
          return true;
        });
        let body = "";
        let workspaceId: string | undefined;
        if (initialResources.length > 0) {
          if (stateCursor) workspaceId = stateCursor.workspaceId;
          else {
            const identity = await fetch(
              new URL("/api/sync/identity", backendOrigin),
            );
            const value: GetResult<"/api/sync/identity"> =
              await identity.json();
            workspaceId = value.workspaceId;
          }

          const details = initialResources.filter(
            (resource) =>
              resource.kind === "agentDetail" ||
              (resource.kind === "state" &&
                streamHash === oldHash &&
                !stateCursor),
          );
          const detailVersions = await readResourceBaselines(
            backendOrigin,
            details,
          );
          const resourceVersions: ResourceRevisionEntry[] =
            initialResources.map(
              (resource) =>
                // This phase models an unchanged source across a same-build
                // reload. Echo its durable cursor as an empty applied delta;
                // later phases explicitly inject source changes and recovery.
                (resource.kind === "state" && stateCursor
                  ? {
                      resource,
                      revision: stateCursor.seq,
                      cursor: stateCursor,
                      replyKind: "delta",
                    }
                  : undefined) ||
                detailVersions.find((entry) =>
                  fixtureResourceMatches(entry.resource, resource),
                ) || { resource, revision: 1 },
            );
          streamDiagnostics.push({
            baselines: resourceVersions.map(
              ({ resource, revision, cursor, replyKind }) => ({
                resource,
                revision,
                cursor,
                replyKind,
              }),
            ),
          });
          body = protocol3SseEvent("resources", {
            protocol: 4,
            workspaceId: workspaceId!,
            epoch: "schema-cache-test",
            revision: 1,
            reason: "initial",
            resources: initialResources,
            resourceVersions,
          } satisfies components["schemas"]["ResourceChangeEvent"]);
        }
        let pendingChange: PendingChange | undefined;
        if (
          streamHash === nextHash &&
          newPage &&
          route.request().frame().page() === newPage &&
          resources.some((resource) => resource.kind === "state") &&
          pendingEntitySequence !== undefined &&
          pendingEntityChanges !== undefined
        ) {
          const entityChanges = pendingEntityChanges;

          const changeBody = protocol3SseEvent("resources", {
            protocol: 4,
            workspaceId: workspaceId!,
            epoch: "schema-cache-test",
            revision: 2,
            reason: "change",
            resources: [{ kind: "state" }],
            resourceVersions: [
              {
                resource: { kind: "state" },
                revision: entityChanges.through,
                cursor: {
                  workspaceId: workspaceId!,
                  seq: entityChanges.through,
                },
                replyKind: "delta",
                entityChanges,
              },
            ],
          } satisfies components["schemas"]["ResourceChangeEvent"]);
          pendingChange = {
            body: changeBody,
            sequence: pendingEntitySequence,
            page: newPage,
            cursor: stateCursor,
            release: allowPendingChangeEventPromise!,
          };
        }
        const streamPath = `/stream/${randomUUID()}`;
        streamFrames.set(streamPath, {
          hash: activeHash,
          sessionId: randomUUID(),
          body,
          pendingChange,
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
          json: {
            error: "temporary first pull failure",
          } satisfies components["schemas"]["ErrorResponse"],
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
          route.request().frame().page() === newPage &&
          requestUrl.pathname === "/api/sync/pull" &&
          requestUrl.searchParams.get("scope") === "state:entities:v1" &&
          requestUrl.searchParams.get("reset") === "1" &&
          requestUrl.searchParams.get("after") === "0" &&
          response.status() === 200
        ) {
          resetPullResponseHeld.resolve();
          await allowResetPullResponse.promise;
        }
        if (
          requestUrl.pathname === "/api/sync/pull" &&
          requestUrl.searchParams.get("scope")?.startsWith("transcript:")
        ) {
          const body = await response.body();
          const transcriptPage: components["schemas"]["SyncPullResponse"] =
            JSON.parse(body.toString("utf8"));
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
          json: {
            error: `Fixture request failed: ${String(error)}`,
          } satisfies components["schemas"]["ErrorResponse"],
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
      port = await waitForFixturePort(fixture, {
        diagnostics: () => fixtureLog,
      });
      const origin = `http://127.0.0.1:${port}`;
      backendOrigin = origin;
      // A remote host name: only a remote frontend keeps the thin cache.
      appOrigin = remoteOrigin(streamOrigin);
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
      assert.equal(
        initialStatePullCount,
        0,
        "the applied SSE baseline needs no entity pull",
      );
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
      const cachedRows: components["schemas"]["SyncPullResponse"]["documents"] =
        [];
      let after = 0;
      let fresh = true;
      for (;;) {
        const pullUrl = new URL("/api/sync/pull", origin);
        pullUrl.searchParams.set("scope", "state:entities:v1");
        pullUrl.searchParams.set("after", String(after));
        pullUrl.searchParams.set("limit", "500");
        if (fresh) pullUrl.searchParams.set("fresh", "1");
        const pull: components["schemas"]["SyncPullResponse"] = await fetch(
          pullUrl,
        ).then((response) => response.json());
        if (pull.reset) {
          after = 0;
          fresh = true;
          continue;
        }
        fresh = false;
        cachedRows.push(...pull.documents);
        if (pull.checkpoint.seq <= after || pull.checkpoint.seq >= pull.maxSeq!)
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
      // A record of a schema hash that no tab uses. The valid tab removes it
      // after its first matching response. The old-build tab's own records
      // are removed too: the cache keeps only the current schema hash.
      const reclaimableHash = "c".repeat(64);
      const staleKeys = [`${workspaceId}|${reclaimableHash}|entities`];
      await page.evaluate(async (keys) => {
        const db = await new Promise<IDBDatabase>((resolve, reject) => {
          const request = indexedDB.open("studio-thin-cache", 1);
          request.onupgradeneeded = () => {
            request.result.createObjectStore("cache");
            request.result.createObjectStore("unsent");
          };
          request.onsuccess = () => resolve(request.result);
          request.onerror = () => reject(request.error);
        });
        const transaction = db.transaction("cache", "readwrite");
        for (const key of keys)
          transaction.objectStore("cache").put({ rows: [] }, key);
        await new Promise<void>((resolve, reject) => {
          transaction.oncomplete = () => resolve();
          transaction.onerror = () => reject(transaction.error);
        });
        db.close();
      }, staleKeys);

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
      await newPage.goto(appOrigin);
      await resetPullResponseHeld.promise;
      // Prove startup while the successful retry is held, before its reset
      // baseline can commit and remove this transient screen.
      await expect(newPage.locator(".startup")).toBeVisible({
        timeout: 10_000,
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
      allowResetPullResponse.resolve();
      await newPageResetPull;
      await expect(leadChat(newPage)).toBeVisible({ timeout: 60_000 });
      await leadChat(newPage).click();
      await expect(newPage.locator("#message")).toBeVisible({
        timeout: 60_000,
      });
      await expect(newPage.locator("#message")).toHaveValue(
        "Draft kept across the schema update",
      );
      await expect
        .poll(async () =>
          (await readCacheKeys(newPage!)).filter((key) =>
            key.startsWith(`${workspaceId}|${nextHash}|`),
          ),
        )
        .not.toEqual([]);
      validCacheKeys = (await readCacheKeys(newPage)).filter((key) =>
        key.startsWith(`${workspaceId}|${nextHash}|`),
      );
      const readNewPageStateCheckpoint = async () => {
        const rows = await readCachedRows(newPage!, "entities");
        const row = (id: string) => rows.find((entry) => entry.id === id);
        return {
          ready: row("state:entities:ready")?.payload,
          checkpoint: row("state:entities:checkpoint")?.seq,
          complete: row("state:entities:complete")?.payload === "complete",
        };
      };
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
        .poll(async () =>
          (await readCacheKeys(newPage!)).filter(
            (key) =>
              key.startsWith(`${workspaceId}|`) &&
              !key.startsWith(`${workspaceId}|${nextHash}|`),
          ),
        )
        .toEqual([]);

      // Reopen the old build after the server hash changes. It is now rejected;
      // its startup cleanup must not remove the live new-build cache.
      await oldPage.reload();
      await expect(
        oldPage.locator('[data-modal-content="true"]').filter({
          hasText: "Studio has been updated. Update this tab",
        }),
      ).toBeVisible({ timeout: 20_000 });
      await expect
        .poll(async () => {
          const keys = new Set(await readCacheKeys(newPage!));
          return validCacheKeys.filter((key) => !keys.has(key));
        })
        .toEqual([]);
      await expect
        .poll(readNewPageStateCheckpoint)
        .toMatchObject({ ready: "ready", complete: true });
      const newPageStateCheckpoint = await readNewPageStateCheckpoint();
      const beforeMutationPullCount = statePullCount();
      assert.ok(
        Number.isSafeInteger(newPageStateCheckpoint.checkpoint) &&
          newPageStateCheckpoint.checkpoint! > 0,
        `the valid tab has a durable nonzero checkpoint: ${JSON.stringify(newPageStateCheckpoint)}`,
      );

      const { token }: GetResult<"/api/session"> = await fetch(
        `${origin}/api/session`,
      ).then((r) => r.json());
      const createdId = randomUUID();
      const mutation = await fetch(`${origin}/api/leads`, {
        method: "POST",
        headers: {
          "Content-Type": "application/json",
          "X-Canvas-Token": token,
          Origin: origin,
          [API_SCHEMA_HASH_HEADER]: oldHash,
        },
        body: JSON.stringify({
          id: createdId,
          cwd: "/",
        } satisfies import("../../src/apiTypes.ts").PostBody<"/api/leads">),
      });
      assert.equal(mutation.status, 200);
      const entityChangeDocuments: EntityChangeBatch["documents"] = [];
      let entityChangeCursor = newPageStateCheckpoint.checkpoint!;
      let mutationEntity: EntityChangeBatch["documents"][number] | undefined;
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
        const entityPage: Omit<
          components["schemas"]["SyncPullResponse"],
          "documents"
        > & { documents: EntityChangeBatch["documents"] } =
          await pageResponse.json();
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
      assert.ok(
        Number(mutationEntity.seq) > newPageStateCheckpoint.checkpoint!,
      );
      pendingEntitySequence = Number(mutationEntity.seq);

      const orderedEntityChangeDocuments = entityChangeDocuments.filter(
        (document) => Number(document.seq) <= pendingEntitySequence!,
      );
      pendingEntityChanges = {
        after: newPageStateCheckpoint.checkpoint!,
        through: pendingEntitySequence,
        documents: orderedEntityChangeDocuments,
      };
      allowPendingChangeEventPromise = new Promise<void>((resolve) => {
        allowPendingChangeEventResolve = resolve;
      });
      await newPage.reload();
      const mutationReconnectCursor = await pendingChangeStreamOpened.promise;
      assert.deepEqual(mutationReconnectCursor, {
        workspaceId: workspaceId!,
        seq: newPageStateCheckpoint.checkpoint,
      });
      await expect(leadChat(newPage)).toBeVisible();
      await expect(newPage.locator("#message")).toBeVisible();
      await expect
        .poll(readNewPageStateCheckpoint)
        .toMatchObject({ ready: "ready", complete: true });
      // The handshake and applied baseline are live; hold only the mutation
      // payload so no alternate recovery pull can stand in for its delivery.
      await expect
        .poll(readNewPageStateCheckpoint)
        .toMatchObject(newPageStateCheckpoint);
      assert.equal(
        statePullCount(),
        beforeMutationPullCount,
        "the cached reconnect receives its applied baseline without an entity pull while the mutation is held",
      );
      await expect(newPage.locator(`[data-chat="${createdId}"]`)).toHaveCount(
        0,
      );
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
      assert.equal(
        statePullCount(),
        beforeMutationPullCount,
        "the released SSE mutation advances the cache without an entity pull",
      );

      const resetBootstrapPull = statePulls.find(
        (pull) => pull.page === newPage && pull.query.reset === "1",
      );
      console.log("Schema variant observed:", {
        sameHashCachedReloadMs: Math.round(cachedReloadMs),
        rendererHashes: [...new Set(rendererHashes)],
        resetBootstrapQuery: resetBootstrapPull?.query,
        mutationReconnectCursor,
        mutationSequence: Number(mutationEntity.seq),
        mutationPhasePulls: statePullCount() - beforeMutationPullCount,
        distAssetsSha256: distAssetsDigest(),
      });
      assert.ok(resetBootstrapPull, "the new schema starts a reset bootstrap");
      assert.equal(
        resetBootstrapPull.after,
        "0",
        "the new schema cache's main-client bootstrap starts at zero",
      );
    } catch (error) {
      console.log(
        "Schema cache stream diagnostics:",
        JSON.stringify(streamDiagnostics),
      );
      throw error;
    } finally {
      allowResetPullResponse.resolve();
      allowPendingChangeEventResolve?.();
      await context.unrouteAll({ behavior: "wait" });
      await context.close();
      fixture.kill();
      streamServer.closeAllConnections();
      await new Promise<void>((resolve) => streamServer.close(() => resolve()));
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
