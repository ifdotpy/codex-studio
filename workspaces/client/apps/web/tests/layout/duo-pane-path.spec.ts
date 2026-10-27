import { duoPhoneUserAgent, openDuoTeam } from "./duo-fixture.ts";
import type { Page, BrowserContext } from "playwright";
import "./keyboardSequences.ts";
import { test, expect, entityPullFixtureForRequest } from "../playwright.ts";
import { sidebarParityFixture } from "../sidebar/sidebar-parity-fixture.ts";
import { fixtureWorkspaceId } from "../servers/multi-server-fixture.ts";

test.use({
  userAgent: duoPhoneUserAgent,
  hasTouch: true,
  serviceWorkers: "block",
  reducedMotion: "no-preference",
});
async function openPaneFixture(page: Page, context: BrowserContext) {
  const fixture = await sidebarParityFixture(context, { paired: false });
  fixture.local.snapshot.threads.push(
    ...Array.from({ length: 4 }, (_, i) => ({
      ...fixture.local.snapshot.threads[0]!,
      id: `pane-worker-${i}`,
      rootId: "overlap",
      isLead: false,
      name: `Pane worker ${i}`,
      status: "running" as const,
      inFlight: true,
    })),
  );
  fixture.local.snapshot.runtime.requests = [];
  fixture.local.snapshot.runtime.complaints = [];
  await page.route(
    (url) => url.origin === fixture.local.origin && url.pathname === "/",
    async (route) => {
      const response = await route.fetch();
      const headers = response.headers();
      const policy = headers["content-security-policy"];
      if (!policy)
        throw new Error("Fixture response has no Content-Security-Policy");
      headers["content-security-policy"] = policy.replace(
        "script-src 'self'",
        "script-src 'self' 'wasm-unsafe-eval'",
      );
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
  await page.setViewportSize({ width: 951, height: 669 });
  await expect(page.locator("#messages .message")).toHaveCount(20);
  return { fixture, cdp };
}
test("Duo pane path keeps Team, conversation and the hinge region in one order", async ({
  page,
  context,
}) => {
  const { fixture, cdp } = await openPaneFixture(page, context);
  try {
    await page.locator("#message").fill("Keep this draft across panes");
    const bounds = async (selector: string) =>
      page.locator(selector).boundingBox();
    const before = await bounds("#sidebar");
    expect(before?.width).toBeCloseTo(316.8, 1);
    await page.evaluate(() => {
      document.documentElement.dataset.nativeHinge = "partial";
      document.documentElement.style.setProperty(
        "--native-hinge-left",
        "455.5px",
      );
      document.documentElement.style.setProperty(
        "--native-hinge-width",
        "40px",
      );
      window.dispatchEvent(new Event("studio-native-viewport"));
    });
    await expect
      .poll(async () => (await bounds("#sidebar"))?.width)
      .toBeCloseTo(455.5, 1);
    expect((await bounds(".workspace"))?.x).toBeCloseTo(495.5, 1);
    await openDuoTeam(page);
    const panel = page.locator(".workspace-panel-inline.duo-pane-panel");
    await expect(panel).toBeVisible();
    await expect(page.locator(".mantine-Drawer-overlay:visible")).toHaveCount(
      0,
    );
    await expect(page.locator("#sidebar")).toBeHidden();
    await expect
      .poll(async () => (await bounds(".workspace"))?.width)
      .toBeCloseTo(455.5, 1);
    await expect
      .poll(async () => (await panel.boundingBox())?.x)
      .toBeCloseTo(495.5, 1);
    await expect
      .poll(async () => (await panel.boundingBox())?.width)
      .toBeCloseTo(371.5, 1);
    await expect(panel.locator("#team")).toBeVisible();
    expect((await panel.locator("#team").boundingBox())?.width).toBeCloseTo(
      await panel.evaluate((node) => {
        const style = getComputedStyle(node);
        return (
          node.getBoundingClientRect().width -
          parseFloat(style.borderLeftWidth) -
          parseFloat(style.borderRightWidth)
        );
      }),
      0,
    );
    await expect(
      panel.getByRole("button", { name: "Full screen", exact: true }),
    ).toHaveCount(0);
    await panel
      .getByRole("searchbox", { name: "Find a subagent" })
      .fill("Pane");
    await page
      .getByRole("button", { name: /conversations/i })
      .first()
      .click();
    await expect(page.locator("#sidebar")).toBeVisible();
    await expect(panel).toBeHidden();
    await page
      .getByRole("button", { name: /conversations/i })
      .first()
      .click();
    await expect(panel).toBeVisible();
    await expect(
      panel.getByRole("searchbox", { name: "Find a subagent" }),
    ).toHaveValue("Pane");
    await page.evaluate(() => {
      document.documentElement.dataset.nativeHinge = "open";
      window.dispatchEvent(new Event("studio-native-viewport"));
    });
    await expect
      .poll(async () => (await panel.boundingBox())?.width)
      .toBeCloseTo(433.5, 1);
    await panel
      .getByRole("button", { name: "Full screen", exact: true })
      .click();
    await expect(page.locator(".workspace")).toBeHidden();
    await expect
      .poll(async () => (await panel.boundingBox())?.width)
      .toBeCloseTo(867, 1);
    await page.evaluate(() => {
      document.documentElement.dataset.nativeHinge = "partial";
      window.dispatchEvent(new Event("studio-native-viewport"));
    });
    await expect(page.locator(".workspace")).toBeVisible();
    await expect(
      panel.getByRole("button", { name: "Restore split view", exact: true }),
    ).toHaveCount(0);
    await expect
      .poll(async () => (await panel.boundingBox())?.x)
      .toBeCloseTo(495.5, 1);
    await page.evaluate(() => {
      document.documentElement.dataset.nativeHinge = "open";
      window.dispatchEvent(new Event("studio-native-viewport"));
    });
    await expect(page.locator(".workspace")).toBeHidden();
    await expect
      .poll(async () => (await panel.boundingBox())?.width)
      .toBeCloseTo(867, 1);
    await panel
      .getByRole("button", { name: "Restore split view", exact: true })
      .click();
    await expect(page.locator(".workspace")).toBeVisible();
    await panel
      .getByRole("button", { name: "Close side panel", exact: true })
      .click();
    await expect(page.locator("#sidebar")).toBeVisible();
    await expect(panel).toBeHidden();
    await expect(page.locator("#message")).toHaveValue(
      "Keep this draft across panes",
    );
    await page.screenshot({
      path: test.info().outputPath("pane-path-return.png"),
    });
  } finally {
    await cdp.detach().catch(() => {});
    await fixture.close();
  }
});

test("Team opened on a narrow PWA stays mounted when the window becomes wide", async ({
  page,
  context,
}) => {
  const { fixture, cdp } = await openPaneFixture(page, context);
  try {
    await cdp.send("Emulation.setSafeAreaInsetsOverride", {
      insets: { top: 0, right: 0, bottom: 34, left: 0 },
    });
    await page.setViewportSize({ width: 390, height: 844 });
    await openDuoTeam(page);
    await expect(
      page.getByRole("dialog", { name: "Team", exact: true }),
    ).toBeVisible();
    await page
      .getByRole("dialog", { name: "Team", exact: true })
      .evaluate(async (node) => {
        await Promise.all(
          node.getAnimations().map((animation) => animation.finished),
        );
      });
    await page.screenshot({ path: test.info().outputPath("phone-team.png") });
    const search = page.getByRole("searchbox", { name: "Find a subagent" });
    await search.fill("Pane");
    await expect(search).toBeFocused();
    const retained = await search.elementHandle();
    await cdp.send("Emulation.setSafeAreaInsetsOverride", {
      insets: { top: 0, right: 84, bottom: 34, left: 0 },
    });
    await page.setViewportSize({ width: 951, height: 669 });
    await expect(
      page.locator(".workspace-panel-inline.duo-pane-panel"),
    ).toBeVisible();
    await expect(page.locator(".duo-pane-panel #worker-search")).toHaveValue(
      "Pane",
    );
    expect(await retained!.evaluate((node) => node.isConnected)).toBe(true);
    await expect(search).toBeFocused();
    await page.screenshot({
      path: test.info().outputPath("team-narrow-to-wide.png"),
    });
  } finally {
    await cdp.detach().catch(() => {});
    await fixture.close();
  }
});

test("Workspace uses the same portrait pair and native closed journey", async ({
  page,
  context,
}) => {
  const { fixture, cdp } = await openPaneFixture(page, context);
  try {
    await page.emulateMedia({ reducedMotion: "reduce" });
    await cdp.send("Emulation.setSafeAreaInsetsOverride", {
      insets: { top: 0, right: 0, bottom: 34, left: 0 },
    });
    await page.setViewportSize({ width: 669, height: 951 });
    await expect
      .poll(async () => (await page.locator("#sidebar").boundingBox())?.width)
      .toBeCloseTo(250, 1);
    await expect
      .poll(async () => (await page.locator(".workspace").boundingBox())?.width)
      .toBeCloseTo(419, 1);
    await expect(page.locator(".workspace")).toHaveCSS(
      "transition-duration",
      "0s",
    );
    await page.screenshot({
      path: test.info().outputPath("portrait-sidebar-conversation.png"),
    });
    await page
      .getByRole("button", { name: "Chat actions", exact: true })
      .click();
    await page.getByRole("menuitem", { name: "Plan", exact: true }).click();
    const panel = page.locator(".duo-pane-panel:visible");
    await expect(panel).toBeVisible();
    await expect
      .poll(async () => (await panel.boundingBox())?.width)
      .toBeCloseTo(334.5, 1);
    await expect(page.locator(".workspace")).toBeVisible();
    await page.screenshot({
      path: test.info().outputPath("portrait-conversation-plan.png"),
    });
    await page.locator("#messages").evaluate((node) => {
      node.dispatchEvent(new WheelEvent("wheel", { deltaY: -10000 }));
      node.scrollTop = 160;
      node.dispatchEvent(new Event("scroll"));
    });
    const before = await page
      .locator("#messages")
      .evaluate((node) => node.scrollTop);
    await page.evaluate(() => {
      document.documentElement.dataset.nativeHinge = "closed";
      window.dispatchEvent(new Event("studio-native-viewport"));
    });
    await page.setViewportSize({ width: 390, height: 844 });
    await expect(page.locator(".workspace")).toBeHidden();
    await expect
      .poll(async () => (await panel.boundingBox())?.width)
      .toBeCloseTo(390, 1);
    await page.evaluate(() => {
      document.documentElement.dataset.nativeHinge = "open";
      window.dispatchEvent(new Event("studio-native-viewport"));
    });
    await page.setViewportSize({ width: 669, height: 951 });
    await expect(page.locator(".workspace")).toBeVisible();
    await expect
      .poll(async () => (await panel.boundingBox())?.width)
      .toBeCloseTo(334.5, 1);
    expect(
      await page.locator("#messages").evaluate((node) => node.scrollTop),
    ).toBeCloseTo(before, 0);
    await panel
      .getByRole("button", { name: "Close side panel", exact: true })
      .click();
    await expect(page.locator("#sidebar")).toBeVisible();
  } finally {
    await cdp.detach().catch(() => {});
    await fixture.close();
  }
});

test("Terminals uses the right pane and leaves no bottom dock over the composer", async ({
  page,
  context,
}) => {
  const { fixture, cdp } = await openPaneFixture(page, context);
  try {
    await expect(page.locator(".terminal-dock")).toBeHidden();
    await page
      .getByRole("button", { name: "Chat actions", exact: true })
      .click();
    await page
      .getByRole("menuitem", { name: "Terminals", exact: true })
      .click();
    const panel = page.locator(".terminal-dock.duo-pane-panel");
    await expect(panel).toBeVisible();
    await expect
      .poll(async () => (await panel.boundingBox())?.x)
      .toBeCloseTo(433.5, 1);
    await expect
      .poll(async () => (await panel.boundingBox())?.width)
      .toBeCloseTo(433.5, 1);
    await expect(page.locator(".workspace")).toBeVisible();
    await expect(page.locator("#sidebar")).toBeHidden();
    await panel
      .getByRole("button", { name: "Close side panel", exact: true })
      .click();
    await expect(panel).toBeHidden();
    await expect(page.locator("#sidebar")).toBeVisible();
  } finally {
    await cdp.detach().catch(() => {});
    await fixture.close();
  }
});

test("A Team panel closed outside the Duo path stays closed on the Duo", async ({
  page,
  context,
}) => {
  const { fixture, cdp } = await openPaneFixture(page, context);
  try {
    await cdp.send("Emulation.setSafeAreaInsetsOverride", {
      insets: { top: 0, right: 0, bottom: 0, left: 0 },
    });
    await page.setViewportSize({ width: 1440, height: 900 });
    await page.getByRole("button", { name: "Team", exact: true }).click();
    const team = page.getByRole("complementary", { name: "Team", exact: true });
    await expect(team).toBeVisible();
    await page.screenshot({ path: test.info().outputPath("desktop-team.png") });
    await team.getByRole("button", { name: "Close team", exact: true }).click();
    await expect(team).toBeHidden();
    await page.setViewportSize({ width: 951, height: 669 });
    await expect(page.locator("#sidebar")).toBeVisible();
    await expect(team).toBeHidden();
  } finally {
    await cdp.detach().catch(() => {});
    await fixture.close();
  }
});

test("A side panel enters with a horizontal slide", async ({
  page,
  context,
}) => {
  const { fixture, cdp } = await openPaneFixture(page, context);
  try {
    await page.evaluate(() => {
      const observer = new MutationObserver(() => {
        const panel = document.querySelector<HTMLElement>(
          '.workspace-panel-inline[aria-label="Team panel"]',
        );
        if (!panel || panel.hidden) return;
        const slide = panel
          .getAnimations()
          .find(
            (animation) =>
              animation instanceof CSSTransition &&
              animation.transitionProperty === "translate",
          );
        document.documentElement.dataset.paneEntry = slide
          ? "slide"
          : "missing";
        if (slide) {
          slide.pause();
          slide.currentTime = 125;
        }
        observer.disconnect();
      });
      observer.observe(document.body, {
        attributes: true,
        childList: true,
        subtree: true,
      });
    });
    await openDuoTeam(page);
    await expect(page.locator("html")).toHaveAttribute(
      "data-pane-entry",
      "slide",
    );
    const panel = page.locator(
      '.workspace-panel-inline[aria-label="Team panel"]',
    );
    expect((await panel.boundingBox())!.x).toBeGreaterThan(433.5);
    await panel.evaluate((node) =>
      node.getAnimations().forEach((animation) => animation.finish()),
    );
    await expect
      .poll(async () => (await panel.boundingBox())?.x)
      .toBeCloseTo(433.5, 1);
  } finally {
    await cdp.detach().catch(() => {});
    await fixture.close();
  }
});

test("Picking a chat from the native closed sidebar returns to the conversation", async ({
  page,
  context,
}) => {
  const { fixture, cdp } = await openPaneFixture(page, context);
  try {
    await page.locator("#message").fill("Keep the closed-display draft");
    await cdp.send("Emulation.setSafeAreaInsetsOverride", {
      insets: { top: 0, right: 0, bottom: 34, left: 0 },
    });
    await page.evaluate(() => {
      document.documentElement.dataset.nativeHinge = "closed";
      window.dispatchEvent(new Event("studio-native-viewport"));
    });
    await page.setViewportSize({ width: 390, height: 844 });
    await expect(page.locator(".workspace")).toBeVisible();
    const toggle = page.locator(".workspace #sidebar-toggle");
    await expect(toggle).toHaveAttribute("aria-expanded", "false");
    await toggle.click();
    await expect(toggle).toHaveAttribute("aria-expanded", "true");
    await expect(page.locator("#sidebar")).toBeVisible();
    await expect(page.locator(".workspace")).toBeHidden();
    await page
      .locator("#sidebar")
      .getByRole("button")
      .filter({ hasText: "Local Saved pin" })
      .filter({ has: page.locator("strong") })
      .first()
      .click();
    await expect(page.locator("#sidebar")).toBeHidden();
    await expect(toggle).toHaveAttribute("aria-expanded", "false");
    await expect(page.locator(".workspace")).toBeVisible();
    await expect(page.locator("#message")).toHaveValue(
      "Keep the closed-display draft",
    );
    await page.screenshot({
      path: test.info().outputPath("closed-conversation.png"),
    });
  } finally {
    await cdp.detach().catch(() => {});
    await fixture.close();
  }
});

test("Portrait PWA keeps its pane pair while the keyboard reduces the visible height", async ({
  page,
  context,
}) => {
  await context.addInitScript(() => {
    const viewport = Object.assign(new EventTarget(), {
      height: 669,
      offsetTop: 0,
      pageTop: 0,
      scale: 1,
    });
    Object.defineProperty(viewport, "width", { get: () => window.innerWidth });
    Object.defineProperty(window, "visualViewport", {
      configurable: true,
      value: viewport,
    });
    window.resizeDuoViewport = (patch) => {
      Object.assign(viewport, patch);
      viewport.dispatchEvent(new Event("resize"));
    };
  });
  const { fixture, cdp } = await openPaneFixture(page, context);
  try {
    await cdp.send("Emulation.setSafeAreaInsetsOverride", {
      insets: { top: 0, right: 0, bottom: 34, left: 0 },
    });
    await page.setViewportSize({ width: 669, height: 951 });
    await page.evaluate(() => window.resizeDuoViewport({ height: 951 }));
    const frame = page.locator(".workspace-frame");
    await expect(frame).toHaveAttribute("data-pane-path", "pair");
    await expect
      .poll(async () => (await page.locator("#sidebar").boundingBox())?.width)
      .toBeCloseTo(250, 1);
    await page.locator("#messages").evaluate((node) => {
      node.dispatchEvent(new WheelEvent("wheel", { deltaY: -10000 }));
      node.scrollTop = 160;
      node.dispatchEvent(new Event("scroll"));
    });
    await expect(page.locator("#jump-latest")).toBeVisible();
    await page.locator("#message").fill("Keep the portrait draft");
    await page.evaluate(() => window.resizeDuoViewport({ height: 500 }));
    await expect(page.locator("html")).toHaveAttribute(
      "data-viewport-keyboard",
      "",
    );
    await expect(frame).toHaveAttribute("data-pane-path", "pair");
    await expect
      .poll(async () => (await page.locator("#sidebar").boundingBox())?.width)
      .toBeCloseTo(250, 1);
    await expect(page.locator("#message")).toHaveValue(
      "Keep the portrait draft",
    );
    await page.screenshot({
      path: test.info().outputPath("portrait-keyboard.png"),
    });
    await page.locator("#message").blur();
    await page.evaluate(() => window.resizeDuoViewport({ height: 951 }));
    await expect(frame).toHaveAttribute("data-pane-path", "pair");
  } finally {
    await cdp.detach().catch(() => {});
    await fixture.close();
  }
});

for (const size of [
  { name: "open", width: 951, height: 669, hinge: "open" },
  { name: "portrait", width: 669, height: 951, hinge: "open" },
  { name: "closed", width: 653, height: 951, hinge: "closed" },
]) {
  test(`Duo ${size.name} pane backgrounds reach the physical bottom`, async ({
    page,
    context,
  }) => {
    const { fixture, cdp } = await openPaneFixture(page, context);
    try {
      await page.evaluate((hinge) => {
        document.documentElement.dataset.nativeHinge = hinge;
        window.dispatchEvent(new Event("studio-native-viewport"));
      }, size.hinge);
      if (size.height > size.width) {
        await cdp.send("Emulation.setSafeAreaInsetsOverride", {
          insets: { top: 0, right: 0, bottom: 34, left: 0 },
        });
      }
      await page.setViewportSize({ width: size.width, height: size.height });
      const sidebar = page.locator("#sidebar");
      if (!(await sidebar.isVisible()))
        await page.locator("#sidebar-toggle:visible").first().click();
      await expect(sidebar).toBeVisible();
      const background = async (selector: string, contentSelector: string) => {
        const node = page.locator(selector);
        const result = await node.evaluate(async (element) => {
          element.getAnimations().forEach((animation) => animation.finish());
          await new Promise<void>((resolve) =>
            requestAnimationFrame(() => resolve()),
          );
          const rect = element.getBoundingClientRect();
          const hit = document.elementFromPoint(
            rect.x + rect.width / 2,
            window.innerHeight - 2,
          );
          return {
            bottom: rect.bottom,
            height: window.innerHeight,
            ownsBottom: !!hit && element.contains(hit),
          };
        });
        expect.soft(result.bottom).toBeCloseTo(result.height, 1);
        expect.soft(result.ownsBottom).toBe(true);
        await expect(page.locator(contentSelector)).toHaveCSS(
          "padding-bottom",
          "34px",
        );
      };
      await expect(page.locator(".viewport-bottom-band")).toHaveCSS(
        "background-image",
        "none",
      );
      await expect(page.locator(".viewport-bottom-band")).toHaveCSS(
        "background-color",
        "rgba(0, 0, 0, 0)",
      );
      await background("#sidebar", "#sidebar");
      await page.screenshot({
        path: test.info().outputPath(`${size.name}-sidebar-bottom.png`),
      });
      if (size.hinge === "closed") {
        await sidebar
          .getByRole("button")
          .filter({ hasText: "Local Saved pin" })
          .filter({ has: page.locator("strong") })
          .first()
          .click();
        await expect(sidebar).toBeHidden();
      }
      await openDuoTeam(page);
      const panel = page.locator(
        '.workspace-panel-inline.duo-pane-panel[aria-label="Team panel"]',
      );
      await expect(panel).toBeVisible();
      await panel.evaluate((node) =>
        node.getAnimations().forEach((animation) => animation.finish()),
      );
      await background(
        '.workspace-panel-inline.duo-pane-panel[aria-label="Team panel"]',
        ".duo-pane-body",
      );
      await page.screenshot({
        path: test.info().outputPath(`${size.name}-team-bottom.png`),
      });
    } finally {
      await cdp.detach().catch(() => {});
      await fixture.close();
    }
  });
}
