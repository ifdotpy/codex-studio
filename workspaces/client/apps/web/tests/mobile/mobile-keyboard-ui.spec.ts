#!/usr/bin/env node
import { installMobileKeyboardUi } from "./mobile-keyboard-ui.fixture.ts";
import { visibleBounds } from "./mobile-ui-fixture.ts";
import { waitForFixturePort } from "../playwright.ts";
// Test viewport geometry in a headless browser with isolated server state.
import assert from "node:assert/strict";
import { mkdtemp } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import {
  expect,
  test,
  spawnFixture as spawn,
  rustUiFixture,
  queryFixtureDatabase,
} from "../playwright.ts";

const mobileBrowsers: [string, string][] = [
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
    const state = await mkdtemp(join(tmpdir(), "codex-mobile-keyboard-"));
    const fixture = spawn(process.execPath, [rustUiFixture, state], {
      stdio: ["pipe", "pipe", "pipe"],
    });
    let context,
      log = "";
    fixture.stderr?.on("data", (chunk) => {
      log += chunk;
    });
    try {
      const port = await waitForFixturePort(fixture, {
        diagnostics: () => log,
      });
      const leadId = queryFixtureDatabase(
        state,
        "SELECT json_extract(record, '$.id') AS id FROM runtime_agents WHERE json_extract(record, '$.name') = 'Release lead'",
      )[0]?.id;
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
      await page.addInitScript(installMobileKeyboardUi);
      const errors: string[] = [];
      page.on("pageerror", (error) => errors.push(error.message));
      await page.goto(`http://127.0.0.1:${port}`);
      await page
        .locator("#sidebar .project-chats .chat-row")
        .filter({ hasText: "Release lead" })
        .first()
        .click();
      await page.locator("#composer").waitFor();
      await expect
        .poll(async () => page.locator("#conversation-title").innerText(), {
          message: "The explicitly selected conversation must be Release lead",
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
          const root = window.mobileKeyboardUi
            .element("#root")
            .getBoundingClientRect();
          const composer = window.mobileKeyboardUi
            .element("#composer")
            .getBoundingClientRect();
          const footerNode = window.mobileKeyboardUi.element(".usage-footer");
          const footer = footerNode.getBoundingClientRect();
          const footerBottomSpace =
            parseFloat(getComputedStyle(footerNode).marginBottom) +
            parseFloat(
              getComputedStyle(window.mobileKeyboardUi.element("#conversation"))
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
            scrollTop: window.mobileKeyboardUi.scrollingElement().scrollTop,
            scrollY,
            bodyOverflow: getComputedStyle(document.body).overflow,
            bottomInset: document.documentElement.style.getPropertyValue(
              "--mobile-safe-area-bottom",
            ),
          };
        });
      const settle = async (
        height: number,
        top: number,
        expectedScrollY = 0,
      ) => {
        // Measure one settled online frame; connection notices are a separate flow.
        let box: Awaited<ReturnType<typeof geometry>> | undefined;
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
        assert.ok(box);
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
      const initialHeight = (await visibleBounds(composer)).height;
      const draft = "Keyboard geometry fixture\n" + "Another line\n".repeat(99);
      await composer.fill(draft);
      await expect
        .poll(async () => (await visibleBounds(composer)).height, {
          message: "A longer draft grows the input",
        })
        .toBeGreaterThan(initialHeight);
      const assertLongPromptFits = async (
        visibleHeight: number,
        label: string,
      ) => {
        const box = await page.evaluate(() => ({
          composer: window.mobileKeyboardUi
            .element("#composer")
            .getBoundingClientRect().height,
          input: window.mobileKeyboardUi.input().getBoundingClientRect().height,
          minimumInput: (() => {
            const input = window.mobileKeyboardUi.input();
            const measurement = input.cloneNode();
            if (!(measurement instanceof HTMLTextAreaElement))
              throw new Error("Textarea clone expected");
            measurement.style.position = "fixed";
            measurement.style.visibility = "hidden";
            measurement.style.left = "-10000px";
            measurement.style.top = "0";
            measurement.style.height = "auto";
            measurement.style.width = `${input.getBoundingClientRect().width}px`;
            if (!input.parentElement)
              throw new Error("Missing textarea parent");
            input.parentElement.append(measurement);
            const height = measurement.getBoundingClientRect().height;
            measurement.remove();
            return height;
          })(),
          scrollHeight: window.mobileKeyboardUi.input().scrollHeight,
          clientHeight: window.mobileKeyboardUi.input().clientHeight,
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
          window.mobileKeyboardUi.setTestViewport({
            height: 790,
            offsetTop: 12,
          }),
        );
        await settle(790, 12);
        assert.equal(
          await page
            .locator("#root")
            .evaluate((node) => getComputedStyle(node).paddingTop),
          "47px",
        );
        assert.ok(
          (await visibleBounds(page.locator(".workspace-header"))).y >= 59,
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
      await composer.evaluate((element: HTMLTextAreaElement) => {
        element.setSelectionRange(8, 16);
        element.addEventListener(
          "blur",
          () => window.mobileKeyboardUi.composerBlurCount++,
        );
        window.mobileKeyboardUi.composerTextarea = element;
        window.mobileKeyboardUi.composerFocusCalls.focus = 0;
        window.mobileKeyboardUi.composerFocusCalls.selection = 0;
      });
      await page.evaluate(() =>
        window.mobileKeyboardUi.setTestViewport({ height: 390, offsetTop: 54 }),
      );
      assert.equal((await settle(390, 54)).bottomInset, "0px");
      await assertLongPromptFits(390, "keyboard-open viewport");
      await page.evaluate(() => {
        window.mobileKeyboardUi.retainTestScroll(0);
        const messages = window.mobileKeyboardUi.element("#messages");
        window.mobileKeyboardUi.composerScrollBefore = {
          page: window.scrollY,
          document: window.mobileKeyboardUi.scrollingElement().scrollTop,
          transcript: messages.scrollTop,
        };
        window.mobileKeyboardUi.setTestViewport(
          { height: 330, offsetTop: 64 },
          "scroll",
        );
        window.dispatchEvent(new Event("resize"));
      });
      await settle(330, 64, 0);
      await assertLongPromptFits(330, "language-picker viewport");
      await page.evaluate(() =>
        window.mobileKeyboardUi.setTestViewport({ height: 390, offsetTop: 54 }),
      );
      await settle(390, 54, 0);
      const composerState = await page.evaluate(() => {
        const element = window.mobileKeyboardUi.input();
        const messages = window.mobileKeyboardUi.element("#messages");
        return {
          sameNode: element === window.mobileKeyboardUi.composerTextarea,
          focused:
            document.activeElement === window.mobileKeyboardUi.composerTextarea,
          blurCount: window.mobileKeyboardUi.composerBlurCount,
          calls: { ...window.mobileKeyboardUi.composerFocusCalls },
          value: element.value,
          selectionStart: element.selectionStart,
          selectionEnd: element.selectionEnd,
          transcriptScrollTop: messages.scrollTop,
          scrollBefore: window.mobileKeyboardUi.composerScrollBefore,
          pageScrollY: window.scrollY,
          documentScrollTop:
            window.mobileKeyboardUi.scrollingElement().scrollTop,
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
        await composer.evaluate((node: HTMLTextAreaElement) => ({
          sameNode: node === window.mobileKeyboardUi.composerTextarea,
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
            node === window.mobileKeyboardUi.composerTextarea &&
            document.activeElement === node,
        ),
        true,
      );
      await page.evaluate(() =>
        window.mobileKeyboardUi.composerTextarea?.blur(),
      );
      await page.evaluate(() => window.mobileKeyboardUi.retainTestScroll(0));
      await page.screenshot({
        path: join(state, "keyboard-open.png"),
        clip: { x: 0, y: 54, width: 390, height: 390 },
      });
      // Pinch zoom must not collapse the application to the magnified viewport.
      await page.evaluate(() =>
        window.mobileKeyboardUi.setTestViewport({
          scale: 2,
          height: 195,
          offsetTop: 140,
        }),
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
        window.mobileKeyboardUi.setTestViewport({
          scale: 1,
          height: 844,
          offsetTop: 0,
        }),
      );
      assert.equal(
        (await settle(844, 0)).bottomInset,
        "env(safe-area-inset-bottom)",
      );
      await page.setViewportSize({ width: 640, height: 390 });
      await page.evaluate(() => {
        window.mobileKeyboardUi.setTestViewport({
          width: 640,
          height: 390,
          offsetTop: 0,
        });
        window.dispatchEvent(new Event("orientationchange"));
      });
      await settle(390, 0);
      await page.setViewportSize({ width: 390, height: 844 });
      await page.evaluate(() =>
        window.mobileKeyboardUi.setTestViewport({
          width: 390,
          height: 844,
          offsetTop: 0,
        }),
      );
      await settle(844, 0);
      // Desktop must regain its normal document layout and terminal space.
      await page.setViewportSize({ width: 1440, height: 960 });
      await page.waitForFunction(
        () =>
          getComputedStyle(window.mobileKeyboardUi.element("#root"))
            .position !== "fixed" &&
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
        window.mobileKeyboardUi.setTestViewport({
          width: 390,
          height: 844,
          offsetTop: 0,
        }),
      );
      await settle(844, 0, 0);
      await composer.fill("");
      const twoRowHeight = (await visibleBounds(composer)).height;
      const shortDraftComposerHeight = (
        await visibleBounds(page.locator("#composer"))
      ).height;
      assert.ok(Math.abs(twoRowHeight - 54.78125) < 1);
      assert.ok(Math.abs(shortDraftComposerHeight - 169.78125) < 1);
      await composer.fill("Short keyboard draft");
      assert.equal((await visibleBounds(composer)).height, twoRowHeight);
      await page.evaluate(() =>
        window.mobileKeyboardUi.setTestViewport({ height: 390, offsetTop: 54 }),
      );
      await settle(390, 54, 0);
      assert.equal((await visibleBounds(composer)).height, twoRowHeight);
      assert.equal(
        (await visibleBounds(page.locator("#composer"))).height,
        shortDraftComposerHeight,
      );
      await page.evaluate(() =>
        window.mobileKeyboardUi.setTestViewport({ height: 330, offsetTop: 64 }),
      );
      const minimumViewport = await settle(330, 64, 0);
      assert.equal((await visibleBounds(composer)).height, twoRowHeight);
      assert.equal(
        (await visibleBounds(page.locator("#composer"))).height,
        shortDraftComposerHeight,
      );
      assert.equal(minimumViewport.footerBottom, 378);

      await page.getByLabel("Back to chats").click();
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
        window.mobileKeyboardUi.setTestViewport({ height: 844, offsetTop: 0 }),
      );
      await settle(844, 0);
      const expandedSettingsHeight = (await visibleBounds(settings)).height;
      assert.equal(expandedSettingsHeight, 108);
      const assertEmptyKeyboard = async (height: number, top: number) => {
        await page.evaluate(
          ({ height, top }) =>
            window.mobileKeyboardUi.setTestViewport({ height, offsetTop: top }),
          { height, top },
        );
        await expect
          .poll(() =>
            page.evaluate(() => ({
              height: window.mobileKeyboardUi
                .element("#root")
                .getBoundingClientRect().height,
              bottom: window.mobileKeyboardUi
                .element("#root")
                .getBoundingClientRect().bottom,
              footerBottom: window.mobileKeyboardUi
                .element(".usage-footer")
                .getBoundingClientRect().bottom,
              composerBottom: window.mobileKeyboardUi
                .element("#composer")
                .getBoundingClientRect().bottom,
              scrollY,
              panelHeight: window.mobileKeyboardUi.element(
                ".empty-chat-settings",
              ).clientHeight,
              panelScrollHeight: window.mobileKeyboardUi.element(
                ".empty-chat-settings",
              ).scrollHeight,
            })),
          )
          .toMatchObject({ height });
        const box = await page.evaluate(() => {
          const root = window.mobileKeyboardUi
            .element("#root")
            .getBoundingClientRect();
          const footer = window.mobileKeyboardUi
            .element(".usage-footer")
            .getBoundingClientRect();
          const composer = window.mobileKeyboardUi
            .element("#composer")
            .getBoundingClientRect();
          const panel = window.mobileKeyboardUi.element(".empty-chat-settings");
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
        let first = await visibleBounds(menus.nth(0));
        let panel = await visibleBounds(settings);
        assert.ok(
          first.y >= panel.y &&
            first.y + first.height <= panel.y + panel.height,
        );
        await settings.evaluate((node) => (node.scrollTop = node.scrollHeight));
        const last = await visibleBounds(menus.nth(1));
        panel = await visibleBounds(settings);
        assert.ok(
          last.y >= panel.y && last.y + last.height <= panel.y + panel.height,
        );
        assert.equal(await page.evaluate(() => scrollY), 0);
      };
      await assertEmptyKeyboard(390, 54);
      await assertEmptyKeyboard(330, 64);
      await page.evaluate(() =>
        window.mobileKeyboardUi.setTestViewport({ height: 844, offsetTop: 0 }),
      );
      await expect
        .poll(async () => (await visibleBounds(settings)).height)
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

test("mobile board opens the explicitly selected chat in a fresh iPhone context", async ({
  browser,
  readOnlyRust,
}) => {
  const context = await browser.newContext({
    viewport: { width: 390, height: 844 },
    isMobile: true,
    hasTouch: true,
    userAgent:
      "Mozilla/5.0 (iPhone; CPU iPhone OS 18_0 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/18.0 Mobile/15E148 Safari/604.1",
  });
  try {
    const page = await context.newPage();
    await readOnlyRust.open(page);
    await expect(
      page.locator("#sidebar .sidebar-project").first(),
    ).toBeVisible();
    await expect(
      page.locator('#sidebar .chat-row[aria-current="true"]'),
    ).toHaveCount(0);
    await page
      .locator("#sidebar .project-chats .chat-row")
      .filter({ hasText: "Other project" })
      .first()
      .click();
    await expect(page.locator("#composer")).toBeVisible();
    await expect(page.locator("#conversation-title")).toHaveText(
      "Other project",
    );
  } finally {
    await context.close();
  }
});
