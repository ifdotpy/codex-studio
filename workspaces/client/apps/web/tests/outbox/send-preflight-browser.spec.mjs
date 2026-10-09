// Real outbox and API calls against a local fixture. No native model requests.
import assert from "node:assert/strict";
import { createRequire } from "node:module";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, dirname } from "node:path";
import { fileURLToPath } from "node:url";

import { test, expect } from "../playwright.mjs";

test("send-preflight-browser", async ({ page: fixturePage }) => {
  test.setTimeout(120_000);
  const root = dirname(
    dirname(dirname(dirname(fileURLToPath(import.meta.url)))),
  );
  const require = createRequire(join(root, "web/package.json"));
  const { createServer } = await import(require.resolve("vite"));
  const temporary = await mkdtemp(join(tmpdir(), "studio-send-preflight-"));
  const server = await createServer({
    configFile: false,
    root: join(root, "web"),
    cacheDir: join(temporary, "vite"),
    optimizeDeps: { include: ["react"] },
    server: { host: "127.0.0.1", port: 0, hmr: false },
  });
  await server.listen();
  const origin = server.resolvedUrls.local[0];
  const workspaceId = "c".repeat(32);
  try {
    const page = fixturePage;
    const errors = [],
      posts = [],
      effects = new Set();
    let preflight;
    let loseResponse = false;
    page.on("pageerror", (error) => errors.push(error.message));
    await page.route("**/check", (route) =>
      route.fulfill({
        contentType: "text/html",
        body: "<!doctype html><title>Send preflight</title>",
      }),
    );
    await page.route("**/api/sync/identity", (route) => {
      if (preflight) preflight.identities.push(route);
      else return route.fulfill({ json: { workspaceId } });
    });
    await page.route("**/api/session", (route) => {
      if (preflight) preflight.sessions.push(route);
      else return route.fulfill({ json: { token: "fixture" } });
    });
    await page.route("**/api/messages", async (route) => {
      const body = route.request().postDataJSON();
      posts.push({ body, headers: route.request().headers() });
      effects.add(body.id);
      if (loseResponse) {
        loseResponse = false;
        await route.abort("failed");
      } else
        await route.fulfill({ json: { id: body.id, status: "delivered" } });
    });
    await page.goto(origin + "check");
    await page.evaluate(async () => {
      window.send = await import("/src/sync/send.ts");
      window.storage = await (
        await import("/src/sync/client.ts")
      ).syncDatabase();
      window.read = async (id) => {
        const doc = await window.storage.db.outbox.findOne(id).exec();
        return doc && JSON.parse(doc.getLatest().payload);
      };
      window.start = (body, copies) => {
        window.result = Promise.all(
          Array.from({ length: copies }, () =>
            window.send.durableSend(body).catch((error) => ({
              error: error.message,
              code: error.status,
            })),
          ),
        );
      };
    });
    const body = (id) => ({
      id,
      room: `chat-${id}`,
      text: `Exact content ${id}`,
      assets: [],
      delivery: "queue",
    });
    const until = async (check, message) => {
      const deadline = Date.now() + 1500;
      while (!(await check())) {
        assert.ok(Date.now() < deadline, message);
        await new Promise((resolve) => setTimeout(resolve, 10));
      }
    };
    const arm = () => {
      preflight = { identities: [], sessions: [] };
    };
    const begin = (value, copies = 1) =>
      page.evaluate(({ value, copies }) => window.start(value, copies), {
        value,
        copies,
      });
    const settle = () => page.evaluate(() => window.result);
    const stored = (id) => page.evaluate((id) => window.read(id), id);
    const prerequisites = () =>
      until(
        () =>
          preflight.identities.length === 1 && preflight.sessions.length === 1,
        "Both prerequisite reads must start before either response arrives",
      );
    const identity = (value = workspaceId, status = 200) =>
      preflight.identities[0].fulfill({
        status,
        json:
          status === 200
            ? { workspaceId: value }
            : { error: "Identity failed" },
      });
    const session = (status = 200) =>
      preflight.sessions[0].fulfill({
        status,
        json:
          status === 200 ? { token: "fixture" } : { error: "Session failed" },
      });

    // Holding identity must not prevent the session read. Concurrent callers share
    // the existing message task and cannot multiply its reads or POST.
    arm();
    const parallel = body("parallel-prerequisites");
    await begin(parallel, 2);
    await prerequisites();
    assert.equal(posts.length, 0);
    await session();
    assert.equal((await stored(parallel.id)).attempted, false);
    assert.equal(
      posts.length,
      0,
      "Session success cannot authorize a POST alone",
    );
    await identity();
    assert.deepEqual(
      (await settle()).map((result) => result.status),
      ["delivered", "delivered"],
    );
    assert.equal(posts.length, 1);
    assert.deepEqual(posts[0].body, parallel);
    assert.equal(posts[0].headers["x-canvas-workspace"], workspaceId);
    assert.equal(posts[0].headers["x-canvas-token"], "fixture");
    await begin(parallel);
    assert.equal((await settle())[0].status, "delivered");
    assert.equal(preflight.identities.length, 1);
    assert.equal(preflight.sessions.length, 1);
    assert.equal(posts.length, 1, "An accepted message must not be sent again");

    // The other prerequisite can finish first without bypassing workspace checks.
    arm();
    const changedWorkspace = body("different-workspace");
    await begin(changedWorkspace);
    await prerequisites();
    await session();
    await identity("d".repeat(32));
    assert.equal((await settle())[0].code, 409);
    assert.equal((await stored(changedWorkspace.id)).attempted, false);
    assert.equal(posts.length, 1);

    // Either failed prerequisite keeps the command off the network.
    arm();
    const failedIdentity = body("failed-identity");
    await begin(failedIdentity);
    await prerequisites();
    await identity(workspaceId, 503);
    assert.equal((await settle())[0].queued, true);
    assert.equal((await stored(failedIdentity.id)).attempted, false);
    await session();
    assert.equal(posts.length, 1);
    arm();
    const failedSession = body("failed-session");
    await begin(failedSession);
    await prerequisites();
    await identity();
    await session(403);
    assert.equal((await settle())[0].code, 403);
    assert.equal((await stored(failedSession.id)).attempted, false);
    assert.equal(posts.length, 1);

    // Queue controls remain effective while the concurrent reads are pending.
    for (const action of ["cancel", "pause", "remove"]) {
      arm();
      const pending = body(`preflight-${action}`);
      await begin(pending);
      await prerequisites();
      await page.evaluate(
        ({ id, action }) =>
          action === "remove"
            ? window.send.stopRemovedMessage(id)
            : window.send.changeOutbox(id, action),
        { id: pending.id, action },
      );
      await identity();
      await session();
      assert.equal(
        (await settle())[0].status,
        action === "pause" ? "paused" : "cancelled",
      );
      assert.equal((await stored(pending.id)).attempted, false);
      assert.equal(posts.length, 1);
    }

    // A lost successful response retains the same payload and request ID. The
    // fixture applies one logical command despite an exact HTTP retry.
    preflight = undefined;
    loseResponse = true;
    const lost = body("lost-response");
    await begin(lost);
    assert.equal((await settle())[0].queued, true);
    assert.equal((await stored(lost.id)).attempted, true);
    await begin(lost);
    assert.equal((await settle())[0].status, "delivered");
    assert.deepEqual(
      posts.filter((post) => post.body.id === lost.id).map((post) => post.body),
      [lost, lost],
    );
    assert.deepEqual([...effects], [parallel.id, lost.id]);
    await begin({ ...lost, text: "Changed command" });
    assert.match((await settle())[0].error, /different content/);
    assert.equal(posts.length, 3);
    expect(errors).toEqual([]);
    console.log(
      "PASS: concurrent prerequisite reads, workspace and failure guards, cancellation, immutable IDs, exact response-loss retry, and accepted-message deduplication",
    );
  } finally {
    await server.close();
    await rm(temporary, { recursive: true, force: true });
  }
});
