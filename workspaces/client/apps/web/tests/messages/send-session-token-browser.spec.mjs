import { test } from "../playwright.mjs";
// Real RxDB and send callers with controlled session and claim delays.
import assert from "node:assert/strict";
import { createRequire } from "node:module";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";

test("Send session token browser", async ({
  browser: testBrowser,
  page: runnerPage,
  context: runnerContext,
}) => {
  test.setTimeout(180_000);
  const root = fileURLToPath(new URL("../../../../../../", import.meta.url));
  const require = createRequire(
    join(root, "workspaces/client/apps/web/package.json"),
  );
  const { createServer } = await import(require.resolve("vite"));
  const temporary = await mkdtemp(join(tmpdir(), "studio-send-session-token-"));
  const workspaceId = "c".repeat(32);
  const server = await createServer({
    configFile: false,
    root: join(root, "web"),
    cacheDir: join(temporary, "vite"),
    optimizeDeps: { include: ["react"] },
    server: { host: "127.0.0.1", port: 0, hmr: false },
  });
  await server.listen();
  try {
    const page = runnerPage;
    page.setDefaultTimeout(5000);
    const errors = [],
      sessions = [],
      posts = [],
      tokenProbes = [],
      uploads = [];
    let holdSessions = false;
    page.on("pageerror", (error) => errors.push(error.message));
    await page.route("**/check", (route) =>
      route.fulfill({
        contentType: "text/html",
        body: "<!doctype html><title>Send session tokens</title>",
      }),
    );
    await page.route("**/api/sync/identity", (route) =>
      route.fulfill({ json: { workspaceId } }),
    );
    await page.route("**/api/session", (route) => {
      sessions.push(route);
      if (sessions.length !== 1 && !holdSessions)
        return route.fulfill({ json: { token: "new-token" } });
    });
    await page.route("**/api/messages", (route) => {
      const body = route.request().postDataJSON();
      const headers = route.request().headers();
      posts.push({ body, headers });
      const current = headers["x-canvas-token"] === "new-token";
      return route.fulfill({
        status: current ? 200 : 403,
        json: current
          ? { id: body.id, status: "delivered" }
          : { error: "Old session token" },
      });
    });
    await page.route("**/api/token-probe", (route) => {
      tokenProbes.push(route.request().headers()["x-canvas-token"]);
      return route.fulfill({ json: { ok: true } });
    });
    await page.route("**/api/assets", (route) => {
      const body = route.request().postDataJSON();
      const headers = route.request().headers();
      uploads.push({ body, headers });
      const current = headers["x-canvas-token"] === "new-token";
      return route.fulfill({
        status: current ? 200 : 403,
        json: current
          ? { id: body.id, name: body.name, mime: "text/plain", size: 16 }
          : { error: "Old upload token" },
      });
    });
    await page.goto(server.resolvedUrls.local[0] + "check");
    await page.evaluate(async () => {
      window.send = await import("/src/sync/send.ts");
      window.api = await import("/src/api.ts");
      window.storage = await (
        await import("/src/sync/client.ts")
      ).syncDatabase();
      window.claimWaiting = false;
      window.claimGate = new Promise(
        (resolve) => (window.releaseClaim = resolve),
      );
      window.start = (body, holdClaim = false) =>
        window.send
          .durableSend(body, [], async () => {
            if (!holdClaim) return;
            const doc = await window.storage.db.outbox.findOne(body.id).exec();
            const modify = doc.incrementalModify.bind(doc);
            let hold = true;
            Object.defineProperty(doc, "incrementalModify", {
              value: async (update) => {
                if (hold) {
                  hold = false;
                  window.claimWaiting = true;
                  await window.claimGate;
                }
                return modify(update);
              },
            });
          })
          .catch((error) => {
            window.sendError = error.message;
            return { code: error.status, error: error.message };
          });
      window.read = async (id) => {
        const doc = await window.storage.db.outbox.findOne(id).exec();
        return JSON.parse(doc.getLatest().payload);
      };
    });
    const body = (id) => ({
      id,
      room: `chat-${id}`,
      text: `Exact message ${id}`,
      assets: [],
      delivery: "after_tool",
    });
    await page.evaluate((body) => {
      window.resultA = window.start(body);
    }, body("old-session"));
    const deadline = Date.now() + 5000;
    while (sessions.length !== 1) {
      assert.ok(Date.now() < deadline, "The first session must start");
      await new Promise((resolve) => setTimeout(resolve, 10));
    }
    await page.evaluate((body) => {
      window.resultB = window.start(body, true);
    }, body("current-session"));
    try {
      await page.waitForFunction(() => window.claimWaiting);
    } catch (error) {
      console.error({
        sendError: await page.evaluate(() => window.sendError),
        posts,
        errors,
      });
      throw error;
    }
    assert.equal(sessions.length, 2);
    assert.equal(posts.length, 0);
    await sessions[0].fulfill({ json: { token: "old-token" } });
    assert.equal(
      (await page.evaluate(() => window.resultA)).status,
      "delivered",
    );
    assert.equal(posts.length, 1);
    assert.deepEqual(posts[0].body, body("old-session"));
    assert.equal(posts[0].headers["x-canvas-token"], "new-token");
    await page.evaluate(async (workspaceId) => {
      await window.api.syncApi(
        "/api/token-probe",
        { id: "after-overlap" },
        {
          workspaceId,
        },
      );
    }, workspaceId);
    assert.deepEqual(tokenProbes, ["new-token"]);
    await page.evaluate(() => {
      // Even an explicit token change after the claim cannot change B's request.
      window.api.setToken("global-token");
      window.releaseClaim();
    });
    assert.equal(
      (await page.evaluate(() => window.resultB)).status,
      "delivered",
    );
    assert.equal(posts.length, 2);
    assert.deepEqual(posts[1].body, body("current-session"));
    assert.equal(posts[1].headers["x-canvas-token"], "new-token");
    assert.equal(posts[1].headers["x-canvas-workspace"], workspaceId);
    assert.equal(
      (await page.evaluate(() => window.read("current-session"))).status,
      "accepted",
    );
    assert.equal(
      (await page.evaluate(() => window.read("old-session"))).status,
      "accepted",
    );
    assert.equal(
      (
        await page.evaluate(
          (body) => window.start(body),
          body("current-session"),
        )
      ).status,
      "delivered",
    );
    assert.equal(posts.length, 2, "A confirmed message must not be sent again");
    assert.equal(sessions.length, 2);
    await page.evaluate(async (workspaceId) => {
      await window.api.syncApi(
        "/api/token-probe",
        { id: "scoped" },
        {
          workspaceId,
          sessionToken: "scoped-token",
        },
      );
      await window.api.syncApi(
        "/api/token-probe",
        { id: "global" },
        { workspaceId },
      );
    }, workspaceId);
    assert.deepEqual(tokenProbes, [
      "new-token",
      "scoped-token",
      "global-token",
    ]);

    // An older response cannot publish while a newer read is still pending.
    holdSessions = true;
    await page.evaluate(() => {
      window.olderRead = window.api.refreshSession();
      window.newerRead = window.api.refreshSession();
    });
    const nextDeadline = Date.now() + 5000;
    while (sessions.length !== 4) {
      assert.ok(Date.now() < nextDeadline, "Both session reads must start");
      await new Promise((resolve) => setTimeout(resolve, 10));
    }
    await sessions[2].fulfill({ json: { token: "early-old-token" } });
    assert.deepEqual(await page.evaluate(() => window.olderRead), {
      token: "early-old-token",
    });
    await page.evaluate(() =>
      window.api.syncApi("/api/token-probe", { id: "newer-still-pending" }),
    );
    assert.equal(tokenProbes.at(-1), "global-token");
    await sessions[3].fulfill({ json: { token: "latest-confirmed-token" } });
    assert.deepEqual(await page.evaluate(() => window.newerRead), {
      token: "latest-confirmed-token",
    });
    await page.evaluate(() =>
      window.api.syncApi("/api/token-probe", { id: "newer-confirmed" }),
    );
    assert.equal(tokenProbes.at(-1), "latest-confirmed-token");

    for (const order of ["newer-first", "older-first"]) {
      const before = sessions.length;
      await page.evaluate(() => {
        window.olderRead = window.api.refreshSession();
        window.newerRead = window.api
          .refreshSession()
          .catch((error) => error.status);
      });
      const failureDeadline = Date.now() + 5000;
      while (sessions.length !== before + 2) {
        assert.ok(
          Date.now() < failureDeadline,
          "Both recovery reads must start",
        );
        await new Promise((resolve) => setTimeout(resolve, 10));
      }
      const success = () =>
        sessions[before].fulfill({ json: { token: `${order}-valid-token` } });
      const failure = () =>
        sessions[before + 1].fulfill({
          status: 503,
          json: { error: "Session unavailable" },
        });
      if (order === "newer-first") {
        await failure();
        assert.equal(await page.evaluate(() => window.newerRead), 503);
        await success();
      } else {
        await success();
        assert.deepEqual(await page.evaluate(() => window.olderRead), {
          token: `${order}-valid-token`,
        });
        await failure();
      }
      assert.equal(await page.evaluate(() => window.newerRead), 503);
      assert.deepEqual(await page.evaluate(() => window.olderRead), {
        token: `${order}-valid-token`,
      });
      await page.evaluate(() =>
        window.api.syncApi("/api/token-probe", {
          id: "after-failed-newer-read",
        }),
      );
      assert.equal(tokenProbes.at(-1), `${order}-valid-token`);
    }

    // Explicit initialization takes ownership from every earlier session read.
    const beforeManual = sessions.length;
    await page.evaluate(() => {
      window.olderRead = window.api.refreshSession();
      window.api.setToken("manual-owner-token");
    });
    const manualDeadline = Date.now() + 5000;
    while (sessions.length !== beforeManual + 1) {
      assert.ok(
        Date.now() < manualDeadline,
        "The read before initialization must start",
      );
      await new Promise((resolve) => setTimeout(resolve, 10));
    }
    await sessions[beforeManual].fulfill({
      json: { token: "late-before-initialization" },
    });
    await page.evaluate(() => window.olderRead);
    await page.evaluate(() =>
      window.api.syncApi("/api/token-probe", { id: "manual-owner" }),
    );
    assert.equal(tokenProbes.at(-1), "manual-owner-token");
    holdSessions = false;

    // Uploads keep their preflight token across the asynchronous local file read.
    await page.evaluate(async (workspaceId) => {
      window.uploads = await import("/src/sync/uploads.ts");
      [window.upload] = await window.uploads.queueUploads(
        "token-fixture",
        "upload-chat",
        [
          new File(["Exact file bytes"], "contract.txt", {
            type: "text/plain",
          }),
        ],
        workspaceId,
      );
      const read = FileReader.prototype.readAsDataURL;
      FileReader.prototype.readAsDataURL = function (blob) {
        window.fileWaiting = true;
        window.releaseFile = () => {
          FileReader.prototype.readAsDataURL = read;
          read.call(this, blob);
        };
      };
      window.uploadResult = window.uploads.deliverUpload(window.upload);
    }, workspaceId);
    await page.waitForFunction(() => window.fileWaiting);
    await page.evaluate(() => {
      window.api.setToken("changed-during-file-read");
      window.releaseFile();
    });
    assert.equal(
      (await page.evaluate(() => window.uploadResult)).id,
      await page.evaluate(() => window.upload.id),
    );
    assert.equal(uploads.length, 1);
    assert.equal(uploads[0].headers["x-canvas-token"], "new-token");
    assert.equal(uploads[0].headers["x-canvas-workspace"], workspaceId);
    assert.equal(
      uploads[0].body.base64,
      Buffer.from("Exact file bytes").toString("base64"),
    );
    assert.deepEqual(errors, []);
    console.log(
      "PASS: ordered session ownership, exact sends, ordinary POST credentials, and captured upload tokens",
    );
  } finally {
    await Promise.all(
      testBrowser
        .contexts()
        .filter((ownedContext) => ownedContext !== runnerContext)
        .map((ownedContext) => ownedContext.close()),
    );
    await server.close();
    await rm(temporary, { recursive: true, force: true });
  }
});
