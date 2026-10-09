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
} from "../../tests/playwright.mjs";

test("state stream persists records across tabs without pulls and repairs missing ranges", async ({
  browser,
}) => {
  test.setTimeout(60_000);
  const { createServer } =
    await import("../../node_modules/vite/dist/node/index.js");
  const workspaceId = "b".repeat(32);
  const streams = new Set();
  const pulls = [];
  const rows = new Map();
  let opened = 0;
  let sequence = 0;
  let revision = 0;
  let epoch = "entity-stream-first";
  const put = (id, name, deleted = false) => {
    const row = {
      id: `entity:agent:${id}`,
      seq: ++sequence,
      payload: JSON.stringify({
        collection: "agent",
        id,
        value: deleted
          ? {}
          : { id, name, status: "running", source: "managed" },
      }),
      _deleted: deleted,
    };
    rows.set(row.id, row);
    return row;
  };
  put("lead", "Initial lead");
  put("removed", "Remove me");

  const frame = (reason, documents, after, reset = false) => ({
    protocol: 3,
    workspaceId,
    epoch,
    revision: ++revision,
    reason,
    resources: [{ kind: "state" }],
    resourceVersions: [
      {
        revision: sequence,
        ...(documents
          ? {
              entitySequences: documents.map((row) => row.seq),
              entityChanges: { after, through: sequence, documents },
            }
          : {}),
        ...(reset ? { entitySequenceReset: true } : {}),
      },
    ],
  });
  const send = (event) => {
    for (const response of streams)
      response.write(protocol3SseEvent("resources", event));
  };
  const cacheDir = await mkdtemp(
    join(tmpdir(), "studio-entity-delivery-vite-"),
  );
  const server = await createServer({
    configFile: false,
    cacheDir,
    root: fileURLToPath(new URL("../..", import.meta.url)),
    server: { host: "127.0.0.1", port: 0 },
    plugins: [
      {
        name: "resource-entity-delivery-fixture",
        configureServer(server) {
          server.middlewares.use((request, response, next) => {
            const url = new URL(request.url, "http://localhost");
            if (url.pathname === "/entity-delivery-check") {
              response.setHeader("Content-Type", "text/html");
              response.end(
                "<!doctype html><title>Entity delivery check</title>",
              );
              return;
            }
            if (url.pathname === "/api/sync/stream") {
              response.setHeader("Content-Type", "text/event-stream");
              response.setHeader("Cache-Control", "no-cache");
              response.write(apiSchemaHandshakeSse());
              response.write(
                protocol3SseEvent(
                  "resources",
                  frame(opened++ ? "reconnect" : "initial"),
                ),
              );
              streams.add(response);
              response.on("close", () => streams.delete(response));
              return;
            }
            if (url.pathname === "/api/sync/identity") {
              response.setHeader("Content-Type", "application/json");
              response.setHeader(API_SCHEMA_HASH_HEADER, readApiSchemaHash());
              response.end(JSON.stringify({ workspaceId }));
              return;
            }
            if (url.pathname === "/api/sync/pull") {
              const scope = url.searchParams.get("scope");
              const after = Number(url.searchParams.get("after"));
              pulls.push({ scope, after });
              response.setHeader("Content-Type", "application/json");
              response.setHeader(API_SCHEMA_HASH_HEADER, readApiSchemaHash());
              response.end(
                JSON.stringify({
                  workspaceId,
                  documents: [...rows.values()]
                    .filter((row) => row.seq > after)
                    .sort((left, right) => left.seq - right.seq),
                  checkpoint: { seq: sequence },
                  initialHigh: Number(url.searchParams.get("initialHigh") || 0),
                  maxSeq: sequence,
                }),
              );
              return;
            }
            next();
          });
        },
      },
    ],
  });
  const context = await browser.newContext();
  const pages = [];
  const errors = [];
  const openPage = async () => {
    const page = await context.newPage();
    pages.push(page);
    page.on("pageerror", (error) => errors.push(error.message));
    await page.goto(
      `http://127.0.0.1:${server.httpServer.address().port}/entity-delivery-check`,
    );
    await page.evaluate(async () => {
      const client = await import("/src/sync/client.ts");
      window.snapshot = null;
      window.syncErrors = [];
      window.connection = "closed";
      window.stopConnection = client.watchResourceConnection(
        (state) => (window.connection = state),
      );
      window.stopProjection = client.subscribeStateProjection(
        (snapshot) => (window.snapshot = snapshot),
        (error) => {
          if (error) window.syncErrors.push(String(error));
        },
      );
      window.checkpoint = async () => {
        const { db } = await client.syncDatabase();
        return (
          await db.projections.findOne("state:entities:checkpoint").exec()
        )?.seq;
      };
    });
    return page;
  };
  const waitForAgent = (page, id, name) =>
    page.waitForFunction(
      ({ id, name }) =>
        window.snapshot?.runtime.agents.find((agent) => agent.id === id)
          ?.name === name,
      { id, name },
    );
  const waitForCheckpoint = (page, seq) =>
    expect.poll(() => page.evaluate(() => window.checkpoint())).toBe(seq);
  const settle = () => new Promise((resolve) => setTimeout(resolve, 150));
  try {
    await server.listen();
    const page = await openPage();
    await waitForAgent(page, "lead", "Initial lead");
    await waitForCheckpoint(page, 2);
    await page.waitForFunction(() => window.connection === "live");
    await settle();
    const initialPulls = pulls.length;
    assert.ok(
      initialPulls > 0,
      "The initial projection comes from the HTTP pull",
    );

    send(frame("change", [put("lead", "Stream update")], 2));
    await waitForAgent(page, "lead", "Stream update");
    await waitForCheckpoint(page, 3);
    await settle();
    assert.equal(
      pulls.length,
      initialPulls,
      "A contiguous stream update needs no HTTP pull",
    );

    send(frame("change", [put("removed", "", true)], 3));
    await page.waitForFunction(
      () =>
        !window.snapshot?.runtime.agents.some(
          (agent) => agent.id === "removed",
        ),
    );
    await waitForCheckpoint(page, 4);
    await settle();
    assert.equal(
      pulls.length,
      initialPulls,
      "A stream tombstone needs no HTTP pull",
    );

    const follower = await openPage();
    await waitForAgent(follower, "lead", "Stream update");
    await follower.waitForFunction(() => window.connection === "live");
    await expect.poll(() => streams.size).toBe(1);
    await settle();
    const beforePeerUpdate = pulls.length;
    send(frame("change", [put("lead", "Shared stream update")], 4));
    await Promise.all(
      pages.map((tab) => waitForAgent(tab, "lead", "Shared stream update")),
    );
    await Promise.all(pages.map((tab) => waitForCheckpoint(tab, 5)));
    await settle();
    assert.equal(
      pulls.length,
      beforePeerUpdate,
      "Both tabs use the same durable stream update without pulls",
    );

    put("missing", "Missing range row");
    const gapRow = put("lead", "After gap");
    const beforeGap = pulls.length;
    send(frame("change", [gapRow], 6));
    await Promise.all(
      pages.map((tab) => waitForAgent(tab, "missing", "Missing range row")),
    );
    await waitForAgent(page, "lead", "After gap");
    await waitForCheckpoint(page, 7);
    assert.ok(
      pulls.length > beforeGap,
      "A missing sequence range requires an HTTP pull",
    );
    assert.ok(
      pulls.slice(beforeGap).some((pull) => pull.after <= 5),
      "The pull must include the missing row",
    );

    epoch = "entity-stream-restarted";
    const beforeEpoch = pulls.length;
    send(frame("change", [put("lead", "New epoch")], 7));
    await Promise.all(
      pages.map((tab) => waitForAgent(tab, "lead", "New epoch")),
    );
    await waitForCheckpoint(page, 8);
    assert.ok(
      pulls.length > beforeEpoch,
      "A changed server epoch requires reconciliation",
    );

    put("overflow", "Overflow recovery");
    const beforeOverflow = pulls.length;
    send(frame("overflow", undefined, undefined, true));
    await Promise.all(
      pages.map((tab) => waitForAgent(tab, "overflow", "Overflow recovery")),
    );
    await waitForCheckpoint(page, 9);
    assert.ok(
      pulls.length > beforeOverflow,
      "Overflow requires a complete range pull",
    );

    const beforeReconnect = pulls.length;
    const beforeOpen = opened;
    for (const response of streams) response.end();
    put("lead", "After reconnect");
    await expect
      .poll(() => opened, { timeout: 20_000 })
      .toBeGreaterThan(beforeOpen);
    await Promise.all(
      pages.map((tab) => waitForAgent(tab, "lead", "After reconnect")),
    );
    await waitForCheckpoint(page, 10);
    assert.ok(
      pulls.length > beforeReconnect,
      "Reconnect fills changes missed while the stream was closed",
    );
    assert.ok(pulls.every((pull) => pull.scope === "state:entities:v1"));
    assert.deepEqual(errors, []);
    for (const tab of pages)
      assert.deepEqual(await tab.evaluate(() => window.syncErrors), []);
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
