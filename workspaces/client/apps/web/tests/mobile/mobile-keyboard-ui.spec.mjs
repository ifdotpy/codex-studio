#!/usr/bin/env node
// Test viewport geometry in a headless browser with isolated server state.
import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { mkdtemp } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
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
    const root = fileURLToPath(new URL("../../../../../../", import.meta.url));
    const state = await mkdtemp(join(tmpdir(), "codex-mobile-keyboard-"));
    const fixture = spawn(
      "python3",
      [
        "-B",
        join(root, "workspaces/runtime/apps/server/tests/simple-ui-fixture.py"),
        state,
      ],
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
      const leadId = execFileSync(
        "python3",
        [
          "-c",
          "import json,sqlite3,sys; db=sqlite3.connect(sys.argv[1]); rows=db.execute('SELECT record FROM runtime_agents').fetchall(); print(next(json.loads(row[0])['id'] for row in rows if json.loads(row[0]).get('name') == 'Release lead'))",
          join(state, "canvas.sqlite3"),
        ],
        { encoding: "utf8" },
      ).trim();
      assert.ok(leadId, "The fixture must provide the Release lead chat ID");
      context = await runnerBrowser.newContext({
        viewport: { width: 390, height: 844 },
        isMobile: true,
        hasTouch: true,
        userAgent,
      });
      // This geometry scenario intentionally exercises the non-empty lead.
      await context.addInitScript((id) => {
        localStorage.setItem("codex-mobile-opened", JSON.stringify(id));
      }, leadId);
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
      await expect
        .poll(async () => page.locator("#conversation-title").innerText(), {
          message:
            "After the initial lead list settles, the default conversation must be Release lead",
          timeout: 10_000,
        })
        .toBe("Release lead");
      await expect
        .poll(
          () =>
            page
              .locator("#messages")
              .evaluate((root) =>
                root.scrollHeight > root.clientHeight + 1
                  ? Math.abs(
                      root.scrollHeight - root.clientHeight - root.scrollTop,
                    )
                  : Number.POSITIVE_INFINITY,
              ),
          {
            message: "The initial transcript settles at the following position",
          },
        )
        .toBeLessThan(1);
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
              '#error, .sync-status[role="alert"]',
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
      const composer = page.locator("#message");
      const initialHeight = (await composer.boundingBox()).height;
      const draft = "Keyboard geometry fixture\n" + "Another line\n".repeat(99);
      await composer.fill(draft);
      await expect
        .poll(async () => (await composer.boundingBox()).height, {
          message: "A longer draft grows the input",
        })
        .toBeGreaterThan(initialHeight);
      const assertLongPromptFits = async (visibleHeight, label) => {
        const box = await page.evaluate(() => ({
          composer: document.querySelector("#composer").getBoundingClientRect()
            .height,
          input: document.querySelector("#message").getBoundingClientRect()
            .height,
          minimumInput: (() => {
            const input = document.querySelector("#message");
            const measurement = input.cloneNode();
            measurement.style.position = "fixed";
            measurement.style.visibility = "hidden";
            measurement.style.left = "-10000px";
            measurement.style.top = "0";
            measurement.style.height = "auto";
            measurement.style.width = `${input.getBoundingClientRect().width}px`;
            input.parentElement.append(measurement);
            const height = measurement.getBoundingClientRect().height;
            measurement.remove();
            return height;
          })(),
          scrollHeight: document.querySelector("#message").scrollHeight,
          clientHeight: document.querySelector("#message").clientHeight,
        }));
        assert.ok(
          box.composer <= visibleHeight * 0.6 + 1,
          `${label}: composer exceeds 60% of visible height: ${JSON.stringify(box)}`,
        );
        assert.ok(
          box.scrollHeight > box.clientHeight,
          `${label}: long draft must scroll inside the input: ${JSON.stringify(box)}`,
        );
        if (visibleHeight === 844)
          assert.ok(
            box.composer >= visibleHeight * 0.55 &&
              box.composer <= visibleHeight * 0.65,
            `A long draft uses about 60% when space is available: ${JSON.stringify(box)}`,
          );
        if (visibleHeight === 390)
          assert.ok(
            box.input > box.minimumInput + 1,
            `A keyboard-open viewport uses available transcript space: ${JSON.stringify(box)}`,
          );
        if (visibleHeight === 330)
          assert.ok(
            Math.abs(box.input - box.minimumInput) <= 1,
            `A 330px viewport keeps the two-row minimum: ${JSON.stringify(box)}`,
          );
        return box;
      };
      await assertLongPromptFits(844, "390x844 viewport");
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
      await assertLongPromptFits(790, "390x790 viewport");
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
        window.composerFocusCalls.focus = 0;
        window.composerFocusCalls.selection = 0;
      });
      await page.evaluate(() =>
        window.setTestViewport({ height: 390, offsetTop: 54 }),
      );
      assert.equal((await settle(390, 54)).bottomInset, "0px");
      await assertLongPromptFits(390, "keyboard-open viewport");
      await page.evaluate(() => {
        window.retainTestScroll(0);
        const messages = document.querySelector("#messages");
        window.composerScrollBefore = {
          page: window.scrollY,
          document: document.scrollingElement.scrollTop,
          transcript: messages.scrollTop,
        };
        window.setTestViewport({ height: 330, offsetTop: 64 }, "scroll");
        window.dispatchEvent(new Event("resize"));
      });
      await settle(330, 64, 0);
      await assertLongPromptFits(330, "language-picker viewport");
      await page.evaluate(() =>
        window.setTestViewport({ height: 390, offsetTop: 54 }),
      );
      await settle(390, 54, 0);
      const composerState = await page.evaluate(() => {
        const element = document.querySelector("#message");
        const messages = document.querySelector("#messages");
        return {
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
      await assertLongPromptFits(390, "restored keyboard viewport");
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
        JSON.stringify(composerState),
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
      await settle(390, 54, 0);
      assert.equal(
        await composer.evaluate(
          (node) =>
            node === window.composerTextarea && document.activeElement === node,
        ),
        true,
      );
      await page.evaluate(() => window.composerTextarea.blur());
      await page.evaluate(() => window.retainTestScroll(0));
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
      await page.setViewportSize({ width: 390, height: 844 });
      await page.evaluate(() =>
        window.setTestViewport({ width: 390, height: 844, offsetTop: 0 }),
      );
      await settle(844, 0, 0);
      await composer.fill("");
      const twoRowHeight = (await composer.boundingBox()).height;
      const shortDraftComposerHeight = (
        await page.locator("#composer").boundingBox()
      ).height;
      assert.ok(Math.abs(twoRowHeight - 54.78125) < 1);
      assert.ok(Math.abs(shortDraftComposerHeight - 169.78125) < 1);
      await composer.fill("Short keyboard draft");
      assert.equal((await composer.boundingBox()).height, twoRowHeight);
      await page.evaluate(() =>
        window.setTestViewport({ height: 390, offsetTop: 54 }),
      );
      await settle(390, 54, 0);
      assert.equal((await composer.boundingBox()).height, twoRowHeight);
      assert.equal(
        (await page.locator("#composer").boundingBox()).height,
        shortDraftComposerHeight,
      );
      await page.evaluate(() =>
        window.setTestViewport({ height: 330, offsetTop: 64 }),
      );
      const minimumViewport = await settle(330, 64, 0);
      assert.equal((await composer.boundingBox()).height, twoRowHeight);
      assert.equal(
        (await page.locator("#composer").boundingBox()).height,
        shortDraftComposerHeight,
      );
      assert.equal(minimumViewport.footerBottom, 378);

      await page.getByLabel("Toggle conversations").click();
      await page
        .locator(".chat-row")
        .filter({ hasText: "Other project" })
        .click();
      await expect(page.locator("#conversation-title")).toHaveText(
        "Other project",
      );
      const settings = page.locator(".empty-chat-settings");
      await expect(settings).toBeVisible();
      await page.evaluate(() =>
        window.setTestViewport({ height: 844, offsetTop: 0 }),
      );
      await settle(844, 0);
      const expandedSettingsHeight = (await settings.boundingBox()).height;
      assert.equal(expandedSettingsHeight, 108);
      const assertEmptyKeyboard = async (height, top) => {
        await page.evaluate(
          ({ height, top }) =>
            window.setTestViewport({ height, offsetTop: top }),
          { height, top },
        );
        await expect
          .poll(() =>
            page.evaluate(() => ({
              height: document.querySelector("#root").getBoundingClientRect()
                .height,
              bottom: document.querySelector("#root").getBoundingClientRect()
                .bottom,
              footerBottom: document
                .querySelector(".usage-footer")
                .getBoundingClientRect().bottom,
              composerBottom: document
                .querySelector("#composer")
                .getBoundingClientRect().bottom,
              scrollY,
              panelHeight: document.querySelector(".empty-chat-settings")
                .clientHeight,
              panelScrollHeight: document.querySelector(".empty-chat-settings")
                .scrollHeight,
            })),
          )
          .toMatchObject({ height });
        const box = await page.evaluate(() => {
          const root = document.querySelector("#root").getBoundingClientRect();
          const footer = document
            .querySelector(".usage-footer")
            .getBoundingClientRect();
          const composer = document
            .querySelector("#composer")
            .getBoundingClientRect();
          const panel = document.querySelector(".empty-chat-settings");
          return {
            bottom: root.bottom,
            footerBottom: footer.bottom,
            composerBottom: composer.bottom,
            scrollY,
            panelHeight: panel.clientHeight,
            panelScrollHeight: panel.scrollHeight,
          };
        });
        assert.ok(box.footerBottom <= box.bottom + 1, JSON.stringify(box));
        assert.ok(box.composerBottom <= box.bottom + 1, JSON.stringify(box));
        assert.equal(box.scrollY, 0);
        await expect(page.locator("#send")).toBeVisible();
        await expect(
          page.locator("#composer .composer-submit-actions"),
        ).toBeVisible();
        const menus = settings.locator(".execution-menu");
        assert.equal(await menus.count(), 2);
        await settings.evaluate((node) => (node.scrollTop = 0));
        let first = await menus.nth(0).boundingBox();
        let panel = await settings.boundingBox();
        assert.ok(
          first.y >= panel.y &&
            first.y + first.height <= panel.y + panel.height,
        );
        await settings.evaluate((node) => (node.scrollTop = node.scrollHeight));
        const last = await menus.nth(1).boundingBox();
        panel = await settings.boundingBox();
        assert.ok(
          last.y >= panel.y && last.y + last.height <= panel.y + panel.height,
        );
        assert.equal(await page.evaluate(() => scrollY), 0);
      };
      await assertEmptyKeyboard(390, 54);
      await assertEmptyKeyboard(330, 64);
      await page.evaluate(() =>
        window.setTestViewport({ height: 844, offsetTop: 0 }),
      );
      await expect
        .poll(async () => (await settings.boundingBox()).height)
        .toBe(expandedSettingsHeight);
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

test("default mobile chat is stable across ten fresh iPhone starts", async ({
  browser,
}) => {
  test.setTimeout(180_000);
  const root = fileURLToPath(new URL("../../../../../../", import.meta.url));
  for (let run = 1; run <= 10; run++) {
    const state = await mkdtemp(join(tmpdir(), "codex-mobile-default-"));
    const fixture = spawn(
      "python3",
      [
        "-B",
        join(root, "workspaces/runtime/apps/server/tests/simple-ui-fixture.py"),
        state,
      ],
      {
        stdio: ["ignore", "pipe", "pipe"],
        env: { ...process.env, TOKEN_RATE_WORKER_COUNT: "1" },
      },
    );
    let log = "";
    fixture.stderr.on("data", (chunk) => {
      log += chunk;
    });
    let context;
    try {
      const port = await new Promise((resolve, reject) => {
        fixture.stdout.once("data", (chunk) =>
          resolve(Number(String(chunk).trim())),
        );
        fixture.once("exit", () => reject(new Error(log)));
      });
      context = await browser.newContext({
        viewport: { width: 390, height: 844 },
        isMobile: true,
        hasTouch: true,
        userAgent:
          "Mozilla/5.0 (iPhone; CPU iPhone OS 18_0 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/18.0 Mobile/15E148 Safari/604.1",
      });
      const page = await context.newPage();
      await page.goto(`http://127.0.0.1:${port}`);
      await page.locator("#composer").waitFor();
      await expect
        .poll(() => page.locator("#conversation-title").innerText(), {
          message: `Fresh iPhone fixture ${run} must default to Other project`,
          timeout: 10_000,
        })
        .toBe("Other project");
    } finally {
      await context?.close();
      const exited = new Promise((resolve) => fixture.once("exit", resolve));
      fixture.kill();
      await exited;
    }
  }
  console.log("mobile-default-chat: PASS (10/10 fresh iPhone fixture states)");
});
