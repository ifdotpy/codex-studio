import type { components } from "../../src/generated/api.ts";
import { requestBody } from "../messages/helpers.ts";
import { readDraft } from "../drafts/draft-fixture-helpers.ts";
import type { PostBody, PostResult } from "../../src/apiTypes.ts";
import { fixtureOrigin } from "../drafts/draft-fixture-helpers.ts";
import assert from "node:assert/strict";
import { fileURLToPath } from "node:url";
import { test, expect, createFixtureViteServer } from "../playwright.ts";

test("draft pushes coalesce at the production replication boundary", async ({
  context,
}) => {
  test.setTimeout(60_000);
  const workspaceId = "f".repeat(32);
  const pushes: PostBody<"/api/sync/drafts">["rows"][] = [];
  const pushTimes: number[] = [];
  const messages: PostBody<"/api/messages">[] = [];
  let failNextPush = false;
  const server = await createFixtureViteServer({
    configFile: false,
    root: fileURLToPath(new URL("../../", import.meta.url)),
    server: { host: "127.0.0.1", port: 0 },
  });
  await server.listen();
  try {
    const page = await context.newPage();
    const errors: string[] = [];
    page.on("pageerror", (error) => errors.push(error.message));
    await page.route("**/check", (route) =>
      route.fulfill({ body: "<!doctype html><title>Draft push</title>" }),
    );
    await page.route("**/api/sync/identity", (route) =>
      route.fulfill({
        json: { workspaceId } satisfies Pick<
          components["schemas"]["SyncIdentityResponse"],
          "workspaceId"
        >,
      }),
    );
    await page.route("**/api/sync/pull?*", (route) =>
      route.fulfill({
        json: {
          workspaceId,
          documents: [],
          checkpoint: { seq: 0 },
        } satisfies Pick<
          components["schemas"]["SyncPullResponse"],
          "workspaceId" | "documents" | "checkpoint"
        >,
      }),
    );
    await page.route("**/api/sync/drafts", async (route) => {
      pushTimes.push(await page.evaluate(() => Date.now()));
      pushes.push(requestBody<PostBody<"/api/sync/drafts">>(route).rows);
      if (failNextPush) {
        failNextPush = false;
        await route.fulfill({ status: 503, body: "temporarily unavailable" });
        return;
      }
      await route.fulfill({ json: [] });
    });
    await page.route("**/api/session", (route) =>
      route.fulfill({
        json: {
          token: "test-session",
        } satisfies components["schemas"]["SessionResponse"],
      }),
    );
    await page.route("**/api/messages", async (route) => {
      const body = requestBody<PostBody<"/api/messages">>(route);
      messages.push(body);
      await route.fulfill({
        json: {
          id: body.id,
          status: "delivered",
        } satisfies PostResult<"/api/messages">,
      });
    });
    const origin = fixtureOrigin(server);
    await page.goto(`${origin}/check`);
    await page.clock.install();
    const timing = await page.evaluate(async () => {
      const client = await import("/src/sync/client.ts");
      return client.DRAFT_SYNC_TIMING_MS;
    });
    await page.evaluate(async () => {
      const { mount } =
        await import("/src/../tests/drafts/draft-push-coalescing-browser.fixture.tsx");
      await mount();
    });
    await page.waitForFunction(() => !!window.coalescingDraft_draft);
    await page.clock.runFor(300);

    const edit = async (text: string) => {
      await page.evaluate((text: string) => {
        window.coalescingDraft_draft.setDrafts((old) => ({
          ...old,
          lead: text,
        }));
        window.coalescingDraft_lastRecoverySnapshot = {
          journal: Object.entries(localStorage)
            .filter(([key]) =>
              key.startsWith(`codex-drafts:${"f".repeat(32)}:pending:`),
            )
            .map(([, value]) => window.coalescingDraft_readDraft(value).text),
          local: window.coalescingDraft_readLocalDraft(
            localStorage.getItem(`codex-chat-draft:${"f".repeat(32)}:lead`) ??
              "null",
          )?.text,
        };
      }, text);
      await page.waitForFunction(
        async (text: string) =>
          (await window.coalescingDraft_db.drafts.find().exec()).some(
            (doc) =>
              window.coalescingDraft_readDraft(doc.payload).text === text,
          ),
        text,
      );
    };

    const before = pushes.length;
    const burstSize = 10;
    for (let index = 0; index < burstSize; index++) {
      await edit(`typing ${index}`);
      if (index + 1 < burstSize) await page.clock.runFor(35);
    }
    const recovery = await page.evaluate(
      () => window.coalescingDraft_lastRecoverySnapshot,
    );
    assert.ok(
      recovery.journal.includes(`typing ${burstSize - 1}`) ||
        recovery.local === `typing ${burstSize - 1}`,
      "The latest recovery value is written synchronously before replication settles",
    );
    await page.clock.runFor(700);
    await expect.poll(() => pushes.length).toBe(before + 1);
    const burst = pushes.slice(before);
    assert.equal(
      readDraft(burst.at(-1)?.[0]?.newDocumentState?.payload || "{}").text,
      `typing ${burstSize - 1}`,
      "The push carries the final draft value",
    );

    const identicalBefore = pushes.length;
    await edit(`typing ${burstSize - 1}`);
    await page.clock.runFor(450);
    assert.equal(
      pushes.length,
      identicalBefore,
      "An identical value must not push",
    );

    const retryBefore = pushes.length;
    failNextPush = true;
    await edit("Latest draft after a failed POST");
    const waitForPushCount = (count: number) =>
      expect.poll(() => pushes.length).toBe(count);
    await page.clock.runFor(timing.pushQuietWait);
    await waitForPushCount(retryBefore + 1);
    await page.clock.runFor(timing.pushRetry + timing.pushQuietWait);
    await waitForPushCount(retryBefore + 2);
    assert.equal(
      readDraft(pushes.at(-1)?.[0]?.newDocumentState?.payload || "{}").text,
      "Latest draft after a failed POST",
      "A rejected push retries and converges to the latest version",
    );

    const maxWaitStart = await page.evaluate(() => Date.now());
    const maxWaitPushIndex = pushes.length;
    let pushedDuringTyping = false;
    for (let index = 0; index < 24; index++) {
      await edit(`continuous ${index}`);
      if (pushes.length > maxWaitPushIndex) pushedDuringTyping = true;
      if (index < 23) await page.clock.runFor(100);
    }
    assert.ok(
      pushedDuringTyping,
      "The maximum wait must push during continuous typing",
    );
    assert.ok(
      (pushTimes[maxWaitPushIndex] ?? NaN) - maxWaitStart <=
        timing.pushMaxWait + 600,
      "The first push is bounded from the start of continuous typing",
    );
    assert.ok(
      (pushTimes[maxWaitPushIndex] ?? NaN) - maxWaitStart >=
        timing.pushMaxWait - timing.pushQuietWait - 200,
      "A quiet-period push must not satisfy the maximum-wait assertion",
    );
    await page.clock.runFor(400);
    await expect
      .poll(
        () =>
          readDraft(pushes.at(-1)?.[0]?.newDocumentState?.payload || "{}").text,
      )
      .toBe("continuous 23");

    await edit("Send this before the draft push timer");
    const sendResult = await page.evaluate(async () => {
      const { durableSend } = await import("/src/sync/send.ts");
      const text = window.coalescingDraft_draft.getDraft("lead");
      const body = {
        id: "draft-coalescing-message",
        room: "lead",
        text,
        delivery: "current_turn",
      } satisfies PostBody<"/api/messages">;
      const result = await durableSend(body, [], () =>
        window.coalescingDraft_draft.setDrafts((old) => ({ ...old, lead: "" })),
      );
      return { body, result };
    });
    assert.equal(sendResult.body.text, "Send this before the draft push timer");
    assert.equal(messages.at(-1)?.text, sendResult.body.text);
    assert.equal(sendResult.result.id, "draft-coalescing-message");
    await page.clock.runFor(400);
    await expect
      .poll(
        () =>
          readDraft(pushes.at(-1)?.[0]?.newDocumentState?.payload || "{}").text,
      )
      .toBe("");
    assert.deepEqual(errors, []);
    console.log("production draft replication coalescing contract passed");
  } finally {
    await server.close();
  }
});
