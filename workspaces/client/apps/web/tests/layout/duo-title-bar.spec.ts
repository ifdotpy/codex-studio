import { writeFileSync } from "node:fs";
import { test, expect, entityPullFixtureForRequest } from "../playwright.ts";
import { sidebarParityFixture } from "../sidebar/sidebar-parity-fixture.ts";
import { fixtureWorkspaceId } from "../servers/multi-server-fixture.ts";

test.use({
  hasTouch: true,
  serviceWorkers: "block",
  reducedMotion: "no-preference",
});
test("Duo rail title floats above the transcript at scroll top and mid-scroll", async ({
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
      .getByRole("button", { name: /^Local Saved pin\b/ })
      .first()
      .click();
    await cdp.send("Emulation.setSafeAreaInsetsOverride", {
      insets: { top: 0, right: 84, bottom: 34, left: 0 },
    });
    await page.setViewportSize({ width: 951, height: 669 });
    await expect(page.locator("#messages .message")).toHaveCount(20);
    const heading = page.locator(".duo-transcript-bar");
    await expect(heading).toBeVisible();
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
    expect(geometry.position).toBe("absolute");
    expect(geometry.backdrop).not.toBe("none");
    expect(geometry.bar.top).toBeCloseTo(geometry.transcript.top, 0);
    expect(geometry.firstTop).toBeGreaterThanOrEqual(geometry.bar.bottom);
    await page.screenshot({
      path: test.info().outputPath("duo-title-top.png"),
    });
    await page.locator("#messages").hover();
    await page.mouse.wheel(0, 600);
    await expect
      .poll(() => page.locator("#messages").evaluate((node) => node.scrollTop))
      .toBeGreaterThan(400);
    const after = await heading.boundingBox();
    expect(after?.y).toBeCloseTo(geometry.bar.top, 0);
    await page.screenshot({
      path: test.info().outputPath("duo-title-middle.png"),
    });
    writeFileSync(
      test.info().outputPath("title-geometry.json"),
      JSON.stringify({ geometry, after }, null, 2),
    );
  } finally {
    await cdp.detach();
    await fixture.close();
  }
});
