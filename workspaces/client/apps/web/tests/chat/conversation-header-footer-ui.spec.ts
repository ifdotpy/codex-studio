import {
  test,
  expect,
  readTestState,
  readFixtureSyncContract,
  stubEntityState,
} from "../playwright.ts";

test("conversation header stays compact and usage facts remain visible", async ({
  browser,
  readOnlyRust,
}, testInfo) => {
  test.setTimeout(90_000);
  const serverAlias = "MAC";
  const snapshot = await readTestState(readOnlyRust.origin);
  const contract = await readFixtureSyncContract(readOnlyRust.origin);
  const lead = snapshot.threads.find((agent) => agent.name === "Release lead");
  snapshot.runtime.sessionCostsByAgent = {
    [lead.id]: {
      rootId: lead.id,
      totalUSD: 0,
      pricingState: "ready",
      cacheAgeSeconds: 0,
      breakdown: { providers: {}, models: {} },
      unknownModels: [],
    },
  };
  for (const width of [1440, 1024, 390, 320])
    for (const scheme of ["light", "dark"]) {
      const page = await browser.newPage({
        viewport: { width, height: 900 },
        isMobile: width < 760,
        hasTouch: width < 760,
        colorScheme: scheme,
      });
      try {
        await page.addInitScript((alias) => {
          localStorage.setItem("studio-local-server-alias-v1", alias);
        }, serverAlias);
        await stubEntityState(page, structuredClone(snapshot), contract);
        await page.route("**/api/sync/pull**", (route) =>
          new URL(route.request().url()).searchParams.get("scope") ===
          "state:entities:v1"
            ? route.fallback()
            : route.continue(),
        );
        await readOnlyRust.open(page);
        await page
          .getByLabel(
            page.viewportSize().width <= 760
              ? "Back to chats"
              : "Toggle conversations",
          )
          .waitFor();
        if (!(await page.locator("#sidebar").isVisible()))
          await page
            .getByLabel(
              page.viewportSize().width <= 760
                ? "Back to chats"
                : "Toggle conversations",
            )
            .click();
        await page
          .locator("[data-chat]")
          .filter({ hasText: "Release lead" })
          .first()
          .click();
        if (width < 760) await expect(page.locator("#sidebar")).toBeHidden();
        await expect(
          page.locator(".conversation-server .chat-server-alias"),
        ).toHaveText(serverAlias);
        await expect(page.locator(".conversation-server")).toHaveAttribute(
          "data-offline",
          "false",
        );
        await expect(
          page.locator(".conversation-server .chat-server-offline"),
        ).toHaveCount(0);
        await expect(page.locator("#conversation-title")).toHaveText(
          "Release lead",
        );
        await expect(page.locator("#conversation-title")).toBeVisible();
        await expect(page.locator(".conversation-server")).toBeVisible();
        await expect(page.locator("#conversation-status")).toHaveCount(0);
        await expect(page.locator("#messages")).toContainText(
          "Review the release",
        );
        const tools = page.locator(
          "#conversation-header-tools .conversation-header-tools-menu",
        );
        await expect(tools).toHaveAttribute(
          "data-compact",
          width === 1440 ? "no" : "yes",
        );
        await expect(tools.locator(".prompt-history-toggle-label")).toHaveText(
          "1 / 1",
        );
        if (width >= 760)
          await expect(
            page.locator('.account-limits-dot-target[aria-current="true"]'),
          ).toBeVisible();
        await expect(
          page.getByRole("button", { name: "Account limits", exact: true }),
        ).toBeVisible();
        await expect(page.locator(".session-cost-summary")).toHaveAttribute(
          "aria-busy",
          "false",
        );
        await expect(page.locator(".session-cost-summary")).toBeVisible();
        await expect(page.locator(".session-cost-summary")).toHaveAttribute(
          "title",
          /Session estimate: \$/,
        );
        await expect(
          page.locator(".session-cost-summary"),
        ).toHaveAccessibleName(/Session estimate: \$/);
        await page.evaluate(
          (s) =>
            document.documentElement.setAttribute(
              "data-mantine-color-scheme",
              s,
            ),
          scheme,
        );
        await page.evaluate(() => document.fonts.ready);
        const geometry = await page.evaluate(() => {
          const rect = (s) =>
            document.querySelector(s).getBoundingClientRect().toJSON();
          return {
            header: rect(".simple-workspace-header"),
            title: rect("#conversation-title"),
            server: rect(".conversation-server"),
            footer: rect(".usage-footer"),
            estimate: rect(".session-cost-summary"),
            dots: rect(".account-limits-dots"),
            dotTargets: [
              ...document.querySelectorAll(".account-limits-dot-target"),
            ].map((element) => element.getBoundingClientRect().toJSON()),
            limits: rect(".account-limits-toggle"),
            controls: [
              ...document.querySelectorAll(
                ".usage-footer >*, .account-limits-toggle",
              ),
            ]
              .filter(
                (e) =>
                  e.getClientRects().length &&
                  getComputedStyle(e).display !== "none",
              )
              .map((e) => e.getBoundingClientRect().toJSON()),
            teamStrip: !!document.querySelector(".conversation-team-strip"),
            overflow: document.documentElement.scrollWidth > innerWidth,
          };
        });
        const result = { width, scheme, ...geometry };
        await page.screenshot({
          path: testInfo.outputPath(`chrome-${width}-${scheme}.png`),
        });
        assertGeometry(result);
        const accountDot = page.locator(
          '.account-limits-dot-target[aria-current="true"]',
        );
        await accountDot.focus();
        await page.keyboard.press("Enter");
        await expect(
          page.getByRole("region", { name: "Account limits details" }),
        ).toBeVisible();
        await expect(accountDot).toHaveAttribute("aria-expanded", "true");
        await page.keyboard.press("Escape");
        await expect(page.locator(".account-limits-popover")).toBeHidden();
        await page
          .getByRole("button", { name: "Chat context", exact: true })
          .click();
        await expect(
          page.getByText("No context data yet.", { exact: true }),
        ).toBeVisible();
        await page.keyboard.press("Escape");
      } finally {
        await page.context().close();
      }
    }
});

