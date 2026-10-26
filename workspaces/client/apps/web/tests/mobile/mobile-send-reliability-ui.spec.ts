import { installMobileSend } from "./mobile-send.fixture.ts";
import { createFixtureViteServer as createServer } from "../playwright.ts";
import {
  fixtureServerOrigin,
  fixturePostBody,
  until,
} from "./browser-fixture.ts";
import { waitForFixturePort } from "../fixture-readiness.ts";
import type { ViteDevServer } from "vite";
import type { BrowserContext, Route } from "@playwright/test";
import type { PostBody, PostResult } from "../../src/apiTypes.ts";

import {
  readTestState,
  test,
  expect,
  spawnFixture as spawn,
  rustUiFixture,
  queryFixtureDatabase,
} from "../playwright.ts";
// Real App, Conversation, and RxDB. Only an isolated fixture receives writes.
import assert from "node:assert/strict";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";

test("mobile-send-reliability-ui", async ({ browser }) => {
  test.setTimeout(120_000);
  const root = fileURLToPath(new URL("../../../../../../", import.meta.url));
  const temporary = await mkdtemp(join(tmpdir(), "studio-mobile-send-ui-"));
  const fixture = spawn(process.execPath, [rustUiFixture, temporary], {
    stdio: ["pipe", "pipe", "pipe"],
  });
  let context: BrowserContext | undefined,
    server: ViteDevServer | undefined,
    log = "";
  fixture.stderr?.on("data", (chunk) => {
    log += chunk;
  });
  try {
    const port = await waitForFixturePort(fixture, { diagnostics: () => log });
    const target = `http://127.0.0.1:${port}`;
    server = await createServer({
      configFile: false,
      root: join(root, "workspaces/client/apps/web"),
      cacheDir: join(temporary, "vite"),
      server: {
        host: "127.0.0.1",
        port: 0,
        hmr: false,
        proxy: {
          "/api": {
            target,
            changeOrigin: true,
            configure(proxy) {
              proxy.on("proxyReq", (request) =>
                request.setHeader("Origin", target),
              );
            },
          },
        },
      },
    });
    await server.listen();
    context = await browser.newContext({
      viewport: { width: 390, height: 844 },
      isMobile: true,
    });
    const page = await context.newPage();
    page.setDefaultTimeout(15000);
    const errors: string[] = [],
      posts: PostBody<"/api/messages">[] = [],
      pending: Route[] = [];
    let replyStatus: "hold" | PostResult<"/api/messages">["status"] = "hold";
    page.on("pageerror", (error) => errors.push(error.message));
    await page.route("**/api/messages", (route) => {
      const body = fixturePostBody<"/api/messages">(route.request());
      posts.push(body);
      if (replyStatus === "hold") pending.push(route);
      else
        return route.fulfill({
          json: {
            id: body.id,
            status: replyStatus,
          } satisfies PostResult<"/api/messages">,
        });
    });
    await page.goto(fixtureServerOrigin(server));
    const waitUntil = (check: () => boolean | Promise<boolean>) =>
      until(check, "Expected mobile send state did not arrive");
    const installSendHarness = () => page.evaluate(installMobileSend);
    const input = page.locator("#message"),
      send = page.locator("#send");
    await input.waitFor();
    await installSendHarness();
    await page.locator('input[type="file"]').setInputFiles(
      Array.from({ length: 9 }, (_, index) => ({
        name: `too-many-${index}.txt`,
        mimeType: "text/plain",
        buffer: Buffer.from("Keep the refused selection"),
      })),
    );
    await page
      .getByText("Attach at most 8 files.", { exact: true })
      .first()
      .waitFor();
    assert.equal(
      await page
        .locator('input[type="file"]')
        .evaluate((input) =>
          input instanceof HTMLInputElement ? input.files?.length : undefined,
        ),
      9,
      "Admission refusal must not clear uncommitted files",
    );
    await page.locator('input[type="file"]').setInputFiles({
      name: "first.txt",
      mimeType: "text/plain",
      buffer: Buffer.from("First send attachment"),
    });
    await page
      .getByRole("button", { name: "Remove first.txt", exact: true })
      .waitFor();
    // A reload can happen after Send but before IndexedDB commits the outbox.
    await page.evaluate(() => {
      window.mobileSend.storage.db.outbox.insert = () => {
        window.mobileSend.insertStarted = true;
        return new Promise(() => {});
      };
    });
    await input.fill("Preserve this uncommitted send");
    await send.click();
    await page.waitForFunction(() => window.mobileSend.insertStarted);
    assert.equal(await input.inputValue(), "Preserve this uncommitted send");
    assert.equal(
      await page
        .getByRole("button", { name: "Remove first.txt", exact: true })
        .count(),
      1,
    );
    assert.equal(posts.length, 0, "No HTTP send before local persistence");
    await page.reload();
    await input.waitFor();
    await waitUntil(
      async () =>
        (await input.inputValue()) === "Preserve this uncommitted send",
    );
    await page
      .getByRole("button", { name: "Remove first.txt", exact: true })
      .waitFor();
    assert.equal(
      posts.length,
      0,
      "Reload does not invent a send for an uncommitted draft",
    );
    await installSendHarness();
    await input.fill("Same intentional message");
    await send.click();
    await waitUntil(() => posts.length === 1);
    assert.ok(posts[0]);
    assert.equal(posts[0].assets?.length, 1);
    await input.fill("Same intentional message");
    const nextStart = Date.now();
    await send.click();
    await waitUntil(() =>
      page.evaluate(
        async () => (await window.mobileSend.queued()).length === 2,
      ),
    );
    assert.ok(
      Date.now() - nextStart < 1500,
      "Second message persists before the first HTTP reply",
    );
    assert.equal(await input.inputValue(), "");
    assert.equal(posts.length, 1, "Same-chat HTTP requests retain their order");
    const initial = await page.evaluate(() => window.mobileSend.queued());
    assert.ok(initial[0]);
    assert.ok(initial[1]);
    assert.notEqual(
      initial[0].id,
      initial[1].id,
      "Two intentional sends have distinct identities",
    );
    assert.equal(initial[0].body.text, initial[1].body.text);
    replyStatus = "delivered";
    const firstPending = pending.shift();
    assert.ok(firstPending);
    await firstPending.fulfill({
      status: 400,
      json: { error: "First message rejected" },
    });
    await waitUntil(() => posts.length === 2);
    await waitUntil(() =>
      page.evaluate(async () =>
        (await window.mobileSend.queued()).some(
          (entry) =>
            entry.body.assets?.length === 0 && entry.status === "accepted",
        ),
      ),
    );
    assert.equal(
      await input.inputValue(),
      "",
      "An older failure cannot restore a newer completed draft",
    );
    assert.equal(
      await page
        .getByRole("button", { name: "Remove first.txt", exact: true })
        .count(),
      0,
      "An older failure cannot restore attachments after a later submission",
    );
    assert.ok(posts[1]);
    assert.equal(posts[1].assets?.length, 0);

    // Hold the next IndexedDB insert. An older HTTP failure must not release the
    // newer send's lock before that send has a durable record.
    replyStatus = "hold";
    await input.fill("Older in-flight send");
    await send.click();
    await waitUntil(() => posts.length === 3);
    await page.evaluate(() => {
      const insert = window.mobileSend.storage.db.outbox.insert.bind(
        window.mobileSend.storage.db.outbox,
      );
      window.mobileSend.storage.db.outbox.insert = async (...args) => {
        if (
          window.mobileSend.readIntention(
            "payload" in args[0] ? String(args[0].payload) : "null",
          ).body.text === "Newer durable write"
        ) {
          window.mobileSend.insertStarted = true;
          await new Promise<void>((resolve) => {
            window.mobileSend.releaseInsert = resolve;
          });
        }
        return insert(...args);
      };
    });
    await input.fill("Newer durable write");
    await send.click();
    await page.waitForFunction(() => window.mobileSend.insertStarted);
    const olderPending = pending.shift();
    assert.ok(olderPending);
    await olderPending.fulfill({
      status: 400,
      json: { error: "Older request rejected" },
    });
    await waitUntil(() =>
      page.evaluate(async () =>
        (await window.mobileSend.queued()).some(
          (entry) =>
            entry.body.text === "Older in-flight send" &&
            entry.status === "failed",
        ),
      ),
    );
    await input.fill("Unsent draft after the click");
    assert.equal(
      await send.isDisabled(),
      true,
      "Older finally cannot release a newer unpersisted send",
    );
    replyStatus = "delivered";
    await page.evaluate(() => window.mobileSend.releaseInsert());
    await waitUntil(() => posts.length === 4);
    await waitUntil(() =>
      page.evaluate(async () =>
        (await window.mobileSend.queued()).some(
          (entry) =>
            entry.body.text === "Newer durable write" &&
            entry.status === "accepted",
        ),
      ),
    );
    assert.equal(await input.inputValue(), "Unsent draft after the click");
    assert.equal(await send.isEnabled(), true);

    // A latest unconfirmed send still restores its own draft and identity.
    replyStatus = "uncertain";
    await input.fill("Latest unconfirmed message");
    await send.click();
    await page.waitForFunction(
      () =>
        document.querySelector<HTMLTextAreaElement>("#message")?.value ===
        "Latest unconfirmed message",
    );
    await waitUntil(() => posts.length === 5);
    const last = posts.at(-1);
    assert.ok(last);
    await send.click();
    await page.waitForFunction(
      () =>
        document.querySelector<HTMLTextAreaElement>("#message")?.value ===
        "Latest unconfirmed message",
    );
    assert.equal(
      posts.length,
      5,
      "Unconfirmed retry retains the prior identity and does not replay HTTP",
    );
    const lastEntries = await page.evaluate(() => window.mobileSend.queued());
    assert.equal(
      lastEntries.filter((entry) => entry.body.text === last.text).length,
      1,
    );
    assert.equal(
      lastEntries.find((entry) => entry.body.text === last.text)?.id,
      last.id,
    );
    await page.context().setOffline(true);
    await input.fill("Mobile offline message");
    await send.click();
    await waitUntil(() =>
      page.evaluate(async () =>
        (await window.mobileSend.queued()).some(
          (entry) =>
            entry.body.text === "Mobile offline message" &&
            entry.status === "queued",
        ),
      ),
    );
    assert.equal(await input.inputValue(), "");
    assert.equal(posts.length, 5);
    replyStatus = "delivered";
    await page.context().setOffline(false);
    await waitUntil(() => posts.length === 6);
    await waitUntil(() =>
      page.evaluate(async () =>
        (await window.mobileSend.queued()).some(
          (entry) =>
            entry.body.text === "Mobile offline message" &&
            entry.status === "accepted",
        ),
      ),
    );
    assert.equal(
      posts.filter((post) => post.text === "Mobile offline message").length,
      1,
    );
    // An old browser receipt can still refer to a message already rejected by
    // the outbox. A reload must not trap every explicit retry on that failed ID.
    const stateDir = (await readTestState(target)).stateDir;
    assert.ok(posts[0]);
    const failedBody: PostBody<"/api/messages"> = {
      id: "old-failed-after-reload",
      room: posts[0].room,
      text: "Retry a stored terminal failure",
      assets: [],
      delivery: "queue",
    };
    const snapshotBeforeRetry = await readTestState(target);
    const failedChatName = snapshotBeforeRetry.runtime.agents.find(
      (agent) => agent.id === failedBody.room,
    )?.name;
    assert.ok(failedChatName, "The stored failure belongs to a fixture chat");
    const pendingKey = `studio-pending-sends:${stateDir}`;
    await page.evaluate(
      async ({ body, pendingKey }) => {
        await window.mobileSend.storage.db.outbox.insert({
          id: body.id,
          seq: 0,
          payload: JSON.stringify({
            body,
            status: "failed",
            error: "Old stored rejection",
            created: Date.now(),
            displayPending: true,
            attachments: [],
          }),
        });
        localStorage.setItem(pendingKey, JSON.stringify({ [body.room]: body }));
        localStorage.setItem("codex-mobile-opened", JSON.stringify(body.room));
      },
      { body: failedBody, pendingKey },
    );
    await page.reload();
    await input.waitFor();
    await page.waitForFunction(
      (name) =>
        document.querySelector("#conversation-title")?.textContent?.trim() ===
        name,
      failedChatName,
    );
    await input.fill(failedBody.text);
    const beforeExplicitRetry = posts.length;
    await send.click();
    await page.waitForFunction(
      ({ pendingKey, room }) => {
        // The renderer wrote this pending-send map.
        const pending: Record<string, PostBody<"/api/messages">> = JSON.parse(
          localStorage.getItem(pendingKey) || "{}",
        );
        return !pending[room];
      },
      { pendingKey, room: failedBody.room },
    );
    await waitUntil(() => posts.length === beforeExplicitRetry + 1);
    assert.notEqual(posts.at(-1)?.id, failedBody.id);
    assert.equal(posts.at(-1)?.text, failedBody.text);

    // A second tab can retry while the first tab has a committed but stalled
    // response. The real HTTP handler and SQLite retain one logical message.
    const snapshot = await readTestState(target);
    const other = snapshot.runtime.agents.find(
      (agent) => agent.name === "Other project",
    );
    assert.ok(other);
    const realBody: PostBody<"/api/messages"> = {
      id: crypto.randomUUID(),
      room: other.id,
      text: "One real message across two mobile tabs",
      assets: [],
      delivery: "queue",
    };
    await page.unroute("**/api/messages");
    let committed = false;
    const held: { response?: Route } = {};
    const realPosts: PostBody<"/api/messages">[] = [];
    await page.context().route("**/api/messages", async (route) => {
      const request = fixturePostBody<"/api/messages">(route.request());
      realPosts.push(request);
      const response = await route.fetch();
      assert.equal(response.status(), 200);
      if (realPosts.length === 1) {
        committed = true;
        held.response = route;
      } else await route.fulfill({ response });
    });
    await page.evaluate(async (body) => {
      window.mobileSend.httpSend = (
        await import("/src/sync/send.ts")
      ).durableSend(body);
    }, realBody);
    await waitUntil(() => committed);
    const peer = await page.context().newPage();
    await peer.route("**/send-peer", (route) =>
      route.fulfill({
        contentType: "text/html",
        body: "<!doctype html><title>Second mobile tab</title>",
      }),
    );
    await peer.goto(fixtureServerOrigin(server) + "send-peer");
    const peerResult = await peer.evaluate(
      async (body) => (await import("/src/sync/send.ts")).durableSend(body),
      realBody,
    );
    assert.equal(peerResult.id, realBody.id);
    assert.ok(realPosts.length >= 2);
    assert.ok(
      realPosts.every(
        (request) => JSON.stringify(request) === JSON.stringify(realBody),
      ),
    );
    assert.ok(held.response);
    await held.response.abort("failed");
    assert.equal(
      (await page.evaluate(() => window.mobileSend.httpSend)).id,
      realBody.id,
    );
    await page.reload();
    await input.waitFor();
    const counts = [
      queryFixtureDatabase(
        temporary,
        "SELECT COUNT(*) AS n FROM runtime_events WHERE id=?",
        realBody.id,
      )[0]?.n,
      queryFixtureDatabase(
        temporary,
        "SELECT COUNT(*) AS n FROM messages WHERE id=?",
        realBody.id,
      )[0]?.n,
    ];
    assert.deepEqual(counts, [1, 1]);
    expect(errors).toEqual([]);
    console.log(
      "PASS: draft and attachments survive reload before outbox commit, responsive mobile composer, ordered distinct sends, late error draft/attachment guard, late finally lock guard, safe uncertain retry, offline reconnect, real cross-tab HTTP deduplication",
    );
  } finally {
    await context?.close();
    await server?.close();
    fixture.stdin?.end();
    await context?.close();
    await rm(join(temporary, "vite"), { recursive: true, force: true });
  }
});
