import {
  readTestState,
  syncIdentityFixture,
  entityPullFixtureForRequest,
  installApiSchemaFixtureHeaders,
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

async function setupNarrowComposer(browser) {
  const repo = dirname(
    dirname(dirname(dirname(fileURLToPath(import.meta.url)))),
  );
  const evidence = await mkdtemp(join(tmpdir(), "studio-composer-narrow-"));
  const fixture = spawn(
    "python3",
    ["-B", join(repo, "tests/simple-ui-fixture.py"), evidence],
    { stdio: ["pipe", "pipe", "pipe"] },
  );
  let log = "";
  fixture.stderr.on("data", (data) => (log += data));
  const port = await new Promise((resolve, reject) => {
    fixture.stdout.once("data", (data) => resolve(Number(String(data).trim())));
    fixture.once("exit", () => reject(Error(log)));
  });
  const origin = `http://127.0.0.1:${port}`;
  const initial = await readTestState(origin);
  const lead = initial.threads.find((agent) => agent.name === "Other project");
  assert.ok(lead, "fixture lead is available");
  const transcript = await (
    await fetch(`${origin}/api/transcript?id=${lead.id}`)
  ).json();
  const page = await browser.newPage({
    viewport: { width: 1440, height: 960 },
  });
  await page.route("**/api/transcript/stream?*", (route) =>
    route.fulfill({
      contentType: "text/event-stream",
      body: `data: ${JSON.stringify({ ...transcript, items: transcript.items || [], order: (transcript.items || []).map((item) => item.id), replace: true })}\n\n`,
    }),
  );
  await page.route("**/api/transcript?*", (route) =>
    route.fulfill({ json: transcript }),
  );
  await page.goto(origin);
  await page.locator(`[data-chat="${lead.id}"]`).waitFor();
  await page.locator(`[data-chat="${lead.id}"]`).click();
  await page.locator("#sidebar-toggle").click();
  await page.locator("#sidebar").waitFor({ state: "hidden" });
  await page.setViewportSize({ width: 390, height: 960 });
  await page.locator("#composer").waitFor();
  await page.locator("#send").waitFor();
  return { page, lead, fixture };
}

async function setComposerFixturePhase(page, fixture, lead, next) {
  fixture.stdin.write(
    JSON.stringify({
      method: "fixture/agent-status",
      params: { agent: lead.id, status: next },
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
}

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
    const backendIdentity = await (
      await fetch(`${origin}/api/sync/identity`)
    ).json();
    const originalTranscript = await (
      await fetch(`${origin}/api/transcript?id=${lead.id}`)
    ).json();
    browser = fixtureBrowser;
    const measurements = [];
    for (const width of [1440, 900, 390]) {
      const transcript = structuredClone(originalTranscript);
      const page = await browser.newPage({
        viewport: { width: 1440, height: 960 },
      });
      pages.add(page);
      page.setDefaultTimeout(12000);
      let queue = [],
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
      await page.route("**/api/queue?*", (route) =>
        route.fulfill({ json: { items: queue } }),
      );
      await page.route("**/api/messages", (route) => {
        pendingSend = route;
        resolveSendCapture?.();
        resolveSendCapture = null;
      });
      await page.route("**/api/stop", (route) => {
        pendingStop = route;
      });
      await page.goto(origin);
      if (width === 390) {
        await page.locator(`[data-chat="${lead.id}"]`).waitFor();
        await page.locator(`[data-chat="${lead.id}"]`).click();
        await page.locator("#sidebar-toggle").click();
        await page.locator("#sidebar").waitFor({ state: "hidden" });
        await page.setViewportSize({ width, height: 960 });
      } else {
        await page.setViewportSize({ width, height: 960 });
        await page.locator(`[data-chat="${lead.id}"]`).click();
      }
      await page.locator("#composer").waitFor();
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
      const boxes = (target = page) =>
        target.evaluate((mobile) => {
          const result = {};
          for (const selector of [
            "#composer",
            ".composer-context-row",
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
      let baseline = await boxes();
      const waitForStableGeometry = async (target = page) => {
        const selectors = Object.keys(baseline);
        await target.evaluate(() => {
          window.composerGeometryFrame = null;
        });
        await target.waitForFunction((selectors) => {
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
      const stable = async (label) => {
        await waitForStableGeometry();
        const current = await boxes();
        measurements.push({ width, label, boxes: current });
        const mobileQueueReflow = width === 390 && label === "queue appears";
        for (const [selector, box] of Object.entries(baseline))
          for (const key of width === 390
            ? ["y", "width", "height"]
            : ["x", "y", "width", "height"]) {
            const previousComposer = baseline["#composer"];
            const currentComposer = current["#composer"];
            const relativeY = mobileQueueReflow && key === "y";
            const previousValue = relativeY
              ? selector === "#composer"
                ? box.y + box.height
                : box.y - previousComposer.y
              : box[key];
            const currentValue = relativeY
              ? selector === "#composer"
                ? currentComposer.y + currentComposer.height
                : current[selector].y - currentComposer.y
              : current[selector][key];
            assert.ok(
              Math.abs(currentValue - previousValue) <= 1,
              `${width}px ${label} ${selector}.${key}${mobileQueueReflow ? " relative to composer/bottom" : ""}: ${previousValue} -> ${currentValue}`,
            );
          }
        assert.equal(
          await page.evaluate(
            () => document.documentElement.scrollWidth > innerWidth,
          ),
          false,
          `${width}px no horizontal page overflow`,
        );
        if (width !== 390) {
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
              `${width}px ${label} ${selector} stays inside composer: ${JSON.stringify(
                {
                  item: current[selector],
                  form,
                },
              )}`,
            );
        }
      };
      const updatePhase = async (next) => {
        await setComposerFixturePhase(page, fixture, lead, next);
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
      await page.evaluate(() => {
        const composer = document.querySelector("#composer");
        window.firstSendComposerHeights = [
          composer.getBoundingClientRect().height,
        ];
        window.firstSendComposerTransitionDone = false;
        let stableFrames = 0;
        let previous = window.firstSendComposerHeights[0];
        const started = performance.now();
        const sample = () => {
          const height = composer.getBoundingClientRect().height;
          window.firstSendComposerHeights.push(height);
          stableFrames =
            Math.abs(height - previous) < 0.1 ? stableFrames + 1 : 0;
          previous = height;
          if (
            (!document.querySelector(".composer-context-values") &&
              stableFrames >= 2) ||
            performance.now() - started >= 10_000
          ) {
            window.firstSendComposerTransitionDone = true;
            return;
          }
          requestAnimationFrame(sample);
        };
        requestAnimationFrame(sample);
      });
      await page.locator("#send").click();
      await waitForSend();
      // The composer releases its lock when the durable outbox owns the request.
      // At that point the message row shows the in-flight state.
      await waitForSendingUI();
      await page.waitForFunction(
        () => window.firstSendComposerTransitionDone,
        undefined,
        { timeout: 12_000 },
      );
      const firstSend = await boxes();
      assert.equal(
        await page.locator(".composer-context-values").count(),
        0,
        "the empty-chat account and project context is removed after first send (81bc27a5)",
      );
      const composerHeightChange =
        baseline["#composer"].height - firstSend["#composer"].height;
      const contextRowHeightChange =
        baseline[".composer-context-row"].height -
        firstSend[".composer-context-row"].height;
      const transition = await page.evaluate(
        () => window.firstSendComposerHeights,
      );
      const roundedTransition = transition.map(Math.round);
      assert.equal(
        roundedTransition[0],
        Math.round(baseline["#composer"].height),
        `${width}px first-send sample starts at the empty-chat baseline`,
      );
      assert.ok(
        roundedTransition.every(
          (height, index) =>
            index === 0 || height <= roundedTransition[index - 1],
        ),
        `${width}px first-send composer never grows during the context change: ${JSON.stringify(roundedTransition)}`,
      );
      assert.equal(
        roundedTransition.filter(
          (height, index) =>
            index > 0 && height !== roundedTransition[index - 1],
        ).length,
        1,
        `${width}px first-send composer shrinks in one transition: ${JSON.stringify(roundedTransition)}`,
      );
      assert.equal(
        roundedTransition.at(-1),
        Math.round(firstSend["#composer"].height),
      );
      assert.ok(
        composerHeightChange > 0,
        `${width}px first send removes the empty-chat context values and shrinks composer once (row height delta ${contextRowHeightChange}px)`,
      );
      baseline = await boxes();
      await stable("sending");
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
      const nonEmptyTranscript = structuredClone(transcript);
      nonEmptyTranscript.items = [
        {
          id: "first-send-steady-state",
          role: "user",
          text: "Check this task",
          at: 1,
          turnId: "first-send-steady-state",
          turnStatus: "completed",
        },
      ];
      nonEmptyTranscript.order = nonEmptyTranscript.items.map(
        (item) => item.id,
      );
      const comparisonPage = await browser.newPage({
        viewport: { width: 1440, height: 960 },
      });
      pages.add(comparisonPage);
      const identityResponse = syncIdentityFixture(backendIdentity.workspaceId);
      installApiSchemaFixtureHeaders(comparisonPage);
      await comparisonPage.route("**/api/sync/identity", (route) =>
        route.fulfill({ json: identityResponse }),
      );
      await comparisonPage.route("**/api/sync/pull?*", (route) =>
        route.fulfill({
          json: {
            workspaceId: identityResponse.workspaceId,
            ...entityPullFixtureForRequest(initial, route.request().url(), {
              transcriptSeq: 1,
              transcript: nonEmptyTranscript,
            }),
          },
        }),
      );
      await comparisonPage.route("**/api/transcript/stream?*", (route) =>
        route.fulfill({
          contentType: "text/event-stream",
          body: `data: ${JSON.stringify({ ...nonEmptyTranscript, replace: true })}\n\n`,
        }),
      );
      await comparisonPage.route("**/api/transcript?*", (route) =>
        route.fulfill({ json: nonEmptyTranscript }),
      );
      await comparisonPage.goto(origin);
      await comparisonPage.locator(`[data-chat="${lead.id}"]`).waitFor();
      await comparisonPage.locator(`[data-chat="${lead.id}"]`).click();
      if (width === 390) {
        await comparisonPage.locator("#sidebar-toggle").click();
        await comparisonPage.locator("#sidebar").waitFor({ state: "hidden" });
      }
      await comparisonPage.setViewportSize({ width, height: 960 });
      await comparisonPage
        .locator('[data-message="first-send-steady-state"]')
        .waitFor();
      await waitForStableGeometry(comparisonPage);
      const nonEmptyChat = await boxes(comparisonPage);
      for (const [selector, box] of Object.entries(firstSend))
        for (const key of width === 390
          ? ["y", "width", "height"]
          : ["x", "y", "width", "height"])
          assert.ok(
            Math.abs(nonEmptyChat[selector][key] - box[key]) <= 1,
            `${width}px post-send ${selector}.${key} matches a steady non-empty chat: ${box[key]} -> ${nonEmptyChat[selector][key]}`,
          );
      await comparisonPage.close();
      pages.delete(comparisonPage);
      baseline = nonEmptyChat;
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
      const queueLayout = async () =>
        page.evaluate(() => {
          const composer = document
            .querySelector("#composer")
            .getBoundingClientRect();
          const queueNode = document.querySelector(".message-queue");
          const queue = queueNode?.getBoundingClientRect();
          const root = document.querySelector("#root");
          return {
            composerBottom: composer.bottom,
            composerTop: composer.top,
            rootScrollTop: root.scrollTop,
            queue: queue
              ? {
                  x: queue.x,
                  y: queue.y,
                  width: queue.width,
                  height: queue.height,
                  bottom: queue.bottom,
                }
              : null,
          };
        });
      const queueLayoutBefore = width === 390 ? await queueLayout() : null;
      queue = [
        {
          id: "queued-fixture",
          text: "Later instruction",
          delivery: "after_turn",
          status: "queued",
        },
      ];
      await page.reload();
      if (width <= 760)
        await page
          .getByRole("button", { name: "Toggle conversations" })
          .click();
      await page.locator(`[data-chat="${lead.id}"]`).click();
      await page
        .getByRole("button", { name: "Delete queued message 1", exact: true })
        .waitFor();
      assert.equal(await page.locator(".message-queue").count(), 1);
      await page
        .locator(".message-queue")
        .getByText("Later instruction", { exact: true })
        .waitFor();
      if (width === 390) {
        const queueLayoutAfter = await queueLayout();
        console.log(
          `390px queue reflow measurements ${JSON.stringify({ before: queueLayoutBefore, after: queueLayoutAfter })}`,
        );
      }
      await stable("queue appears");
      await page.locator("#stop").click();
      await page.waitForTimeout(80);
      assert.ok(pendingStop, "stop reaches API");
      await stable("stopping request");
      await pendingStop.fulfill({ json: {} });
      await updatePhase("completed");
      await stable("turn completed");
      expect(errors).toEqual([]);
      await page.close();
      pages.delete(page);
    }
    await writeFile(
      join(evidence, "geometry.json"),
      JSON.stringify(measurements, null, 2),
    );
    console.log(
      `PASS: stable composer geometry across idle, sending, failed delivery, starting, running, approval, queue, stop and completion at 1440, 900, and 390px. ${evidence}`,
    );
  } finally {
    for (const page of pages) if (!page.isClosed()) await page.close();
  }
});

test("390px composer setup reaches the selected empty chat and starting phase", async ({
  browser,
}) => {
  const { page, lead, fixture } = await setupNarrowComposer(browser);
  try {
    assert.equal(
      await page.locator("#conversation-title").innerText(),
      lead.name,
    );
    assert.equal(await page.locator(".composer-context-values").count(), 1);
    await setComposerFixturePhase(page, fixture, lead, "starting");
    await page.locator("#stop:not(:disabled)").waitFor();
  } finally {
    await page.close();
  }
});

test("390px mobile composer horizontal invariants", async ({ browser }) => {
  const { page, lead, fixture } = await setupNarrowComposer(browser);
  try {
    const phases = {};
    const capture = async (name) => {
      phases[name] = await page.evaluate(() => {
        const root = document.querySelector("#root");
        const composer = document
          .querySelector("#composer")
          .getBoundingClientRect();
        const send = document.querySelector("#send").getBoundingClientRect();
        return {
          composerX: composer.x,
          composerRight: composer.right,
          sendX: send.x,
          sendRight: send.right,
          rootScrollLeft: root.scrollLeft,
          rootScrollWidth: root.scrollWidth,
          rootClientWidth: root.clientWidth,
        };
      });
    };
    await capture("idle");
    await setComposerFixturePhase(page, fixture, lead, "starting");
    await page.locator("#stop:not(:disabled)").waitFor();
    await capture("starting");
    await setComposerFixturePhase(page, fixture, lead, "running");
    await capture("running");
    let stopRequest;
    const stopCaptured = new Promise((resolve) => {
      stopRequest = resolve;
    });
    await page.route("**/api/stop", async (_route) => {
      stopRequest();
      await new Promise(() => {});
    });
    await page.locator("#stop").click();
    await stopCaptured;
    await page.locator('#stop[aria-busy="true"]').waitFor();
    await capture("stopping");
    const result = {
      sendInsideComposer: Object.values(phases).every(
        (phase) =>
          phase.sendX >= phase.composerX - 1 &&
          phase.sendRight <= phase.composerRight + 1,
      ),
      sendXUnchanged: Math.abs(phases.idle.sendX - phases.starting.sendX) <= 1,
      composerXUnchanged: ["starting", "running", "stopping"].every(
        (phase) =>
          Math.abs(phases.idle.composerX - phases[phase].composerX) <= 1,
      ),
      rootDoesNotScroll: Object.values(phases).every(
        (phase) =>
          phase.rootScrollLeft === 0 &&
          phase.rootScrollWidth <= phase.rootClientWidth,
      ),
      phases,
    };
    test.fail(
      true,
      "See chat-known-failures.md: 390px mobile composer action overflow shifts the composer and scrolls #root.",
    );
    assert.ok(
      result.sendInsideComposer &&
        result.sendXUnchanged &&
        result.composerXUnchanged &&
        result.rootDoesNotScroll,
      `390px mobile composer horizontal invariants hold through idle/starting/running/stopping: ${JSON.stringify(result)}`,
    );
  } finally {
    await page.close();
  }
});