function assertGeometry(r) {
  expect(r.header.height, JSON.stringify(r)).toBeLessThanOrEqual(45);
  expect(r.teamStrip).toBe(false);
  expect(r.server.width).toBeGreaterThan(0);
  if (r.width < 760) {
    expect(r.title.width).toBeGreaterThan(0);
    expect(r.title.height).toBeGreaterThan(0);
    expect(r.server.height).toBeGreaterThan(0);
    expect(r.server.top).toBeGreaterThanOrEqual(r.title.bottom);
    expect(r.server.left).toBeCloseTo(r.title.left, 0);
    for (const rect of [r.title, r.server]) {
      expect(rect.left).toBeGreaterThanOrEqual(r.header.left);
      expect(rect.right).toBeLessThanOrEqual(r.header.right);
      expect(rect.top).toBeGreaterThanOrEqual(r.header.top);
      expect(rect.bottom).toBeLessThanOrEqual(r.header.bottom);
    }
  } else {
    expect(r.server.left).toBeGreaterThanOrEqual(r.title.right);
  }
  expect(r.limits.width).toBeGreaterThan(0);
  expect(r.dots.width).toBeGreaterThan(0);
  expect(r.dots.height).toBeGreaterThan(0);
  for (const [index, target] of r.dotTargets.entries()) {
    expect(target.width).toBeGreaterThanOrEqual(r.width < 760 ? 24 : 12);
    expect(target.height).toBeGreaterThanOrEqual(24);
    if (index)
      expect(target.left).toBeGreaterThanOrEqual(r.dotTargets[index - 1].right);
  }

  expect(r.estimate.width).toBeGreaterThan(0);
  const centers = r.controls.map((e) => e.top + e.height / 2);
  expect(
    Math.max(...centers) - Math.min(...centers),
    JSON.stringify(r),
  ).toBeLessThanOrEqual(1);
  expect(r.overflow).toBe(false);
}
