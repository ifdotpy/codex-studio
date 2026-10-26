import type { FixturePullResponse } from "./helpers.ts";
import { fixtureResources } from "./helpers.ts";
import type {} from "./sync-required-browser.fixture.tsx";
import type { IncomingMessage, ServerResponse } from "node:http";
import type { Connect } from "vite";
import { fixtureOrigin } from "./helpers.ts";
// Unsupported endpoints fail visibly and never bypass the durable outbox.
import assert from "node:assert/strict";
import { fileURLToPath } from "node:url";
import {
  apiSchemaHeaders,
  apiSchemaHandshakeSse,
  FIXTURE_OPTIMIZE_DEPS,
  installApiSchemaFixtureHeaders,
  test,
  expect,
} from "../playwright.ts";

test("sync required browser", async ({ page: runnerPage }) => {
  const { createFixtureViteServer: createServer } =
    await import("../playwright.ts");
  const server = await createServer({
    configFile: false,
    root: fileURLToPath(new URL("../../../web", import.meta.url)),
    optimizeDeps: {
      ...FIXTURE_OPTIMIZE_DEPS,
      include: [
        ...FIXTURE_OPTIMIZE_DEPS.include,
        "rxdb",
        "rxdb/plugins/storage-dexie",
        "rxdb/plugins/leader-election",
        "rxdb/plugins/replication",
      ],
    },
    server: { host: "127.0.0.1", port: 0 },
  });
  await server.listen();
  try {
    const page = runnerPage,
      errors: string[] = [];
    page.on("pageerror", (error) => errors.push(error.message));
    let status = 404,
      probes = 0,
      sends = 0;
    installApiSchemaFixtureHeaders(page);
    await page.route("**/sync-check", (route) =>
      route.fulfill({
        contentType: "text/html",
        body: "<!doctype html><title>Required sync</title>",
      }),
    );
    await page.route("**/api/sync/identity", (route) => {
      probes++;
      return route.fulfill({
        status,
        json:
          status === 200
            ? { workspaceId: "a".repeat(32) }
            : { error: "Identity unavailable" },
      });
    });
    await page.route("**/api/messages", (route) => {
      sends++;
      return route.fulfill({ json: { status: "queued" } });
    });
    const url = fixtureOrigin(server) + `/sync-check`;
    const mount = async () => (
      await page.addScriptTag({
        type: "module",
        url: "/tests/sync/sync-required-browser.fixture.tsx",
      }),
      await page.evaluate(() => window.syncRequiredBrowserMount1())
    );
    await page.clock.install();
    await page.goto(url);
    await mount();
    await page.waitForFunction(() => window.syncRequiredBrowserFixtureState);
    await page.evaluate(() =>
      window.syncRequiredBrowserFixtureState.drafts.setDrafts({
        lead: "Saved local draft",
      }),
    );
    await page.waitForFunction(
      () =>
        JSON.parse(
          localStorage.getItem("codex-chat-draft:unassigned:lead") ?? "null",
        )?.text === "Saved local draft",
    );
    await expect.poll(() => probes).toBeGreaterThan(0);
    assert.equal(
      await page.evaluate(() => localStorage.getItem("codex-drafts:legacy")),
      null,
    );
    await page.waitForFunction(
      () =>
        window.syncRequiredBrowserFixtureState.outbox?.error &&
        window.syncRequiredBrowserFixtureState.drafts.error,
    );
    const sendError = await page.evaluate(async () => {
      try {
        await (
          await import("/src/sync/send.ts")
        ).durableSend({ id: "missing-sync", room: "lead", text: "message" });
      } catch (error) {
        return error instanceof Error ? error.message : String(error);
      }
    });
    assert.ok(sendError);
    assert.match(sendError, /Identity unavailable/);
    assert.equal(sends, 0);
    await page.reload();
    await mount();
    await page.waitForFunction(
      () =>
        window.syncRequiredBrowserFixtureState?.drafts.drafts.lead ===
        "Saved local draft",
    );
    status = 503;
    await page.reload();
    await mount();
    await page.waitForFunction(
      () =>
        window.syncRequiredBrowserFixtureState?.drafts.error &&
        window.syncRequiredBrowserFixtureState?.outbox?.error,
    );
    assert.equal(
      await page.evaluate(
        () => window.syncRequiredBrowserFixtureState.drafts.error,
      ),
      "Draft sync paused. Retrying automatically.",
    );
    // Initial connection failures retry without a reload or loss of the local draft.
    await page.route("**/api/sync/pull?*", (route) =>
      route.fulfill({
        json: {
          workspaceId: "a".repeat(32),
          documents: [],
          checkpoint: { seq: 0 },
        } satisfies FixturePullResponse,
      }),
    );
    await page.route("**/api/sync/drafts", (route) =>
      route.fulfill({ json: [] }),
    );
    status = 200;
    await page.waitForFunction(
      () => window.syncRequiredBrowserFixtureState.drafts.error === "",
    );
    assert.equal(
      await page.evaluate(
        () => window.syncRequiredBrowserFixtureState.drafts.drafts.lead,
      ),
      "Saved local draft",
    );
    // Successful remote reads cannot clear a local storage failure.
    await page.evaluate(async () => {
      const { db } = await (await import("/src/sync/client.ts")).syncDatabase();
      const original = db.drafts.incrementalUpsert;
      window.syncRequiredBrowserFixtureRestoreDraftWrites = () => {
        db.drafts.incrementalUpsert = original;
      };
      db.drafts.incrementalUpsert = async () => {
        throw new Error("Local write fixture");
      };
      window.syncRequiredBrowserFixtureState.drafts.setDrafts({
        lead: "Unsaved local edit",
      });
    });
    await page.waitForFunction(() =>
      window.syncRequiredBrowserFixtureState.drafts.error.includes(
        "could not be saved",
      ),
    );
    await page.clock.runFor(3500);
    assert.match(
      await page.evaluate(
        () => window.syncRequiredBrowserFixtureState.drafts.error,
      ),
      /could not be saved/,
    );
    await page.evaluate(() => {
      window.syncRequiredBrowserFixtureRestoreDraftWrites();
      window.syncRequiredBrowserFixtureState.drafts.setDrafts({
        lead: "Recovered local edit",
      });
    });
    await page.waitForFunction(
      () => window.syncRequiredBrowserFixtureState.drafts.error === "",
    );
    expect(errors).toEqual([]);
    console.log("sync endpoint requirement browser contract passed");
  } finally {
    await server.close();
  }
});

