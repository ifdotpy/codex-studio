import type {} from "./queue-mutation-deadline-browser.fixture.tsx";
import { createFixtureViteServer } from "../playwright.ts";
import { required, requestBody } from "../messages/helpers.ts";
import type { GetResult, PostBody, PostResult } from "../../src/apiTypes.ts";
// Real queue callers, local HTTP receipts, and no native model requests.
import assert from "node:assert/strict";

import { join } from "node:path";
import { fileURLToPath } from "node:url";

import { test, expect } from "../playwright.ts";

test("queue-mutation-deadline-browser @performance", async ({
  page: fixturePage,
}) => {
  test.setTimeout(120_000);
  const root = fileURLToPath(new URL("../../../../../../", import.meta.url));

  const workspaceId = "c".repeat(32);
  const server = await createFixtureViteServer({
    configFile: false,
    root: join(root, "workspaces/client/apps/web"),
    optimizeDeps: { include: ["react", "react-dom/client"] },
    server: { host: "127.0.0.1", port: 0, hmr: false },
  });
  await server.listen();
  try {
    const page = fixturePage;
    page.setDefaultTimeout(5000);
    const errors: string[] = [];
    const posts: PostBody<"/api/queue">[] = [];
    const nativeRequests: string[] = [];
    const receipts = new Map<
      string | null | undefined,
      { body: PostBody<"/api/queue">; reply: PostResult<"/api/queue"> }
    >();
    let view: Omit<GetResult<"/api/queue">, "capabilities"> = {
      items: [],
      revision: "initial",
    };
    let applied = 0;
    page.on("pageerror", (error) => errors.push(error.message));
    page.on("request", (request) => {
      if (
        ["/api/messages", "/api/stop"].includes(new URL(request.url()).pathname)
      )
        nativeRequests.push(request.url());
    });
    await page.route("**/check", (route) =>
      route.fulfill({
        contentType: "text/html",
        body: "<!doctype html><title>Queue deadlines</title>",
      }),
    );
    await page.route("**/api/sync/identity", (route) =>
      route.fulfill({
        json: { workspaceId } satisfies Pick<
          GetResult<"/api/sync/identity">,
          "workspaceId"
        >,
      }),
    );
    await page.route(/\/api\/queue(?:\?.*)?$/, (route) => {
      if (route.request().method() !== "POST")
        return route.fulfill({
          json: {
            ...view,
            capabilities: { receipts: true, reorder: true },
          } satisfies GetResult<"/api/queue">,
        });
      const body = requestBody<PostBody<"/api/queue">>(route);
      posts.push(body);
      const previous = receipts.get(body.request_id);
      if (previous) {
        assert.deepEqual(
          body,
          previous.body,
          "A retry must preserve every field",
        );
        return route.fulfill({ json: previous.reply });
      }
      assert.equal(body.expected_revision, view.revision);
      assert.equal(
        route.request().headers()["x-canvas-workspace"],
        workspaceId,
      );
      const reply = {} satisfies PostResult<"/api/queue">;
      receipts.set(body.request_id, { body, reply });
      applied++;
      if (body.action === "cancel")
        view.items = view.items.filter((item) => item.id !== body.id);
      else if (body.action === "edit")
        view.items = view.items.map((item) =>
          item.id === body.id ? { ...item, text: body.text } : item,
        );
      else if (body.action === "reorder")
        view.items = body.ordered_ids.map((id) =>
          required(view.items.find((item) => item.id === id)),
        );
      else assert.fail(`Unexpected queue action: ${body.action}`);
      view.revision += ":committed";
      // The operation commits, but its response stays pending until the deadline.
    });
    await page.goto(required(server.resolvedUrls?.local[0]) + "check");
    await page.evaluate(async (workspaceId) => {
      window.queueDeadlineObserved = [];
      window.queueDeadlineEdited = [];
      window.queueDeadlineAdvance = 0;
      const now = Date.now;
      Date.now = () => now() + window.queueDeadlineAdvance;
      const fixturePath =
        "/tests/queue/queue-mutation-deadline-browser.fixture.tsx";
      await import(fixturePath);
      window.queueDeadlineReactRoot = window.queueDeadlineMount(workspaceId);
    }, workspaceId);
    await page.waitForFunction(
      () => window.queueDeadlineQueue && !window.queueDeadlineQueue.loading,
    );
    const untilPosts = async (count: number) => {
      const deadline = Date.now() + 5000;
      while (posts.length < count) {
        assert.ok(Date.now() < deadline, "The queue POST must start");
        await new Promise((resolve) => setTimeout(resolve, 10));
      }
    };
    const expire = () =>
      page.evaluate(() => {
        window.queueDeadlineAdvance += 16000;
        window.dispatchEvent(new Event("pageshow"));
      });
    for (const action of ["cancel", "edit", "reorder"]) {
      view = {
        items: [1, 2].map((number) => ({
          id: `${action}-${number}`,
          kind: "user",
          created: 0,
          status: "pending",
          text: `Exact ${action} ${number}`,
        })),
        revision: `revision-${action}`,
      };
      await page.evaluate(() => window.queueDeadlineQueue.reload());
      await page.waitForFunction(
        (action) => window.queueDeadlineQueue.items[0]?.id === `${action}-1`,
        action,
      );
      const before = posts.length;
      await page.evaluate((action) => {
        window.queueDeadlineOutcome = "pending";
        const queue = window.queueDeadlineQueue;
        const first = queue.items[0];
        if (!first) throw new Error("Missing queue item");
        const operation =
          action === "edit"
            ? queue.edit(first, "Exact replacement")
            : action === "reorder"
              ? queue.reorder(queue.items.map((item) => item.id).reverse())
              : queue.cancel(first);
        window.queueDeadlineOperation = operation.then(
          () => (window.queueDeadlineOutcome = "success"),
          (error: Error) => (window.queueDeadlineOutcome = error.name),
        );
      }, action);
      await untilPosts(before + 1);
      const request = posts[before];
      const observed = await page.evaluate(() => ({
        observed: window.queueDeadlineObserved.length,
        edited: window.queueDeadlineEdited.length,
      }));
      await expire();
      await page.waitForFunction(
        () =>
          window.queueDeadlineOutcome !== "pending" &&
          !window.queueDeadlineQueue.busy,
      );
      assert.equal(
        await page.evaluate(() => window.queueDeadlineOutcome),
        "NetworkTimeoutError",
      );
      assert.deepEqual(
        await page.evaluate(() => window.queueDeadlineQueue.pending),
        request,
      );
      assert.deepEqual(
        await page.evaluate(() => ({
          observed: window.queueDeadlineObserved.length,
          edited: window.queueDeadlineEdited.length,
        })),
        observed,
        "An unknown result cannot report confirmed cancellation or editing",
      );
      assert.match(
        await page.evaluate(() => {
          const first = window.queueDeadlineQueue.items[0];
          if (!first) throw new Error("Missing queue item");
          return window.queueDeadlineQueue.cancel(first).then(
            () => "",
            (error: Error) => error.message,
          );
        }),
        /Retry the previous queue change/,
      );
      assert.equal(posts.length, before + 1);
      await page.evaluate(() => window.queueDeadlineQueue.retry());
      assert.deepEqual(posts[before + 1], request);
      assert.equal(
        await page.evaluate(() => window.queueDeadlineQueue.pending),
        null,
      );
      assert.equal(
        await page.evaluate(() => window.queueDeadlineQueue.busy),
        false,
      );
    }
    await page.evaluate(() => window.queueDeadlineReactRoot.unmount());
    assert.equal(applied, 3);

    // Removal has its own saved request. Its retry must work after the row is gone.
    view = {
      items: [
        {
          id: "remove-1",
          kind: "user",
          created: 0,
          status: "pending",
          text: "Exact removal",
        },
      ],
      revision: "revision-remove",
    };
    const before = posts.length;
    await page.evaluate(async (workspaceId) => {
      window.queueDeadlineRemoval =
        await import("/src/components/removeSendingMessages.ts");
      window.queueDeadlineRemoved =
        await import("/src/components/removedMessages.ts");
      window.queueDeadlineScope = { stateDir: "deadline-fixture", workspaceId };
      window.queueDeadlineTarget = {
        chat: "chat",
        message: {
          id: "chat:remove-1",
          clientMessageId: "remove-1",
          role: "user",
          text: "Exact removal",
          deliveryStatus: "sending",
        },
      };
      window.queueDeadlineOutcome = "pending";
      window.queueDeadlineOperation = window.queueDeadlineRemoval
        .removeSendingMessage(
          window.queueDeadlineScope,
          window.queueDeadlineTarget,
        )
        .then(
          () => (window.queueDeadlineOutcome = "success"),
          (error: Error) => (window.queueDeadlineOutcome = error.name),
        );
    }, workspaceId);
    await untilPosts(before + 1);
    const request = posts[before];
    await expire();
    await page.waitForFunction(() => window.queueDeadlineOutcome !== "pending");
    assert.equal(
      await page.evaluate(() => window.queueDeadlineOutcome),
      "NetworkTimeoutError",
    );
    assert.equal(
      await page.evaluate(() =>
        window.queueDeadlineRemoved.isRemovedMessage(
          window.queueDeadlineScope.stateDir,
          "agent",
          "chat",
          window.queueDeadlineTarget.message,
        ),
      ),
      false,
      "An unknown queue result must remain available for exact retry",
    );
    await page.evaluate(() =>
      window.queueDeadlineRemoval.removeSendingMessage(
        window.queueDeadlineScope,
        window.queueDeadlineTarget,
      ),
    );
    assert.deepEqual(posts[before + 1], request);
    assert.equal(applied, 4);
    assert.equal(
      await page.evaluate(() =>
        window.queueDeadlineRemoved.isRemovedMessage(
          window.queueDeadlineScope.stateDir,
          "agent",
          "chat",
          window.queueDeadlineTarget.message,
        ),
      ),
      true,
    );
    assert.deepEqual(nativeRequests, []);
    expect(errors).toEqual([]);
    console.log(
      "PASS: queue cancel/edit/reorder and removal expire, preserve exact requests, and retry one committed effect",
    );
  } finally {
    await server.close();
  }
});
