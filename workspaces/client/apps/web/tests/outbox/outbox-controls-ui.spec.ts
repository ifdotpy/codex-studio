import { responseJson } from "../messages/helpers.ts";
import { requestBody } from "../messages/helpers.ts";
import { readTranscript } from "../drafts/draft-fixture-helpers.ts";
import type { Browser, BrowserContext, Route } from "playwright";
import type { ViteDevServer } from "vite";
import type { components } from "../../src/generated/api.ts";
type TranscriptIdentity = Pick<
  components["schemas"]["TranscriptInput"],
  "id" | "clientMessageId"
>;
import type {
  PostBody,
  GetResult,
  TranscriptPageData,
} from "../../src/apiTypes.ts";
declare global {
  interface Window {
    outboxControls_outboxModule: typeof import("../../src/sync/send.ts");
    outboxControls_syncModule: typeof import("../../src/sync/client.ts");
    outboxControls_raceResult: ReturnType<
      typeof import("../../src/sync/send.ts").durableSend
    >;
  }
}
import { fixtureOrigin } from "../drafts/draft-fixture-helpers.ts";
// Full React client and shared RxDB, isolated server. No live agent requests.
import { FIXTURE_OPTIMIZE_DEPS } from "../playwright.ts";
import assert from "node:assert/strict";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";

import {
  installApiSchemaFixtureHeaders,
  test,
  expect,
  spawnFixture as spawn,
  rustUiFixture,
} from "../playwright.ts";

