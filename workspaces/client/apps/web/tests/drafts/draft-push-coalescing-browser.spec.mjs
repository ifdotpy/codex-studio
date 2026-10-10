import assert from "node:assert/strict";
import { fileURLToPath } from "node:url";
import { test } from "../playwright.mjs";

test("draft pushes coalesce at the production replication boundary", async ({
  context,
}) => {
  test.setTimeout(60_000);
  const { createServer } = await import(
    new URL("../../node_modules/vite/dist/node/index.js", import.meta.url)
  );
  const workspaceId = "f".repeat(32);
  const pushes = [];
  const pushTimes = [];
  const messages = [];
  let failNextPush = false;
  const server = await createServer({
    configFile: false,
    root: fileURLToPath(new URL("../../", import.meta.url)),
    server: { host: "127.0.0.1", port: 0 },
  });
  await server.listen();
  try {
    const page = await context.newPage();
    const errors = [];
    page.on("pageerror", (error) => errors.push(error.message));
    await page.route("**/check", (route) =>
      route.fulfill({ body: "<!doctype html><title>Draft push</title>" }),
    );
    await page.route("**/api/sync/identity", (route) =>
      route.fulfill({ json: { workspaceId } }),
    );
    await page.route("**/api/sync/pull?*", (route) =>
      route.fulfill({
        json: { workspaceId, documents: [], checkpoint: { seq: 0 } },
      }),
    );
    await page.route("**/api/sync/drafts", async (route) => {
      pushes.push(route.request().postDataJSON().rows);
      pushTimes.push(Date.now());
      if (failNextPush) {
        failNextPush = false;
        await route.fulfill({ status: 503, body: "temporarily unavailable" });
        return;
      }
      await route.fulfill({ json: [] });
    });
    await page.route("**/api/session", (route) =>
      route.fulfill({ json: { token: "test-session" } }),
    );
    await page.route("**/api/messages", async (route) => {
      const body = route.request().postDataJSON();
      messages.push(body);
      await route.fulfill({
        json: { id: body.id, status: "delivered" },
      });
    });
    const origin = `http://127.0.0.1:${server.httpServer.address().port}`;
    await page.goto(`${origin}/check`);
    const timing = await page.evaluate(async () => {
      const client = await import("/src/sync/client.ts");
      return client.DRAFT_SYNC_TIMING_MS;
    });
    await page.evaluate(async () => {
      const { useSyncedDrafts } = await import("/src/sync/drafts.ts");
      const { useOutbox } = await import("/src/sync/send.ts");
      const { db } = await (await import("/src/sync/client.ts")).syncDatabase();
      window.db = db;
      const reactModule = await import("/node_modules/.vite/deps/react.js");
      const domModule =
        await import("/node_modules/.vite/deps/react-dom_client.js");
      const React = reactModule.default || reactModule;
      const { createRoot } = domModule.default || domModule;
      const node = document.createElement("div");
      document.body.appendChild(node);
      window.root = createRoot(node);
      window.root.render(
        React.createElement(function Harness() {
          window.draft = useSyncedDrafts();
          window.outbox = useOutbox();
          return null;
        }),
      );
    });
    await page.waitForFunction(() => !!window.draft);
    await page.waitForTimeout(300);

    const edit = async (text) => {
      await page.evaluate((text) => {
        window.draft.setDrafts((old) => ({ ...old, lead: text }));
        window.lastRecoverySnapshot = {
          journal: Object.entries(localStorage)
            .filter(([key]) =>
              key.startsWith(`codex-drafts:${"f".repeat(32)}:pending:`),
            )
            .map(([, value]) => JSON.parse(value).text),
          local: JSON.parse(
            localStorage.getItem(`codex-chat-draft:${"f".repeat(32)}:lead`),
          )?.text,
        };
      }, text);
      await page.waitForFunction(
        async (text) =>
          (await window.db.drafts.find().exec()).some(
            (doc) => JSON.parse(doc.payload).text === text,
          ),
        text,
      );
    };

    const before = pushes.length;
    const burstSize = 10;
    for (let index = 0; index < burstSize; index++) {
      await edit(`typing ${index}`);
      if (index + 1 < burstSize) await page.waitForTimeout(35);
    }
    const recovery = await page.evaluate(() => window.lastRecoverySnapshot);
    assert.ok(
      recovery.journal.includes(`typing ${burstSize - 1}`) ||
        recovery.local === `typing ${burstSize - 1}`,
      "The latest recovery value is written synchronously before replication settles",
    );
    await page.waitForTimeout(700);
    const burst = pushes.slice(before);
    console.log(`typing burst measured POST count: ${burst.length}`);
    assert.ok(
      burst.length <= 1,
      `A ${burstSize}-edit burst should send at most one draft POST; observed ${burst.length}`,
    );
    assert.equal(
      JSON.parse(burst.at(-1)?.[0]?.newDocumentState?.payload || "{}").text,
      `typing ${burstSize - 1}`,
      "The push carries the final draft value",
    );

    const identicalBefore = pushes.length;
    await edit(`typing ${burstSize - 1}`);
    await page.waitForTimeout(450);
    assert.equal(
      pushes.length,
      identicalBefore,
      "An identical value must not push",
    );

    const retryBefore = pushes.length;
    failNextPush = true;
    await edit("Latest draft after a failed POST");
    const waitForPushCount = async (count) => {
      const deadline = Date.now() + 8_000;
      while (pushes.length < count && Date.now() < deadline)
        await page.waitForTimeout(50);
      assert.ok(
        pushes.length >= count,
        `Expected at least ${count} draft POSTs`,
      );
    };
    await waitForPushCount(retryBefore + 2);
    assert.equal(
      JSON.parse(pushes.at(-1)?.[0]?.newDocumentState?.payload || "{}").text,
      "Latest draft after a failed POST",
      "A rejected push retries and converges to the latest version",
    );

    const maxWaitStart = Date.now();
    const maxWaitPushIndex = pushes.length;
    let pushedDuringTyping = false;
    for (let index = 0; index < 60; index++) {
      await edit(`continuous ${index}`);
      if (pushes.length > maxWaitPushIndex) pushedDuringTyping = true;
      if (index < 59) await page.waitForTimeout(100);
    }
    assert.ok(
      pushedDuringTyping,
      "The maximum wait must push during continuous typing",
    );
    assert.ok(
      pushTimes[maxWaitPushIndex] - maxWaitStart <= timing.pushMaxWait + 600,
      "The first push is bounded from the start of continuous typing",
    );
    assert.ok(
      pushTimes[maxWaitPushIndex] - maxWaitStart >=
        timing.pushMaxWait - timing.pushQuietWait - 200,
      "A quiet-period push must not satisfy the maximum-wait assertion",
    );
    await page.waitForTimeout(400);
    const continuous = pushes.slice(maxWaitPushIndex);
    assert.equal(
      JSON.parse(continuous.at(-1)?.[0]?.newDocumentState?.payload || "{}")
        .text,
      "continuous 59",
      "The quiet-period push after continuous typing carries its final value",
    );

    await edit("Send this before the draft push timer");
    const sendResult = await page.evaluate(async () => {
      const { durableSend } = await import("/src/sync/send.ts");
      const text = window.draft.getDraft("lead");
      const body = {
        id: "draft-coalescing-message",
        room: "lead",
        text,
        delivery: "after_tool",
      };
      const result = await durableSend(body, [], () =>
        window.draft.setDrafts((old) => ({ ...old, lead: "" })),
      );
      return { body, result };
    });
    assert.equal(sendResult.body.text, "Send this before the draft push timer");
    assert.equal(messages.at(-1)?.text, sendResult.body.text);
    assert.equal(sendResult.result.id, "draft-coalescing-message");
    await page.waitForTimeout(400);
    assert.equal(
      JSON.parse(pushes.at(-1)?.[0]?.newDocumentState?.payload || "{}").text,
      "",
      "The clear after durable send still propagates upstream",
    );
    assert.deepEqual(errors, []);
    console.log("production draft replication coalescing contract passed");
  } finally {
    await server.close();
  }
});
