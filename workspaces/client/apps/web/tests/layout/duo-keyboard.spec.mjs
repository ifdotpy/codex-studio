import { test, expect, entityPullFixtureForRequest } from "../playwright.mjs";
import { fixtureWorkspaceId } from "../servers/multi-server-fixture.mjs";
import {
  sidebarParityFixture,
  scope,
} from "../sidebar/sidebar-parity-fixture.mjs";

import {
  keyboardSequences,
  keyboardCycleFrames,
} from "./keyboardSequences.mjs";

test.use({ hasTouch: true });
for (const count of [1, 8]) {
  test(`Duo keyboard keeps ${count} attachment chips inside the composer`, async ({
    page,
    context,
  }) => {
    const fixture = await sidebarParityFixture(context, { paired: false });
    const pending = structuredClone(fixture.local.snapshot.runtime.requests);
    fixture.local.snapshot.runtime.requests = [];
    fixture.local.snapshot.runtime.complaints = [];
    fixture.local.snapshot.runtime.sessionCostsByAgent = {
      overlap: {
        rootId: "overlap",
        totalUSD: 0.5,
        pricedSamples: 1,
        breakdown: { providers: {}, models: {} },
        unknownModels: [],
        estimated: true,
        pricingState: "ready",
        cacheAgeSeconds: 0,
        refreshing: false,
      },
    };
    fixture.local.snapshot.runtime.monitors = [
      {
        id: "duo-monitor",
        agent: "overlap",
        name: "Duo monitor",
        command: "fixture command",
        status: "running",
        created: Date.now() / 1000 - 240,
      },
    ];
    await context.addInitScript(
      ({ scope, count }) => {
        const viewport = Object.assign(new EventTarget(), {
          width: 867,
          height: 669,
          offsetTop: 0,
          pageTop: 0,
          scale: 1,
        });
        let innerHeight = 669,
          scrollY = 0;
        Object.defineProperty(window, "innerHeight", {
          configurable: true,
          get: () => innerHeight,
        });
        Object.defineProperty(window, "scrollY", {
          configurable: true,
          get: () => scrollY,
        });
        Object.defineProperty(window, "visualViewport", {
          configurable: true,
          value: viewport,
        });
        window.keyboardVisualOrigin = false;
        const nativeRect = Element.prototype.getBoundingClientRect;
        Element.prototype.getBoundingClientRect = function () {
          const r = nativeRect.call(this);
          return new DOMRect(
            r.x,
            r.y - (window.keyboardVisualOrigin ? viewport.offsetTop : 0),
            r.width,
            r.height,
          );
        };
        window.resizeDuoViewport = (patch) => {
          innerHeight = patch.innerHeight ?? innerHeight;
          scrollY = patch.scrollY ?? scrollY;
          Object.assign(viewport, patch);
          viewport.dispatchEvent(new Event("resize"));
        };
        localStorage.setItem(
          `codex-agent-attachments:${scope}`,
          JSON.stringify({
            overlap: Array.from({ length: count }, (_, i) => ({
              id: `fixture-${i}`,
              name: `image-${i}-a-long-attachment-name.png`,
              mime: "image/png",
              image: true,
              size: 123,
            })),
          }),
        );
      },
      { scope, count },
    );
    await page.route("**/api/sync/pull?*", (route) => {
      const url = route.request().url();
      if (!new URL(url).searchParams.get("scope")?.startsWith("transcript:"))
        return route.fallback();
      return route.fulfill({
        json: {
          workspaceId: fixtureWorkspaceId("Local"),
          ...entityPullFixtureForRequest(fixture.local.snapshot, url, {
            transcript: {
              agent: fixture.local.snapshot.threads.find(
                (a) => a.id === "overlap",
              ),
              items: Array.from({ length: 20 }, (_, i) => ({
                id: `line-${i}`,
                role: "assistant",
                text:
                  `Conversation line ${i}. ` + "Visible content. ".repeat(8),
              })),
              truncated: false,
              nextCursor: null,
              nextAfterCursor: null,
              historyVersion: "duo-keyboard",
            },
          }),
        },
      });
    });
    const cdp = await context.newCDPSession(page);
    await cdp.send("Emulation.setSafeAreaInsetsOverride", {
      insets: { top: 0, right: 0, bottom: 34, left: 0 },
    });
    await page.setViewportSize({ width: 867, height: 669 });
    try {
      const sidebar = await fixture.open(page);
      await sidebar
        .getByRole("button")
        .filter({ hasText: "Local Saved pin" })
        .filter({ has: page.locator("strong") })
        .first()
        .click();
      await expect(
        page.locator(".workspace-header .session-activity"),
      ).toBeVisible();
      await expect
        .poll(
          async () =>
            (await page.locator(".workspace-header").boundingBox()).height,
        )
        .toBe(44);
      await expect(page.locator(".workspace > .session-activity")).toHaveCount(
        0,
      );
      await expect(page.locator(".session-cost-summary")).toContainText(
        "Session estimate",
      );
      await expect(page.locator("#composer .attachment-chip")).toHaveCount(
        count,
      );
      await page.locator("#message").fill("Keep this draft");
      const fullSidebarHeight = (await page.locator("#sidebar").boundingBox())
        .height;
      await page.evaluate(() =>
        window.resizeDuoViewport({
          height: 323,
          innerHeight: 323,
          offsetTop: 346,
          pageTop: 346,
          scrollY: 346,
        }),
      );
      await expect(page.locator("#root")).toHaveCSS("height", "669px");
      await expect
        .poll(() =>
          page
            .locator("#root")
            .evaluate(
              (n) =>
                n.getBoundingClientRect().top - window.visualViewport.offsetTop,
            ),
        )
        .toBe(0);
      await expect(page.locator("#root")).toHaveCSS("padding-bottom", "34px");
      const bounds = await page.evaluate(() => {
        const rect = (node) => {
          const r = node.getBoundingClientRect();
          return {
            top: r.top - window.visualViewport.offsetTop,
            bottom: r.bottom - window.visualViewport.offsetTop,
            left: r.left,
            right: r.right,
            height: r.height,
          };
        };
        const composer = document.querySelector("#composer");
        const messages = document.querySelector("#messages");
        const sidebar = document.querySelector("#sidebar");
        return {
          composer: rect(composer),
          messages: rect(messages),
          lineHeight: parseFloat(getComputedStyle(messages).lineHeight),
          chips: [...composer.querySelectorAll(".attachment-chip")].map(rect),
          sidebar: rect(sidebar),
          sidebarScroll: rect(sidebar.querySelector(".chat-scroll")),
        };
      });
      console.log("Duo keyboard bounds", count, JSON.stringify(bounds));
      for (const chip of bounds.chips) {
        expect(chip.top).toBeGreaterThanOrEqual(bounds.composer.top);
        expect(chip.bottom).toBeLessThanOrEqual(bounds.composer.bottom);
      }
      await expect(page.locator(".viewport-bottom-band")).toBeHidden();
      await expect(page.locator(".session-cost-summary")).toBeHidden();
      expect(
        Math.min(bounds.messages.bottom, bounds.composer.top) -
          bounds.messages.top,
      ).toBeGreaterThanOrEqual(bounds.lineHeight * 3);
      expect(bounds.composer.bottom).toBeLessThanOrEqual(323);
      expect(bounds.sidebar.height).toBe(fullSidebarHeight);
      expect(bounds.sidebarScroll.height).toBeGreaterThan(0);
      await page.locator("#messages").evaluate((n) => {
        n.scrollTop = n.scrollHeight;
      });
      console.log(
        "scroll-clearance",
        await page.locator("#messages").evaluate((n) => ({
          padding: getComputedStyle(n).paddingBottom,
          clearance: n.style.getPropertyValue("--keyboard-scroll-clearance"),
          top: n.scrollTop,
          height: n.clientHeight,
          total: n.scrollHeight,
          last: n.querySelector(".message:last-child")?.getBoundingClientRect()
            .bottom,
        })),
      );
      await expect
        .poll(async () => {
          const b = await page
            .locator("#messages .message")
            .last()
            .boundingBox();
          return b.y + b.height - 346;
        })
        .toBeLessThanOrEqual(bounds.composer.top);
      await page.screenshot({
        path: test.info().outputPath(`keyboard-${count}.png`),
        clip: { x: 0, y: 346, width: 867, height: 323 },
      });
      await expect(page.locator("#message")).toHaveValue("Keep this draft");
      if (count > 1) {
        const list = page.locator("#composer .attachment-list");
        expect(await list.evaluate((n) => n.scrollWidth > n.clientWidth)).toBe(
          true,
        );
        await list.evaluate((n) => {
          n.scrollLeft = n.scrollWidth;
        });
        const last = await list
          .locator(".attachment-chip")
          .last()
          .boundingBox();
        expect(last.x + last.width).toBeLessThanOrEqual(bounds.composer.right);
      }
      await page.evaluate(() =>
        window.resizeDuoViewport({
          height: 669,
          innerHeight: 669,
          offsetTop: 0,
          pageTop: 0,
          scrollY: 0,
        }),
      );
      await expect(page.locator("#root")).toHaveCSS("height", "669px");
      await expect(page.locator(".viewport-bottom-band")).toHaveCSS(
        "height",
        "34px",
      );
      await expect(page.locator(".session-cost-summary")).toBeVisible();
      const frameRecords = [];
      for (const [name, steps] of Object.entries(keyboardSequences)) {
        await page.evaluate((name) => {
          window.keyboardVisualOrigin =
            name === "visual origin" || name === "device final";
          document.querySelector("#root").style.transform =
            name === "transformed root" ? "translateZ(0)" : "";
        }, name);
        const capture = (patch) =>
          page.evaluate((patch) => {
            window.resizeDuoViewport(patch);
            const visibleTop = window.keyboardVisualOrigin
              ? 0
              : window.visualViewport.offsetTop;
            const rect = (selector) => {
              const r = document
                .querySelector(selector)
                .getBoundingClientRect();
              return {
                x: r.x,
                y: r.y - visibleTop,
                width: r.width,
                height: r.height,
              };
            };
            return {
              sidebar: rect("#sidebar"),
              header: rect(".workspace-header"),
              composer: rect("#composer"),
              visibleHeight: window.visualViewport.height,
            };
          }, patch);
        const stable = await capture({});
        for (const [index, patch] of keyboardCycleFrames(steps).entries()) {
          const frame = await capture(patch);
          frameRecords.push({ name, index, patch, frame });
          expect(frame.sidebar, `${name} frame${index} sidebar`).toEqual(
            stable.sidebar,
          );
          expect(frame.header, `${name} frame${index} header`).toEqual(
            stable.header,
          );
          expect(frame.composer.y).toBeGreaterThanOrEqual(0);
          expect(frame.composer.y + frame.composer.height).toBeLessThanOrEqual(
            frame.visibleHeight,
          );
        }
      }
      await page.evaluate(() => {
        window.keyboardVisualOrigin = false;
        document.querySelector("#root").style.transform = "";
      });
      await test.info().attach("actual-app-keyboard-frames", {
        body: JSON.stringify(frameRecords, null, 2),
        contentType: "application/json",
      });
      const header = page.locator(".workspace-header");
      await expect(header.locator("#messages-toggle")).toHaveCount(0);
      await header.locator("#sidebar-toggle").focus();
      await page.keyboard.press("Tab");
      await expect(header.locator(".session-activity-row")).toBeFocused();
      await page.keyboard.press("Tab");
      await expect(
        header.getByRole("button", { name: "Chat actions", exact: true }),
      ).toBeFocused();
      const more = header.getByRole("button", {
        name: "Chat actions",
        exact: true,
      });
      await more.click();
      await expect(
        page.getByRole("menuitem", { name: "Browse prompts", exact: true }),
      ).toBeVisible();
      await page
        .getByRole("menuitem", { name: "Browse prompts", exact: true })
        .click();
      await expect(
        page.getByRole("dialog", { name: "Prompt history" }),
      ).toBeVisible();
      await page.keyboard.press("Escape");
      await page.keyboard.press("Escape");
      await header.locator(".session-activity-row").click();
      await expect(
        page.getByRole("dialog", { name: /^Current activity/ }),
      ).toBeVisible();
      await page.keyboard.press("Escape");
      await expect(
        page.getByRole("dialog", { name: /^Current activity/ }),
      ).toBeHidden();
      fixture.local.snapshot.runtime.requests = pending.map((request) => ({
        ...request,
        agent: "overlap",
      }));
      fixture.local.complete("attention");
      await expect(
        header.locator("#messages-toggle .attention-count"),
      ).toHaveText("1");
      await header.locator(".session-activity-row").focus();
      await page.keyboard.press("Tab");
      await expect(header.locator("#messages-toggle")).toBeFocused();
      await page.keyboard.press("Tab");
      await expect(more).toBeFocused();
      fixture.local.snapshot.runtime.requests = [];
      fixture.local.complete("attention");
      await expect(header.locator("#messages-toggle")).toHaveCount(0);
      await page.setViewportSize({ width: 951, height: 585 });
      await page.evaluate(() =>
        window.resizeDuoViewport({ width: 951, height: 585, innerHeight: 585 }),
      );
      const overflow = await page.evaluate(
        () =>
          document.documentElement.scrollWidth -
          document.documentElement.clientWidth,
      );
      expect(overflow).toBeLessThanOrEqual(1);
      for (const target of await header.getByRole("button").all()) {
        if (!(await target.isVisible())) continue;
        const b = await target.boundingBox();
        expect(b.x).toBeGreaterThanOrEqual(0);
        expect(b.x + b.width).toBeLessThanOrEqual(951);
      }
      for (const size of [
        { width: 1200, height: 800 },
        { width: 390, height: 844 },
      ]) {
        await page.setViewportSize(size);
        await page.evaluate(
          (size) =>
            window.resizeDuoViewport({ ...size, innerHeight: size.height }),
          size,
        );
        await expect(header).not.toHaveAttribute(
          "data-compact-landscape",
          "true",
        );
        await expect(
          page.locator(".workspace > .session-activity"),
        ).toBeVisible();
        await expect(header.locator("#chat-settings-toggle")).toHaveCount(1);
      }
    } finally {
      await fixture.close();
    }
  });
}
