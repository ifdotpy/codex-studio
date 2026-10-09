// Unsupported endpoints fail visibly and never bypass the durable outbox.
import assert from "node:assert/strict";
import { fileURLToPath } from "node:url";
import { test, expect, apiSchemaHandshakeSse } from "../playwright.mjs";

test("sync required browser", async ({ page: runnerPage }) => {
  const { createServer } = await import(
    new URL(
      "../../../web/node_modules/vite/dist/node/index.js",
      import.meta.url,
    )
  );
  const server = await createServer({
    configFile: false,
    root: fileURLToPath(new URL("../../../web", import.meta.url)),
    server: { host: "127.0.0.1", port: 0 },
  });
  await server.listen();
  try {
    const page = runnerPage,
      errors = [];
    page.on("pageerror", (error) => errors.push(error.message));
    let status = 404,
      probes = 0,
      sends = 0;
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
    const url = `http://127.0.0.1:${server.httpServer.address().port}/sync-check`;
    const mount = async () =>
      page.evaluate(async () => {
        const { useOutbox } = await import("/src/sync/send.ts");
        const { useSyncedDrafts } = await import("/src/sync/drafts.ts");
        const r = await import("/node_modules/.vite/deps/react.js"),
          d = await import("/node_modules/.vite/deps/react-dom_client.js");
        const React = r.default || r,
          { createRoot } = d.default || d;
        const root = document.createElement("div");
        document.body.appendChild(root);
        createRoot(root).render(
          React.createElement(function Harness() {
            const outbox = useOutbox(),
              drafts = useSyncedDrafts();
            window.state = { outbox, drafts };
            return null;
          }),
        );
      });
    await page.goto(url);
    await mount();
    await page.waitForFunction(() => window.state);
    await page.evaluate(() =>
      window.state.drafts.setDrafts({ lead: "Saved local draft" }),
    );
    await page.waitForTimeout(250);
    assert.ok(probes > 0);
    assert.equal(
      await page.evaluate(
        () =>
          JSON.parse(localStorage.getItem("codex-chat-draft:unassigned:lead"))
            ?.text,
      ),
      "Saved local draft",
    );
    assert.equal(
      await page.evaluate(() => localStorage.getItem("codex-drafts:legacy")),
      null,
    );
    await page.waitForFunction(
      () => window.state.outbox.error && window.state.drafts.error,
    );
    const sendError = await page.evaluate(async () => {
      try {
        await (
          await import("/src/sync/send.ts")
        ).durableSend({ id: "missing-sync", room: "lead", text: "message" });
      } catch (error) {
        return error.message;
      }
    });
    assert.match(sendError, /Identity unavailable/);
    assert.equal(sends, 0);
    await page.reload();
    await mount();
    await page.waitForFunction(
      () => window.state?.drafts.drafts.lead === "Saved local draft",
    );
    status = 503;
    await page.reload();
    await mount();
    await page.waitForFunction(
      () => window.state?.drafts.error && window.state?.outbox.error,
    );
    assert.equal(
      await page.evaluate(() => window.state.drafts.error),
      "Draft sync paused. Retrying automatically.",
    );
    // Initial connection failures retry without a reload or loss of the local draft.
    await page.route("**/api/sync/pull?*", (route) =>
      route.fulfill({
        json: {
          workspaceId: "a".repeat(32),
          documents: [],
          checkpoint: { seq: 0 },
        },
      }),
    );
    await page.route("**/api/sync/drafts", (route) =>
      route.fulfill({ json: [] }),
    );
    status = 200;
    await page.waitForFunction(() => window.state.drafts.error === "");
    assert.equal(
      await page.evaluate(() => window.state.drafts.drafts.lead),
      "Saved local draft",
    );
    // Successful remote reads cannot clear a local storage failure.
    await page.evaluate(async () => {
      const { db } = await (await import("/src/sync/client.ts")).syncDatabase();
      const original = db.drafts.incrementalUpsert;
      window.restoreDraftWrites = () => {
        db.drafts.incrementalUpsert = original;
      };
      db.drafts.incrementalUpsert = async () => {
        throw new Error("Local write fixture");
      };
      window.state.drafts.setDrafts({ lead: "Unsaved local edit" });
    });
    await page.waitForFunction(() =>
      window.state.drafts.error.includes("could not be saved"),
    );
    await page.waitForTimeout(3500);
    assert.match(
      await page.evaluate(() => window.state.drafts.error),
      /could not be saved/,
    );
    await page.evaluate(() => {
      window.restoreDraftWrites();
      window.state.drafts.setDrafts({ lead: "Recovered local edit" });
    });
    await page.waitForFunction(() => window.state.drafts.error === "");
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
  const { createServer } = await import(
    new URL(
      "../../../web/node_modules/vite/dist/node/index.js",
      import.meta.url,
    )
  );
  const workspaceId = "c".repeat(32);
  let identityStatus = 503;
  let identityRequests = 0;
  let draftPulls = 0;
  let draftPushes = 0;
  const streams = new Set();
  const server = await createServer({
    configFile: false,
    root: fileURLToPath(new URL("../../../web", import.meta.url)),
    server: { host: "127.0.0.1", port: 0 },
  });
  server.middlewares.stack.unshift({
    route: "",
    handle(req, res, next) {
      const url = new URL(req.url, "http://localhost");
      const json = (value, status = 200) => {
        res.writeHead(status, { "Content-Type": "application/json" });
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
        const resources = JSON.parse(url.searchParams.get("resources") || "[]");
        res.writeHead(200, {
          "Content-Type": "text/event-stream",
          "Cache-Control": "no-cache",
        });
        res.write(apiSchemaHandshakeSse());
        res.write(
          `event: resources\ndata: ${JSON.stringify({
            protocol: 3,
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
    const errors = [];
    page.on("pageerror", (error) => errors.push(error.message));
    await page.clock.install();
    await page.goto(
      `http://127.0.0.1:${server.httpServer.address().port}/check`,
    );
    await page.evaluate(async () => {
      const { useSyncedDrafts } = await import("/src/sync/drafts.ts");
      const r = await import("/node_modules/.vite/deps/react.js");
      const d = await import("/node_modules/.vite/deps/react-dom_client.js");
      const React = r.default || r;
      const { createRoot } = d.default || d;
      const root = createRoot(document.getElementById("app"));
      root.render(
        React.createElement(function Harness() {
          const drafts = useSyncedDrafts();
          window.state = { drafts };
          return null;
        }),
      );
    });
    const waitForIdentityCount = async (count) => {
      for (let attempt = 0; attempt < 100; attempt++) {
        if (identityRequests >= count) return;
        await page.waitForTimeout(20);
      }
      throw new Error(
        `Expected ${count} identity attempts, got ${identityRequests}`,
      );
    };
    const waitForDraftPull = async () => {
      for (let attempt = 0; attempt < 100; attempt++) {
        if (draftPulls > 0) return;
        await page.waitForTimeout(20);
      }
      throw new Error(
        "Recovered draft replication did not issue its baseline pull",
      );
    };
    const waitForDraftPush = async () => {
      for (let attempt = 0; attempt < 100; attempt++) {
        if (draftPushes > 0) return;
        await page.waitForTimeout(20);
      }
      throw new Error("Retained local draft was not pushed after recovery");
    };
    await waitForIdentityCount(1);
    for (const [delay, expected] of [
      [1_000, 2],
      [3_000, 3],
      [10_000, 4],
    ]) {
      await page.clock.fastForward(delay);
      await waitForIdentityCount(expected);
    }
    await page.waitForFunction(
      () =>
        window.state?.drafts.error ===
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
      window.state.drafts.setDrafts({
        lead: "Retained while identity is unavailable",
      }),
    );
    await waitForIdentityCount(5);
    await page.waitForFunction(
      () =>
        window.state.drafts.drafts.lead ===
        "Retained while identity is unavailable",
    );
    assert.equal(
      await page.evaluate(
        () =>
          JSON.parse(localStorage.getItem("codex-chat-draft:unassigned:lead"))
            ?.text,
      ),
      "Retained while identity is unavailable",
      "Manual retry keeps the local journal copy while the identity request fails",
    );

    identityStatus = 200;
    await page.evaluate(() => window.dispatchEvent(new Event("online")));
    await waitForIdentityCount(6);
    await page.waitForFunction(
      () => window.state.drafts.error === "" && window.state.drafts.drafts.lead,
    );
    await waitForDraftPull();
    await waitForDraftPush();
    assert.equal(
      await page.evaluate(() => window.state.drafts.drafts.lead),
      "Retained while identity is unavailable",
    );
    expect(errors).toEqual([]);
  } finally {
    for (const stream of streams) stream.end();
    await server.close();
  }
});