test("outbox-controls-ui", async ({ browser: fixtureBrowser }) => {
  test.setTimeout(45_000);
  const repo = fileURLToPath(new URL("../../../../../../", import.meta.url));

  const { createFixtureViteServer: createServer } =
    await import("../playwright.ts");
  const evidence = await mkdtemp(join(tmpdir(), "studio-outbox-controls-"));
  const fixture = spawn(process.execPath, [rustUiFixture, evidence], {
    stdio: ["ignore", "pipe", "pipe"],
    // Selects SettingsRuntime, which publishes changes without starting the waiting turn.
    env: { ...process.env, FIXTURE_WORKER_COUNT: "1" },
  });
  let browser: Browser;
  let context: BrowserContext | undefined;
  let server: ViteDevServer | undefined;
  let log = "";
  fixture.stderr?.on("data", (chunk) => {
    log += chunk;
  });
  async function until(check: () => unknown | Promise<unknown>) {
    const deadline = Date.now() + 15000;
    while (Date.now() < deadline) {
      if (await check()) return;
      await new Promise((resolve) => setTimeout(resolve, 25));
    }
    throw new Error("The expected outbox state did not arrive.");
  }
  try {
    const port = await new Promise<number>((resolve, reject) => {
      fixture.stdout?.once("data", (chunk) =>
        resolve(Number(String(chunk).trim())),
      );
      fixture.once("exit", () => reject(new Error(log)));
    });
    const target = `http://127.0.0.1:${port}`;
    const identity = await responseJson<GetResult<"/api/sync/identity">>(
      await fetch(target + "/api/sync/identity"),
    );
    server = await createServer({
      optimizeDeps: { ...FIXTURE_OPTIMIZE_DEPS },
      configFile: false,
      root: join(repo, "workspaces/client/apps/web"),
      cacheDir: join(evidence, "vite"),
      server: {
        host: "127.0.0.1",
        port: 0,
        proxy: {
          "/api": {
            target,
            changeOrigin: true,
            configure(proxy) {
              proxy.on("proxyReq", (request) =>
                request.setHeader("origin", target),
              );
            },
          },
        },
      },
    });
    await server.listen();
    const origin = fixtureOrigin(server);
    browser = fixtureBrowser;
    context = await browser.newContext({
      viewport: { width: 1440, height: 960 },
    });
    installApiSchemaFixtureHeaders(context);
    const errors: string[] = [],
      posts: PostBody<"/api/messages">[] = [];
    let available = true,
      holdSession: ((route: Route) => void | Promise<void>) | null = null,
      holdPost: ((route: Route) => void | Promise<void>) | null = null,
      showDeliveredOutbox = false;
    let deliveredTranscriptDocument:
      | Extract<
          GetResult<"/api/sync/pull">,
          { documents: unknown }
        >["documents"][number]
      | null = null;
    await context.route("**/api/sync/identity", (route) =>
      route.fulfill({
        status: available ? 200 : 503,
        json: available ? identity : { error: "Connection unavailable" },
      }),
    );
    await context.route("**/api/session", async (route) => {
      if (holdSession) return holdSession(route);
      return route.fallback();
    });
    await context.route("**/api/messages", async (route) => {
      if (route.request().method() !== "POST") return route.fallback();
      const body = requestBody<PostBody<"/api/messages">>(route);
      posts.push(body);
      if (holdPost) return holdPost(route);
      const response = await fetch(target + "/api/messages", {
        method: "POST",
        headers: {
          "content-type": "application/json",
          origin: target,
          "X-Canvas-Token": route.request().headers()["x-canvas-token"] ?? "",
        },
        body: JSON.stringify(body),
      });
      await route.fulfill({
        status: response.status,
        contentType: "application/json",
        body: await response.text(),
      });
    });
    const deliverTranscriptOutbox = async (route: Route) => {
      if (!showDeliveredOutbox) return route.fallback();
      const response = await route.fetch();
      const transcript = await responseJson<TranscriptPageData>(response);
      transcript.items = markOutboxDelivered(transcript.items);
      await route.fulfill({ response, json: transcript });
    };
    const isLostResponse = (item: TranscriptIdentity) =>
      item.clientMessageId === "lost-response-pause" ||
      item.id === "lost-response-pause" ||
      (typeof item.id === "string" &&
        (item.id.endsWith(":lost-response-pause") ||
          item.id.endsWith(":lost-response-pause:0")));
    const markOutboxDelivered = (
      items: TranscriptPageData["items"],
    ): TranscriptPageData["items"] =>
      items.map((item) => {
        if (isLostResponse(item))
          return {
            ...item,
            deliveryStatus: "delivered",
            materialized: true,
            pending: false,
          };
        if (!item.inputs) return item;
        return {
          ...item,
          inputs: item.inputs.map((input) =>
            isLostResponse(input)
              ? {
                  ...input,
                  deliveryStatus: "delivered",
                  materialized: true,
                  pending: false,
                }
              : input,
          ),
        };
      });
    // Queue-mode acceptance stays pending in the fixture after the retry.
    // Serve its later transcript materialization to the reloaded page.
    await context.route("**/api/transcript?*", deliverTranscriptOutbox);
    await context.route("**/api/transcript/page?*", deliverTranscriptOutbox);
    await context.route("**/api/queue?*", async (route) => {
      if (!showDeliveredOutbox || route.request().method() !== "GET")
        return route.fallback();
      const response = await route.fetch();
      const queue = await responseJson<GetResult<"/api/queue">>(response);
      queue.items = queue.items.filter(
        (item) => item.id !== "lost-response-pause",
      );
      await route.fulfill({ response, json: queue });
    });
    await context.route("**/api/sync/pull?*", async (route) => {
      if (!showDeliveredOutbox) return route.fallback();
      const pullUrl = new URL(route.request().url());
      const scope = pullUrl.searchParams.get("scope");
      if (!scope?.startsWith("transcript:")) return route.fallback();
      const response = await route.fetch();
      const result = await responseJson<GetResult<"/api/sync/pull">>(response);
      if (!("documents" in result))
        throw new Error("Unexpected reset response");
      const current = result.documents.find(
        (document) => document.id === scope,
      );
      if (!deliveredTranscriptDocument) {
        const transcript: TranscriptPageData =
          current && "payload" in current
            ? readTranscript(current.payload)
            : await responseJson<TranscriptPageData>(
                await fetch(
                  `${target}/api/transcript?id=${scope.slice("transcript:".length)}`,
                ),
              );
        transcript.items = markOutboxDelivered(transcript.items);
        const after = Number(pullUrl.searchParams.get("after") || 0);
        deliveredTranscriptDocument = {
          id: scope,
          seq:
            Math.max(after, Number(result.maxSeq) || 0, current?.seq || 0) + 1,
          payload: JSON.stringify(transcript),
          _deleted: false,
        };
      }
      const after = Number(pullUrl.searchParams.get("after") || 0);
      const seq = Math.max(
        Number(result.maxSeq) || 0,
        deliveredTranscriptDocument.seq,
      );
      await route.fulfill({
        response,
        json: {
          ...result,
          documents:
            after < deliveredTranscriptDocument.seq
              ? [deliveredTranscriptDocument]
              : [],
          checkpoint: {
            ...result.checkpoint,
            seq: Math.max(after, deliveredTranscriptDocument.seq),
          },
          maxSeq: seq,
        },
      });
    });
    const page = await context.newPage();
    page.setDefaultTimeout(15000);
    page.on("pageerror", (error) => errors.push(error.message));
    page.on("console", (message) => {
      if (
        message.type() === "error" &&
        message.text().includes("Encountered two children")
      )
        errors.push(message.text());
    });
    await page.goto(origin);
    await page.locator("#message").waitFor();
    await page.evaluate(async () => {
      window.outboxControls_outboxModule = await import("/src/sync/send.ts");
      window.outboxControls_syncModule = await import("/src/sync/client.ts");
      await window.outboxControls_syncModule.syncDatabase();
    });
    const entry = (text: string) =>
      page.locator(".message").filter({ hasText: text });
    const confirmedMessage = async (text: string) => {
      await entry(text).waitFor();
      await entry(text).getByRole("status").waitFor({ state: "detached" });
      assert.equal(
        await entry(text).getByRole("status").count(),
        0,
        "The accepted message is visible without a pending status",
      );
    };
    const serverTranscriptHas = async (
      body: PostBody<"/api/messages">,
      text: string,
    ) => {
      const transcript = await responseJson<TranscriptPageData>(
        await fetch(`${target}/api/transcript?id=${body.room}`),
      );
      const key = `${body.room}:${body.id}`;
      const matches = (item: TranscriptIdentity) =>
        item.clientMessageId === body.id ||
        item.id === body.id ||
        item.id === key ||
        (!item.clientMessageId && item.id === `${key}:0`);
      return transcript.items.some(
        (item) =>
          (matches(item) && item.text === text) ||
          item.inputs?.some((input) => matches(input) && input.text === text),
      );
    };
    const accepted = async (text: string, { rendered = true } = {}) => {
      try {
        await until(() =>
          page.evaluate(async (text) => {
            const { syncDatabase } = await import("/src/sync/client.ts");
            const { db } = await syncDatabase();
            const { readOutbox } =
              await import("/src/../tests/drafts/draft-fixture-helpers.ts");
            const rows = await db.outbox.find().exec();
            return rows.some((row) => {
              const value = readOutbox(row.payload);
              return value.body.text === text && value.status === "accepted";
            });
          }, text),
        );
      } catch {
        const rows = await page.evaluate(async () => {
          const { db } = await (
            await import("/src/sync/client.ts")
          ).syncDatabase();
          return (await db.outbox.find().exec()).map((row) => ({
            payload: row.payload,
            status: "status" in row ? row.status : undefined,
            error: "error" in row ? row.error : undefined,
          }));
        });
        throw new Error(
          `Expected accepted outbox state for ${text}: ${JSON.stringify({ posts, rows })}`,
        );
      }
      if (rendered) {
        await entry(text).waitFor();
        await expect(entry(text).getByRole("status")).toHaveText(
          "Waiting for agent",
        );
      } else {
        const body = posts.at(-1);
        assert.ok(body);
        assert.ok(
          await serverTranscriptHas(body, text),
          "The accepted outbox request appears in the server transcript",
        );
      }
    };
    available = false;
    await page.locator("#message").fill("Cancel while offline");
    await page.locator("#send").click();
    await entry("Cancel while offline")
      .getByRole("button", { name: "Cancel send", exact: true })
      .click();
    await entry("Cancel while offline")
      .getByRole("status")
      .filter({ hasText: /^Cancelled$/ })
      .waitFor();
    assert.equal(posts.length, 0);
    available = true;
    await page.reload();
    await page.locator("#message").waitFor();
    await page.evaluate(() => window.dispatchEvent(new Event("pageshow")));
    await entry("Cancel while offline")
      .getByRole("status")
      .filter({ hasText: /^Cancelled$/ })
      .waitFor();
    assert.equal(posts.length, 0, "Cancellation survives reload and resume");

    await page.locator('input[type="file"]').setInputFiles({
      name: "notes.txt",
      mimeType: "text/plain",
      buffer: Buffer.from("Saved attachment"),
    });
    await page
      .getByRole("button", { name: "Remove notes.txt", exact: true })
      .waitFor();
    available = false;
    await page.locator("#message").fill("Edit while offline");
    await page.locator("#send").click();
    await entry("Edit while offline")
      .getByRole("button", { name: "Edit message", exact: true })
      .waitFor();
    await page.locator("#message").fill("Another draft");
    await entry("Edit while offline")
      .getByRole("button", { name: "Edit message", exact: true })
      .click();
    await entry("Edit while offline").getByRole("alert").waitFor();
    assert.equal(await page.locator("#message").inputValue(), "Another draft");
    await page.locator("#message").fill("");
    await entry("Edit while offline")
      .getByRole("button", { name: "Edit message", exact: true })
      .click();
    await page.waitForFunction(
      () =>
        document.querySelector<HTMLTextAreaElement>("#message")?.value ===
        "Edit while offline",
    );
    await page
      .getByRole("button", { name: "Remove notes.txt", exact: true })
      .waitFor();
    await entry("Edit while offline")
      .getByRole("status")
      .filter({ hasText: /^Cancelled$/ })
      .waitFor();
    await page.locator("#message").fill("Corrected offline message");
    available = true;
    await page.locator("#send").click();
    await accepted("Corrected offline message");
    assert.equal(posts.length, 1);
    assert.equal(posts[0]?.text, "Corrected offline message");
    assert.equal(posts[0]?.assets?.length, 1);
    await page.evaluate(async () => {
      window.outboxControls_outboxModule = await import("/src/sync/send.ts");
      window.outboxControls_syncModule = await import("/src/sync/client.ts");
    });

    const second = await context.newPage();
    await second.route("**/empty-check", (route) =>
      route.fulfill({
        contentType: "text/html",
        body: "<!doctype html><title>Second tab</title>",
      }),
    );
    await second.goto(origin + "empty-check");
    await second.evaluate(async () => {
      window.outboxControls_outboxModule = await import("/src/sync/send.ts");
      await (await import("/src/sync/client.ts")).syncDatabase();
    });
    const room = posts[0]?.room;
    assert.ok(room);
    const beforeRaces = posts.length;
    const pendingSessions: Route[] = [];
    assert.equal(
      await second.evaluate(() =>
        window.outboxControls_outboxModule.stopRemovedMessage(
          "remove-before-store",
        ),
      ),
      false,
    );
    const removedBeforeStore = await page.evaluate(
      (room) =>
        window.outboxControls_outboxModule.durableSend({
          id: "remove-before-store",
          room,
          text: "Removed before the durable insert",
        }),
      room,
    );
    assert.equal(removedBeforeStore.status, "cancelled");
    assert.equal(
      posts.length,
      beforeRaces,
      "Removal before the document exists prevents a later HTTP claim.",
    );
    holdSession = (route) => {
      pendingSessions.push(route);
    };
    await page.evaluate((room) => {
      window.outboxControls_raceResult =
        window.outboxControls_outboxModule.durableSend({
          id: "cancel-before-post",
          room,
          text: "Never post this",
        });
    }, room);
    await until(() =>
      page.evaluate(async () => {
        const { db } = await window.outboxControls_syncModule.syncDatabase();
        return !!(await db.outbox.findOne("cancel-before-post").exec());
      }),
    );
    assert.equal(
      await second.evaluate(() =>
        window.outboxControls_outboxModule.stopRemovedMessage(
          "cancel-before-post",
        ),
      ),
      true,
    );
    holdSession = null;
    for (const route of pendingSessions) await route.fallback();
    const cancelled = await page.evaluate(
      () => window.outboxControls_raceResult,
    );
    assert.equal(cancelled.status, "cancelled");
    assert.equal(
      posts.length,
      beforeRaces,
      "A second tab cancels before the HTTP claim",
    );

    const heldPost: { route?: Route } = {};
    holdPost = (route) => {
      heldPost.route = route;
    };
    const body = {
      id: "lost-response-pause",
      room,
      text: "Keep the immutable request",
      assets: [],
      delivery: "queue",
    } satisfies PostBody<"/api/messages">;
    await page.evaluate((body) => {
      window.outboxControls_raceResult =
        window.outboxControls_outboxModule.durableSend(body);
    }, body);
    await until(() =>
      page.evaluate(async () => {
        const { db } = await window.outboxControls_syncModule.syncDatabase();
        const doc = await db.outbox.findOne("lost-response-pause").exec();
        const { readOutbox } =
          await import("/src/../tests/drafts/draft-fixture-helpers.ts");
        return doc && readOutbox(doc.payload).attempted;
      }),
    );
    const forbidden = await second.evaluate(async () => {
      try {
        await window.outboxControls_outboxModule.changeOutbox(
          "lost-response-pause",
          "cancel",
        );
        return "";
      } catch (error) {
        if (!(error instanceof Error)) throw error;
        return error.message;
      }
    });
    assert.match(forbidden, /Delivery may have started/);
    assert.equal(
      await second.evaluate(() =>
        window.outboxControls_outboxModule.stopRemovedMessage(
          "lost-response-pause",
        ),
      ),
      false,
    );
    await second.close();
    const deadline = Date.now() + 5000;
    while (!heldPost.route && Date.now() < deadline)
      await new Promise((resolve) => setTimeout(resolve, 20));
    assert.ok(heldPost.route);
    await heldPost.route.abort("failed");
    holdPost = null;
    const paused = await page.evaluate(() => window.outboxControls_raceResult);
    assert.equal(paused.status, "paused");
    await entry("Keep the immutable request")
      .getByRole("button", { name: "Resume retries", exact: true })
      .waitFor();
    const pausedCount = posts.length;
    await page.reload();
    await page.getByText("Resume retries", { exact: true }).waitFor();
    await page.evaluate(() => window.dispatchEvent(new Event("pageshow")));
    assert.equal(posts.length, pausedCount);
    await page.getByText("Resume retries", { exact: true }).click();
    await accepted("Keep the immutable request", { rendered: false });
    assert.deepEqual(
      posts.slice(-2),
      [body, body],
      "Explicit resume retries the same body and ID",
    );
    showDeliveredOutbox = true;
    await page.reload();
    await page.locator("#message").waitFor();
    await confirmedMessage("Keep the immutable request");
    expect(errors).toEqual([]);
    const nativePosts = posts.length;
    const forbiddenNativeMutations: string[] = [];
    page.on("request", (request) => {
      if (
        request.method() === "POST" &&
        ["/api/queue", "/api/stop"].includes(new URL(request.url()).pathname)
      )
        forbiddenNativeMutations.push(request.url());
    });
    await page.evaluate(
      async ({ room, stateDir, workspaceId }) => {
        const { removeSendingMessage } =
          await import("/src/components/removeSendingMessages.ts");
        await removeSendingMessage(
          { stateDir, workspaceId },
          {
            chat: room,
            message: {
              id: `${room}:native-dispatch`,
              clientMessageId: "native-dispatch",
              role: "user",
              text: "Already in native dispatch",
              deliveryStatus: "dispatching",
            },
          },
        );
        const { readRemovedKeys } =
          await import("/src/../tests/drafts/draft-fixture-helpers.ts");
        const keys = readRemovedKeys(
          localStorage.getItem(
            `studio-removed-messages:${JSON.stringify([stateDir, "agent", room])}`,
          ) ?? "null",
        );
        if (!keys.includes("client:native-dispatch"))
          throw new Error("Native message was not removed.");
      },
      { room, stateDir: evidence, workspaceId: identity.workspaceId },
    );
    assert.equal(posts.length, nativePosts);
    assert.deepEqual(forbiddenNativeMutations, []);
    console.log(
      "PASS: offline cancel, edit with attachments, existing draft guard, reload, cross-tab cancellation boundary, pause after lost response, immutable resume",
    );
  } finally {
    await context?.close();
    await server?.close();
    await rm(join(evidence, "vite"), { recursive: true, force: true });
  }
});
