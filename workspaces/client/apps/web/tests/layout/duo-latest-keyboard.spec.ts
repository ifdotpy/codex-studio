import { duoPhoneUserAgent } from "./duo-fixture.ts";
import { writeFileSync } from "node:fs";
import { test, expect, entityPullFixtureForRequest } from "../playwright.ts";
import { sidebarParityFixture } from "../sidebar/sidebar-parity-fixture.ts";
import { fixtureWorkspaceId } from "../servers/multi-server-fixture.ts";
import "./keyboardSequences.ts";

test.use({
  userAgent: duoPhoneUserAgent,
  hasTouch: true,
  serviceWorkers: "block",
  reducedMotion: "no-preference",
});
test("Latest shares the composer motion while the reader stays above the last message", async ({
  page,
  context,
}) => {
  const fixture = await sidebarParityFixture(context, { paired: false });
  fixture.local.snapshot.runtime.requests = [];
  fixture.local.snapshot.runtime.complaints = [];
  await page.route(
    (url) => url.origin === fixture.local.origin && url.pathname === "/",
    async (route) => {
      const response = await route.fetch();
      const headers = response.headers();
      headers["content-security-policy"] = headers[
        "content-security-policy"
      ].replace("script-src 'self'", "script-src 'self' 'wasm-unsafe-eval'");
      await route.fulfill({ response, headers });
    },
  );
  await context.addInitScript(() => {
    const viewport = Object.assign(new EventTarget(), {
      width: 867,
      height: 669,
      offsetTop: 0,
      pageTop: 0,
      scale: 1,
    });
    Object.defineProperty(window, "visualViewport", {
      configurable: true,
      value: viewport,
    });
    window.resizeDuoViewport = (patch) => {
      Object.assign(viewport, patch);
      viewport.dispatchEvent(new Event("resize"));
    };
  });
  await page.route("**/api/sync/pull?*", (route) => {
    const url = route.request().url();
    if (!new URL(url).searchParams.get("scope")?.startsWith("transcript:"))
      return route.fallback();
    return route.fulfill({
      json: {
        workspaceId: fixtureWorkspaceId("Local"),
        ...entityPullFixtureForRequest(fixture.local.snapshot, url, {
          transcript: {
            agent: { id: "overlap", status: "completed", inFlight: false },
            items: Array.from({ length: 20 }, (_, i) => ({
              id: `line-${i}`,
              role: "assistant",
              text: `Conversation ${i}. ` + "Readable content. ".repeat(20),
            })),
            truncated: false,
            nextCursor: null,
            nextAfterCursor: null,
            historyVersion: "duo-latest",
          },
        }),
      },
    });
  });
  const cdp = await context.newCDPSession(page);
  await page.setViewportSize({ width: 1200, height: 669 });
  try {
    const sidebar = await fixture.open(page);
    await sidebar
      .getByRole("button")
      .filter({ hasText: "Local Saved pin" })
      .filter({ has: page.locator("strong") })
      .first()
      .click();
    await cdp.send("Emulation.setSafeAreaInsetsOverride", {
      insets: { top: 0, right: 0, bottom: 34, left: 0 },
    });
    await page.setViewportSize({ width: 867, height: 669 });
    await expect(page.locator("#messages .message")).toHaveCount(20);
    await expect(page.locator("#jump-latest")).toHaveCount(0);
    await page.locator("#messages").hover();
    await page.mouse.wheel(0, -600);
    await expect(page.locator("#jump-latest")).toBeVisible();
    await page.locator("#message").fill("Keep the reader position");
    const baselineGap = await page.evaluate(
      () =>
        document.querySelector("#composer")!.getBoundingClientRect().top -
        document.querySelector("#jump-latest")!.getBoundingClientRect().bottom,
    );
    const expectedGap = await page
      .locator("#composer")
      .evaluate((node) =>
        parseFloat(
          getComputedStyle(node).getPropertyValue("--layout-duo-latest-gap"),
        ),
      );
    expect(baselineGap).toBeCloseTo(expectedGap, 1);
    await expect(
      page.locator("#composer > .jump-slot #jump-latest"),
    ).toBeVisible();
    const frames = await page.evaluate(async () => {
      const composer = document.querySelector<HTMLElement>("#composer")!;
      const frame = (phase: string, ms: number) => {
        const latest = document.querySelector<HTMLElement>("#jump-latest");
        const rect = latest?.getBoundingClientRect();
        const form = composer.getBoundingClientRect();
        return {
          phase,
          ms,
          composerTop: form.top,
          composerBottom: form.bottom,
          latestBottom: rect?.bottom,
          latestTop: rect?.top,
          visible:
            !!latest &&
            getComputedStyle(latest).visibility === "visible" &&
            !!latest.getClientRects().length,
        };
      };
      const frames = [];
      for (const phase of ["open", "close"]) {
        if (phase === "open") window.resizeDuoViewport({ height: 337 });
        else {
          document.querySelector<HTMLTextAreaElement>("#message")!.blur();
          window.resizeDuoViewport({ height: 669 });
        }
        document.body.getBoundingClientRect();
        const animations = composer.getAnimations();
        animations.forEach((animation) => animation.pause());
        for (let ms = 0; ms <= 400; ms += 16) {
          animations.forEach((animation) => {
            animation.currentTime = ms;
          });
          await new Promise(requestAnimationFrame);
          frames.push(frame(phase, ms));
        }
        animations.forEach((animation) => animation.finish());
        await new Promise(requestAnimationFrame);
        frames.push(frame(phase, 400));
      }
      return frames;
    });
    writeFileSync(
      test.info().outputPath("latest-frames.json"),
      JSON.stringify({ baselineGap, frames }, null, 2),
    );
    for (const frame of frames) {
      expect(frame.visible, `${frame.phase} ${frame.ms}`).toBe(true);
      expect(frame.latestBottom).toBeDefined();
      expect(frame.composerTop - frame.latestBottom!).toBeCloseTo(
        baselineGap,
        1,
      );
      expect(frame.latestTop).toBeGreaterThanOrEqual(0);
    }
    expect(
      frames.findLast((frame) => frame.phase === "open")!.composerBottom,
    ).toBeCloseTo(337, 0);
    await page.locator("#jump-latest").click();
    await expect(page.locator("#jump-latest")).toHaveCount(0);
  } finally {
    await cdp.detach();
    await fixture.close();
  }
});
