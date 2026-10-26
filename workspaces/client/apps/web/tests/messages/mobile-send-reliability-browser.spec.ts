import { requestBody } from "./helpers.ts";
import type { Page, Route } from "@playwright/test";
import type { MessageRequest } from "./helpers.ts";
import { required } from "./helpers.ts";
import type {} from "./mobile-send-reliability-browser.fixture.tsx";
import { createFixtureViteServer } from "../playwright.ts";
import { assert } from "../chat/assertions.ts";
import {
  legacySnapshotRoute,
  test,
  syncIdentityFixture,
} from "../playwright.ts";
// Real RxDB, with controlled HTTP failures. No live server writes.
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
test("Mobile Send Reliability Browser", async ({
  browser: testBrowser,
  context: _testContext,
  page: _testPage,
}) => {
  const root = fileURLToPath(new URL("../../../../../../", import.meta.url));
  const createServer = createFixtureViteServer;
  const temporary = await mkdtemp(join(tmpdir(), "studio-mobile-send-"));
  const server = await createServer({
    configFile: false,
    root: join(root, "workspaces/client/apps/web"),
    cacheDir: join(temporary, "vite"),
    optimizeDeps: { include: ["react", "react-dom/client"] },
    server: { host: "127.0.0.1", port: 0, hmr: false },
  });
  await server.listen();
  const origin = required(server.resolvedUrls).local[0];
  let browser, context;
  try {
    browser = testBrowser;
    context = await browser.newContext({
      viewport: { width: 390, height: 844 },
      isMobile: true,
    });
    const errors: string[] = [],
      posts: MessageRequest[] = [];
    const pending = new Map<string, Route[]>();
    let holdSession = false;
    let missingSession = false;
    let sessionReads = 0;
    let snapshotReads = 0;
    const sessions: Route[] = [];
    await context.route("**/check", (route) =>
      route.fulfill({
        contentType: "text/html",
        body: "<!doctype html><title>Mobile sends</title>",
      }),
    );
    const identity = syncIdentityFixture();
    await context.route(legacySnapshotRoute, (route) => {
      snapshotReads++;
      return route.continue();
    });
    await context.route("**/api/sync/identity", (route) =>
      route.fulfill({ json: identity }),
    );
    await context.route("**/api/session", (route) => {
      sessionReads++;
      if (missingSession)
        return route.fulfill({ status: 404, json: { error: "Not found" } });
      if (holdSession) sessions.push(route);
      else
        return route.fulfill({
          json: {
            token: "fixture",
          } satisfies import("../playwright.ts").SessionResponse,
        });
    });
    await context.route("**/api/messages", (route) => {
      const body = requestBody<import("./helpers.ts").MessageRequest>(route);
      posts.push(body);
      if (body.id.startsWith("hold-")) {
        const routes = pending.get(body.id) || [];
        routes.push(route);
        pending.set(body.id, routes);
      } else if (body.id === "statusless-receipt") {
        return route.fulfill({
          json: {
            id: body.id,
            room: body.room,
            text: body.text,
            deliveries: {},
          },
        });
      } else
        return route.fulfill({
          json: {
            id: body.id,
            status: "delivered",
          } satisfies import("../../src/api.ts").PostResult<"/api/messages">,
        });
    });
    const mount = async (page: Page) => {
      page.on("pageerror", (error) => errors.push(error.message));
      page.setDefaultTimeout(10000);
      await page.goto(origin + "check");
      await page.addScriptTag({
        type: "module",
        url: "/tests/messages/mobile-send-reliability-browser.fixture.tsx",
      });
      await page.evaluate(() => window.mobileSendInitialize());
    };
    const first = await context.newPage(),
      second = await context.newPage();
    await mount(first);
    await mount(second);
    const body = (id: string): MessageRequest => ({
      id,
      room: "mobile-chat",
      text: `Exact content ${id}`,
      assets: [],
      delivery: "queue",
    });
    const until = async (check: () => boolean | Promise<boolean>) => {
      const deadline = Date.now() + 10000;
      while (!(await check())) {
        if (Date.now() > deadline)
          throw new Error("Expected send state did not arrive");
        await new Promise((resolve) => setTimeout(resolve, 20));
      }
    };
    const stored = (page: Page, id: string) =>
      page.evaluate((id) => window.mobileSendRead(id), id);
    const start = (page: Page, value: MessageRequest) =>
      page.evaluate((value) => window.mobileSendStart(value), value);
    const settle = (page: Page) => page.evaluate(() => window.mobileSendResult);
    const reply = (
      route: Route,
      id: string,
      status: Awaited<
        ReturnType<Window["mobileSendSend"]["durableSend"]>
      >["status"],
      code = 200,
    ) =>
      route.fulfill({
        status: code,
        json: code === 200 ? { id, status } : { error: "Old request failed" },
      });

    // Offline writes do not wait for a network timeout or mark an HTTP attempt.
    await context.setOffline(true);
    const offlineStart = Date.now();
    await start(first, body("offline-first"));
    assert.equal((await settle(first)).queued, true);
    assert.ok(Date.now() - offlineStart < 1000);
    assert.equal(await first.evaluate(() => window.mobileSendFetchCount), 0);
    assert.deepEqual(
      await first.evaluate(() => window.mobileSendPersistedRecord),
      {
        body: body("offline-first"),
        status: "queued",
        attempted: false,
        created: required(await stored(first, "offline-first")).created,
        displayPending: true,
        attachments: [],
      },
    );
    assert.equal(posts.length, 0);
    await context.setOffline(false);
    await first.evaluate(async () => {
      window.mobileSendReactRoot = window.mobileSendMount();
    });
    try {
      await until(
        async () =>
          (await stored(first, "offline-first"))?.status === "accepted",
      );
    } catch (error) {
      console.error({
        errors,
        posts,
        page: await first.evaluate(() => ({
          online: navigator.onLine,
          outbox: window.mobileSendOutbox,
          fetchCount: window.mobileSendFetchCount,
        })),
      });
      throw error;
    }
    assert.deepEqual(posts, [body("offline-first")]);
    await first.evaluate(() => window.mobileSendReactRoot.unmount());

    // Persistence releases the caller before HTTP. Another tab can still cancel
    // an unattempted intention while the session read is in progress.
    holdSession = true;
    await start(first, body("cancel-before-post"));
    await until(() => sessions.length === 1);
    assert.equal(await first.evaluate(() => window.mobileSendPersisted), true);
    assert.equal(await first.evaluate(() => window.mobileSendSettled), false);
    assert.equal(
      required(await first.evaluate(() => window.mobileSendPersistedRecord))
        .attempted,
      false,
    );
    await second.evaluate(() =>
      window.mobileSendSend.changeOutbox("cancel-before-post", "cancel"),
    );
    holdSession = false;
    await required(sessions.shift()).fulfill({
      json: {
        token: "fixture",
      } satisfies import("../playwright.ts").SessionResponse,
    });
    assert.equal((await settle(first)).status, "cancelled");
    assert.equal(posts.length, 1);

    // A suspended sender must not hold a browser lock over the network. A second
    // tab retries the same ID while the first response remains stalled.
    await start(first, body("hold-locked"));
    await until(() => pending.get("hold-locked")?.length === 1);
    const retryStart = Date.now();
    await start(second, body("hold-locked"));
    await until(() => pending.get("hold-locked")?.length === 2);
    assert.ok(Date.now() - retryStart < 1000);
    await reply(
      required(required(pending.get("hold-locked"))[1]),
      "hold-locked",
      "delivered",
    );
    assert.equal((await settle(second)).status, "delivered");
    await reply(
      required(required(pending.get("hold-locked"))[0]),
      "hold-locked",
      "queued",
    );
    assert.equal((await settle(first)).status, "delivered");
    await start(second, body("hold-locked"));
    assert.equal((await settle(second)).status, "delivered");
    assert.equal(required(pending.get("hold-locked")).length, 2);

    // Late replies cannot regress a newer accepted receipt or add a false error.
    for (const code of [200, 503, 400]) {
      const id = `hold-fallback-${code}`;
      await start(first, body(id));
      await until(() => pending.get(id)?.length === 1);
      await start(second, body(id));
      await until(() => pending.get(id)?.length === 2);
      await reply(required(required(pending.get(id))[1]), id, "delivered");
      assert.equal((await settle(second)).status, "delivered");
      await reply(required(required(pending.get(id))[0]), id, "queued", code);
      assert.equal((await settle(first)).status, "delivered");
      const value = await stored(first, id);
      assert.equal(required(value).status, "accepted");
      assert.equal(required(required(value).receipt).status, "delivered");
      assert.equal(required(value).error, undefined);
      assert.deepEqual(
        posts.filter((post) => post.id === id),
        [body(id), body(id)],
      );
    }

    for (const finalStatus of ["delivered", "uncertain"] as const) {
      const id = `hold-upgrade-${finalStatus}`;
      await start(first, body(id));
      await until(() => pending.get(id)?.length === 1);
      await start(second, body(id));
      await until(() => pending.get(id)?.length === 2);
      await reply(required(required(pending.get(id))[0]), id, "queued");
      assert.equal((await settle(first)).status, "queued");
      await reply(required(required(pending.get(id))[1]), id, finalStatus);
      assert.equal((await settle(second)).status, finalStatus);
      assert.equal(
        required(required(await stored(second, id)).receipt).status,
        finalStatus,
      );
    }

    // The persistence callback cannot authorize reuse of an identity with new text.
    const mismatch = await first.evaluate(
      async (value) => {
        let persisted = false;
        try {
          await window.mobileSendSend.durableSend(value, [], () => {
            persisted = true;
          });
          return { persisted };
        } catch (error) {
          if (!(error instanceof Error)) throw error;
          return { persisted, error: error.message };
        }
      },
      { ...body("hold-locked"), text: "Changed body" },
    );
    assert.equal(mismatch.persisted, false);
    assert.match(mismatch.error, /different content/);
    await first.evaluate(async () => {
      await window.mobileSendStorage.db.outbox.insert({
        id: "failed-before-send",
        seq: 0,
        payload: JSON.stringify({
          body: {
            id: "failed-before-send",
            room: "mobile-chat",
            text: "Rejected",
          },
          status: "failed",
          error: "Rejected previously",
          created: Date.now(),
        }),
      });
    });
    const failed = await first.evaluate(async () => {
      let persisted = false;
      try {
        await window.mobileSendSend.durableSend(
          { id: "failed-before-send", room: "mobile-chat", text: "Rejected" },
          [],
          () => {
            persisted = true;
          },
        );
        return { persisted };
      } catch (error) {
        if (!(error instanceof Error)) throw error;
        return { persisted, error: error.message };
      }
    });
    assert.deepEqual(failed, {
      persisted: false,
      error: "Rejected previously",
    });

    // A real stalled fetch expires, preserves the request, and accepts an exact retry.
    const hung = body("hold-timeout");
    await first.clock.install();
    await start(first, hung);
    await until(() => pending.get(hung.id)?.length === 1);
    await first.clock.setSystemTime(
      (await first.evaluate(() => Date.now())) + 16_000,
    );
    await first.evaluate(() => window.dispatchEvent(new Event("pageshow")));
    const timeoutResult = await settle(first);
    assert.equal(timeoutResult.queued, true);
    assert.match(timeoutResult.error, /did not respond in time/);
    assert.equal(required(await stored(first, hung.id)).attempted, true);
    await required(required(pending.get(hung.id))[0])
      .abort("failed")
      .catch(() => {});
    await start(second, hung);
    await until(() => pending.get(hung.id)?.length === 2);
    await reply(
      required(required(pending.get(hung.id))[1]),
      hung.id,
      "delivered",
    );
    assert.equal((await settle(second)).status, "delivered");
    assert.equal(required(await stored(second, hung.id)).error, undefined);
    assert.deepEqual(
      posts.filter((post) => post.id === hung.id),
      [hung, hung],
    );
    await first.clock.setSystemTime(new Date());
    // Reconnect keeps same-chat order without making another chat wait for HTTP.
    await context.setOffline(true);
    const orderedFirst = { ...body("hold-order-first"), room: "slow-chat" };
    const orderedSecond = { ...body("hold-order-second"), room: "slow-chat" };
    const independent = { ...body("independent-chat-send"), room: "fast-chat" };
    for (const value of [orderedFirst, orderedSecond, independent]) {
      await start(first, value);
      assert.equal((await settle(first)).queued, true);
    }
    await context.setOffline(false);
    await first.evaluate(async () => {
      window.mobileSendReactRoot = window.mobileSendMount();
    });
    await until(
      () =>
        pending.get(orderedFirst.id)?.length === 1 &&
        posts.some((post) => post.id === independent.id),
    );
    assert.equal(pending.has(orderedSecond.id), false);
    const continuationStart = Date.now();
    await reply(
      required(required(pending.get(orderedFirst.id))[0]),
      orderedFirst.id,
      "delivered",
    );
    await until(() => pending.get(orderedSecond.id)?.length === 1);
    assert.ok(
      Date.now() - continuationStart < 1500,
      "Next send does not wait for a poll",
    );
    await reply(
      required(required(pending.get(orderedSecond.id))[0]),
      orderedSecond.id,
      "delivered",
    );
    await until(
      async () =>
        required(await stored(first, orderedSecond.id)).status === "accepted",
    );
    await first.evaluate(() => window.mobileSendReactRoot.unmount());
    // A missing current endpoint cannot silently obtain credentials elsewhere.
    missingSession = true;
    const failure = await first.evaluate(async (value) => {
      try {
        await window.mobileSendSend.durableSend(value);
      } catch (error) {
        if (!(error instanceof Error)) throw error;
        return error.message;
      }
    }, body("missing-session-send"));
    assert.match(failure, /Not found/);
    assert.equal(
      required(await stored(first, "missing-session-send")).status,
      "failed",
    );
    assert.ok(sessionReads > 0);
    assert.equal(snapshotReads, 0, "The snapshot fallback is never requested");
    assert.deepEqual(
      posts.filter((post) => post.id === "missing-session-send"),
      [],
    );
    missingSession = false;
    await start(first, body("statusless-receipt"));
    assert.equal((await settle(first)).queued, true);
    const unmatched = await stored(first, "statusless-receipt");
    assert.equal(required(unmatched).status, "queued");
    assert.equal(required(unmatched).attempted, true);
    assert.match(required(unmatched).error, /does not match/);
    assert.equal(required(unmatched).receipt, undefined);
    assert.deepEqual(errors, []);
    console.log(
      "PASS: offline persistence, immediate callback, reconnect, cancellation boundary, concurrent tabs, late receipt/error guards, immutable IDs, stalled HTTP retry, ordered parallel chat recovery",
    );
  } finally {
    await context?.close();
    await server.close();
    await rm(temporary, { recursive: true, force: true });
  }
});
