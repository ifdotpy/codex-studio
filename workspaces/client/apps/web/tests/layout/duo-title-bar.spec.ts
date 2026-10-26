import { writeFileSync } from "node:fs";
import { test, expect, entityPullFixtureForRequest } from "../playwright.ts";
import { sidebarParityFixture } from "../sidebar/sidebar-parity-fixture.ts";
import { fixtureWorkspaceId } from "../servers/multi-server-fixture.ts";

test.use({
  hasTouch: true,
  serviceWorkers: "block",
  reducedMotion: "no-preference",
});
for (const size of [
  { width: 951, height: 669 },
  { width: 669, height: 951 },
]) {
  test(`Duo ${size.width}x${size.height} rail title floats above the transcript at scroll top and mid-scroll`, async ({
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
        insets: { top: 0, right: 84, bottom: 34, left: 0 },
      });
      await page.setViewportSize(size);
      if (size.height > size.width) {
        // Stand-in for the iOS shell DOM until PR #528 merges.
        // The shell owns the top strip attribute, rail and native safe inset.
        await page.locator(".edge-rail").evaluate((rail) => {
          document.documentElement.setAttribute("data-viewport-top-strip", "");
          document.documentElement.style.setProperty(
            "--native-safe-top",
            "82px",
          );
          rail.setAttribute("data-side", "top");
          Object.assign((rail as HTMLElement).style, {
            position: "fixed",
            left: "0px",
            right: "auto",
            top: "0px",
            width: "545px",
            height: "82px",
            flexDirection: "row",
            justifyContent: "flex-end",
            alignItems: "flex-end",
            paddingBottom: "10px",
            gap: "8px",
          });
        });
      }
      await expect(page.locator("#messages .message")).toHaveCount(20);
      const heading = page.locator(".duo-transcript-bar");
      await expect(heading).toBeVisible();
      if (size.height > size.width) {
        await expect
          .poll(async () => (await heading.boundingBox())?.width)
          .toBe(size.width);
        await expect(heading).toHaveCSS("height", "82px");
        const textRight = await heading
          .locator(".conversation-heading")
          .evaluate((node) => node.getBoundingClientRect().right);
        const firstRailButton = (await page
          .locator(".edge-rail button")
          .first()
          .boundingBox())!;
        expect(textRight).toBeLessThan(firstRailButton.x);
      }
      await expect(heading.locator("#conversation-title")).toContainText(
        "Local Saved pin",
      );
      await expect(heading.locator(".conversation-project")).toContainText(
        "Home project",
      );
      await expect(heading.locator("button")).toHaveCount(0);
      const geometry = await page.evaluate(() => {
        const messages = document.querySelector<HTMLElement>("#messages")!;
        messages.scrollTop = 0;
        messages.dispatchEvent(new Event("scroll"));
        const bar = document.querySelector<HTMLElement>(".duo-transcript-bar")!;
        const style = getComputedStyle(bar);
        return {
          bar: bar.getBoundingClientRect().toJSON(),
          transcript: messages.getBoundingClientRect().toJSON(),
          firstTop: messages.querySelector(".message")!.getBoundingClientRect()
            .top,
          position: style.position,
          backdrop: style.backdropFilter,
          background: style.backgroundColor,
        };
      });
      expect(geometry.position).toBe(
        size.height > size.width ? "fixed" : "absolute",
      );
      expect(geometry.backdrop).not.toBe("none");
      expect(geometry.bar.top).toBeCloseTo(geometry.transcript.top, 0);
      expect(geometry.firstTop).toBeGreaterThanOrEqual(geometry.bar.bottom);
      await page.screenshot({
        path: test.info().outputPath("duo-title-top.png"),
      });
      await page.locator("#messages").hover();
      await page.mouse.wheel(0, 600);
      await expect
        .poll(() =>
          page.locator("#messages").evaluate((node) => node.scrollTop),
        )
        .toBeGreaterThan(400);
      const after = await heading.boundingBox();
      expect(after?.y).toBeCloseTo(geometry.bar.top, 0);
      await page.screenshot({
        path: test.info().outputPath("duo-title-middle.png"),
      });
      const keyboardFrameReads = await heading.evaluate(async (node) => {
        const original = node.getBoundingClientRect;
        let reads = 0;
        node.getBoundingClientRect = () => {
          reads++;
          return original.call(node);
        };
        try {
          for (let frame = 0; frame < 8; frame++) {
            document.documentElement.style.setProperty(
              "--fixture-keyboard-frame",
              String(frame),
            );
            await new Promise<void>((resolve) =>
              requestAnimationFrame(() => resolve()),
            );
          }
          return reads;
        } finally {
          node.getBoundingClientRect = original;
          document.documentElement.style.removeProperty(
            "--fixture-keyboard-frame",
          );
        }
      });
      expect(keyboardFrameReads).toBeLessThanOrEqual(2);
      writeFileSync(
        test.info().outputPath("title-geometry.json"),
        JSON.stringify({ geometry, after }, null, 2),
      );
    } finally {
      await cdp.detach();
      await fixture.close();
    }
  });
}