test("draft bootstrap retries pause and recover on explicit activity", async ({
  page: runnerPage,
}) => {
  test.setTimeout(120_000);
  const { createFixtureViteServer: createServer } =
    await import("../playwright.ts");
  const workspaceId = "c".repeat(32);
  let identityStatus = 503;
  let identityRequests = 0;
  let draftPulls = 0;
  let draftPushes = 0;
  const streams = new Set<ServerResponse>();
  const server = await createServer({
    configFile: false,
    root: fileURLToPath(new URL("../../../web", import.meta.url)),
    optimizeDeps: {
      ...FIXTURE_OPTIMIZE_DEPS,
      include: [
        ...FIXTURE_OPTIMIZE_DEPS.include,
        "rxdb",
        "rxdb/plugins/storage-dexie",
        "rxdb/plugins/leader-election",
        "rxdb/plugins/replication",
      ],
    },
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
      if (url.pathname === "/check") {
        res.setHeader("Content-Type", "text/html");
        res.end("<!doctype html><div id=app></div>");
      } else if (url.pathname === "/api/sync/identity") {
        identityRequests++;
        json(
          identityStatus === 200
            ? { workspaceId }
            : { error: "Identity unavailable" },
          identityStatus,
        );
      } else if (url.pathname === "/api/sync/stream") {
        const resources = fixtureResources(url.searchParams.get("resources"));
        res.writeHead(200, {
          "Content-Type": "text/event-stream",
          "Cache-Control": "no-cache",
        });
        res.write(apiSchemaHandshakeSse());
        res.write(
          `event: resources\ndata: ${JSON.stringify({
            protocol: 4,
            workspaceId,
            epoch: "draft-bootstrap",
            revision: 0,
            reason: "initial",
            resources,
            resourceVersions: resources.map((resource) => ({
              resource,
              revision: 0,
            })),
          })}\n\n`,
        );
        streams.add(res);
        res.on("close", () => streams.delete(res));
      } else if (url.pathname === "/api/sync/pull") {
        draftPulls++;
        json({ workspaceId, documents: [], checkpoint: { seq: 0 } });
      } else if (url.pathname === "/api/sync/drafts") {
        draftPushes++;
        json([]);
      } else next();
    },
  });
  await server.listen();
  try {
    const page = runnerPage;
    const errors: string[] = [];
    page.on("pageerror", (error) => errors.push(error.message));
    await page.clock.install();
    await page.goto(fixtureOrigin(server) + `/check`);
    await page.addScriptTag({
      type: "module",
      url: "/tests/sync/sync-required-browser.fixture.tsx",
    });
    await page.evaluate(() => window.syncRequiredBrowserMount2());
    const waitForIdentityCount = async (count: number) => {
      await expect.poll(() => identityRequests).toBeGreaterThanOrEqual(count);
    };
    const waitForDraftPull = async () => {
      await expect.poll(() => draftPulls).toBeGreaterThan(0);
    };
    const waitForDraftPush = async () => {
      await expect.poll(() => draftPushes).toBeGreaterThan(0);
    };
    await waitForIdentityCount(1);
    for (const [delay, expected] of [
      [1_000, 2],
      [3_000, 3],
      [10_000, 4],
    ] as const) {
      await page.clock.runFor(delay);
      await waitForIdentityCount(expected);
    }
    await page.waitForFunction(
      () =>
        window.syncRequiredBrowserFixtureState?.drafts.error ===
        "Draft sync paused. Edit a draft or reconnect to retry.",
    );
    await page.clock.runFor(20_000);
    assert.equal(
      identityRequests,
      4,
      "Bootstrap attempts stop after the finite budget",
    );
    assert.equal(draftPulls, 0, "Failed bootstrap does not start draft pulls");

    await page.evaluate(() =>
      window.syncRequiredBrowserFixtureState.drafts.setDrafts({
        lead: "Retained while identity is unavailable",
      }),
    );
    await waitForIdentityCount(5);
    await page.waitForFunction(
      () =>
        window.syncRequiredBrowserFixtureState.drafts.drafts.lead ===
        "Retained while identity is unavailable",
    );
    assert.equal(
      await page.evaluate(
        () =>
          JSON.parse(
            localStorage.getItem("codex-chat-draft:unassigned:lead") ?? "null",
          )?.text,
      ),
      "Retained while identity is unavailable",
      "Manual retry keeps the local journal copy while the identity request fails",
    );

    identityStatus = 200;
    await page.evaluate(() => window.dispatchEvent(new Event("online")));
    await waitForIdentityCount(6);
    await page.waitForFunction(
      () =>
        window.syncRequiredBrowserFixtureState.drafts.error === "" &&
        window.syncRequiredBrowserFixtureState.drafts.drafts.lead,
    );
    await waitForDraftPull();
    await waitForDraftPush();
    assert.equal(
      await page.evaluate(
        () => window.syncRequiredBrowserFixtureState.drafts.drafts.lead,
      ),
      "Retained while identity is unavailable",
    );
    expect(errors).toEqual([]);
  } finally {
    for (const stream of streams) stream.end();
    await server.close();
  }
});
