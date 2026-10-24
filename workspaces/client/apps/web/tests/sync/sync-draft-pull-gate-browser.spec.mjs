import assert from "node:assert/strict";
import { fileURLToPath } from "node:url";

import {
  apiSchemaHandshakeSse,
  apiSchemaHeaders,
  syncIdentityFixture,
  test,
  expect,
} from "../playwright.mjs";

test("draft pull waits for a real trigger without blocking push recovery", async ({
  page: fixturePage,
}) => {
  test.setTimeout(45_000);
  const { createFixtureViteServer: createServer } =
    await import("../playwright.mjs");
  const workspaceId = "c".repeat(32);
  const streams = new Set();
  let pulls = 0;
  let pushes = 0;
  let failPush = false;
  let pullSucceeds = false;
  let revision = 0;
  const notifyDrafts = () => {
    revision++;
    for (const stream of streams) {
      if (!stream.resources.some((resource) => resource.kind === "drafts"))
        continue;
      stream.response.write(
        `event: resources\ndata: ${JSON.stringify({
          protocol: 4,
          workspaceId,
          epoch: "drafts-epoch",
          revision,
          reason: "change",
          resources: [{ kind: "drafts" }],
          resourceVersions: [{ resource: { kind: "drafts" }, revision }],
        })}\n\n`,
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
    handle(req, res, next) {
      const url = new URL(req.url || "/", "http://localhost");
      const json = (value, status = 200) => {
        res.writeHead(
          status,
          apiSchemaHeaders({ "Content-Type": "application/json" }),
        );
        res.end(JSON.stringify(value));
      };
      if (url.pathname === "/check") res.end("<!doctype html>");
      else if (url.pathname === "/api/sync/identity")
        json(syncIdentityFixture(workspaceId));
      else if (url.pathname === "/api/sync/stream") {
        const resources = JSON.parse(url.searchParams.get("resources") || "[]");
        res.writeHead(200, { "Content-Type": "text/event-stream" });
        res.write(apiSchemaHandshakeSse());
        res.write(
          `event: resources\ndata: ${JSON.stringify({
            protocol: 4,
            workspaceId,
            epoch: "drafts-epoch",
            revision,
            reason: "initial",
            resources,
            resourceVersions: resources.map((resource) => ({
              resource,
              revision,
            })),
          })}\n\n`,
        );
        const stream = { response: res, resources };
        streams.add(stream);
        res.on("close", () => streams.delete(stream));
      } else if (url.pathname === "/api/sync/pull") {
        assert.equal(url.searchParams.get("scope"), "drafts");
        pulls++;
        if (!pullSucceeds)
          return json({ error: "temporary pull failure" }, 500);
        json({ workspaceId, documents: [], checkpoint: { seq: 0 } });
      } else if (url.pathname === "/api/sync/drafts") {
        pushes++;
        if (failPush) return json({ error: "temporary push failure" }, 500);
        json([]);
      } else next();
    },
  });
  await server.listen();
  try {
    const page = fixturePage;
    const errors = [];
    page.on("pageerror", (error) => errors.push(error.message));
    await page.clock.install();
    await page.goto(
      `http://127.0.0.1:${server.httpServer.address().port}/check`,
    );
    const draftTiming = await page.evaluate(async () => {
      const client = await import("/src/sync/client.ts");
      window.replicationErrors = [];
      window.rejectCancelOnce = false;
      window.cancelRejections = 0;
      window.draftTiming = client.DRAFT_SYNC_TIMING_MS;
      window.stopDrafts = await client.startDraftReplication(
        (error) =>
          window.replicationErrors.push(
            error
              ? {
                  code: typeof error === "object" ? error.code : undefined,
                  message: String(error),
                }
              : null,
          ),
        {
          testOnly: {
            cancel: async (cancel) => {
              await cancel();
              if (window.rejectCancelOnce) {
                window.rejectCancelOnce = false;
                window.cancelRejections++;
                throw new Error("injected cancel rejection");
              }
            },
          },
        },
      );
      return client.DRAFT_SYNC_TIMING_MS;
    });
    await page.waitForFunction(() => window.replicationErrors.length > 0);
    await page.evaluate(async () => {
      const { db } = await (await import("/src/sync/client.ts")).syncDatabase();
      await db.drafts.insert({
        id: "local-draft",
        payload: JSON.stringify({ session: "chat", text: "pending push" }),
        seq: 1,
      });
    });
    await expect.poll(() => pushes).toBeGreaterThan(0);
    assert.equal(pushes, 1, "A failed pull does not freeze the pending push");
    await page.clock.runFor(
      draftTiming.pushRetry + draftTiming.pushQuietWait + 100,
    );
    assert.equal(
      pulls,
      1,
      "The failed pull does not run periodic HTTP retries",
    );
    assert.ok(
      await page.evaluate(() => window.replicationErrors.some(Boolean)),
      "The failed pull remains visible while idle",
    );

    pullSucceeds = true;
    notifyDrafts();
    await page.waitForFunction(() =>
      window.replicationErrors.some((error) => error === null),
    );
    assert.equal(pulls, 2, "One draft event retries the failed pull once");
    assert.equal(pushes, 1);

    const healthyBurstStart = pulls;
    for (let index = 0; index < 6; index++) notifyDrafts();
    await expect.poll(() => pulls).toBeGreaterThan(healthyBurstStart);
    await page.clock.runFor(250);
    assert.equal(
      pulls - healthyBurstStart,
      1,
      "A healthy burst coalesces to one pull",
    );

    pullSucceeds = false;
    const outageStart = pulls;
    notifyDrafts();
    await expect.poll(() => pulls).toBeGreaterThan(outageStart);
    assert.equal(pulls, outageStart + 1, "The outage trigger starts one pull");
    await page.clock.runFor(
      draftTiming.pushRetry + draftTiming.pushQuietWait + 100,
    );
    assert.equal(
      pulls,
      outageStart + 1,
      "One trigger after a healthy burst must not leave retry tokens",
    );
    pullSucceeds = true;
    failPush = true;
    const pushBeforeRestart = pushes;
    const errorsBeforePush = await page.evaluate(
      () => window.replicationErrors.length,
    );
    await page.evaluate(async () => {
      const { db } = await (await import("/src/sync/client.ts")).syncDatabase();
      await db.drafts.insert({
        id: "restart-cancel-probe",
        payload: JSON.stringify({ session: "chat", text: "retry me" }),
        seq: 1,
      });
    });
    await expect.poll(() => pushes).toBeGreaterThan(pushBeforeRestart);
    assert.ok(pushes > pushBeforeRestart, "A push failure is observed");
    await page.waitForFunction(
      (count) => window.replicationErrors.slice(count).some(Boolean),
      errorsBeforePush,
    );
    const beforeRestartPull = pulls;
    await page.evaluate(() => {
      window.rejectCancelOnce = true;
      window.dispatchEvent(new Event("online"));
    });
    await expect.poll(() => pulls).toBeGreaterThan(beforeRestartPull);
    assert.ok(
      pulls > beforeRestartPull,
      "A replacement starts after cancel rejects",
    );
    assert.ok(
      await page.evaluate(() => window.cancelRejections > 0),
      "the cancel seam rejected and the replacement still started",
    );
    failPush = false;
    const postsBeforeRetry = pushes;
    await page.clock.runFor(draftTiming.pushRetry + draftTiming.pushQuietWait);
    await expect.poll(() => pushes).toBeGreaterThan(postsBeforeRetry);
    await page.evaluate(async () => {
      window.rejectCancelOnce = true;
      await window.stopDrafts();
    });
    assert.ok(
      await page.evaluate(() =>
        window.replicationErrors.some((error) =>
          (error?.message || String(error)).includes(
            "injected cancel rejection",
          ),
        ),
      ),
      "A stop cancellation rejection is reported through replication status",
    );
    expect(errors).toEqual([]);
  } finally {
    for (const stream of streams) stream.response.end();
    await server.close();
  }
});
