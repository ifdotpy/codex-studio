import type { components } from "../../src/generated/api.ts";
import type {
  SyncIdentityResponse,
  SyncPullResponse,
  SyncEntity,
} from "../playwright.ts";
import type {} from "./mobile-sync.fixture.ts";
import { createFixtureViteServer as createServer } from "../playwright.ts";
import { fixtureServerOrigin } from "./browser-fixture.ts";
import type { ServerResponse } from "node:http";
import type { ResourceRef } from "../playwright.ts";

// Real RxDB and EventSource, with an isolated server and simulated PWA lifecycle.
import assert from "node:assert/strict";
import { fileURLToPath } from "node:url";
import {
  API_SCHEMA_HASH_HEADER,
  apiSchemaHandshakeSse,
  protocol3SseEvent,
  readApiSchemaHash,
  test,
  expect,
} from "../playwright.ts";

test("mobile sync browser", async ({ page }) => {
  const streams = new Set<ServerResponse>();
  const streamResources = new Map<ServerResponse, ResourceRef[]>();
  let opened = 0;
  let revision = 1;
  const workspaceId = "e".repeat(32);
  const streamScopes: string[] = [];
  const server = await createServer({
    configFile: false,
    root: fileURLToPath(new URL("../../", import.meta.url)),
    server: { host: "127.0.0.1", port: 0 },
    plugins: [
      {
        name: "sync-stream-fixture",
        configureServer(server) {
          server.middlewares.use("/api/sync/stream", (_request, response) => {
            response.setHeader("Content-Type", "text/event-stream");
            response.setHeader("Cache-Control", "no-cache");
            response.write(apiSchemaHandshakeSse());
            const query = new URL(_request.url ?? "", "http://localhost")
              .searchParams;
            const resources: ResourceRef[] = JSON.parse(
              query.get("resources") || "[]",
            );
            response.write(
              protocol3SseEvent("resources", {
                protocol: 4,
                workspaceId,
                epoch: "fixture-epoch",
                revision,
                reason: "change",
                resources,
                resourceVersions: resources.map((resource) => ({
                  resource,
                  revision,
                })),
              }),
            );
            opened++;
            streamResources.set(response, resources);
            streamScopes.push(
              new URL(_request.url ?? "", "http://localhost").searchParams.get(
                "scope",
              ) || "legacy",
            );
            streams.add(response);
            response.on("close", () => {
              streams.delete(response);
              streamResources.delete(response);
            });
          });
        },
      },
    ],
  });
  await server.listen();
  const delay = (ms: number) =>
    new Promise((resolve) => setTimeout(resolve, ms));
  async function until(test: () => boolean, message: string) {
    for (let count = 0; count < 100; count++) {
      if (test()) return;
      await delay(30);
    }
    assert.ok(test(), message);
  }
  try {
    const errors: string[] = [];
    page.on("pageerror", (error) => errors.push(error.message));
    let identities = 0;
    const pulls: (string | null)[] = [];
    const pullCursors: { scope: string | null; after: number }[] = [];
    await page.route("**/check", (route) =>
      route.fulfill({
        contentType: "text/html",
        body: '<!doctype html><script type="module" src="/tests/mobile/mobile-sync.fixture.ts"></script>',
      }),
    );
    await page.route("**/api/sync/identity", (route) => {
      identities++;
      return route.fulfill({
        // Legacy fixture omits the protocol field; preserved for baseline comparison.
        json: { workspaceId } satisfies Pick<
          SyncIdentityResponse,
          "workspaceId"
        >,
        headers: { [API_SCHEMA_HASH_HEADER]: readApiSchemaHash() },
      });
    });
    await page.route("**/api/sync/pull?*", (route) => {
      const url = new URL(route.request().url());
      const scope = url.searchParams.get("scope");
      assert.ok(scope);
      const after = Number(url.searchParams.get("after"));
      pulls.push(scope);
      pullCursors.push({ scope, after });
      const entityScope = scope === "state:entities:v1";
      return route.fulfill({
        json: {
          workspaceId,
          documents:
            scope !== "drafts" && after < revision
              ? [
                  {
                    id: entityScope ? "entity:agent:lead" : scope,
                    seq: revision,
                    payload: JSON.stringify(
                      entityScope
                        ? {
                            collection: "agent",
                            id: "lead",
                            value: {
                              id: "lead",
                              name: `Agent ${revision}`,
                              status: "running",
                              source: "managed",
                            },
                          }
                        : { revision },
                    ),
                  },
                ]
              : [],
          checkpoint: { seq: revision },
          ...(entityScope
            ? {
                initialHigh: Number(url.searchParams.get("initialHigh") || 0),
                maxSeq: revision,
              }
            : {}),
        } satisfies Omit<SyncPullResponse, "generation" | "documents"> & {
          documents: (
            | Omit<components["schemas"]["SyncDocument"], "_deleted">
            | SyncEntity
          )[];
        },
      });
    });
    await page.route("**/api/sync/drafts", (route) =>
      route.fulfill({ json: [] }),
    );
    await page.goto(fixtureServerOrigin(server) + "check");
    await page.evaluate(async () => {
      const client = await import("/src/sync/client.ts");
      window.mobileSync.values = {};
      window.mobileSync.stops = ["state", "team", "history:lead", "state"].map(
        (scope) => {
          const accept = (value: unknown) => {
            window.mobileSync.values[scope] = value;
          };
          return scope === "state"
            ? client.subscribeStateProjection(
                (value) => {
                  window.mobileSync.values.state = value;
                },
                () => {},
              )
            : client.subscribeTranscriptProjection(
                `transcript:${scope}`,
                accept,
                () => {},
              );
        },
      );
      window.mobileSync.stops.push(
        await client.startDraftReplication(() => {}),
      );
      window.mobileSync.resumeCalls = 0;
      window.mobileSync.stopResume = (
        await import("/src/sync/resume.ts")
      ).onResume(() => window.mobileSync.resumeCalls++);
    });
    await page.waitForFunction(
      () =>
        Object.values(window.mobileSync.values).length === 3 &&
        window.mobileSync.values.state?.runtime?.agents?.[0]?.name ===
          "Agent 1" &&
        window.mobileSync.revision("team") === 1 &&
        window.mobileSync.revision("history:lead") === 1,
    );
    await until(
      () => streams.size === 1,
      `The shared sync coordinator opens one stream, opened=${opened} scopes=${streamScopes.join(",")} pulls=${pulls.join(",")}`,
    );
    assert.equal(
      opened,
      1,
      "One shared stream serves entity, legacy, and transcript watchers",
    );
    await delay(250);
    const beforeBurst = pulls.length;
    revision = 2;
    for (let index = 0; index < 30; index++)
      for (const stream of streams) {
        const resources = streamResources.get(stream) || [];
        stream.write(
          protocol3SseEvent("resources", {
            protocol: 4,
            workspaceId,
            epoch: "fixture-epoch",
            reason: "change",
            revision,
            resources,
            resourceVersions: resources.map((resource) => ({
              resource,
              revision,
            })),
          }),
        );
      }
    await page.waitForFunction(
      () =>
        window.mobileSync.values.state?.runtime?.agents?.[0]?.name ===
          "Agent 2" &&
        window.mobileSync.revision("team") === 2 &&
        window.mobileSync.revision("history:lead") === 2,
    );
    await delay(150);
    const burstPulls = pulls.slice(beforeBurst);
    assert.equal(
      burstPulls.filter((scope) => scope !== "state:entities:v1").length,
      3,
      "A burst coalesces each transcript scope and draft replication",
    );
    assert.ok(
      burstPulls.filter((scope) => scope === "state:entities:v1").length >= 1 &&
        burstPulls.filter((scope) => scope === "state:entities:v1").length <= 2,
      `A burst refreshes the entity projection and can settle its sequence checkpoint: ${JSON.stringify(burstPulls)}`,
    );

    await page.evaluate(() => {
      Object.defineProperty(document, "hidden", {
        configurable: true,
        value: true,
      });
      document.dispatchEvent(new Event("visibilitychange"));
    });
    await until(
      () => streams.size === 0,
      "Backgrounding closes the live sockets",
    );
    await delay(100);
    const hiddenPulls = pulls.length;
    await delay(3200);
    assert.equal(
      pulls.length,
      hiddenPulls,
      "Background fallback polling stops",
    );
    revision = 3;
    const resumedAt = Date.now();
    await page.evaluate(() => {
      Object.defineProperty(document, "hidden", {
        configurable: true,
        value: false,
      });
      document.dispatchEvent(new Event("visibilitychange"));
      window.dispatchEvent(new Event("pageshow"));
      window.dispatchEvent(new Event("online"));
    });
    await page.waitForFunction(
      () =>
        window.mobileSync.values.state?.runtime?.agents?.[0]?.name ===
          "Agent 3" &&
        window.mobileSync.revision("team") === 3 &&
        window.mobileSync.revision("history:lead") === 3,
    );
    assert.ok(
      Date.now() - resumedAt < 1500,
      "Visible state recovers without waiting for fallback polling",
    );
    await page.waitForFunction(() => window.mobileSync.resumeCalls === 1);
    await until(
      () => opened >= 2,
      `Resume opens replacement streams, opened=${opened} scopes=${streamScopes.join(",")}`,
    );
    assert.equal(
      await page.evaluate(() => window.mobileSync.resumeCalls),
      1,
      "One PWA return invokes each resume consumer once",
    );
    assert.equal(opened, 2, "One PWA return recreates the shared live stream");

    await page.evaluate(() => {
      Object.defineProperty(navigator, "onLine", {
        configurable: true,
        value: false,
      });
      window.dispatchEvent(new Event("offline"));
    });
    await until(() => streams.size === 0, "Offline closes the stream");
    await page.evaluate(() => window.dispatchEvent(new Event("pageshow")));
    await delay(120);
    assert.equal(
      opened,
      2,
      "History restore does not open a socket while offline",
    );
    await page.evaluate(() => {
      Object.defineProperty(navigator, "onLine", {
        configurable: true,
        value: true,
      });
      window.dispatchEvent(new Event("online"));
    });
    await until(() => streams.size === 1, "Online recreates the shared stream");
    assert.equal(opened, 3);
    await page.evaluate(() => {
      for (const stop of window.mobileSync.stops) stop();
      window.mobileSync.stopResume();
    });
    await until(
      () => streams.size === 0,
      "Last subscription closes the stream",
    );
    await delay(100);
    const stoppedPulls = pulls.length;
    await page.evaluate(() => window.dispatchEvent(new Event("online")));
    await delay(3200);
    assert.equal(
      pulls.length,
      stoppedPulls,
      "Unmounted subscriptions leave no polling work",
    );

    revision = 4;
    const beforeCompact = pulls.length;
    await page.reload();
    await page.evaluate(async () => {
      window.mobileSync.value = null;
      window.mobileSync.stopCompact = (
        await import("/src/sync/client.ts")
      ).subscribeStateProjection(
        (value) => (window.mobileSync.value = value),
        () => {},
      );
    });
    await page.waitForFunction(
      () => window.mobileSync.value?.runtime?.agents?.[0]?.name === "Agent 4",
    );
    assert.equal(
      pullCursors.find((pull) => pull.scope === "state:entities:v1")?.after,
      0,
      "The new projection starts with its own checkpoint",
    );
    assert.ok(
      pulls.slice(beforeCompact).includes("state:entities:v1"),
      "The current client uses the entity projection scope",
    );
    assert.ok(
      !pulls.slice(beforeCompact).includes("state"),
      "Entity clients do not fetch the legacy state projection",
    );
    const localState = await page.evaluate(async () => {
      const { db } = await (await import("/src/sync/client.ts")).syncDatabase();
      return {
        ready: (await db.projections.findOne("state:entities:ready").exec())
          ?.payload,
        entity: window.mobileSync.readCachedEntity(
          (await db.projections.findOne("entity:agent:lead").exec())?.payload ??
            "null",
        ),
      };
    });
    assert.equal(
      localState.entity.value.name,
      "Agent 4",
      "A new response replaces the cached entity",
    );
    assert.equal(
      localState.ready,
      "ready",
      "The entity projection publishes its ready marker",
    );
    await page.evaluate(() => window.mobileSync.stopCompact());

    await page.addInitScript(() => {
      Object.defineProperty(navigator, "onLine", {
        configurable: true,
        value: false,
      });
    });
    const beforeOfflineIdentity = identities;
    await page.reload();
    const cached = await page.evaluate(async () => {
      const started = performance.now();
      const { db, workspaceId } = await (
        await import("/src/sync/client.ts")
      ).syncDatabase();
      return {
        workspaceId,
        value: window.mobileSync.readCachedEntity(
          (await db.projections.findOne("entity:agent:lead").exec())?.payload ??
            "null",
        ),
        elapsed: performance.now() - started,
      };
    });
    assert.equal(cached.workspaceId, workspaceId);
    assert.equal(
      cached.value.value.name,
      "Agent 4",
      "Offline startup retains the cached server projection",
    );
    assert.equal(
      identities,
      beforeOfflineIdentity,
      "Explicit offline startup does not wait for identity HTTP",
    );
    assert.ok(
      cached.elapsed < 1500,
      "Offline database opens without the HTTP deadline",
    );
    expect(errors).toEqual([]);
    console.log(
      "PASS: scoped SSE, coalesced pulls, hidden/offline suspension, resume recovery, complete disposal, cached offline startup",
    );
  } finally {
    for (const response of streams) response.end();
    await server.close();
  }
});
