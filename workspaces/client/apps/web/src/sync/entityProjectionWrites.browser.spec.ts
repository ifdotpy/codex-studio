import type { LegacySyncIdentityResponse } from "./syncBrowserFixture.ts";
import type { components } from "../generated/api";
import type { ResourceChangeEvent } from "./resourceEvents";
import type { Page } from "@playwright/test";
import {
  initializeWrites,
  persistAcceptedWrite,
} from "./entityProjectionWrites.browser.fixture.ts";
import { fixturePort, legacyPullResponse } from "./syncBrowserFixture.ts";
import assert from "node:assert/strict";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import {
  API_SCHEMA_HASH_HEADER,
  apiSchemaHandshakeSse,
  expect,
  protocol3SseEvent,
  readApiSchemaHash,
  test,
} from "../../tests/playwright.ts";

for (const writerTab of [0, 1]) {
  test(`projection reset fences an accepted write from tab ${writerTab + 1}`, async ({
    browser,
  }) => {
    test.setTimeout(45_000);
    const { createFixtureViteServer: createServer } =
      await import("../../tests/playwright.ts");
    const workspaceId = "e".repeat(32);
    const streams = new Set<import("node:http").ServerResponse>();
    const rows = new Map<string, components["schemas"]["SyncEntity"]>();
    let sequence = 2;
    let revision = 0;
    let resetPending = false;
    const row = (
      id: string,
      name: string,
      seq: number,
    ): components["schemas"]["SyncEntity"] => ({
      op: "upsert",
      collection: "agent",
      id,
      value: { id, name, status: "running", source: "managed" },
      seq: seq,
    });
    rows.set("lead", row("lead", "Lead", 1));
    rows.set("existing", row("existing", "Existing", 2));
    const frame = (
      reason: ResourceChangeEvent["reason"],
      changes?: components["schemas"]["EntityChangeBatch"] | null,
      reset = false,
    ): ResourceChangeEvent => ({
      protocol: 4,
      workspaceId,
      epoch: "projection-reset-race",
      revision: ++revision,
      reason,
      resources: [{ kind: "state" }],
      resourceVersions: [
        {
          revision: sequence,
          ...(changes
            ? {
                entitySequences: changes.documents.map(
                  (document) => document.seq,
                ),
                entityChanges: changes,
              }
            : {}),
          ...(reset ? { entitySequenceReset: true } : {}),
        },
      ],
    });
    const send = (event: ResourceChangeEvent) => {
      for (const response of streams)
        response.write(protocol3SseEvent("resources", event));
    };
    const cacheDir = await mkdtemp(
      join(tmpdir(), "studio-entity-writes-vite-"),
    );
    const server = await createServer({
      configFile: false,
      cacheDir,
      root: fileURLToPath(new URL("../..", import.meta.url)),
      server: { host: "127.0.0.1", port: 0 },
      plugins: [
        {
          name: "entity-projection-reset-race",
          configureServer(server) {
            server.middlewares.use((request, response, next) => {
              const url = new URL(request.url ?? "", "http://localhost");
              if (url.pathname === "/projection-reset-check") {
                response.setHeader("Content-Type", "text/html");
                response.end(
                  "<!doctype html><title>Projection reset check</title>",
                );
                return;
              }
              if (url.pathname === "/api/sync/identity") {
                response.setHeader("Content-Type", "application/json");
                response.setHeader(API_SCHEMA_HASH_HEADER, readApiSchemaHash());
                response.end(
                  JSON.stringify({
                    workspaceId,
                  } satisfies LegacySyncIdentityResponse),
                );
                return;
              }
              if (url.pathname === "/api/sync/stream") {
                response.setHeader("Content-Type", "text/event-stream");
                response.write(apiSchemaHandshakeSse());
                response.write(
                  protocol3SseEvent("resources", frame("initial")),
                );
                streams.add(response);
                response.on("close", () => streams.delete(response));
                return;
              }
              if (url.pathname === "/api/sync/pull") {
                const after = Number(url.searchParams.get("after"));
                response.setHeader("Content-Type", "application/json");
                response.setHeader(API_SCHEMA_HASH_HEADER, readApiSchemaHash());
                if (resetPending && after > 0) {
                  resetPending = false;
                  response.end(
                    legacyPullResponse({
                      workspaceId,
                      reset: true,
                      floor: 4,
                      maxSeq: sequence,
                    }),
                  );
                } else {
                  response.end(
                    legacyPullResponse({
                      workspaceId,
                      documents: [...rows.values()]
                        .filter((document) => document.seq > after)
                        .sort((left, right) => left.seq - right.seq),
                      checkpoint: { seq: sequence },
                      initialHigh: Number(
                        url.searchParams.get("initialHigh") || 0,
                      ),
                      maxSeq: sequence,
                    }),
                  );
                }
                return;
              }
              next();
            });
          },
        },
      ],
    });
    const context = await browser.newContext();
    const pages: Page[] = [];
    const errors: string[] = [];
    const openPage = async () => {
      const page = await context.newPage();
      pages.push(page);
      page.on("pageerror", (error) => errors.push(error.message));
      await page.goto(
        `http://127.0.0.1:${fixturePort(server)}/projection-reset-check`,
      );
      await page.evaluate(initializeWrites);
      await page.waitForFunction(
        () =>
          window.syncWrites.snapshot?.runtime.agents.some(
            (agent) => agent.id === "lead",
          ) && window.syncWrites.connection === "live",
      );
      return page;
    };
    try {
      await server.listen();
      const primary = await openPage();
      if (writerTab) await openPage();
      await expect.poll(() => streams.size).toBe(1);
      const writer = pages[writerTab];
      assert.ok(writer);
      await writer.evaluate(() => {
        const db = window.syncWrites.db;
        if (!db) throw new Error("Missing writer fixture database");
        const storage = db.projections.storageInstance;
        const original = storage.bulkWrite.bind(storage);
        storage.bulkWrite = async (writes, ...rest) => {
          if (
            !window.syncWrites.writeStarted &&
            writes.some(
              (entry) =>
                entry.document.id === "entity:agent:ghost" &&
                entry.document.seq === 3,
            )
          ) {
            window.syncWrites.writeStarted = true;
            await new Promise<void>(
              (resolve) => (window.syncWrites.releaseWrite = resolve),
            );
          }
          return original(writes, ...rest);
        };
      });
      sequence = 3;
      const accepted = frame("change", {
        after: 2,
        through: 3,
        documents: [row("ghost", "Already deleted", 3)],
      });
      if (writerTab) {
        await writer.evaluate(persistAcceptedWrite, JSON.stringify(accepted));
      } else {
        send(accepted);
      }
      await writer.waitForFunction(() => window.syncWrites.writeStarted);
      sequence = 4;
      rows.set("fresh", row("fresh", "Replacement baseline", 4));
      resetPending = true;
      send(frame("overflow", null, true));
      await primary.waitForTimeout(150);
      assert.equal(
        await primary.evaluate(() =>
          window.syncWrites.snapshot?.runtime.agents.some(
            (agent) => agent.id === "fresh",
          ),
        ),
        false,
        "Reset must wait for the accepted storage write",
      );
      await writer.evaluate(() => window.syncWrites.releaseWrite());
      await Promise.all(
        pages.map((page) =>
          page.waitForFunction(
            async () =>
              window.syncWrites.snapshot?.runtime.agents.some(
                (agent) => agent.id === "fresh",
              ) && (await window.syncWrites.checkpoint()) === 4,
          ),
        ),
      );
      await primary.waitForTimeout(150);
      for (const page of pages) {
        assert.equal(
          await page.evaluate(() =>
            window.syncWrites.snapshot?.runtime.agents.some(
              (agent) => agent.id === "ghost",
            ),
          ),
          false,
          "Reset must delete the old insert after its storage write finishes",
        );
      }
      send(accepted);
      await primary.waitForTimeout(150);
      for (const page of pages) {
        assert.equal(
          await page.evaluate(() =>
            window.syncWrites.snapshot?.runtime.agents.some(
              (agent) => agent.id === "ghost",
            ),
          ),
          false,
          "A stale peer frame must not resurrect a row removed by reset",
        );
        assert.deepEqual(
          await page.evaluate(() => window.syncWrites.errors),
          [],
        );
      }
      assert.deepEqual(errors, []);
    } finally {
      try {
        await context.close();
        for (const response of streams) response.end();
        await server.close();
      } finally {
        await rm(cacheDir, { recursive: true, force: true });
      }
    }
  });
}
