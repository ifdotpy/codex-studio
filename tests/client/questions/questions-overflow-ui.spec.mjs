#!/usr/bin/env node
// Overflow fixture for the production request answer form. No model requests or user state.

import assert from "node:assert/strict";
import { mkdtemp } from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { test, expect, spawnFixture as spawn } from "../playwright.mjs";

const REQUESTS_VISIBLE_HEIGHT_RATIO = 0.5;

test("Questions overflow", async ({ browser }) => {
  const project = dirname(
    dirname(dirname(dirname(fileURLToPath(import.meta.url)))),
  );
  const state = await mkdtemp(join(tmpdir(), "codex-questions-overflow-"));
  const fixture = spawn(
    "python3",
    [
      "-B",
      join(project, "tests/simple-ui-questions-overflow-fixture.py"),
      state,
    ],
    {
      stdio: ["pipe", "pipe", "pipe"],
      env: { ...process.env, CODEX_BOARD_STATE_DIR: join(state, "board") },
    },
  );
  let context,
    log = "";
  fixture.stderr.on("data", (chunk) => {
    log += chunk;
  });
  try {
    const port = await new Promise((resolve, reject) => {
      fixture.stdout.once("data", (chunk) =>
        resolve(Number(String(chunk).trim())),
      );
      fixture.once("exit", () =>
        reject(new Error(`Question fixture exited\n${log}`)),
      );
    });
    context = await browser.newContext({
      viewport: { width: 1280, height: 800 },
    });
    const view = await context.newPage();
    await view.addInitScript(() => {
      const viewport = new EventTarget();
      Object.assign(viewport, {
        width: 1280,
        height: 800,
        offsetTop: 0,
        offsetLeft: 0,
        scale: 1,
      });
      Object.defineProperty(window, "visualViewport", {
        configurable: true,
        value: viewport,
      });
      window.setTestViewport = (patch) => {
        Object.assign(viewport, patch);
        viewport.dispatchEvent(new Event("resize"));
      };
    });
    await view.goto(`http://127.0.0.1:${port}`);
    await view
      .locator("[data-chat]")
      .filter({ hasText: "Release lead" })
      .click();
    const compactCard = view.locator('[data-request="compact-question"]');
    await compactCard.getByRole("button", { name: "Answer" }).click();
    const compactHeight = await compactCard
      .locator(".request-answer-form")
      .evaluate((form) => form.getBoundingClientRect().height);
    assert.ok(
      compactHeight < 220,
      `A short answer form stays content-sized: ${compactHeight}px`,
    );
    await compactCard.getByRole("button", { name: "Close reply" }).click();

    const card = view.locator('[data-request="overflow-question"]');
    await card.getByRole("button", { name: "Answer" }).click();
    const messageScroller = view.locator("#messages");
    const requestsScroller = view.locator("#requests");
    await expect
      .poll(() =>
        card.evaluate((node) => {
          const requests = document
            .querySelector("#requests")
            .getBoundingClientRect();
          const bounds = node.getBoundingClientRect();
          return bounds.top >= requests.top && bounds.top < requests.bottom;
        }),
      )
      .toBe(true);

    let previousTranscript;
    let stableBottomSamples = 0;
    await expect
      .poll(
        async () => {
          const current = await messageScroller.evaluate((node) => ({
            messages: node.querySelectorAll("[data-message]").length,
            scrollHeight: node.scrollHeight,
            clientHeight: node.clientHeight,
            scrollTop: node.scrollTop,
          }));
          const atBottom =
            current.messages > 0 &&
            current.scrollHeight > current.clientHeight + 1 &&
            Math.abs(
              current.scrollHeight - current.clientHeight - current.scrollTop,
            ) < 1;
          if (
            atBottom &&
            previousTranscript &&
            current.messages === previousTranscript.messages &&
            current.scrollHeight === previousTranscript.scrollHeight &&
            current.clientHeight === previousTranscript.clientHeight &&
            current.scrollTop === previousTranscript.scrollTop
          ) {
            stableBottomSamples += 1;
          } else {
            stableBottomSamples = 0;
          }
          previousTranscript = current;
          return stableBottomSamples;
        },
        {
          message: "The loaded transcript settles at the following position",
          interval: 100,
        },
      )
      .toBeGreaterThanOrEqual(3);
    const initialScroll = await view.evaluate(() => ({
      requests: document.querySelector("#requests").scrollTop,
      messages: document.querySelector("#messages").scrollTop,
    }));
    await view.locator(".message-content").evaluate((node) => {
      const arrivingMessage = document.createElement("div");
      arrivingMessage.dataset.message = "fixture-arriving-message";
      arrivingMessage.style.height = "80px";
      node.append(arrivingMessage);
    });
    await expect
      .poll(() =>
        messageScroller.evaluate(
          (node) => node.scrollTop < node.scrollHeight - node.clientHeight - 32,
        ),
      )
      .toBe(true);

    await requestsScroller.evaluate((node) => {
      node.scrollTop = node.scrollHeight;
    });
    const requestsAtEnd = await view.evaluate(() => ({
      requests: document.querySelector("#requests").scrollTop,
      messages: document.querySelector("#messages").scrollTop,
      bounds: document.querySelector("#requests").getBoundingClientRect(),
    }));
    assert.ok(requestsAtEnd.requests > initialScroll.requests);
    assert.equal(requestsAtEnd.messages, initialScroll.messages);
    await view.mouse.move(
      requestsAtEnd.bounds.left + 12,
      requestsAtEnd.bounds.bottom - 12,
    );
    await view.mouse.wheel(0, 500);
    await expect
      .poll(() => messageScroller.evaluate((node) => node.scrollTop))
      .toBe(initialScroll.messages);
    assert.deepEqual(
      await view.evaluate(
        () =>
          getComputedStyle(document.querySelector("#requests"))
            .overscrollBehaviorY,
      ),
      "contain",
    );

    await requestsScroller.evaluate((node, top) => {
      node.scrollTop = top;
    }, initialScroll.requests);
    await view.setViewportSize({ width: 390, height: 844 });
    await view.evaluate(() => window.setTestViewport({ height: 390 }));
    await expect
      .poll(() =>
        card.evaluate((node) => {
          const messages = document
            .querySelector("#messages")
            .getBoundingClientRect();
          const bounds = node.getBoundingClientRect();
          return bounds.top >= messages.top - 1 && bounds.top < messages.bottom;
        }),
      )
      .toBe(true);

    for (const viewport of [
      { width: 1280, height: 800, visibleHeight: 800 },
      { width: 390, height: 844, visibleHeight: 844 },
      { width: 390, height: 844, visibleHeight: 390 },
    ]) {
      await view.setViewportSize({
        width: viewport.width,
        height: viewport.height,
      });
      if (viewport.visibleHeight !== viewport.height)
        await view.evaluate(() => window.setTestViewport({ height: 390 }));
      else if (viewport.width === 390)
        await view.evaluate(() => window.setTestViewport({ height: 844 }));
      await view.locator(".request-answer-actions").waitFor();
      for (const edge of ["first", "last"]) {
        const optionInput = card
          .locator(".request-answer-fields input[type=checkbox]")
          [edge === "first" ? "first" : "last"]();
        await optionInput.evaluate((option) => {
          option.blur();
          option.focus({ preventScroll: true });
        });
        await expect
          .poll(async () =>
            optionInput.evaluate((option) => {
              const optionCopy = option
                .closest(".request-answer-option")
                .querySelector(".request-option-copy");
              const optionBounds = optionCopy.getBoundingClientRect();
              const requestsBounds = document
                .querySelector("#requests")
                .getBoundingClientRect();
              const actionsBounds = document
                .querySelector('[data-request="overflow-question"]')
                .querySelector(".request-answer-actions")
                .getBoundingClientRect();
              return (
                optionBounds.top >= requestsBounds.top &&
                optionBounds.bottom <= requestsBounds.bottom &&
                actionsBounds.top >= requestsBounds.top &&
                actionsBounds.bottom <= requestsBounds.bottom
              );
            }),
          )
          .toBe(true);
        const box = await view.evaluate((focusEdge) => {
          const rect = (selector) => {
            const { top, bottom, height } = document
              .querySelector(selector)
              .getBoundingClientRect();
            return { top, bottom, height };
          };
          const messages = document.querySelector("#messages");
          const requestsNode = document.querySelector("#requests");
          const card = document.querySelector(
            '[data-request="overflow-question"]',
          );
          const fields = card.querySelector(".request-answer-fields");
          const options = card.querySelectorAll(
            ".request-answer-fields input[type=checkbox]",
          );
          const focusedInput =
            focusEdge === "first" ? options.item(0) : options.item(79);
          const focusedOption = focusedInput.closest(".request-answer-option");
          const optionCopy = focusedOption
            .closest(".request-answer-option")
            .querySelector(".request-option-copy");
          const actions = card
            .querySelector(".request-answer-actions")
            .getBoundingClientRect();
          const requestsBounds = requestsNode.getBoundingClientRect();
          const optionBounds = focusedOption.getBoundingClientRect();
          const copyBounds = optionCopy.getBoundingClientRect();
          const scrollableAncestors = [];
          for (
            let node = focusedOption.parentElement;
            node;
            node = node.parentElement
          ) {
            if (node.id === "messages") break;
            const overflowY = getComputedStyle(node).overflowY;
            if (overflowY === "auto" || overflowY === "scroll")
              scrollableAncestors.push(node.id || node.className);
            if (node.id === "conversation") break;
          }
          const documentRoot = document.scrollingElement;
          return {
            viewport: { width: innerWidth, height: innerHeight },
            requests: {
              ...rect("#requests"),
              clientHeight: requestsNode.clientHeight,
              scrollHeight: requestsNode.scrollHeight,
              scrollTop: requestsNode.scrollTop,
            },
            form: (() => {
              const { top, bottom, height } = card
                .querySelector(".request-answer-form")
                .getBoundingClientRect();
              return { top, bottom, height };
            })(),
            fields: {
              ...rect(".request-answer-fields"),
              clientHeight: fields.clientHeight,
              scrollHeight: fields.scrollHeight,
              scrollTop: fields.scrollTop,
            },
            actions: rect(".request-answer-actions"),
            focusedOption: {
              bounds: {
                top: optionBounds.top,
                bottom: optionBounds.bottom,
              },
              active: document.activeElement === focusedInput,
              insideFields:
                optionBounds.top >= fields.getBoundingClientRect().top - 1 &&
                optionBounds.bottom <=
                  fields.getBoundingClientRect().bottom + 1,
              labelInsideRequests:
                copyBounds.top >= requestsBounds.top - 1 &&
                copyBounds.bottom <= requestsBounds.bottom + 1,
              labelVisible:
                copyBounds.top >= requestsBounds.top &&
                copyBounds.bottom <= requestsBounds.bottom,
              submitInsideRequests:
                actions.top >= requestsBounds.top &&
                actions.bottom <= requestsBounds.bottom,
              scrollableAncestors,
            },
            messages: {
              clientHeight: messages.clientHeight,
              scrollHeight: messages.scrollHeight,
              scrollTop: messages.scrollTop,
            },
            document: {
              clientHeight: documentRoot.clientHeight,
              scrollHeight: documentRoot.scrollHeight,
            },
          };
        }, edge);
        assert.ok(
          box.requests.height <=
            viewport.visibleHeight * REQUESTS_VISIBLE_HEIGHT_RATIO + 1,
          `#requests exceeds its visible-height cap: ${JSON.stringify(box)}`,
        );
        assert.ok(
          box.requests.scrollHeight > box.requests.clientHeight,
          "#requests owns overflow from its questions",
        );
        assert.equal(
          box.document.scrollHeight,
          box.document.clientHeight,
          "The document itself does not gain overflow",
        );
        assert.equal(box.focusedOption.active, true);
        assert.equal(box.focusedOption.insideFields, true);
        assert.equal(box.focusedOption.labelInsideRequests, true);
        assert.equal(
          box.focusedOption.labelVisible,
          true,
          JSON.stringify({ viewport, edge, box }),
        );
        assert.equal(
          box.focusedOption.submitInsideRequests,
          true,
          JSON.stringify({ viewport, edge, box }),
        );
        assert.deepEqual(box.focusedOption.scrollableAncestors, ["requests"]);
      }
    }
    assert.equal(await card.getByRole("button", { name: "Send" }).count(), 1);

    await view
      .locator('[data-message="fixture-arriving-message"]')
      .evaluate((node) => node.remove());
    await view.setViewportSize({ width: 1280, height: 800 });
    await view.evaluate(() => window.setTestViewport({ height: 800 }));
    await expect
      .poll(() => messageScroller.evaluate((node) => node.clientHeight))
      .toBeGreaterThan(0);
    const scrollBeforeReload = await messageScroller.evaluate(
      (node) => node.scrollTop,
    );
    await view.reload();
    await view.locator('[data-request="overflow-question"]').waitFor();
    await expect
      .poll(() =>
        messageScroller.evaluate(
          (node, savedTop) =>
            node.scrollTop ===
            Math.min(savedTop, node.scrollHeight - node.clientHeight),
          scrollBeforeReload,
        ),
      )
      .toBe(true);
  } finally {
    await context?.close();
    fixture.kill("SIGTERM");
  }
});
