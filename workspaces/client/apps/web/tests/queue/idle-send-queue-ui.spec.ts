import { queueUiFixture } from "./queue-ui.fixture.ts";
import { required, requestBody, responseJson } from "../messages/helpers.ts";
import type {
  PostBody,
  GetResult,
  TranscriptPageData,
} from "../../src/apiTypes.ts";
import type { BrowserContext, Page } from "@playwright/test";
import { test, expect } from "../playwright.ts";
// Production renderer and isolated durable queue. No model service or user state.
import assert from "node:assert/strict";
import { join } from "node:path";
test("idle send queue ui", async ({ browser }) => {
  const { root, initial, lead, wait, origin } = await queueUiFixture({
    FIXTURE_SCRIPT: JSON.stringify([
      { prompt: "Normal idle send", steps: [{ hold: true }] },
    ]),
  });
  let context: BrowserContext | undefined;
  let page: Page | undefined;
  const diagnostics: unknown[] = [];
  try {
    // Fault injection must observe fetches after reload, including in WebKit.
    context = await browser.newContext({
      viewport: { width: 1440, height: 960 },
      serviceWorkers: "block",
    });
    page = await context.newPage();
    page.setDefaultTimeout(10000);
    const errors: string[] = [];
    const sent: PostBody<"/api/messages">[] = [];
    const mutations: PostBody<"/api/queue">[] = [];
    page.on("pageerror", (error) => {
      errors.push(error.message);
      diagnostics.push({
        kind: "pageerror",
        message: error.message,
        stack: error.stack,
        at: Date.now(),
      });
    });
    page.on("requestfailed", (request) =>
      diagnostics.push({
        kind: "requestfailed",
        url: request.url(),
        failure: request.failure(),
        at: Date.now(),
      }),
    );
    page.on("request", (request) => {
      if (request.method() !== "POST") return;
      const path = new URL(request.url()).pathname;
      if (path === "/api/messages")
        sent.push(
          requestBody<PostBody<"/api/messages">>({ request: () => request }),
        );
      if (path === "/api/queue")
        mutations.push(
          requestBody<PostBody<"/api/queue">>({ request: () => request }),
        );
    });
    await page.addInitScript(
      ({ id, stateDir }) => {
        localStorage.setItem(
          `codex-desktop-opened:${stateDir}`,
          JSON.stringify(id),
        );
        localStorage.setItem("codex-mobile-opened", JSON.stringify(id));
      },
      { id: lead.id, stateDir: initial.stateDir },
    );
    let deliveredId: string | null = null;
    await page.route("**/api/transcript?*", async (route) => {
      const response = await route.fetch();
      const payload: TranscriptPageData =
        await responseJson<GetResult<"/api/transcript">>(response);
      if (
        deliveredId &&
        new URL(route.request().url()).searchParams.get("id") === lead.id
      ) {
        payload.items = payload.items.map((item) =>
          item.clientMessageId === deliveredId ||
          item.id === `${lead.id}:${deliveredId}`
            ? {
                ...item,
                pending: false,
                materialized: true,
                deliveryStatus: "delivered",
              }
            : item,
        );
      }
      await route.fulfill({ response, json: payload });
    });
    await page.goto(origin);
    await page.locator(`[data-chat="${lead.id}"]`).click();
    const composer = page.locator("#message"),
      panel = page.getByTestId("message-queue");
    const bubble = (text: string) =>
      required(page)
        .locator("#messages .message.user")
        .filter({ hasText: text });
    assert(!lead.inFlight && !lead.turnId, "Fixture chat has no active turn");
    await composer.fill("Normal idle send");
    await composer.press("Enter");
    await wait(async () => sent.length === 1, "Idle Send reaches scheduler");
    assert.equal(sent.length, 1);
    assert.equal(required(sent[0]).delivery, "current_turn");
    // A message for after the current tool call is never queued. It stays in
    // the chat while it waits, also after a reload.
    await bubble("Normal idle send").waitFor();
    // Tab sends the draft for after the turn; it waits in the queue panel.
    await composer.fill("Tab queues this draft");
    await composer.press("Tab");
    await wait(async () => sent.length === 2, "Tab input reaches scheduler");
    assert.equal(required(sent[1]).delivery, "next_turn");
    assert.equal(await composer.inputValue(), "");
    await panel.getByText("Tab queues this draft", { exact: true }).waitFor();
    assert.equal(await bubble("Tab queues this draft").count(), 0);
    await page.waitForTimeout(3500);
    assert.equal(
      await panel.getByText("Normal idle send", { exact: true }).count(),
      0,
      "an after-tool message is not in the queue",
    );
    await page.reload();
    await bubble("Normal idle send").waitFor();
    assert.equal(
      await panel.getByText("Normal idle send", { exact: true }).count(),
      0,
      "Reload keeps the after-tool message out of the queue",
    );
    assert.equal(sent.length, 2, "Reload does not send input again");
    expect(errors).toEqual([]);
    console.log(
      "PASS idle send, Tab queues after the turn, after-tool messages stay out of the queue, reload identity",
    );
  } catch (error) {
    console.error(error);
    console.error(JSON.stringify({ evidence: root, diagnostics }, null, 2));
    await page?.screenshot({ path: join(root, "failure.png") }).catch(() => {});
    throw error;
  } finally {
    await page?.unrouteAll({ behavior: "ignoreErrors" });
    await context?.close();
  }
});
