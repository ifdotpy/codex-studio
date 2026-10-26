import type {
  LegacyPullFixture,
  LegacyIdentityFixture,
} from "./prefetch-storage.fixture.ts";
import {
  readProjectionStores,
  projectionIsStored,
  completedTranscriptAgent,
} from "./prefetch-storage.fixture.ts";
import { fixtureOrigin } from "./browser.ts";
import type { BrowserContext, Page, Route } from "@playwright/test";
import type { ViteDevServer } from "vite";
import { SYNC_PROTOCOL_VERSION } from "../../src/generated/api.ts";
import type { components } from "../../src/generated/api.ts";
import type { Agent } from "../../src/types.ts";
import { fixtureGetResult } from "./http.ts";
import { initializeInstrumentation } from "./chat-prefetch-ui.fixture.ts";
import type { GetResult, PostBody } from "../../src/apiTypes.ts";

// The renderer sends the generated resource list to this fixture stream.
function fixtureResources(
  source: string,
): components["schemas"]["ResourceRef"][] {
  return JSON.parse(source) as components["schemas"]["ResourceRef"][];
}

// This scenario deliberately inspects the legacy serialized entity envelope.
function legacyEntityName(
  document:
    | components["schemas"]["SyncPullResponse"]["documents"][number]
    | undefined,
): unknown {
  const payload: unknown = JSON.parse(
    document && "payload" in document ? document.payload : "null",
  );
  if (!payload || typeof payload !== "object" || !("value" in payload))
    return undefined;
  const value = payload.value;
  return value && typeof value === "object" && "name" in value
    ? value.name
    : undefined;
}
import {
  test,
  createFixtureViteServer,
  expect,
  spawnFixture as spawn,
  apiSchemaHandshakeSse,
  readTestState,
  rustUiFixture,
} from "../playwright.ts";
// Production App and real RxDB. Only the isolated fixture receives requests.
import assert from "node:assert/strict";
import { mkdtemp } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
test("chat prefetch ui @performance", async ({ browser }) => {
  const repo = fileURLToPath(new URL("../../../../../../", import.meta.url));
  const legacySync = process.env.LEGACY_SYNC === "1";
  const dir = await mkdtemp(join(tmpdir(), "studio-chat-prefetch-"));
  const fixture = spawn(process.execPath, [rustUiFixture, dir], {
    stdio: ["ignore", "pipe", "pipe"],
    env: {
      ...process.env,
      CODEX_BOARD_STATE_DIR: join(dir, "board"),
      FIXTURE_WORKER_COUNT: "1",
    },
  });
  let log = "",
    context: BrowserContext | undefined,
    cleanupPage: Page | undefined,
    server: ViteDevServer | undefined,
    heartbeatTimer: ReturnType<typeof setInterval> | undefined;
  const streamConnections = new Set<{
    response: import("node:http").ServerResponse;
    resources: components["schemas"]["ResourceRef"][];
    openedAt: number;
  }>();
  fixture.stderr?.on("data", (chunk) => {
    log += chunk;
  });
  const until = async (
    predicate: () => boolean | Promise<boolean>,
    label: string,
    timeout = 12000,
  ) => {
    const deadline = Date.now() + timeout;
    while (Date.now() < deadline) {
      if (await predicate()) return;
      await new Promise((resolve) => setTimeout(resolve, 25));
    }
    throw new Error(label);
  };
  try {
    const port = await new Promise<number>((resolve, reject) => {
      fixture.stdout?.once("data", (chunk) =>
        resolve(Number(String(chunk).trim())),
      );
      fixture.once("exit", () => reject(new Error(log)));
    });
    const origin = `http://127.0.0.1:${port}`;
    const original = await readTestState(origin);
    const identity = await fixtureGetResult(
      "/api/sync/identity",
      await fetch(origin + "/api/sync/identity"),
    );
    let workspaceId = identity.workspaceId;
    const entityDocuments = new Map<
      string,
      components["schemas"]["SyncPullResponse"]["documents"][number]
    >();
    let entityMaxSeq = 0;
    let entityCursor = 0;
    do {
      const result = await fixtureGetResult(
        "/api/sync/pull",
        await fetch(
          `${origin}/api/sync/pull?scope=state%3Aentities%3Av1&after=${entityCursor}&limit=100`,
        ),
      );
      if (!("documents" in result))
        throw new Error("Unexpected reset in fixture pull");
      for (const document of result.documents || [])
        entityDocuments.set(document.id, document);
      entityMaxSeq = result.maxSeq || entityMaxSeq;
      entityCursor = result.checkpoint?.seq || entityCursor;
    } while (entityCursor < entityMaxSeq);
    const a = original.threads.find((agent) => agent.name === "Other project");
    const b = original.threads.find((agent) => agent.name === "Release lead");
    assert.ok(a && b, "Expected both fixture chats");
    assert.ok(
      typeof a.name === "string" && typeof b.name === "string",
      "Expected named fixture chats",
    );
    let currentBName: string = b.name;
    const agents = [a, b].map((agent) => ({
      ...agent,
      status: "completed",
      inFlight: false,
      turnId: null,
    }));
    const payload = (
      agent: Agent,
      tag: string,
    ): import("../../src/types.ts").TranscriptPageData &
      Partial<
        Pick<components["schemas"]["TranscriptStreamDelta"], "order">
      > => ({
      agent: completedTranscriptAgent(agent),
      items: Array.from({ length: 40 }, (_, index) => ({
        id: `${tag}-${index}`,
        role: "assistant",
        at: index,
        text: `${tag} exact message ${index}. ${"Saved conversation text. ".repeat(18)}`,
      })),
      historyVersion: "fixture-history",
      truncated: agent.id === a.id,
      nextCursor: agent.id === a.id ? "older-a" : null,
    });
    const values = new Map([
      [a.id, { seq: 200, data: payload(a, "A-current") }],
      [b.id, { seq: 300, data: payload(b, "B-first") }],
    ]);
    const reads: { id: string; seq: number; at: number; after: number }[] = [];
    const completedReads: Pick<(typeof reads)[number], "id" | "seq">[] = [];
    const streams: (string | null)[] = [];
    const entityPulls: {
      at: number;
      after: number;
      ids: string[];
      renamedEntity: unknown[];
      maxSeq: number;
    }[] = [];
    const streamQueries: {
      at: number;
      resources: components["schemas"]["ResourceRef"][];
    }[] = [];
    const streamEvents: ((typeof streamQueries)[number] & { kind: "open" })[] =
      [];
    const emittedEntityEvents: {
      at: number;
      revision: number;
      reason: components["schemas"]["ResourceChangeEvent"]["reason"];
      resources: components["schemas"]["ResourceRef"][];
      pending: {
        query: components["schemas"]["ResourceRef"][];
        openedAt: number;
      }[];
    }[] = [];
    const held: Route[] = [];
    const identities: string[] = [];
    let holdNetwork = false,
      holdWorkspaceB = false,
      holdRecoveryB = false,
      failRecoveryBOnce = false,
      staleB = false;
    let staleReplies = 0,
      releaseA: (() => void) | undefined,
      releaseRecoveryB: (() => void) | undefined,
      recoveryReadStarted = false;
    let notificationRevision = 0;
    server = await createFixtureViteServer({
      configFile: false,
      root: join(repo, "workspaces/client/apps/web"),
      server: { host: "127.0.0.1", port: 0 },
      plugins: [
        {
          name: "chat-prefetch-persistent-sync-stream",
          configureServer(vite) {
            vite.middlewares.use("/api/sync/stream", (request, response) => {
              const url = new URL(request.url || "/", "http://localhost");
              const resources = fixtureResources(
                url.searchParams.get("resources") || "[]",
              );
              streamEvents.push({ at: Date.now(), kind: "open", resources });
              response.writeHead(200, {
                "Content-Type": "text/event-stream",
                "Cache-Control": "no-cache",
                Connection: "keep-alive",
              });
              response.write(apiSchemaHandshakeSse());
              response.write(
                `retry: 100\nevent: resources\ndata: ${JSON.stringify({
                  protocol: SYNC_PROTOCOL_VERSION,
                  workspaceId,
                  epoch: "prefetch-epoch",
                  revision: notificationRevision,
                  reason: "initial",
                  resources,
                  resourceVersions: resources.map((resource) => ({
                    resource,
                    revision: notificationRevision,
                  })),
                } satisfies components["schemas"]["ResourceChangeEvent"])}\n\n`,
              );
              const connection = { response, resources, openedAt: Date.now() };
              streamConnections.add(connection);
              streamQueries.push({ at: Date.now(), resources });
              response.on("close", () => streamConnections.delete(connection));
            });
          },
        },
      ],
    });
    await server.listen();
    const streamOrigin = fixtureOrigin(server);
    heartbeatTimer = setInterval(() => {
      const heartbeat = `event: heartbeat\ndata: ${JSON.stringify({
        protocol: SYNC_PROTOCOL_VERSION,
        workspaceId,
        epoch: "prefetch-epoch",
        revision: notificationRevision,
      } satisfies components["schemas"]["ResourceHeartbeatEvent"])}\n\n`;
      for (const { response } of streamConnections) response.write(heartbeat);
    }, 5000);
    context = await browser.newContext({
      viewport: { width: 1440, height: 960 },
      // This test controls HTTP replies. A worker can bypass Playwright routes
      // after reload; the separate loading suite tests the production worker.
      serviceWorkers: "block",
    });
    const page = (cleanupPage = await context.newPage());
    page.setDefaultTimeout(12000);
    const errors: string[] = [];
    page.on("pageerror", (error) => errors.push(error.message));
    await page.addInitScript(initializeInstrumentation, {
      stateDir: original.stateDir,
      id: a.id,
    });

    const handle = async (route: Route) => {
      const url = new URL(route.request().url());
      if (url.pathname === "/api/sync/stream") return route.fallback();
      if (holdNetwork) {
        held.push(route);
        return;
      }
      if (url.pathname === "/api/sync/identity") {
        identities.push(workspaceId);
        return route.fulfill({
          json: {
            workspaceId,
            ...(legacySync ? {} : { chatState: true }),
          } satisfies LegacyIdentityFixture,
        });
      }
      if (url.pathname === "/api/transcript/stream") {
        streams.push(url.searchParams.get("id"));
        return route.fulfill({ status: 204, body: "" });
      }
      if (url.pathname === "/api/search")
        return route.fulfill({
          json: {
            results: [
              {
                kind: "message",
                id: "A-focused-20",
                agent: a.id,
                text: "Find the focused history",
              },
            ],
          } satisfies Pick<GetResult<"/api/search">, "results">,
        });
      if (url.pathname === "/api/search/item")
        return route.fulfill({
          json: {
            id: "A-focused-20",
            room: a.id,
            text: "Find the focused history",
          } satisfies Pick<
            GetResult<"/api/search/item">,
            "id" | "room" | "text"
          >,
        });
      if (url.pathname === "/api/transcript/page") {
        assert.equal(url.searchParams.get("id"), a.id);
        const focused = !!url.searchParams.get("around");
        const older = payload(a, focused ? "A-focused" : "A-older");
        older.items = older.items.map((item, index) => ({
          ...item,
          at: index - 100,
        }));
        return route.fulfill({
          json: {
            ...older,
            nextCursor: null,
            nextAfterCursor: focused ? "focused-next" : null,
          } satisfies import("../../src/types.ts").TranscriptPageData,
        });
      }
      if (url.pathname === "/api/transcript") {
        const id = url.searchParams.get("id");
        if (holdWorkspaceB && id === b.id) {
          held.push(route);
          return;
        }
        return route.fulfill({
          json: payload(id === a.id ? a : b, "raw-obsolete"),
        });
      }
      if (url.pathname !== "/api/sync/pull") return route.fallback();
      const scope = url.searchParams.get("scope");
      const after = Number(url.searchParams.get("after") || 0);
      if (scope === "state:entities:v1") {
        const limit = Number(url.searchParams.get("limit") || 100);
        const requestedInitialHigh = Number(
          url.searchParams.get("initialHigh") || 0,
        );
        const initialHigh = requestedInitialHigh || entityMaxSeq;
        const documents = [...entityDocuments.values()]
          .filter((document) => document.seq > after)
          .sort((left, right) => left.seq - right.seq)
          .slice(0, limit);
        entityPulls.push({
          at: Date.now(),
          after,
          ids: documents.map((document) => document.id),
          renamedEntity: documents
            .filter((document) => document.id === `entity:agent:${b.id}`)
            .map((document) => legacyEntityName(document)),
          maxSeq: entityMaxSeq,
        });
        return route.fulfill({
          json: {
            workspaceId,
            documents,
            checkpoint: { seq: documents.at(-1)?.seq ?? after },
            maxSeq: entityMaxSeq,
            initialHigh,
          } satisfies LegacyPullFixture,
        });
      }
      if (scope === "drafts")
        return route.fulfill({
          json: {
            workspaceId,
            documents: [],
            checkpoint: { seq: after },
          } satisfies LegacyPullFixture,
        });
      if (!scope?.startsWith("transcript:")) return route.fallback();
      const id = scope.slice(11);
      if (holdWorkspaceB && id === b.id) {
        held.push(route);
        return;
      }
      if (id === a.id && !releaseA)
        await new Promise<void>((resolve) => {
          releaseA = resolve;
        });
      let value = values.get(id);
      if (!value)
        return route.fulfill({
          status: 400,
          json: {
            error: "Unknown fixture chat",
          } satisfies components["schemas"]["ErrorResponse"],
        });
      const forcedStale = staleB && id === b.id;
      if (forcedStale) {
        staleB = false;
        staleReplies++;
        value = { seq: 300, data: payload(b, "B-obsolete") };
      }
      if (id) reads.push({ id, seq: value.seq, at: Date.now(), after });
      if (holdRecoveryB && id === b.id) {
        recoveryReadStarted = true;
        await new Promise<void>((resolve) => {
          releaseRecoveryB = resolve;
        });
      }
      if (failRecoveryBOnce && id === b.id) {
        failRecoveryBOnce = false;
        return route.fulfill({
          status: 503,
          json: {
            error: "One simulated transcript pull failure",
          } satisfies components["schemas"]["ErrorResponse"],
        });
      }
      await route.fulfill({
        json: {
          workspaceId,
          documents:
            after < value.seq || forcedStale
              ? [
                  {
                    id: scope,
                    payload: JSON.stringify(value.data),
                    seq: value.seq,
                    _deleted: false,
                  },
                ]
              : [],
          checkpoint: { seq: Math.max(after, value.seq) },
        } satisfies LegacyPullFixture,
      });
      completedReads.push({ id, seq: value.seq });
    };
    await page.route("**/api/**", handle);
    const queryHas = (
      query: components["schemas"]["ResourceRef"][],
      ref: components["schemas"]["ResourceRef"],
    ) =>
      query.some(
        (item) =>
          item.kind === ref.kind &&
          (!("agentId" in ref) ||
            ("agentId" in item && item.agentId === ref.agentId)),
      );
    const emitResourceChange = async (
      resources: components["schemas"]["ResourceRef"][],
      reason: components["schemas"]["ResourceChangeEvent"]["reason"] = "change",
      beforeNotify = () => {},
      requiredQueryRefs: components["schemas"]["ResourceRef"][] = [
        { kind: "state" },
      ],
    ) => {
      await until(
        () =>
          [...streamConnections].some(({ resources: query }) =>
            requiredQueryRefs.every((ref) => queryHas(query, ref)),
          ),
        `A resource stream subscribed to ${JSON.stringify(requiredQueryRefs)} is ready`,
      );
      beforeNotify();
      notificationRevision = Math.max(notificationRevision, entityMaxSeq) + 1;
      const pending = [...streamConnections].filter(({ resources: query }) =>
        requiredQueryRefs.every((ref) => queryHas(query, ref)),
      );
      emittedEntityEvents.push({
        at: Date.now(),
        revision: notificationRevision,
        reason,
        resources,
        pending: pending.map(({ resources: query, openedAt }) => ({
          query,
          openedAt,
        })),
      });
      const data = `event: resources\ndata: ${JSON.stringify({
        protocol: SYNC_PROTOCOL_VERSION,
        workspaceId,
        epoch: "prefetch-epoch",
        revision: notificationRevision,
        reason,
        resources,
      } satisfies Omit<
        components["schemas"]["ResourceChangeEvent"],
        "resourceVersions"
      >)}\n\n`;
      for (const { response } of pending) response.write(data);
      return notificationRevision;
    };
    const invalidate = async (name: string) => {
      const response = await fetch(origin + "/api/rename", {
        method: "POST",
        headers: {
          "Content-Type": "application/json",
          "X-Canvas-Token": original.token,
        },
        body: JSON.stringify({
          id: b.id,
          name,
        } satisfies PostBody<"/api/rename">),
      });
      assert.ok(response.ok, await response.text());
      currentBName = name;
      let after = entityMaxSeq;
      let maxSeq = entityMaxSeq;
      const updatedDocuments = new Map<
        string,
        components["schemas"]["SyncPullResponse"]["documents"][number]
      >();
      do {
        const result = await fixtureGetResult(
          "/api/sync/pull",
          await fetch(
            `${origin}/api/sync/pull?scope=state%3Aentities%3Av1&after=${after}&limit=100`,
          ),
        );
        if (!("documents" in result))
          throw new Error("Unexpected reset in fixture pull");
        for (const document of result.documents || [])
          updatedDocuments.set(document.id, document);
        maxSeq = result.maxSeq || maxSeq;
        after = result.checkpoint?.seq || after;
      } while (after < maxSeq);
      const updatedAgent = updatedDocuments.get(`entity:agent:${b.id}`);
      assert.equal(
        legacyEntityName(updatedAgent),
        name,
        "The state fixture must expose the renamed entity before notifying subscribers",
      );
      await emitResourceChange(
        [{ kind: "state" }, { kind: "transcript", agentId: b.id }],
        "change",
        () => {
          for (const [id, document] of updatedDocuments)
            entityDocuments.set(id, document);
          entityMaxSeq = maxSeq;
        },
        [{ kind: "state" }, { kind: "transcript", agentId: b.id }],
      );
      const eventAt = emittedEntityEvents.at(-1)?.at ?? Date.now();
      await until(async () => {
        const pulledRename = entityPulls.some(
          (pull) => pull.at >= eventAt && pull.renamedEntity.includes(name),
        );
        const rowUpdated = (
          await page.locator(`[data-chat="${b.id}"]:visible`).allTextContents()
        ).some((text) => text.includes(name));
        return pulledRename && rowUpdated;
      }, "The state invalidation must update the renamed chat in the sidebar").catch(
        async (error) => {
          const projection = await page.evaluate(
            readProjectionStores,
            `rxdb-dexie-studio${workspaceId}--0--projections`,
          );
          const diagnostic = await page.evaluate(
            async ({ id, workspaceId, projection, storedAgentName }) => {
              const { stored, names, dbName } = projection;
              const rows = stored.flat();
              const agent = rows.find((row) => row.id === `entity:agent:${id}`);
              return {
                workspaceId,
                databaseNames: (await indexedDB.databases()).map(
                  (database) => database.name,
                ),
                objectStores: names,
                dbName,
                firstStoredRowKeys: Object.keys(rows[0] || {}),
                matchingStoredRows: rows
                  .filter((row) => JSON.stringify(row).includes(id))
                  .slice(0, 4),
                objectStoreRowCounts: stored.map((rows, index) => ({
                  store: names[index],
                  count: rows.length,
                })),
                title: document.querySelector("#conversation-title")
                  ?.textContent,
                rows: [
                  ...document.querySelectorAll(
                    `[data-chat="${CSS.escape(id)}"]`,
                  ),
                ].map((row) => ({
                  text: row.textContent,
                  visible: row.getClientRects().length > 0,
                })),
                selected: document
                  .querySelector(`[data-chat="${CSS.escape(id)}"]`)
                  ?.getAttribute("aria-current"),
                storedAgent: agent
                  ? {
                      seq: agent.seq,
                      name: storedAgentName,
                    }
                  : null,
                ready: rows.find((row) => row.id === "state:entities:ready")
                  ?.payload,
                checkpoint: rows.find(
                  (row) => row.id === "state:entities:checkpoint",
                )?.seq,
              };
            },
            {
              id: b.id,
              workspaceId,
              projection,
              storedAgentName: legacyEntityName(
                projection.stored
                  .flat()
                  .find((row) => row.id === `entity:agent:${b.id}`),
              ),
            },
          );
          throw new Error(
            `${error instanceof Error ? error.message : String(error)}; state invalidation: ${JSON.stringify(
              {
                name,
                updatedFixture: legacyEntityName(updatedAgent),
                entityPulls: entityPulls.slice(-8),
                emittedEntityEvents: emittedEntityEvents.slice(-4),
                streamQueries: streamQueries.slice(-5),
                identities,
                diagnostic,
                errors,
              },
            )}`,
          );
        },
      );
    };
    const selected = () => page.locator("#conversation-title").innerText();
    const marker = (tag: string) =>
      page.locator(`[data-message="${tag}-39"]:visible`);
    await page.goto(streamOrigin);
    await until(() => !!releaseA, "The first foreground request must start");
    const selectedBeforeForegroundRelease = await selected();
    await page.waitForTimeout(1250);
    assert.equal(
      reads.some((read) => read.id === b.id),
      false,
      `Background requests wait for ${selectedBeforeForegroundRelease}: ${JSON.stringify(reads)}`,
    );
    assert.equal(streams.includes(b.id), false);
    assert.ok(releaseA);
    releaseA();
    await marker("A-current").waitFor();
    await until(
      () => reads.some((read) => read.id === b.id && read.seq === 300),
      "Never-opened B must load in the background",
    );
    let stableBReads = reads.filter((read) => read.id === b.id).length;
    let stableSince = Date.now();
    while (Date.now() - stableSince < 400) {
      await page.waitForTimeout(25);
      const currentBReads = reads.filter((read) => read.id === b.id).length;
      if (currentBReads !== stableBReads) {
        stableBReads = currentBReads;
        stableSince = Date.now();
      }
    }
    assert.equal(await selected(), a.name);
    assert.equal(
      streams.includes(b.id),
      false,
      "Background B must not open a transcript SSE stream",
    );

    await page.evaluate(async () => {
      const { watchResourceChanges } = await import("/src/sync/client.ts");
      window.prefetchUiStopUnrelatedWatch = watchResourceChanges(
        { kind: "transcript", agentId: "unrelated-agent" },
        () => {},
      );
    });
    await until(
      () =>
        [...streamConnections].some(
          ({ resources }) =>
            queryHas(resources, { kind: "transcript", agentId: b.id }) &&
            queryHas(resources, {
              kind: "transcript",
              agentId: "unrelated-agent",
            }),
        ),
      "The persistent stream must include B and the unrelated test resource",
    );
    const readsBeforeBaselineSettle = reads.filter(
      (read) => read.id === b.id,
    ).length;
    let quietCount = readsBeforeBaselineSettle;
    let quietSince = Date.now();
    while (Date.now() - quietSince < 800) {
      await page.waitForTimeout(25);
      const current = reads.filter((read) => read.id === b.id).length;
      if (current !== quietCount) {
        quietCount = current;
        quietSince = Date.now();
      }
    }

    const beforeUnrelated = reads.filter((read) => read.id === b.id).length;
    await emitResourceChange(
      [{ kind: "transcript", agentId: "unrelated-agent" }],
      "change",
      undefined,
      [
        { kind: "transcript", agentId: b.id },
        { kind: "transcript", agentId: "unrelated-agent" },
      ],
    );
    await page.waitForTimeout(150);
    assert.equal(
      reads.filter((read) => read.id === b.id).length,
      beforeUnrelated,
      `A different agent transcript event does not refetch B: ${JSON.stringify({ reads: reads.filter((read) => read.id === b.id), emittedEntityEvents: emittedEntityEvents.slice(-2), streamQueries: streamQueries.slice(-4) })}`,
    );

    // A real shared SSE invalidation announces a newer B while the reader stays in A.
    values.set(b.id, { seq: 301, data: payload(b, "B-newest") });
    const updatedB = agents.find((agent) => agent.id === b.id);
    assert.ok(updatedB);
    updatedB.updated = Date.now();
    const backgroundUpdateAt = Date.now();
    await invalidate("Prefetch background update");
    await until(
      () => reads.some((read) => read.id === b.id && read.seq === 301),
      "B must refresh in the background after the shared invalidation",
    );
    const backgroundUpdateMs = Date.now() - backgroundUpdateAt;
    assert.ok(
      backgroundUpdateMs < 1500,
      `A changed background chat must refresh before the old polling delay: ${backgroundUpdateMs} ms`,
    );
    const bReads = reads.filter((read) => read.id === b.id).length;
    await page.waitForTimeout(5500);
    assert.equal(
      reads.filter((read) => read.id === b.id).length,
      bReads,
      "Stable chat revisions do not trigger periodic history pulls",
    );
    const beforePause = reads.filter((read) => read.id === b.id).length;
    await page.evaluate(() => {
      Object.defineProperty(document, "hidden", {
        configurable: true,
        value: true,
      });
      document.dispatchEvent(new Event("visibilitychange"));
      Object.defineProperty(navigator, "onLine", {
        configurable: true,
        value: false,
      });
      window.dispatchEvent(new Event("offline"));
    });
    values.set(b.id, { seq: 302, data: payload(b, "B-resume") });
    await page.waitForTimeout(200);
    assert.equal(
      reads.filter((read) => read.id === b.id).length,
      beforePause,
      "Hidden/offline prefetch stops history reads",
    );
    for (const { response } of streamConnections) response.end();
    await until(
      () => streamConnections.size === 0,
      "The explicit offline recovery setup closes the old SSE connection",
    );
    holdRecoveryB = true;
    recoveryReadStarted = false;
    await page.evaluate(() => {
      Object.defineProperty(document, "hidden", {
        configurable: true,
        value: false,
      });
      document.dispatchEvent(new Event("visibilitychange"));
      Object.defineProperty(navigator, "onLine", {
        configurable: true,
        value: true,
      });
      window.dispatchEvent(new Event("online"));
    });
    await until(
      () => streamConnections.size > 0 && recoveryReadStarted,
      "A resumed transcript baseline must refetch the changed cached chat",
    );
    const readsAfterResume = reads.filter((read) => read.id === b.id).length;
    await page.evaluate(() =>
      window.dispatchEvent(
        new PageTransitionEvent("pageshow", { persisted: true }),
      ),
    );
    await page.waitForTimeout(100);
    assert.equal(
      reads.filter((read) => read.id === b.id).length,
      readsAfterResume,
      "Online and pageshow coalesce while the recovery transcript read is in flight",
    );
    const flushAckCount = await page.evaluate(
      () => window.prefetchUiResourceFlushAcks.length,
    );
    const streamOpenCount = streamEvents.filter(
      (event) => event.kind === "open",
    ).length;
    for (const { response } of streamConnections) response.end();
    await until(
      () =>
        streamEvents.filter((event) => event.kind === "open").length >
        streamOpenCount,
      "A resumed stream reconnects while the history pull is held",
    );
    try {
      await until(
        () =>
          page.evaluate(
            ({ count, revision }) =>
              window.prefetchUiResourceFlushAcks.length > count &&
              window.prefetchUiResourceFlushAcks.at(-1) === revision,
            { count: flushAckCount, revision: notificationRevision },
          ),
        "The same-version reconnect baseline reaches the transcript watcher while its pull is held",
      );
    } catch (error) {
      const diagnostic = await page.evaluate(() => ({
        acks: window.prefetchUiResourceFlushAcks,
        flushes: window.prefetchUiResourceFlushObserved,
        pending: window.prefetchUiPendingResourceFlushRevisions,
      }));
      throw new Error(
        `${error instanceof Error ? error.message : String(error)}; flush diagnostic: ${JSON.stringify({ diagnostic, notificationRevision, streamEvents: streamEvents.slice(-5), streamQueries: streamQueries.slice(-4) })}`,
      );
    }
    const releaseHeldRecovery = () => releaseRecoveryB?.();
    releaseHeldRecovery();
    holdRecoveryB = false;
    await until(
      () => completedReads.some((read) => read.id === b.id && read.seq === 302),
      "The first recovered transcript pull completes",
    );
    await page.waitForTimeout(300);
    assert.equal(
      reads.filter((read) => read.id === b.id).length,
      readsAfterResume,
      "A same-version recovery baseline does not queue a duplicate after the held read succeeds",
    );

    holdRecoveryB = true;
    recoveryReadStarted = false;
    releaseRecoveryB = undefined;
    values.set(b.id, { seq: 303, data: payload(b, "B-trigger-pull") });
    await emitResourceChange(
      [{ kind: "transcript", agentId: b.id }],
      "change",
      () => {},
      [{ kind: "transcript", agentId: b.id }],
    );
    await until(
      () => recoveryReadStarted,
      "A newer transcript event starts the second held history pull",
    );
    const readsBeforeNewerFollowup = reads.filter(
      (read) => read.id === b.id,
    ).length;
    values.set(b.id, { seq: 304, data: payload(b, "B-during-recovery") });
    const inFlightRevision = await emitResourceChange(
      [{ kind: "transcript", agentId: b.id }],
      "change",
      () => {},
      [{ kind: "transcript", agentId: b.id }],
    );
    await until(
      () =>
        page.evaluate(
          (revision) => window.prefetchUiResourceFlushAcks.includes(revision),
          inFlightRevision,
        ),
      "The targeted watch callback must queue work for the newer revision while B's pull is held",
    );
    await page.waitForTimeout(50);
    assert.equal(
      reads.filter((read) => read.id === b.id).length,
      readsBeforeNewerFollowup,
      "The newer notification is acknowledged before releasing the in-flight read",
    );
    releaseHeldRecovery();
    holdRecoveryB = false;
    await until(
      () => reads.some((read) => read.id === b.id && read.seq === 304),
      "A newer transcript revision arriving during recovery gets one followup pull",
    );
    const settledResumeReads = reads.filter((read) => read.id === b.id).length;
    await page.waitForTimeout(750);
    assert.equal(
      reads.filter((read) => read.id === b.id).length,
      settledResumeReads,
      `Resume coalesces unchanged callbacks and preserves one newer in-flight event: ${JSON.stringify({ reads: reads.filter((read) => read.id === b.id), readsAfterResume, readsBeforeNewerFollowup, streamEvents: streamEvents.slice(-8), emittedEntityEvents: emittedEntityEvents.slice(-4), streamQueries: streamQueries.slice(-8) })}`,
    );

    holdRecoveryB = true;
    recoveryReadStarted = false;
    releaseRecoveryB = undefined;
    values.set(b.id, { seq: 305, data: payload(b, "B-during-recovery") });
    await emitResourceChange(
      [{ kind: "transcript", agentId: b.id }],
      "change",
      () => {},
      [{ kind: "transcript", agentId: b.id }],
    );
    await until(
      () => recoveryReadStarted,
      "The failure scenario starts its target transcript read",
    );
    const failedReadStartCount = reads.filter(
      (read) => read.id === b.id && read.seq === 305,
    ).length;
    const retryAckCount = await page.evaluate(
      () => window.prefetchUiResourceFlushAcks.length,
    );
    const retryStreamOpenCount = streamEvents.filter(
      (event) => event.kind === "open",
    ).length;
    failRecoveryBOnce = true;
    for (const { response } of streamConnections) response.end();
    await until(
      () =>
        streamEvents.filter((event) => event.kind === "open").length >
        retryStreamOpenCount,
      "The failed pull scenario reconnects its resource stream",
    );
    await until(
      () =>
        page.evaluate(
          ({ count, revision }) =>
            window.prefetchUiResourceFlushAcks.length > count &&
            window.prefetchUiResourceFlushAcks.at(-1) === revision,
          { count: retryAckCount, revision: notificationRevision },
        ),
      "A same-version recovery callback arrives before the held history read fails",
    );
    releaseHeldRecovery();
    holdRecoveryB = false;
    await until(
      () => completedReads.some((read) => read.id === b.id && read.seq === 305),
      "A failed same-version history pull receives one bounded retry",
    );
    await page.waitForTimeout(750);
    assert.equal(
      reads.filter((read) => read.id === b.id && read.seq === 305).length,
      failedReadStartCount + 1,
      "A suppressed recovery signal allows exactly one retry after failure",
    );
    assert.equal(await selected(), a.name);
    await until(
      () =>
        page.evaluate(projectionIsStored, {
          databaseName: `rxdb-dexie-studio${workspaceId}--0--projections`,
          documentId: `transcript:${b.id}`,
          sequence: 305,
        }),

      "RxDB must persist B's refreshed projection before network interruption",
    );
    await page.locator("#messages").evaluate((element) => {
      element.scrollTop = 900;
      element.dispatchEvent(new Event("scroll"));
    });
    const savedA = await page
      .locator("#messages")
      .evaluate((element) => element.scrollTop);
    holdNetwork = true;
    const switchCached = async (
      id: string,
      title: Agent["name"],
      tag: string,
      forbidden: string,
      expectedScroll: number | null = null,
    ) => {
      await page.evaluate(
        ({ id, title, tag, forbidden }) => {
          window.prefetchUiSwitchFrames = [];
          const capture = (window.prefetchUiCaptureSwitch =
            (window.prefetchUiCaptureSwitch || 0) + 1);
          const frame = () => {
            if (window.prefetchUiCaptureSwitch !== capture) return;
            if (
              document
                .querySelector(`[data-chat="${CSS.escape(id)}"]`)
                ?.getAttribute("aria-current") === "true"
            ) {
              const ids = [
                ...document.querySelectorAll("#messages [data-message]"),
              ]
                .filter((node) => node.getClientRects().length > 0)
                .map((node) => node.getAttribute("data-message"));
              window.prefetchUiSwitchFrames.push({
                time: performance.now(),
                fresh: ids.includes(`${tag}-39`),
                wrong: ids.some((value) => value?.startsWith(forbidden)),
                title: document
                  .querySelector("#conversation-title")
                  ?.textContent?.trim(),
                titleMatches:
                  document
                    .querySelector("#conversation-title")
                    ?.textContent?.trim() === title?.trim(),
                count: ids.length,
                scrollTop: document.querySelector("#messages")?.scrollTop,
              });
            }
            requestAnimationFrame(frame);
          };
          requestAnimationFrame(frame);
        },
        { id, title, tag, forbidden },
      );
      const chatButton = page.locator(`[data-chat="${id}"]`);
      await chatButton.click({ trial: true });
      await chatButton.evaluate((button) => {
        window.prefetchUiSwitchStartedAt = null;
        button.addEventListener(
          "pointerdown",
          () => (window.prefetchUiSwitchStartedAt = performance.now()),
          { once: true, capture: true },
        );
      });
      await chatButton.click();
      await marker(tag).waitFor();
      const elapsed = await page.evaluate(
        () => performance.now() - (window.prefetchUiSwitchStartedAt ?? NaN),
      );
      assert.ok(elapsed < 500, `Cached ${tag} switch took ${elapsed} ms`);
      await page.waitForFunction(
        () => window.prefetchUiSwitchFrames.length >= 3,
      );
      const frames = await page.evaluate(() => {
        window.prefetchUiCaptureSwitch++;
        return window.prefetchUiSwitchFrames;
      });
      assert.ok(
        frames.every(
          (frame) =>
            frame.fresh &&
            !frame.wrong &&
            frame.titleMatches &&
            frame.count > 0,
        ),
        `Cached switch exposed an empty, old, or wrong-chat frame: ${JSON.stringify(frames)}`,
      );
      if (expectedScroll !== null)
        assert.ok(
          frames.every(
            (frame) => Math.abs((frame.scrollTop ?? NaN) - expectedScroll) < 2,
          ),
          `The first cached frame must restore scroll ${expectedScroll}: ${JSON.stringify(frames)}`,
        );
      return elapsed;
    };
    const coldCached = await switchCached(
      b.id,
      currentBName,
      "B-during-recovery",
      "A-",
    );
    assert.equal(await marker("B-first").count(), 0);
    await page.locator("#messages").evaluate((element) => {
      element.scrollTop = 1100;
      element.dispatchEvent(new Event("scroll"));
    });
    const savedB = await page
      .locator("#messages")
      .evaluate((element) => element.scrollTop);
    const revisitA = await switchCached(
      a.id,
      a.name,
      "A-current",
      "B-",
      savedA,
    );
    assert.ok(
      Math.abs(
        (await page
          .locator("#messages")
          .evaluate((element) => element.scrollTop)) - savedA,
      ) < 2,
    );
    const revisitB = await switchCached(
      b.id,
      currentBName,
      "B-during-recovery",
      "A-",
    );
    assert.ok(
      Math.abs(
        (await page
          .locator("#messages")
          .evaluate((element) => element.scrollTop)) - savedB,
      ) < 2,
    );
    assert.equal(
      streams.includes(b.id),
      false,
      "A prefetched switch does not create a per-chat SSE stream",
    );

    holdNetwork = false;
    staleB = true;
    for (const route of held.splice(0)) await handle(route).catch(() => {});
    await invalidate("Prefetch stale response test");
    await until(
      () => staleReplies > 0,
      "The fixture must deliver a lower-sequence B projection",
    );
    await page.waitForTimeout(250);
    assert.equal(
      await marker("B-obsolete").count(),
      0,
      "A late lower-sequence RxDB reply cannot roll history back",
    );
    assert.equal(await marker("B-during-recovery").count(), 1);

    // Both appended and focused history must survive a return before any HTTP reply.
    await page.locator(`[data-chat="${a.id}"]`).click();
    await marker("A-current").waitFor();
    await page.locator("#earlier-messages").click();
    await marker("A-older").waitFor();
    const checkPageReturn = async (tag: string, offset: number) => {
      await page.locator("#messages").evaluate((element, top) => {
        element.scrollTop = top;
        element.dispatchEvent(new Event("scroll"));
      }, offset);
      const saved = await page
        .locator("#messages")
        .evaluate((element) => element.scrollTop);
      assert.ok(
        saved > 0,
        "The historical page has a measurable scroll position",
      );
      holdNetwork = true;
      await switchCached(b.id, currentBName, "B-during-recovery", "A-");
      const elapsed = await switchCached(a.id, a.name, tag, "B-", saved);
      holdNetwork = false;
      for (const route of held.splice(0)) await handle(route).catch(() => {});
      return elapsed;
    };
    const olderReturn = await checkPageReturn("A-older", 650);
    await page
      .getByRole("button", { name: "Search chats", exact: true })
      .click();
    const drawer = page.getByRole("dialog", {
      name: "Search messages",
      exact: true,
    });
    await drawer
      .getByRole("textbox", { name: "Search all conversations" })
      .fill("Find focused history");
    await drawer
      .getByRole("button", { name: "Search", exact: true })
      .last()
      .click();
    await drawer.getByText("Find the focused history", { exact: true }).click();
    await page
      .locator(".mantine-Modal-content:visible")
      .getByRole("button", { name: "Open chat", exact: true })
      .click();
    await drawer.waitFor({ state: "hidden" });
    await marker("A-focused").waitFor();
    await page
      .getByRole("button", { name: "Return to latest messages", exact: true })
      .waitFor();
    assert.equal(
      await marker("A-current").count(),
      0,
      "Focused history excludes the recent tail",
    );
    const focusedReturn = await checkPageReturn("A-focused", 750);
    assert.equal(await marker("A-current").count(), 0);
    await page
      .getByRole("button", { name: "Return to latest messages", exact: true })
      .waitFor();

    // A different workspace at the same origin reuses chat IDs but must use another database.
    workspaceId = "f".repeat(32);
    values.set(a.id, { seq: 20, data: payload(a, "Workspace2-A") });
    values.set(b.id, { seq: 30, data: payload(b, "Workspace2-B") });
    holdWorkspaceB = true;
    await page.reload();
    try {
      await marker("Workspace2-A").waitFor();
    } catch (error) {
      const diagnostic = await page.evaluate(() => ({
        title: document.querySelector("#conversation-title")?.textContent,
        selectedRows: [
          ...document.querySelectorAll("[data-chat][aria-current=true]"),
        ].map((row) => row.getAttribute("data-chat")),
        rows: [...document.querySelectorAll("[data-chat]")].map((row) => ({
          id: row.getAttribute("data-chat"),
          name: row.textContent.trim(),
          selected: row.getAttribute("aria-current"),
        })),
        markerVisible: !!document
          .querySelector('[data-message="Workspace2-A-39"]')
          ?.getClientRects().length,
        transcriptElements: document.querySelectorAll(
          "#messages [data-message]",
        ).length,
        bodyText: document.body.innerText.slice(0, 500),
      }));
      throw new Error(
        `${error instanceof Error ? error.message : String(error)}; workspace switch diagnostic: ${JSON.stringify({ diagnostic, identities, errors, reads: reads.slice(-12) })}`,
      );
    }
    await page.locator(`[data-chat="${b.id}"]`).click();
    await page.waitForTimeout(200);
    assert.equal(
      await marker("B-during-recovery").count(),
      0,
      "An old workspace cannot supply a same-ID chat",
    );
    assert.equal(await marker("A-current").count(), 0);
    assert.equal(
      await marker("Workspace2-A").count(),
      0,
      "The previous chat stays scoped during a cold switch",
    );
    holdWorkspaceB = false;
    for (const route of held.splice(0)) await handle(route).catch(() => {});
    await marker("Workspace2-B").waitFor();
    expect(errors).toEqual([]);
    await page.screenshot({ path: join(dir, "workspace-separated.png") });
    console.log(
      JSON.stringify({
        result: "PASS",
        browser: process.env.BROWSER === "webkit" ? "WebKit" : "Chromium",
        legacySync,
        neverOpenedSwitchMs: coldCached,
        backgroundUpdateMs,
        revisitA,
        revisitB,
        olderReturn,
        focusedReturn,
        evidence: dir,
        checks: [
          "foreground before background",
          "background freshness",
          "paged and focused first-frame scroll",
          "no wrong or empty frame",
          "scroll restore",
          "late HTTP and RxDB guards",
          "workspace isolation",
        ],
      }),
    );
  } catch (error) {
    await cleanupPage
      ?.screenshot({ path: join(dir, "failure.png") })
      .catch(() => {});
    console.error("Evidence:", dir, log);
    throw error;
  } finally {
    clearInterval(heartbeatTimer);
    await context?.close();
    for (const { response } of streamConnections || []) response.end();
    if (server) await server.close();
    if (fixture.exitCode === null) fixture.kill();
  }
});
