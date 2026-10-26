import type { FixtureResourceChange, FixturePullResponse } from "./helpers.ts";
import type { ServerResponse } from "node:http";
import type { ResourceRef } from "../playwright.ts";
import { fixtureResources } from "./helpers.ts";
import type { IncomingMessage } from "node:http";
import type { Connect } from "vite";
import { fixtureOrigin } from "./helpers.ts";
import assert from "node:assert/strict";
import { fileURLToPath } from "node:url";

import {
  apiSchemaHandshakeSse,
  apiSchemaHeaders,
  syncIdentityFixture,
  syncProtocolFixture,
  test,
  expect,
} from "../playwright.ts";

test("sync-read-retry-browser", async ({ page: fixturePage }) => {
  test.setTimeout(120_000);
  const { createFixtureViteServer: createServer } =
    await import("../playwright.ts");
  const workspaceId = "e".repeat(32);
  const attempts = new Map<string, number>();
  const streams = new Set<{
    resources: ResourceRef[];
    response: ServerResponse;
  }>();
  let revision = 0;
  const invalidate = (agentId: string) => {
    const resource = { kind: "transcript", agentId } satisfies ResourceRef;
    for (const stream of streams) {
      if (
        !stream.resources.some(
          (entry) => JSON.stringify(entry) === JSON.stringify(resource),
        )
      )
        continue;
      revision++;
      stream.response.write(
        `event: resources\ndata: ${JSON.stringify({
          protocol: 4,
          workspaceId,
          epoch: "fixture-epoch",
          revision,
          reason: "change",
          resources: [resource],
          resourceVersions: [{ resource, revision }],
        } satisfies FixtureResourceChange)}\n\n`,
      );
    }
  };
  const server = await createServer({
    configFile: false,
    optimizeDeps: { noDiscovery: true },
    root: fileURLToPath(new URL("../../", import.meta.url)),
    server: { host: "127.0.0.1", port: 0 },
  });
  server.middlewares.stack.unshift({
    route: "",
    handle(
      req: IncomingMessage,
      res: ServerResponse,
      next: Connect.NextFunction,
    ) {
      const url = new URL(req.url ?? "/", "http://localhost");
      const json = (value: unknown, status = 200) => {
        res.writeHead(
          status,
          apiSchemaHeaders({ "Content-Type": "application/json" }),
        );
        res.end(JSON.stringify(value));
      };
      if (url.pathname === "/check") res.end("<!doctype html>");
      else if (url.pathname === "/api/sync/identity")
        json(syncIdentityFixture(workspaceId));
      else if (url.pathname === "/api/sync/protocol")
        json(syncProtocolFixture());
      else if (url.pathname === "/api/sync/stream") {
        res.writeHead(200, { "Content-Type": "text/event-stream" });
        res.write(apiSchemaHandshakeSse());
        const resources = fixtureResources(url.searchParams.get("resources"));
        res.write(
          `event: resources\ndata: ${JSON.stringify({
            protocol: 4,
            workspaceId,
            epoch: "fixture-epoch",
            revision: 0,
            reason: "initial",
            resources,
            resourceVersions: resources.map((resource) => ({
              resource,
              revision: 0,
            })),
          })}\n\n`,
        );
        const stream = {
          response: res,
          resources: fixtureResources(url.searchParams.get("resources")),
        };
        streams.add(stream);
        res.on("close", () => streams.delete(stream));
      } else if (url.pathname === "/api/sync/pull") {
        const scope = url.searchParams.get("scope") ?? "";
        const kind = scope.slice(11);
        const count = (attempts.get(kind) || 0) + 1;
        attempts.set(kind, count);
        if (kind === "503" && count === 1)
          return json({ error: "Temporary failure" }, Number(kind));
        if (kind === "403") return json({ error: "Access denied" }, 403);
        json({
          workspaceId: kind === "workspace" ? "f".repeat(32) : workspaceId,
          documents: [
            {
              id: scope,
              seq: 1,
              payload:
                kind === "decode" && count === 1
                  ? "invalid-json"
                  : JSON.stringify({ items: [] }),
            },
          ],
          checkpoint: { seq: 1 },
        } satisfies FixturePullResponse);
      } else next();
    },
  });
  await server.listen();
  try {
    const page = fixturePage;
    const errors: string[] = [];
    page.on("pageerror", (error) => errors.push(error.message));
    await page.clock.install();
    await page.goto(fixtureOrigin(server) + `/check`);
    await page.evaluate(async () => {
      window.syncSyncReadRetryBrowserClient =
        await import("/src/sync/client.ts");
      const { readRetryDelay } = await import("/src/sync/readRetry.ts");
      window.syncSyncReadRetryBrowserRetryDelay = readRetryDelay(0);
      window.syncSyncReadRetryBrowserStates = [];
      window.syncSyncReadRetryBrowserStop =
        window.syncSyncReadRetryBrowserClient.subscribeTranscriptProjection(
          "transcript:503",
          () => {},
          (error) =>
            window.syncSyncReadRetryBrowserStates.push(
              error ? String(error) : "live",
            ),
        );
    });
    await expect.poll(() => attempts.get("503")).toBe(1);
    const retryDelay = await page.evaluate(
      () => window.syncSyncReadRetryBrowserRetryDelay,
    );
    await page.clock.runFor(retryDelay);
    await page.waitForFunction(
      () => window.syncSyncReadRetryBrowserStates.at(-1) === "live",
    );
    assert.equal(
      attempts.get("503"),
      2,
      "A live stream recovers a failed HTTP read",
    );
    await page.clock.runFor(2 * retryDelay);
    assert.equal(attempts.get("503"), 2, "A successful read stops retrying");
    await page.evaluate(() => window.syncSyncReadRetryBrowserStop());
    for (const kind of ["403", "workspace"]) {
      await page.evaluate((kind) => {
        window.syncSyncReadRetryBrowserStates = [];
        window.syncSyncReadRetryBrowserStop =
          window.syncSyncReadRetryBrowserClient.subscribeTranscriptProjection(
            `transcript:${kind}`,
            () => {},
            (error) =>
              window.syncSyncReadRetryBrowserStates.push(
                error ? String(error) : "live",
              ),
          );
      }, kind);
      await page.waitForFunction(
        () => window.syncSyncReadRetryBrowserStates.length > 0,
      );
      await page.clock.runFor(2 * retryDelay);
      assert.equal(
        attempts.get(kind),
        1,
        `${kind} must not authorize a retry.`,
      );
      assert.ok(
        await page.evaluate(
          () => !window.syncSyncReadRetryBrowserStates.includes("live"),
        ),
      );
      await page.evaluate(() => window.syncSyncReadRetryBrowserStop());
    }
    await page.evaluate(() => {
      window.syncSyncReadRetryBrowserStates = [];
      window.syncSyncReadRetryBrowserStop =
        window.syncSyncReadRetryBrowserClient.subscribeTranscriptProjection(
          "transcript:decode",
          () => {},
          (error) =>
            window.syncSyncReadRetryBrowserStates.push(
              error ? String(error) : "live",
            ),
        );
    });
    await page.waitForFunction(() =>
      window.syncSyncReadRetryBrowserStates.some((state) => state !== "live"),
    );
    invalidate("decode");
    await page.waitForFunction(
      () => window.syncSyncReadRetryBrowserStates.at(-1) === "live",
    );
    assert.equal(
      attempts.get("decode"),
      2,
      "A new stream hint must recover a malformed projection response.",
    );
    await page.evaluate(() => window.syncSyncReadRetryBrowserStop());
    expect(errors).toEqual([]);
    console.log("sync read retry browser contract passed");
  } finally {
    for (const stream of streams) stream.response.end();
    await server.close();
  }
});

declare global {
  interface Window {
    syncSyncReadRetryBrowserClient: typeof import("../../src/sync/client.ts");
    syncSyncReadRetryBrowserRetryDelay: number;
    syncSyncReadRetryBrowserStates: string[];
    syncSyncReadRetryBrowserStop: () => void;
  }
}
