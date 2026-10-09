import {
  readTestState,
  test,
  expect,
  spawnFixture as spawn,
} from "../playwright.mjs";
// Real production React, isolated HTTP fixture, controlled status and delivery timing.
import assert from "node:assert/strict";
import { mkdtemp, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

test("composer-stability-ui", async ({ browser: fixtureBrowser }) => {
  test.setTimeout(120_000);
  const repo = dirname(
    dirname(dirname(dirname(fileURLToPath(import.meta.url)))),
  );
  const evidence = await mkdtemp(join(tmpdir(), "studio-composer-stability-"));
  const fixture = spawn(
    "python3",
    ["-B", join(repo, "tests/simple-ui-fixture.py"), evidence],
    { stdio: ["pipe", "pipe", "pipe"] },
  );
  let browser,
    pages = new Set(),
    log = "";
  fixture.stderr.on("data", (data) => (log += data));
  try {
    const port = await new Promise((resolve, reject) => {
      fixture.stdout.once("data", (data) =>
        resolve(Number(String(data).trim())),
      );
      fixture.once("exit", () => reject(Error(log)));
    });
    const origin = `http://127.0.0.1:${port}`;
    const initial = await readTestState(origin);
    const lead = initial.threads.find(
      (agent) => agent.name === "Other project",
    );
    const emptyWorker = initial.threads.find(
      (agent) => agent.name === "Standalone reviewer",
    );
    assert.ok(lead, "fixture provides an empty managed lead chat");
    assert.ok(emptyWorker, "fixture provides an empty managed reviewer chat");
    browser = fixtureBrowser;
    const measurements = [];
    for (const width of [1440, 900, 390]) {
      const openedChat = width === 390 ? lead : emptyWorker;
      const originalTranscript = await (
        await fetch(`${origin}/api/transcript?id=${openedChat.id}`)
      ).json();
      const transcript = structuredClone(originalTranscript);
      const page = await browser.newPage({
        viewport: { width: 1440, height: 960 },
      });
      pages.add(page);
      page.setDefaultTimeout(12000);
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
      let queue = [],
        queueResponseGate = null,
        resolveQueueResponse,
        resolveQueueRequested,
        queueRequested = new Promise((resolve) => {
          resolveQueueRequested = resolve;
        }),
        pendingSend,
        pendingStop,
        resolveSendCapture,
        sendCaptured = new Promise((resolve) => {
          resolveSendCapture = resolve;
        });
      const waitForSend = () =>
        Promise.race([
          sendCaptured,
          page.waitForTimeout(12000).then(() => {
            throw Error("send API request was not captured");
          }),
        ]);
      const errors = [];
      page.on("pageerror", (error) => errors.push(error.message));
      await page.route("**/api/transcript/stream?*", (route) =>
        route.fulfill({
          contentType: "text/event-stream",
          body: `data: ${JSON.stringify({ ...transcript, items: transcript.items || [], order: (transcript.items || []).map((item) => item.id), replace: true })}\n\n`,
        }),
      );
      await page.route("**/api/transcript?*", (route) =>
        route.fulfill({ json: transcript }),
      );
      await page.route("**/api/queue?*", async (route) => {
        resolveQueueRequested?.();
        resolveQueueRequested = null;
        if (queueResponseGate) await queueResponseGate;
        await route.fulfill({ json: { items: queue } });
      });
      await page.route("**/api/messages", (route) => {
        pendingSend = route;
        resolveSendCapture?.();
        resolveSendCapture = null;
      });
      await page.route("**/api/stop", (route) => {
        pendingStop = route;
      });
      await page.setViewportSize({ width, height: 960 });
      await page.goto(origin);
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
      const openedTranscript = openedChatId
        ? await (
            await fetch(`${origin}/api/transcript?id=${openedChatId}`)
          ).json()
        : { items: [] };
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
        const message = document.querySelector("#message");
        const send = document.querySelector("#send");
        return (
          message?.value === "Check this task" &&
          send &&
          !send.disabled &&
          !document.querySelector("#send-state").textContent
        );
      });
      const boxes = () =>
        page.evaluate((mobile) => {
          const result = {};
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
            const box = node.getBoundingClientRect();
            result[selector] = Object.fromEntries(
              ["x", "y", "width", "height"].map((key) => [key, box[key]]),
            );
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
            const rect = document
              .querySelector(selector)
              .getBoundingClientRect();
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
      // 81bc27a5 introduced the phone layout swap: the empty-chat settings
      // panel below the composer gives way to the context row inside it after
      // the first message, so only this one position transition is expected.
      if (emptyPhoneStart)
        assert.equal(
          transcript.items.length,
          0,
          "390px empty-chat transition starts from the selected empty chat",
        );
      const stable = async (
        label,
        { allowFirstPromptTitleResize = false } = {},
      ) => {
        await waitForStableGeometry();
        const current = await boxes();
        measurements.push({ width, label, boxes: current });
        for (const [selector, box] of Object.entries(baseline))
          for (const key of ["x", "y", "width", "height"])
            if (
              allowFirstPromptTitleResize &&
              selector === "#conversation-title" &&
              key === "width"
            )
              continue;
            else
              assert.ok(
                Math.abs(current[selector][key] - box[key]) <= 1,
                `${width}px ${label} ${selector}.${key}: ${box[key]} -> ${current[selector][key]}`,
              );
        assert.equal(
          await page.evaluate(
            () => document.documentElement.scrollWidth > innerWidth,
          ),
          false,
          `${width}px no horizontal page overflow`,
        );
        const form = current["#composer"];
        for (const selector of [
          ".attach-button",
          ...(width <= 760 ? [] : [".dictation-trigger"]),
          "#send",
        ])
          assert.ok(
            current[selector].x >= form.x &&
              current[selector].x + current[selector].width <=
                form.x + form.width + 1,
            `${selector} stays inside composer`,
          );
      };
      const updatePhase = async (next) => {
        fixture.stdin.write(
          JSON.stringify({
            method: "fixture/agent-status",
            params: { agent: openedChat.id, status: next },
          }) + "\n",
        );
        await page.evaluate(() => window.dispatchEvent(new Event("online")));
        await page.waitForFunction(
          (idle) =>
            idle
              ? !document.querySelector("#stop")
              : !!document.querySelector("#stop:not(:disabled)"),
          next === "completed",
        );
        await page.waitForTimeout(80);
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
          window.composerTextareaBefore = document.querySelector("#message");
          window.composerTransitionSamples = [];
          window.composerTransitionSampling = true;
          const sample = () => {
            if (!window.composerTransitionSampling) return;
            const box = document
              .querySelector("#composer")
              .getBoundingClientRect();
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
        await page.locator("#send").evaluate((button) => button.click());
      else await page.locator("#send").click();
      await waitForSend();
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
          2,
          "390px first-send transition has exactly one layout change and no intermediate composer position",
        );
        assert.equal(
          positions[0],
          Math.round(baseline["#composer"].y),
          "390px transition starts at the empty-chat composer position",
        );
        assert.equal(
          positions[1],
          Math.round(current["#composer"].y),
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
              document.querySelector("#message") ===
                window.composerTextareaBefore &&
              document.querySelector("#message")?.isConnected &&
              document.activeElement === document.querySelector("#message"),
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
          baseline = measurements.at(-1).boxes;
        }
      }
      await pendingSend.fulfill({
        json: { id: pendingSend.request().postDataJSON().id, status: "sent" },
      });
      await page.waitForFunction(
        () => !document.querySelector("#message").value,
      );
      await page.waitForFunction(
        () => !document.querySelector("#send-state").textContent,
      );
      await stable("sent and draft cleared");
      for (const next of ["starting", "running", "approval"]) {
        await updatePhase(next);
        await stable(next);
      }
      await page.locator("#message").fill("Additional instruction");
      pendingSend = null;
      sendCaptured = new Promise((resolve) => {
        resolveSendCapture = resolve;
      });
      await updatePhase("completed");
      await page.locator("#send").click();
      await waitForSend();
      await stable("sending before failed delivery");
      if (emptyPhoneStart)
        for (const key of ["y", "height"])
          assert.equal(
            measurements.at(-1).boxes["#composer"][key],
            baseline["#composer"][key],
            `390px subsequent non-empty send preserves composer ${key}`,
          );
      assert.equal(
        pendingSend.request().postDataJSON().delivery,
        "after_tool",
        "An ordinary Send action keeps the current after-tool delivery mode",
      );
      await pendingSend.fulfill({
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
          delivery: "after_turn",
          status: "queued",
        },
      ];
      queueRequested = new Promise((resolve) => {
        resolveQueueRequested = resolve;
      });
      queueResponseGate = new Promise((resolve) => {
        resolveQueueResponse = resolve;
      });
      await page.reload();
      await page.locator("#composer").waitFor();
      await expect(page.locator("#conversation-title")).toContainText(
        openedChat.name,
      );
      await queueRequested;
      if (width > 760) await page.locator(".terminal-dock").waitFor();
      await page.locator("#stop:not(:disabled)").waitFor();
      await waitForStableGeometry();
      // Reload restores the running turn before queue rendering; establish the
      // post-reload geometry, then verify the queue response itself is stable.
      baseline = await boxes();
      resolveQueueResponse();
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
      await page.waitForTimeout(80);
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
