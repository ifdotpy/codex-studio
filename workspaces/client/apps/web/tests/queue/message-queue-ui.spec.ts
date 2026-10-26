import { queueUiFixture } from "./queue-ui.fixture.ts";
import { required, requestBody } from "../messages/helpers.ts";
import type { PostBody } from "../../src/apiTypes.ts";
import type { BrowserContext, Page } from "@playwright/test";
import { test, expect, queryFixtureDatabase } from "../playwright.ts";
// Production renderer and isolated durable queue. No model service or user state.
import assert from "node:assert/strict";
import { join } from "node:path";

test("message-queue-ui", async ({ browser }) => {
  test.setTimeout(45_000);
  const { root, initial, lead, queue, wait, origin } = await queueUiFixture({
    FIXTURE_HOLD: "lead",
  });
  let context: BrowserContext | undefined;
  let page: Page | undefined;
  const diagnostics: unknown[] = [];
  try {
    const waitForQueueMutationToSettle = (label: string) =>
      wait(
        async () =>
          required(page).evaluate(() =>
            Object.keys(localStorage)
              .filter((key) => key.startsWith("studio-queue-change:"))
              .every((key) => localStorage.getItem(key) === null),
          ),
        label,
      );
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
    await page.goto(origin);
    await page.locator(`[data-chat="${lead.id}"]`).click();
    const composer = page.locator("#message"),
      panel = page.getByTestId("message-queue"),
      list = page.getByRole("list", { name: "Queued messages", exact: true });
    const button = (name: string) =>
      panel.getByRole("button", { name, exact: true });
    const editor = () =>
      panel.getByRole("textbox", {
        name: "Edit queued message",
        exact: true,
      });
    await composer.fill("First input");
    // Exercise the composer's file-drop path, not only the file picker.
    const transfer = await page.evaluateHandle(() => {
      const data = new DataTransfer();
      data.items.add(
        new File(["Queue attachment fixture"], "queued-evidence.txt", {
          type: "text/plain",
        }),
      );
      return data;
    });
    await composer.dispatchEvent("drop", { dataTransfer: transfer });
    await transfer.dispose();
    await page
      .getByRole("button", { name: "Remove queued-evidence.txt", exact: true })
      .waitFor();
    await composer.press("Tab");
    await wait(
      async () => (await queue()).items.length === 1,
      "First input reaches queue",
    );
    assert.equal(sent.length, 1);
    assert.equal(
      required(sent[0]).delivery,
      "next_turn",
      "Tab sends next-turn input",
    );
    await panel.waitFor();
    await list.getByText("First input", { exact: true }).waitFor();
    for (const width of [1440, 390]) {
      await page.setViewportSize({ width, height: 960 });
      for (const colorScheme of ["dark", "light"] as const) {
        await page.emulateMedia({ colorScheme });
        await expect(async () => {
          const size = await panel.evaluate((element) => {
            const panelBox = element.getBoundingClientRect();
            const content = element.querySelector("ol");
            const preview = element.querySelector(".message-queue-preview");
            if (!content || !preview) throw new Error("Missing queue preview");
            const contentBox = content.getBoundingClientRect();
            return {
              gap: panelBox.bottom - contentBox.bottom,
              previewHeight: preview.getBoundingClientRect().height,
              previewText: preview.textContent,
            };
          });
          assert(
            size.gap >= 0 && size.gap <= 2,
            `Queue fits its content: ${JSON.stringify(size)}`,
          );
          assert(
            size.previewHeight <= 37,
            `Short text occupies one row: ${JSON.stringify(size)}`,
          );
          assert.equal(size.previewText, "First input");
        }).toPass({ timeout: 5000 });
      }
    }
    await page.setViewportSize({ width: 1440, height: 960 });
    const asset = required(required((await queue()).items[0]).assets?.[0]).id;

    for (const text of ["Second input", "Third input"]) {
      await composer.fill(text);
      await composer.press("Tab");
      await wait(
        async () => (await queue()).items.some((item) => item.text === text),
        `${text} reaches queue`,
      );
    }
    await wait(
      async () => (await queue()).items.length === 3,
      "All inputs are durable",
    );
    assert.equal(sent.length, 3);
    assert(sent.every((entry) => entry.delivery === "next_turn"));
    await button("Edit queued message 1").click();
    await editor().fill("Revised first input");
    await button("Save queued message").click();
    await wait(
      async () =>
        required((await queue()).items[0]).text === "Revised first input",
      "Edit persists",
    );
    await waitForQueueMutationToSettle("Edit acknowledgement completes");
    assert.equal(
      required(required((await queue()).items[0]).assets?.[0]).id,
      asset,
    );
    await button("Move queued message 3 up").click();
    await wait(
      async () => required((await queue()).items[1]).text === "Third input",
      "Reorder persists",
    );
    await waitForQueueMutationToSettle("Reorder acknowledgement completes");
    const reorderedItems = (await queue()).items;
    await expect
      .poll(() =>
        list
          .locator("li")
          .evaluateAll((rows) => rows.map((row) => row.dataset.messageId)),
      )
      .toEqual(reorderedItems.map((item) => item.id));
    const sourceId = required(
      reorderedItems.find((item) => item.text === "Second input"),
    ).id;
    const targetId = required(
      reorderedItems.find((item) => item.text === "Third input"),
    ).id;
    const grip = list
      .locator(`li[data-message-id="${sourceId}"]`)
      .locator(".message-queue-grip");
    await expect(grip).toHaveAttribute("draggable", "true");
    await grip.dragTo(list.locator(`li[data-message-id="${targetId}"]`));
    await wait(
      async () => required((await queue()).items[1]).text === "Second input",
      "Pointer drag reorders the durable queue",
    );
    await waitForQueueMutationToSettle("Drag acknowledgement completes");
    const order = (await queue()).items.map((item) => item.id);
    await page.reload();
    await panel.waitFor();
    assert.deepEqual(
      (await queue()).items.map((item) => item.id),
      order,
    );
    await list.getByText("Revised first input", { exact: true }).waitFor();
    const longText =
      "A long queued message with details that span several lines. ".repeat(9);
    await composer.fill(longText);
    await composer.press("Tab");
    await button("Expand queued message 4").waitFor();
    const shortHeight = await list.locator("li").nth(3).boundingBox();
    await button("Expand queued message 4").click();
    await button("Collapse queued message 4").waitFor();
    const fullHeight = await list.locator("li").nth(3).boundingBox();
    assert(
      required(fullHeight).height > required(shortHeight).height,
      "Long text expands",
    );
    await button("Collapse queued message 4").click();
    const sendNowRequests: PostBody<"/api/queue">[] = [];
    await page.route("**/api/queue", async (route) => {
      if (
        route.request().method() === "POST" &&
        requestBody<PostBody<"/api/queue">>(route).action === "send_now"
      ) {
        sendNowRequests.push(requestBody<PostBody<"/api/queue">>(route));
        if (sendNowRequests.length === 1) return route.abort("failed");
      }
      await route.continue();
    });
    await button("Send queued message 1 now").click();
    await panel.getByRole("alert").waitFor();
    assert.equal(sendNowRequests.length, 1);
    await page.getByRole("button", { name: "Retry queue change" }).click();
    await wait(
      async () => sendNowRequests.length === 2,
      "Retry uses the saved send-now request",
    );
    assert.deepEqual(sendNowRequests[1], sendNowRequests[0]);
    // A promoted current-turn input may leave the queue after native admission.
    // Its durable metadata must retain both the effective and original moment.
    const promoted = () =>
      queryFixtureDatabase(
        root,
        "SELECT e.id,m.record FROM runtime_events e JOIN runtime_event_meta m ON m.id=e.id WHERE e.id=?",
        required(order[0]),
      );
    await wait(
      async () =>
        JSON.parse(String(required(promoted()[0]).record)).delivery ===
        "current_turn",
      "Send now durably promotes the original input to the current turn",
    );
    assert.equal(promoted().length, 1);
    assert.equal(required(promoted()[0]).id, required(sendNowRequests[0]).id);
    assert.equal(
      JSON.parse(String(required(promoted()[0]).record)).requestedDelivery,
      "next_turn",
    );
    await page.unrouteAll({ behavior: "wait" });
    expect(errors).toEqual([]);
    console.log(
      "PASS Tab next-turn queue, edit, asset, reorder, reload and send-now retry identity",
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
