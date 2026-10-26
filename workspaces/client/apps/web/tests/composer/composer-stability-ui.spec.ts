import type { GetResult, PostResult } from "../../src/apiTypes.ts";
import type { Page, Route } from "@playwright/test";
import type { TestAgent } from "../playwright.ts";
import { fixtureGetResult, fixturePostBody } from "../chat/http.ts";
import {
  waitForFixturePort,
  readTestState,
  test,
  expect,
  spawnFixture as spawn,
  rustUiFixture,
  readFixtureSyncContract,
  stubEntityState,
} from "../playwright.ts";
import assert from "node:assert/strict";
import { mkdtemp, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
type Geometry = Pick<DOMRect, "x" | "y" | "width" | "height">;
type Boxes = Record<string, Geometry>;
const boxAt = (boxes: Boxes, selector: string) => {
  const box = boxes[selector];
  assert.ok(box, `Missing geometry for ${selector}`);
  return box;
};
declare global {
  interface Window {
    composerGeometryFrame: string | null;
    composerTextareaBefore: Element | null;
    composerTransitionSamples: Pick<Geometry, "y" | "height">[];
    composerTransitionSampling: boolean;
  }
}
// Real production React, isolated HTTP fixture, controlled status and delivery timing.

test("composer-stability-ui", async ({ browser: fixtureBrowser }) => {
  test.setTimeout(120_000);
  const evidence = await mkdtemp(join(tmpdir(), "studio-composer-stability-"));
  const fixture = spawn(process.execPath, [rustUiFixture, evidence], {
    stdio: ["pipe", "pipe", "pipe"],
    env: { ...process.env, FIXTURE_WORKER_COUNT: "1" },
  });
  const pages = new Set<Page>();
  let log = "";
  fixture.stderr?.on("data", (data) => (log += data));
  try {
    const port = await waitForFixturePort(fixture, { diagnostics: () => log });
    const origin = `http://127.0.0.1:${port}`;
    const initial = await readTestState(origin);
    const lead = initial.threads.find(
      (agent) => agent.name === "Other project",
    );
    const emptyWorker = initial.threads.find(
      (agent) => !agent.isLead && agent.parentId,
    );
    assert.ok(lead, "fixture provides an empty managed lead chat");
    assert.ok(emptyWorker, "fixture provides a persistent managed worker chat");

    const measurements: { width: number; label: string; boxes: Boxes }[] = [];
    for (const width of [1440, 900, 390]) {
      const openedChat: TestAgent = width === 390 ? lead : emptyWorker;
      assert.ok(openedChat.name);
      const originalTranscript = await fixtureGetResult(
        "/api/transcript",
        await fetch(`${origin}/api/transcript?id=${openedChat.id}`),
      );
      const state = structuredClone(initial);
      const controlledAgent = state.runtime.agents.find(
        (agent) => agent.id === openedChat.id,
      );
      assert.ok(controlledAgent);
      Object.assign(controlledAgent, {
        empty: true,
        status: "completed",
        inFlight: false,
        turnId: null,
      } satisfies Partial<TestAgent>);
      // This fixture sends the entity projection in place of the transcript agent projection.
      const transcript = {
        ...structuredClone(originalTranscript),
        agent: controlledAgent,
        items: [],
      } satisfies Omit<GetResult<"/api/transcript">, "agent"> & {
        agent: TestAgent;
      };
      const page = await fixtureBrowser.newPage({
        viewport: { width: 1440, height: 960 },
      });
      pages.add(page);
      page.setDefaultTimeout(12000);
      const { update: updateEntityState } = await stubEntityState(
        page,
        state,
        await readFixtureSyncContract(origin),
      );
      await page.addInitScript(
        ({ stateDir, chatId }) => {
          localStorage.setItem(
            `codex-desktop-opened:${stateDir}`,
            JSON.stringify(chatId),
          );
          localStorage.setItem("codex-mobile-opened", JSON.stringify(chatId));
        },
        { stateDir: initial.stateDir, chatId: openedChat.id },
      );
      // Legacy queue fixture intentionally predates QueueItem; preserve its wire payload.
      let queue: unknown[] = [];
      let queueResponseGate: Promise<void> | null = null;
      let queueRequested = Promise.withResolvers<void>();
      let queueResponse = Promise.withResolvers<void>();
      const stopCaptured = Promise.withResolvers<Route>();
      let sendCaptured = Promise.withResolvers<Route>();
      const waitForSend = () =>
        Promise.race([
          sendCaptured.promise,
          page.waitForTimeout(12000).then(() => {
            throw Error("send API request was not captured");
          }),
        ]);
      const errors: string[] = [];
      page.on("pageerror", (error) => errors.push(error.message));
      await page.route("**/api/transcript/stream?*", (route) =>
        route.fulfill({
          contentType: "text/event-stream",
          body: `data: ${JSON.stringify({ ...transcript, items: transcript.items || [], order: [], replace: true })}\n\n`,
        }),
      );
      await page.route("**/api/transcript?*", (route) =>
        route.fulfill({ json: transcript }),
      );
      await page.route("**/api/queue?*", async (route) => {
        queueRequested.resolve();
        if (queueResponseGate) await queueResponseGate;
        await route.fulfill({ json: { items: queue } });
      });
      await page.route("**/api/messages", (route) => {
        sendCaptured.resolve(route);
      });
      await page.route("**/api/stop", (route) => {
        stopCaptured.resolve(route);
      });
      await page.setViewportSize({ width, height: 960 });
      await page.goto(origin);
      // Phones intentionally start at the fresh board; choose the fixture chat.
      if (width <= 760)
        await page
          .locator("[data-chat]")
          .filter({ hasText: openedChat.name })
          .first()
          .click();
      await page.locator("#composer").waitFor();
      if (width > 760) await page.locator(".terminal-dock").waitFor();
      const chatStorageKey =
        width <= 760
          ? "codex-mobile-opened"
          : `codex-desktop-opened:${initial.stateDir}`;
      const openedChatId = await page.evaluate((key) => {
        const value = localStorage.getItem(key);
        return value ? JSON.parse(value) : null;
      }, chatStorageKey);
      const openedTranscript = transcript;
      console.log(
        "Composer stability fixture:",
        JSON.stringify({
          width,
          openedChatId,
          requestedChatId: openedChat.id,
          transcriptItemCount: openedTranscript.items.length,
        }),
      );
      assert.equal(
        openedChatId,
        openedChat.id,
        `${width}px fixture opened the explicitly selected chat`,
      );
      await expect(page.locator("#conversation-title")).toContainText(
        openedChat.name,
      );
      await page
        .locator(
          width === 390 ? ".empty-chat-settings" : ".composer-context-row",
        )
        .waitFor();
      assert.equal(
        await page.locator("#stop").count(),
        0,
        "Stop is absent while idle",
      );
      await page.locator("#message").fill("Check this task");
      await page.waitForFunction(() => {
        const message = document.querySelector<HTMLTextAreaElement>("#message");
        const send = document.querySelector<HTMLButtonElement>("#send");
        return (
          message?.value === "Check this task" &&
          send &&
          !send.disabled &&
          !document.querySelector("#send-state")?.textContent
        );
      });
      const boxes = () =>
        page.evaluate((mobile) => {
          const result: Boxes = {};
          for (const selector of [
            "#composer",
            ...(mobile ? [] : ["#conversation-title"]),
            "#message",
            ".composer-bar",
            ".attach-button",
            ...(mobile ? [] : [".dictation-trigger"]),
            "#send",
            ...(mobile ? [] : [".usage-footer"]),
          ]) {
            const node = document.querySelector(selector);
            if (!node) throw new Error(`Missing ${selector}`);
            const { x, y, width, height } = node.getBoundingClientRect();
            result[selector] = { x, y, width, height };
          }
          return result;
        }, width <= 760);
      const selectors = [
        "#composer",
        "#messages",
        ...(width <= 760 ? [] : ["#conversation-title"]),
        "#message",
        ".composer-bar",
        ".attach-button",
        ...(width <= 760 ? [] : [".dictation-trigger"]),
        "#send",
        ...(width <= 760 ? [] : [".usage-footer"]),
      ];
      const waitForStableGeometry = async () => {
        await page.evaluate(() => {
          window.composerGeometryFrame = null;
        });
        await page.waitForFunction((selectors) => {
          const current = selectors.map((selector) => {
            const node = document.querySelector(selector);
            if (!node) throw new Error(`Missing ${selector}`);
            const rect = node.getBoundingClientRect();
            return [rect.x, rect.y, rect.width, rect.height].map(Math.round);
          });
          const serialized = JSON.stringify(current);
          const stable = window.composerGeometryFrame === serialized;
          window.composerGeometryFrame = serialized;
          return stable;
        }, selectors);
      };
      await waitForStableGeometry();
      let baseline = await boxes();
      const emptyPhoneStart =
        width === 390 &&
        (await page.locator(".empty-chat-settings").count()) > 0;
      const promptHistoryToggle = page.locator(
        "#conversation-header-tools .prompt-history-toggle",
      );
      const hadPromptHistoryToggle = (await promptHistoryToggle.count()) > 0;
      // The inline role summary stays in the composer before and after the
      // first message. Sending must preserve position and textarea focus.
      if (emptyPhoneStart)
        assert.equal(
          transcript.items.length,
          0,
          "390px empty-chat transition starts from the selected empty chat",
        );
      const stable = async (
        label: string,
        { allowFirstPromptTitleResize = false } = {},
      ) => {
        await waitForStableGeometry();
        const current = await boxes();
        measurements.push({ width, label, boxes: current });
        for (const [selector, box] of Object.entries(baseline))
          for (const key of ["x", "y", "width", "height"] as const)
            if (
              allowFirstPromptTitleResize &&
              selector === "#conversation-title" &&
              key === "width"
            )
              continue;
            else
              assert.ok(
                Math.abs(boxAt(current, selector)[key] - box[key]) <= 1,
                `${width}px ${label} ${selector}.${key}: ${box[key]} -> ${boxAt(current, selector)[key]}`,
              );
        assert.equal(
          await page.evaluate(
            () => document.documentElement.scrollWidth > innerWidth,
          ),
          false,
          `${width}px no horizontal page overflow`,
        );
        const form = boxAt(current, "#composer");
        for (const selector of [
          ".attach-button",
          ...(width <= 760 ? [] : [".dictation-trigger"]),
          "#send",
        ])
          assert.ok(
            boxAt(current, selector).x >= form.x &&
              boxAt(current, selector).x + boxAt(current, selector).width <=
                form.x + form.width + 1,
            `${selector} stays inside composer`,
          );
      };
      const updatePhase = async (next: TestAgent["status"]) => {
        const active =
          next === "starting" || next === "running" || next === "approval";
        Object.assign(controlledAgent, {
          status: next,
          inFlight: active,
          turnId: active ? "fixture-turn" : null,
        } satisfies Partial<TestAgent>);
        await updateEntityState(state, { origin, token: initial.token });
        await page.evaluate(() => window.dispatchEvent(new Event("online")));
        await page.waitForFunction(
          (idle) =>
            idle
              ? !document.querySelector("#stop")
              : !!document.querySelector("#stop:not(:disabled)"),
          next === "completed",
        );
      };
      const waitForSendingUI = () =>
        page.waitForFunction(() => {
          const composerState =
            document.querySelector("#send-state")?.textContent || "";
          const messageState = Array.from(
            document.querySelectorAll(".message-delivery-status"),
          ).some((node) => node.textContent.includes("Sending"));
          return composerState.includes("Sending") || messageState;
        });
      const pageScrollY = emptyPhoneStart
        ? await page.evaluate(() => window.scrollY)
        : null;
      if (emptyPhoneStart) {
        await page.locator("#message").focus();
        await page.evaluate(() => {
          window.composerTextareaBefore =
            document.querySelector<HTMLTextAreaElement>("#message");
          window.composerTransitionSamples = [];
          window.composerTransitionSampling = true;
          const sample = () => {
            if (!window.composerTransitionSampling) return;
            const composer = document.querySelector("#composer");
            if (!composer) throw new Error("Missing composer");
            const box = composer.getBoundingClientRect();
            window.composerTransitionSamples.push({
              y: box.y,
              height: box.height,
            });
            requestAnimationFrame(sample);
          };
          requestAnimationFrame(sample);
        });
        await page.waitForFunction(
          () => window.composerTransitionSamples.length > 0,
        );
      }
      if (emptyPhoneStart)
        await page
          .locator("#send")
          .evaluate((button: HTMLButtonElement) => button.click());
      else await page.locator("#send").click();
      const firstSend = await waitForSend();
      // The composer releases its lock when the durable outbox owns the request.
      // At that point the message row shows the in-flight state.
      await waitForSendingUI();
      if (emptyPhoneStart) {
        await waitForStableGeometry();
        const transition = await page.evaluate(() => {
          window.composerTransitionSampling = false;
          return window.composerTransitionSamples;
        });
        const current = await boxes();
        const positions = [
          ...new Set(transition.map((sample) => Math.round(sample.y))),
        ];
        assert.equal(
          positions.length,
          1,
          "390px first send keeps the inline composer at one position",
        );
        assert.equal(
          positions[0],
          Math.round(boxAt(baseline, "#composer").y),
          "390px transition starts at the empty-chat composer position",
        );
        assert.equal(
          positions[0],
          Math.round(boxAt(current, "#composer").y),
          "390px transition ends at the non-empty composer position",
        );
        assert.equal(
          await page.evaluate(() => window.scrollY),
          pageScrollY,
          "390px first-send transition does not scroll the page",
        );
        assert.equal(
          await page.evaluate(
            () =>
              document.querySelector<HTMLTextAreaElement>("#message") ===
                window.composerTextareaBefore &&
              document.querySelector<HTMLTextAreaElement>("#message")
                ?.isConnected &&
              document.activeElement ===
                document.querySelector<HTMLTextAreaElement>("#message"),
          ),
          true,
          "390px first-send transition keeps the focused textarea mounted",
        );
        baseline = current;
      } else {
        const fullHeaderTools = await page
          .locator(
            "#conversation-header-tools .conversation-header-tools-menu[data-compact='no']",
          )
          .count();
        const firstPromptNavigationAppeared =
          !hadPromptHistoryToggle && (await promptHistoryToggle.count()) > 0;
        const firstFullHeaderPromptNavigation =
          fullHeaderTools > 0 && firstPromptNavigationAppeared;
        if (firstFullHeaderPromptNavigation)
          await expect(promptHistoryToggle).toBeVisible();
        await stable("sending", {
          allowFirstPromptTitleResize: firstFullHeaderPromptNavigation,
        });
        if (firstFullHeaderPromptNavigation) {
          // e4ef04aa places prompt navigation in the header tools. That
          // navigation mounts after the first prompt, and the flexible title
          // yields width to its prompt history control once.
          const latest = measurements.at(-1);
          assert.ok(latest);
          baseline = latest.boxes;
        }
      }
      const firstMessage = fixturePostBody(
        "/api/messages",
        firstSend.request().postDataJSON(),
      );
      assert.ok(firstMessage.id);
      await firstSend.fulfill({
        json: {
          id: firstMessage.id,
          status: "sent",
        } satisfies PostResult<"/api/messages">,
      });
      await page.waitForFunction(
        () => !document.querySelector<HTMLTextAreaElement>("#message")?.value,
      );
      await page.waitForFunction(
        () => !document.querySelector("#send-state")?.textContent,
      );
      await stable("sent and draft cleared");
      for (const next of ["starting", "running", "approval"] as const) {
        await updatePhase(next);
        await stable(next);
      }
      await page.locator("#message").fill("Additional instruction");
      sendCaptured = Promise.withResolvers<Route>();
      await updatePhase("completed");
      await page.locator("#send").click();
      const secondSend = await waitForSend();
      await stable("sending before failed delivery");
      if (emptyPhoneStart)
        for (const key of ["y", "height"] as const)
          assert.equal(
            boxAt(measurements.at(-1)?.boxes ?? {}, "#composer")[key],
            boxAt(baseline, "#composer")[key],
            `390px subsequent non-empty send preserves composer ${key}`,
          );
      assert.equal(
        fixturePostBody("/api/messages", secondSend.request().postDataJSON())
          .delivery,
        "current_turn",
        "An ordinary Send action keeps the current after-tool delivery mode",
      );
      await secondSend.fulfill({
        status: 400,
        json: { error: "Fixture delivery failed" },
      });
      await page
        .getByText("Fixture delivery failed", { exact: true })
        .waitFor();
      await stable("failed delivery is recoverable");
      await page
        .getByRole("button", { name: "Restore draft", exact: true })
        .click();
      await stable("failed message restored to draft");
      assert.equal(
        await page.locator("#message").inputValue(),
        "Additional instruction",
      );
      await updatePhase("running");
      queue = [
        {
          id: "queued-fixture",
          text: "Later instruction",
          delivery: "next_turn",
          status: "queued",
        },
      ];
      queueRequested = Promise.withResolvers<void>();
      queueResponse = Promise.withResolvers<void>();
      queueResponseGate = queueResponse.promise;
      await page.reload();
      if (width <= 760)
        await page.locator(`[data-chat="${openedChat.id}"]`).first().click();
      await page.locator("#composer").waitFor();
      await expect(page.locator("#conversation-title")).toContainText(
        openedChat.name,
      );
      await queueRequested.promise;
      if (width > 760) await page.locator(".terminal-dock").waitFor();
      await page.locator("#stop:not(:disabled)").waitFor();
      await waitForStableGeometry();
      // Reload restores the running turn before queue rendering; establish the
      // post-reload geometry, then verify the queue response itself is stable.
      baseline = await boxes();
      queueResponse.resolve();
      queueResponseGate = null;
      await page
        .getByRole("button", { name: "Delete queued message 1", exact: true })
        .waitFor();
      assert.equal(await page.locator(".message-queue").count(), 1);
      await page
        .locator(".message-queue")
        .getByText("Later instruction", { exact: true })
        .waitFor();
      await stable("queue appears");
      await page.locator("#stop").click();
      const pendingStop = await stopCaptured.promise;
      assert.ok(pendingStop, "stop reaches API");
      await stable("stopping request");
      await pendingStop.fulfill({ json: {} });
      await updatePhase("completed");
      await stable("turn completed");
      await page.screenshot({
        path: join(evidence, `composer-${width}.png`),
        fullPage: true,
      });
      expect(errors).toEqual([]);
      await page.close();
      pages.delete(page);
    }
    await writeFile(
      join(evidence, "geometry.json"),
      JSON.stringify(measurements, null, 2),
    );
    console.log(
      `PASS: stable composer geometry across idle, sending, failed delivery, starting, running, approval, queue, stop and completion at 1440, 900 and 390px. ${evidence}`,
    );
  } finally {
    for (const page of pages) if (!page.isClosed()) await page.close();
  }
});
