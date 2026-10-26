import { requestBody } from "./helpers.ts";
import type { Route } from "@playwright/test";
import type { PostBody } from "../../src/api.ts";
import type { MessageRequest } from "./helpers.ts";
import type { OutboxEntry } from "../../src/sync/send.ts";
import { required } from "./helpers.ts";
import { createFixtureViteServer } from "../playwright.ts";
import { test } from "../playwright.ts";
// Real RxDB and send callers with controlled session and claim delays.
import assert from "node:assert/strict";
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
  const createServer = createFixtureViteServer;
  const temporary = await mkdtemp(join(tmpdir(), "studio-send-session-token-"));
  const workspaceId = "c".repeat(32);
  const server = await createServer({
    configFile: false,
    root: join(root, "workspaces/client/apps/web"),
    cacheDir: join(temporary, "vite"),
    optimizeDeps: { include: ["react"] },
    server: { host: "127.0.0.1", port: 0, hmr: false },
  });
  await server.listen();
  try {
    const page = runnerPage;
    page.setDefaultTimeout(5000);
    const errors: string[] = [],
      sessions: Route[] = [],
      posts: Array<{ body: MessageRequest; headers: Record<string, string> }> =
        [],
      tokenProbes: Array<string | undefined> = [],
      uploads: Array<{
        body: PostBody<"/api/assets">;
        headers: Record<string, string>;
      }> = [];
    let holdSessions = false;
    page.on("pageerror", (error) => errors.push(error.message));
    await page.route("**/check", (route) =>
      route.fulfill({
        contentType: "text/html",
        body: "<!doctype html><title>Send session tokens</title>",
      }),
    );
    await page.route("**/api/sync/identity", (route) =>
      route.fulfill({
        json: { workspaceId } satisfies Pick<
          import("../playwright.ts").SyncIdentityResponse,
          "workspaceId"
        >,
      }),
    );
    await page.route("**/api/session", (route) => {
      sessions.push(route);
      if (sessions.length !== 1 && !holdSessions)
        return route.fulfill({
          json: {
            token: "new-token",
          } satisfies import("../playwright.ts").SessionResponse,
        });
    });
    await page.route("**/api/messages", (route) => {
      const body = requestBody<import("./helpers.ts").MessageRequest>(route);
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
      const body =
        requestBody<import("../../src/api.ts").PostBody<"/api/assets">>(route);
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
    await page.goto(required(server.resolvedUrls).local[0] + "check");
    await page.evaluate(async () => {
      window.tokenSendSend = await import("/src/sync/send.ts");
      window.tokenSendApi = await import("/src/api.ts");
      window.tokenSendProbe = (id, options) => {
        return window.tokenSendApi.syncPost(
          // @ts-expect-error fixture-only route /api/token-probe is served by this spec's fixture server, not the production API
          "/api/token-probe",
          { id },
          options,
        );
      };
      window.tokenSendStorage = await (
        await import("/src/sync/client.ts")
      ).syncDatabase();
      window.tokenSendClaimWaiting = false;
      window.tokenSendClaimGate = new Promise<void>(
        (resolve) => (window.tokenSendReleaseClaim = resolve),
      );
      window.tokenSendStart = (body, holdClaim = false) =>
        window.tokenSendSend
          .durableSend(body, [], async () => {
            if (!holdClaim) return;
            const doc = await window.tokenSendStorage.db.outbox
              .findOne(body.id)
              .exec();
            if (!doc) throw new Error("Missing saved send");
            const modify = doc.incrementalModify.bind(doc);
            let hold = true;
            Object.defineProperty(doc, "incrementalModify", {
              value: async (update: Parameters<typeof modify>[0]) => {
                if (hold) {
                  hold = false;
                  window.tokenSendClaimWaiting = true;
                  await window.tokenSendClaimGate;
                }
                return modify(update);
              },
            });
          })
          .catch((error: unknown) => {
            if (!(error instanceof window.tokenSendApi.ApiError)) throw error;
            window.tokenSendSendError = error.message;
            return { code: error.status, error: error.message };
          });
      window.tokenSendRead = async (id) => {
        const doc = await window.tokenSendStorage.db.outbox.findOne(id).exec();
        if (!doc) throw new Error("Missing saved send");
        return JSON.parse(doc.getLatest().payload);
      };
    });
    const body = (id: string): MessageRequest => ({
      id,
      room: `chat-${id}`,
      text: `Exact message ${id}`,
      assets: [],
      delivery: "current_turn",
    });
    await page.evaluate((body) => {
      window.tokenSendResultA = window.tokenSendStart(body);
    }, body("old-session"));
    const deadline = Date.now() + 5000;
    while (sessions.length !== 1) {
      assert.ok(Date.now() < deadline, "The first session must start");
      await new Promise((resolve) => setTimeout(resolve, 10));
    }
    await page.evaluate((body) => {
      window.tokenSendResultB = window.tokenSendStart(body, true);
    }, body("current-session"));
    try {
      await page.waitForFunction(() => window.tokenSendClaimWaiting);
    } catch (error) {
      console.error({
        sendError: await page.evaluate(() => window.tokenSendSendError),
        posts,
        errors,
      });
      throw error;
    }
    assert.equal(sessions.length, 2);
    assert.equal(posts.length, 0);
    await required(sessions[0]).fulfill({
      json: {
        token: "old-token",
      } satisfies import("../playwright.ts").SessionResponse,
    });
    assert.equal(
      (await page.evaluate(() => window.tokenSendResultA)).status,
      "delivered",
    );
    assert.equal(posts.length, 1);
    assert.deepEqual(required(posts[0]).body, body("old-session"));
    assert.equal(required(posts[0]).headers["x-canvas-token"], "new-token");
    await page.evaluate(async (workspaceId) => {
      await window.tokenSendProbe("after-overlap", {
        workspaceId,
      });
    }, workspaceId);
    assert.deepEqual(tokenProbes, ["new-token"]);
    await page.evaluate(() => {
      // Even an explicit token change after the claim cannot change B's request.
      window.tokenSendApi.setToken("global-token");
      window.tokenSendReleaseClaim();
    });
    assert.equal(
      (await page.evaluate(() => window.tokenSendResultB)).status,
      "delivered",
    );
    assert.equal(posts.length, 2);
    assert.deepEqual(required(posts[1]).body, body("current-session"));
    assert.equal(required(posts[1]).headers["x-canvas-token"], "new-token");
    assert.equal(required(posts[1]).headers["x-canvas-workspace"], workspaceId);
    assert.equal(
      (await page.evaluate(() => window.tokenSendRead("current-session")))
        .status,
      "accepted",
    );
    assert.equal(
      (await page.evaluate(() => window.tokenSendRead("old-session"))).status,
      "accepted",
    );
    assert.equal(
      (
        await page.evaluate(
          (body) => window.tokenSendStart(body),
          body("current-session"),
        )
      ).status,
      "delivered",
    );
    assert.equal(posts.length, 2, "A confirmed message must not be sent again");
    assert.equal(sessions.length, 2);
    await page.evaluate(async (workspaceId) => {
      await window.tokenSendProbe("scoped", {
        workspaceId,
        sessionToken: "scoped-token",
      });
      await window.tokenSendProbe("global", { workspaceId });
    }, workspaceId);
    assert.deepEqual(tokenProbes, [
      "new-token",
      "scoped-token",
      "global-token",
    ]);

    // An older response cannot publish while a newer read is still pending.
    holdSessions = true;
    await page.evaluate(() => {
      window.tokenSendOlderRead = window.tokenSendApi.refreshSession();
      window.tokenSendNewerRead = window.tokenSendApi.refreshSession();
    });
    const nextDeadline = Date.now() + 5000;
    while (sessions.length !== 4) {
      assert.ok(Date.now() < nextDeadline, "Both session reads must start");
      await new Promise((resolve) => setTimeout(resolve, 10));
    }
    await required(sessions[2]).fulfill({
      json: {
        token: "early-old-token",
      } satisfies import("../playwright.ts").SessionResponse,
    });
    assert.deepEqual(await page.evaluate(() => window.tokenSendOlderRead), {
      token: "early-old-token",
    });
    await page.evaluate(() => window.tokenSendProbe("newer-still-pending"));
    assert.equal(tokenProbes.at(-1), "global-token");
    await required(sessions[3]).fulfill({
      json: {
        token: "latest-confirmed-token",
      } satisfies import("../playwright.ts").SessionResponse,
    });
    assert.deepEqual(await page.evaluate(() => window.tokenSendNewerRead), {
      token: "latest-confirmed-token",
    });
    await page.evaluate(() => window.tokenSendProbe("newer-confirmed"));
    assert.equal(tokenProbes.at(-1), "latest-confirmed-token");

    for (const order of ["newer-first", "older-first"]) {
      const before = sessions.length;
      await page.evaluate(() => {
        window.tokenSendOlderRead = window.tokenSendApi.refreshSession();
        window.tokenSendNewerRead = window.tokenSendApi
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
        required(sessions[before]).fulfill({
          json: {
            token: `${order}-valid-token`,
          } satisfies import("../playwright.ts").SessionResponse,
        });
      const failure = () =>
        required(sessions[before + 1]).fulfill({
          status: 503,
          json: { error: "Session unavailable" },
        });
      if (order === "newer-first") {
        await failure();
        assert.equal(await page.evaluate(() => window.tokenSendNewerRead), 503);
        await success();
      } else {
        await success();
        assert.deepEqual(await page.evaluate(() => window.tokenSendOlderRead), {
          token: `${order}-valid-token`,
        });
        await failure();
      }
      assert.equal(await page.evaluate(() => window.tokenSendNewerRead), 503);
      assert.deepEqual(await page.evaluate(() => window.tokenSendOlderRead), {
        token: `${order}-valid-token`,
      });
      await page.evaluate(() =>
        window.tokenSendProbe("after-failed-newer-read"),
      );
      assert.equal(tokenProbes.at(-1), `${order}-valid-token`);
    }

    // Explicit initialization takes ownership from every earlier session read.
    const beforeManual = sessions.length;
    await page.evaluate(() => {
      window.tokenSendOlderRead = window.tokenSendApi.refreshSession();
      window.tokenSendApi.setToken("manual-owner-token");
    });
    const manualDeadline = Date.now() + 5000;
    while (sessions.length !== beforeManual + 1) {
      assert.ok(
        Date.now() < manualDeadline,
        "The read before initialization must start",
      );
      await new Promise((resolve) => setTimeout(resolve, 10));
    }
    await required(sessions[beforeManual]).fulfill({
      json: {
        token: "late-before-initialization",
      } satisfies import("../playwright.ts").SessionResponse,
    });
    await page.evaluate(() => window.tokenSendOlderRead);
    await page.evaluate(() => window.tokenSendProbe("manual-owner"));
    assert.equal(tokenProbes.at(-1), "manual-owner-token");
    holdSessions = false;

    // Uploads keep their preflight token across the asynchronous local file read.
    await page.evaluate(async (workspaceId) => {
      window.tokenSendUploads = await import("/src/sync/uploads.ts");
      const [upload] = await window.tokenSendUploads.queueUploads(
        "token-fixture",
        "upload-chat",
        [
          new File(["Exact file bytes"], "contract.txt", {
            type: "text/plain",
          }),
        ],
        workspaceId,
      );
      if (!upload) throw new Error("Missing fixture upload");
      window.tokenSendUpload = upload;
      const read = FileReader.prototype.readAsDataURL;
      FileReader.prototype.readAsDataURL = function (blob) {
        window.tokenSendFileWaiting = true;
        window.tokenSendReleaseFile = () => {
          FileReader.prototype.readAsDataURL = read;
          read.call(this, blob);
        };
      };
      window.tokenSendUploadResult = window.tokenSendUploads.deliverUpload(
        window.tokenSendUpload,
      );
    }, workspaceId);
    await page.waitForFunction(() => window.tokenSendFileWaiting);
    await page.evaluate(() => {
      window.tokenSendApi.setToken("changed-during-file-read");
      window.tokenSendReleaseFile();
    });
    assert.equal(
      required(await page.evaluate(() => window.tokenSendUploadResult)).id,
      await page.evaluate(() => window.tokenSendUpload.id),
    );
    assert.equal(uploads.length, 1);
    assert.equal(required(uploads[0]).headers["x-canvas-token"], "new-token");
    assert.equal(
      required(uploads[0]).headers["x-canvas-workspace"],
      workspaceId,
    );
    assert.equal(
      required(uploads[0]).body.base64,
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

declare global {
  interface Window {
    tokenSendSend: typeof import("../../src/sync/send.ts");
    tokenSendApi: typeof import("../../src/api.ts");
    tokenSendProbe: (
      id: string,
      options?: Parameters<typeof import("../../src/api.ts").syncPost>[2],
    ) => Promise<unknown>;
    tokenSendStorage: Awaited<
      ReturnType<typeof import("../../src/sync/client.ts").syncDatabase>
    >;
    tokenSendClaimWaiting: boolean;
    tokenSendClaimGate: Promise<void>;
    tokenSendReleaseClaim: () => void;
    tokenSendStart: (
      body: MessageRequest,
      holdClaim?: boolean,
    ) => Promise<
      Partial<
        Awaited<ReturnType<typeof import("../../src/sync/send.ts").durableSend>>
      > & { code?: number | undefined }
    >;
    tokenSendSendError: string;
    tokenSendRead: (id: string) => Promise<Omit<OutboxEntry, "id">>;
    tokenSendResultA: ReturnType<Window["tokenSendStart"]>;
    tokenSendResultB: ReturnType<Window["tokenSendStart"]>;
    tokenSendOlderRead: ReturnType<
      typeof import("../../src/api.ts").refreshSession
    >;
    tokenSendNewerRead: Promise<
      | Awaited<ReturnType<typeof import("../../src/api.ts").refreshSession>>
      | number
    >;
    tokenSendUploads: typeof import("../../src/sync/uploads.ts");
    tokenSendUpload: Awaited<
      ReturnType<typeof import("../../src/sync/uploads.ts").queueUploads>
    >[number];
    tokenSendFileWaiting: boolean;
    tokenSendReleaseFile: () => void;
    tokenSendUploadResult: ReturnType<
      typeof import("../../src/sync/uploads.ts").deliverUpload
    >;
  }
}
