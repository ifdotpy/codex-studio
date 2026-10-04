import assert from "node:assert/strict";
import { fileURLToPath } from "node:url";

import { test, expect } from "../playwright.mjs";

test("draft pull waits for a real trigger without blocking push recovery", async ({
  page: fixturePage,
}) => {
  test.setTimeout(45_000);
  const { createServer } = await import(
    new URL(
      "../../../web/node_modules/vite/dist/node/index.js",
      import.meta.url,
    )
  );
  const workspaceId = "c".repeat(32);
  const streams = new Set();
  let pulls = 0;
  let pushes = 0;
  let pullSucceeds = false;
  let revision = 0;
  const notifyDrafts = () => {
    revision++;
    for (const stream of streams) {
      if (!stream.resources.some((resource) => resource.kind === "drafts"))
        continue;
      stream.response.write(
        `event: resources\ndata: ${JSON.stringify({
          protocol: 3,
          workspaceId,
          epoch: "drafts-epoch",
          revision,
          reason: "change",
          resources: [{ kind: "drafts" }],
        })}\n\n`,
      );
    }
  };
  const server = await createServer({
    configFile: false,
    root: fileURLToPath(new URL("../../../web", import.meta.url)),
    server: { host: "127.0.0.1", port: 0 },
  });
  server.middlewares.stack.unshift({
    route: "",
    handle(req, res, next) {
      const url = new URL(req.url || "/", "http://localhost");
      const json = (value, status = 200) => {
        res.writeHead(status, { "Content-Type": "application/json" });
        res.end(JSON.stringify(value));
      };
      if (url.pathname === "/check") res.end("<!doctype html>");
      else if (url.pathname === "/api/sync/identity") json({ workspaceId });
      else if (url.pathname === "/api/sync/stream") {
        const resources = JSON.parse(url.searchParams.get("resources") || "[]");
        res.writeHead(200, { "Content-Type": "text/event-stream" });
        res.write(
          `event: resources\ndata: ${JSON.stringify({
            protocol: 3,
            workspaceId,
            epoch: "drafts-epoch",
            revision,
            reason: "initial",
            resources,
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
        json([]);
      } else next();
    },
  });
  await server.listen();
  try {
    const page = fixturePage;
    const errors = [];
    page.on("pageerror", (error) => errors.push(error.message));
    await page.goto(
      `http://127.0.0.1:${server.httpServer.address().port}/check`,
    );
    await page.evaluate(async () => {
      const client = await import("/src/sync/client.ts");
      window.replicationErrors = [];
      window.stopDrafts = await client.startDraftReplication((error) =>
        window.replicationErrors.push(error ? String(error) : null),
      );
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
    for (let attempt = 0; attempt < 250 && pushes === 0; attempt++)
      await new Promise((resolve) => setTimeout(resolve, 20));
    assert.equal(pushes, 1, "A failed pull does not freeze the pending push");
    await new Promise((resolve) => setTimeout(resolve, 3_400));
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
    expect(errors).toEqual([]);
    await page.evaluate(() => window.stopDrafts());
  } finally {
    for (const stream of streams) stream.response.end();
    await server.close();
  }
});
