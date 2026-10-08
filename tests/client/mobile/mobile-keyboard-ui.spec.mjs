#!/usr/bin/env node
// Test viewport geometry in a headless browser with isolated server state.
import assert from "node:assert/strict";
import { mkdtemp } from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { expect, test, spawnFixture as spawn } from "../playwright.mjs";

const mobileBrowsers = [
  [
    "Android",
    "Mozilla/5.0 (Linux; Android 14; Pixel 7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/130.0.0.0 Mobile Safari/537.36",
  ],
  [
    "iPhone",
    "Mozilla/5.0 (iPhone; CPU iPhone OS 18_0 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/18.0 Mobile/15E148 Safari/604.1",
  ],
];

for (const [platform, userAgent] of mobileBrowsers) {
  test(`mobile keyboard ui ${platform}`, async ({
    browser: runnerBrowser,
    browserName,
  }) => {
    const root = dirname(
      dirname(dirname(dirname(fileURLToPath(import.meta.url)))),
    );
    const state = await mkdtemp(join(tmpdir(), "codex-mobile-keyboard-"));
    const fixture = spawn(
      "python3",
      ["-B", join(root, "tests/simple-ui-fixture.py"), state],
      { stdio: ["pipe", "pipe", "pipe"] },
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
        fixture.once("exit", () => reject(new Error(log)));
      });
      context = await runnerBrowser.newContext({
        viewport: { width: 390, height: 844 },
        isMobile: true,
        hasTouch: true,
        userAgent,
      });
      const page = await context.newPage();
      await page.addInitScript(() => {
        const viewport = new EventTarget();
        Object.assign(viewport, {
          width: 390,
          height: 844,
          offsetTop: 0,
          offsetLeft: 0,
          scale: 1,
        });
        Object.defineProperty(window, "visualViewport", {
          configurable: true,
          value: viewport,
        });
        window.setTestViewport = (patch, type = "resize") => {
          Object.assign(viewport, patch);
          viewport.dispatchEvent(new Event(type));
        };
        let retainedScroll = 0;
        const originalScroll = window.scrollTo.bind(window);
        Object.defineProperty(window, "scrollY", {
          configurable: true,
          get: () => retainedScroll,
        });
        window.scrollTo = (...args) => {
          retainedScroll = 0;
          originalScroll(...args);
        };
        window.retainTestScroll = (value) => {
          retainedScroll = value;
          window.dispatchEvent(new Event("scroll"));
        };
        const focusCalls = { focus: 0, selection: 0 };
        const focus = HTMLElement.prototype.focus;
        HTMLElement.prototype.focus = function (...args) {
          focusCalls.focus++;
          return focus.apply(this, args);
        };
        const setSelectionRange =
          HTMLTextAreaElement.prototype.setSelectionRange;
        HTMLTextAreaElement.prototype.setSelectionRange = function (...args) {
          focusCalls.selection++;
          return setSelectionRange.apply(this, args);
        };
        window.composerFocusCalls = focusCalls;
        window.composerBlurCount = 0;
      });
      const errors = [];
      page.on("pageerror", (error) => errors.push(error.message));
      await page.goto(`http://127.0.0.1:${port}`);
      await page.locator("#composer").waitFor();
      const geometry = () =>
        page.evaluate(() => {
          const root = document.querySelector("#root").getBoundingClientRect();
          const composer = document
            .querySelector("#composer")
            .getBoundingClientRect();
          const footerNode = document.querySelector(".usage-footer");
          const footer = footerNode.getBoundingClientRect();
          const footerBottomSpace =
            parseFloat(getComputedStyle(footerNode).marginBottom) +
            parseFloat(
              getComputedStyle(document.querySelector("#conversation"))
                .paddingBottom,
            );
          return {
            layout: [
              ...document.querySelectorAll(
                ".workspace-header, #conversation > *, #message",
              ),
            ].map((node) => ({
              id: node.id || node.className,
              height: node.getBoundingClientRect().height,
              minHeight: getComputedStyle(node).minHeight,
            })),
            connectionNotice: !!document.querySelector(
              '.sync-status[role="alert"]',
            ),
            top: root.top,
            height: root.height,
            bottom: root.bottom,
            composerBottom: composer.bottom,
            footerTop: footer.top,
            footerBottom: footer.bottom,
            footerBottomSpace,
            scrollTop: document.scrollingElement.scrollTop,
            scrollY,
            bodyOverflow: getComputedStyle(document.body).overflow,
            bottomInset: document.documentElement.style.getPropertyValue(
              "--mobile-safe-area-bottom",
            ),
          };
        });
      const settle = async (height, top, expectedScrollY = 0) => {
        // Measure one settled online frame; connection notices are a separate flow.
        let box;
        await expect
          .poll(async () => {
            box = await geometry();
            return (
              !box.connectionNotice &&
              Math.abs(box.height - height) < 1 &&
              Math.abs(box.top - top) < 1
            );
          })
          .toBe(true);
        assert.equal(box.scrollY, expectedScrollY);
        assert.equal(box.scrollTop, 0);
        assert.equal(box.bodyOverflow, "hidden");
        assert.ok(box.composerBottom <= box.bottom + 1, JSON.stringify(box));
        assert.ok(
          box.composerBottom <= box.footerTop + 1,
          `Composer overlaps usage: ${JSON.stringify(box)}`,
        );
        if (box.footerBottom > box.bottom + 1) {
          await page.screenshot({ path: join(state, "overflow.png") });
          console.log("Overflow screenshot:", join(state, "overflow.png"));
        }
        assert.ok(
          box.footerBottom <= box.bottom + 1,
          `Usage leaves the viewport: ${JSON.stringify(box)}`,
        );
        assert.ok(
          Math.abs(box.bottom - box.footerBottom - box.footerBottomSpace) <= 1,
          `No blank area below usage: ${JSON.stringify(box)}`,
        );
        return box;
      };
      await settle(844, 0);
      assert.equal(
        await page
          .locator('meta[name="apple-mobile-web-app-status-bar-style"]')
          .getAttribute("content"),
        "black",
      );
      if (browserName === "chromium") {
        const cdp = await page.context().newCDPSession(page);
        await cdp.send("Emulation.setSafeAreaInsetsOverride", {
          insets: { top: 59, bottom: 0, left: 0, right: 0 },
        });
        await page.evaluate(() =>
          window.setTestViewport({ height: 790, offsetTop: 12 }),
        );
        await settle(790, 12);
        assert.equal(
          await page
            .locator("#root")
            .evaluate((node) => getComputedStyle(node).paddingTop),
          "47px",
        );
        assert.ok(
          (await page.locator(".workspace-header").boundingBox()).y >= 59,
          "A partial viewport shift must not move the header under the status bar",
        );
      }
      const composer = page.locator("#message");
      const initialHeight = (await composer.boundingBox()).height;
      const draft = "Keyboard geometry fixture\n" + "Another line\n".repeat(12);
      await composer.fill(draft);
      assert.equal(
        (await composer.boundingBox()).height,
        initialHeight,
        "Mobile input keeps its height when the draft grows",
      );
      assert.equal(
        await composer.evaluate(
          (node) => node.scrollHeight > node.clientHeight,
        ),
        true,
        "Long drafts scroll inside the input",
      );
      await composer.evaluate((element) => {
        element.setSelectionRange(8, 16);
        element.addEventListener("blur", () => window.composerBlurCount++);
        window.composerTextarea = element;
        window.composerResizeMutations = [];
        new MutationObserver((records) => {
          window.composerResizeMutations.push(
            ...records.map((record) => record.attributeName),
          );
        }).observe(element, {
          attributes: true,
          attributeFilter: ["style", "rows"],
        });
        window.composerFocusCalls.focus = 0;
        window.composerFocusCalls.selection = 0;
      });
      await page.evaluate(() =>
        window.setTestViewport({ height: 390, offsetTop: 54 }),
      );
      assert.equal((await settle(390, 54)).bottomInset, "0px");
      assert.equal(
        (await composer.boundingBox()).height,
        initialHeight,
        "Opening the keyboard keeps input height stable",
      );
      await page.evaluate(() => {
        window.retainTestScroll(150);
        const messages = document.querySelector("#messages");
        window.composerScrollBefore = {
          page: window.scrollY,
          document: document.scrollingElement.scrollTop,
          transcript: messages.scrollTop,
        };
        window.setTestViewport({ height: 330, offsetTop: 64 }, "scroll");
        window.dispatchEvent(new Event("resize"));
      });
      await settle(330, 64, 150);
      assert.equal(
        (await composer.boundingBox()).height,
        initialHeight,
        "Language picker geometry keeps input height stable",
      );
      await page.evaluate(() =>
        window.setTestViewport({ height: 390, offsetTop: 54 }),
      );
      await settle(390, 54, 150);
      const composerState = await page.evaluate(() => {
        const element = document.querySelector("#message");
        const messages = document.querySelector("#messages");
        return {
          resizeMutations: window.composerResizeMutations,
          sameNode: element === window.composerTextarea,
          focused: document.activeElement === window.composerTextarea,
          blurCount: window.composerBlurCount,
          calls: { ...window.composerFocusCalls },
          value: element.value,
          selectionStart: element.selectionStart,
          selectionEnd: element.selectionEnd,
          transcriptScrollTop: messages.scrollTop,
          scrollBefore: window.composerScrollBefore,
          pageScrollY: window.scrollY,
          documentScrollTop: document.scrollingElement.scrollTop,
        };
      });
      assert.equal(composerState.sameNode, true, "textarea node stays mounted");
      assert.equal(composerState.focused, true, "composer remains focused");
      assert.equal(composerState.blurCount, 0, "viewport changes do not blur");
      assert.deepEqual(composerState.calls, { focus: 0, selection: 0 });
      assert.equal(composerState.value, draft);
      assert.deepEqual(
        composerState.resizeMutations,
        [],
        "Viewport events do not resize the focused textarea",
      );
      assert.equal(composerState.selectionStart, 8);
      assert.equal(composerState.selectionEnd, 16);
      assert.equal(composerState.pageScrollY, composerState.scrollBefore.page);
      assert.equal(
        composerState.documentScrollTop,
        composerState.scrollBefore.document,
      );
      assert.equal(
        composerState.transcriptScrollTop,
        composerState.scrollBefore.transcript,
      );
      // Rotation can cross the desktop breakpoint while the native keyboard is open.
      await page.setViewportSize({ width: 844, height: 390 });
      await page.waitForFunction(
        () =>
          !document.documentElement.style.getPropertyValue(
            "--mobile-viewport-height",
          ),
      );
      assert.deepEqual(
        await composer.evaluate((node) => ({
          sameNode: node === window.composerTextarea,
          focused: document.activeElement === node,
          start: node.selectionStart,
          end: node.selectionEnd,
          value: node.value,
        })),
        { sameNode: true, focused: true, start: 8, end: 16, value: draft },
      );
      await page.setViewportSize({ width: 390, height: 844 });
      await settle(390, 54, 150);
      assert.equal(
        await composer.evaluate(
          (node) =>
            node === window.composerTextarea && document.activeElement === node,
        ),
        true,
      );
      await page.evaluate(() => window.composerTextarea.blur());
      await page.evaluate(() => window.retainTestScroll(150));
      await page.screenshot({
        path: join(state, "keyboard-open.png"),
        clip: { x: 0, y: 54, width: 390, height: 390 },
      });
      // Pinch zoom must not collapse the application to the magnified viewport.
      await page.evaluate(() =>
        window.setTestViewport({ scale: 2, height: 195, offsetTop: 140 }),
      );
      await page.evaluate(
        () =>
          new Promise((resolve) =>
            requestAnimationFrame(() => requestAnimationFrame(resolve)),
          ),
      );
      const zoomed = await geometry();
      assert.equal(zoomed.height, 390);
      assert.equal(zoomed.top, 54);
      await page.evaluate(() =>
        window.setTestViewport({ scale: 1, height: 844, offsetTop: 0 }),
      );
      assert.equal(
        (await settle(844, 0)).bottomInset,
        "env(safe-area-inset-bottom)",
      );
      await page.setViewportSize({ width: 640, height: 390 });
      await page.evaluate(() => {
        window.setTestViewport({ width: 640, height: 390, offsetTop: 0 });
        window.dispatchEvent(new Event("orientationchange"));
      });
      await settle(390, 0);
      await page.setViewportSize({ width: 390, height: 844 });
      await page.evaluate(() =>
        window.setTestViewport({ width: 390, height: 844, offsetTop: 0 }),
      );
      await settle(844, 0);
      // Desktop must regain its normal document layout and terminal space.
      await page.setViewportSize({ width: 1440, height: 960 });
      await page.waitForFunction(
        () =>
          getComputedStyle(document.querySelector("#root")).position !==
            "fixed" &&
          !document.documentElement.style.getPropertyValue(
            "--mobile-viewport-height",
          ),
      );
      assert.equal(
        await page.evaluate(() =>
          document.documentElement.style.getPropertyValue(
            "--mobile-viewport-height",
          ),
        ),
        "",
      );
      assert.equal(errors.length, 0, errors.join("\n"));
      console.log(
        `mobile-keyboard-ui: PASS (simulated keyboard geometry, input stability, offset, scroll reset, close, rotation, pinch, desktop). Screenshot: ${join(state, "keyboard-open.png")}`,
      );
    } catch (error) {
      console.error(log);
      throw error;
    } finally {
      await context?.close();
    }
  });
}
