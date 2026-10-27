import { duoPhoneUserAgent, openDuoTeam } from "./duo-fixture.ts";
import { writeFileSync } from "node:fs";
import { test, expect } from "../playwright.ts";
import { sidebarParityFixture } from "../sidebar/sidebar-parity-fixture.ts";
import { fixtureWorkspaceId } from "../servers/multi-server-fixture.ts";

const phone = duoPhoneUserAgent;
async function openReview(
  page: import("playwright").Page,
  context: import("playwright").BrowserContext,
  paired = false,
) {
  const fixture = await sidebarParityFixture(context, { paired });
  fixture.local.snapshot.threads.push({
    ...fixture.local.snapshot.threads[0]!,
    id: "review-worker",
    rootId: "overlap",
    isLead: false,
    name: "Review worker",
  });
  await page.addInitScript(
    (scope) =>
      localStorage.setItem(`studio-workspace-panel-width:${scope}`, "20"),
    fixtureWorkspaceId("Local"),
  );
  const sidebar = await fixture.open(page);
  await sidebar
    .getByRole("button")
    .filter({ hasText: "Local Saved pin" })
    .filter({ has: page.locator("strong") })
    .first()
    .click();
  return fixture;
}

test("Desktop both-panel boxes keep Workspace and Team beside the conversation", async ({
  page,
  context,
}) => {
  await page.setViewportSize({ width: 1440, height: 900 });
  const fixture = await openReview(page, context);
  try {
    await expect.soft(page.locator("#team")).toHaveCount(0);
    await page
      .getByRole("button", { name: "Toggle conversations", exact: true })
      .click();
    await page.locator("#team-toggle").click();
    await page.locator("#messages-toggle").click();
    const team = page.locator("#team");
    const workspace = page.locator(
      ".workspace-panel-inline:not(.team-panel-inline)",
    );
    await expect(team).toBeVisible();
    await expect(workspace).toBeVisible();
    const boxes = await page.evaluate(() =>
      Object.fromEntries(
        [
          ".workspace",
          "#team",
          ".workspace-panel-inline:not(.team-panel-inline)",
        ].map((selector) => [
          selector,
          document.querySelector(selector)!.getBoundingClientRect().toJSON(),
        ]),
      ),
    );
    writeFileSync(
      test.info().outputPath("desktop-both-panel-boxes.json"),
      JSON.stringify(boxes, null, 2),
    );
    await page.screenshot({
      path: test.info().outputPath("desktop-both-panels.png"),
    });
    const t = (await team.boundingBox())!,
      w = (await workspace.boundingBox())!;
    expect(t.y).toBeCloseTo(w.y, 0);
    expect(t.y + t.height).toBeLessThanOrEqual(900);
    expect(t.x >= w.x + w.width - 1 || w.x >= t.x + t.width - 1).toBe(true);
  } finally {
    await fixture.close();
  }
});

test.describe("phone pane review", () => {
  test.use({ hasTouch: true, userAgent: phone, reducedMotion: "reduce" });
  test("Partial fold without position falls back to equal cells and ignores a horizontal band", async ({
    page,
    context,
  }) => {
    await page.setViewportSize({ width: 1200, height: 900 });
    const fixture = await openReview(page, context);
    const cdp = await context.newCDPSession(page);
    try {
      await cdp.send("Emulation.setSafeAreaInsetsOverride", {
        insets: { top: 0, right: 84, bottom: 34, left: 0 },
      });
      await page.setViewportSize({ width: 951, height: 669 });
      await page.evaluate(() => {
        document.documentElement.dataset.nativeHinge = "partial";
        window.dispatchEvent(new Event("studio-native-viewport"));
      });
      await expect(page.locator(".workspace-frame")).toHaveAttribute(
        "data-pane-path",
        "pair",
      );
      await expect
        .poll(async () => (await page.locator("#sidebar").boundingBox())?.width)
        .toBeCloseTo(433.5, 0);
      await page.evaluate(() => {
        const root = document.documentElement;
        root.style.setProperty("--native-hinge-left", "0px");
        root.style.setProperty("--native-hinge-width", "669px");
        root.style.setProperty("--native-hinge-height", "40px");
      });
      await page.setViewportSize({ width: 669, height: 951 });
      await expect
        .poll(async () => (await page.locator("#sidebar").boundingBox())?.width)
        .toBe(250);
      await expect(page.locator(".duo-hinge-gap")).toHaveCount(0);
    } finally {
      await cdp.detach();
      await fixture.close();
    }
  });
  test("A pane move transfers focus from the hidden conversation to the visible Team pane", async ({
    page,
    context,
  }) => {
    await page.setViewportSize({ width: 1200, height: 900 });
    const fixture = await openReview(page, context);
    try {
      await page.setViewportSize({ width: 951, height: 669 });
      await openDuoTeam(page);
      await page.locator("#message").focus();
      await page.setViewportSize({ width: 390, height: 844 });
      await page.evaluate(() => {
        document.documentElement.dataset.nativeHinge = "closed";
        window.dispatchEvent(new Event("studio-native-viewport"));
      });
      await expect(
        page.locator(".duo-pane-panel[aria-label='Team panel']"),
      ).toBeVisible();
      await expect
        .poll(() =>
          page.evaluate(
            () => !!document.activeElement?.closest(".duo-pane-panel"),
          ),
        )
        .toBe(true);
    } finally {
      await fixture.close();
    }
  });
  test("The paired shell retains pane navigation when the selected server requires an update", async ({
    page,
    context,
  }) => {
    await page.setViewportSize({ width: 1200, height: 900 });
    const fixture = await openReview(page, context, true);
    const cdp = await context.newCDPSession(page);
    try {
      await cdp.send("Emulation.setSafeAreaInsetsOverride", {
        insets: { top: 0, right: 84, bottom: 34, left: 0 },
      });
      await page.setViewportSize({ width: 951, height: 669 });
      await expect(page.locator(".multi-server-shell")).toHaveAttribute(
        "data-pane-path",
        "pair",
      );
      await page
        .locator("#sidebar")
        .getByRole("button")
        .filter({ hasText: "Remote Saved pin" })
        .filter({ has: page.locator("strong") })
        .first()
        .click();
      await expect(page.locator("#conversation-title")).toContainText(
        "Remote Saved pin",
      );
      fixture.remote.mismatchSchema();
      await expect(
        page.getByRole("main", { name: "Server update required" }),
      ).toBeVisible();
      await expect(page.locator(".multi-server-shell")).toHaveAttribute(
        "data-pane-path",
        "pair",
      );
      await expect(page.locator("#sidebar")).toBeVisible();
      await page.setViewportSize({ width: 669, height: 951 });
      await expect
        .poll(async () => (await page.locator("#sidebar").boundingBox())?.width)
        .toBeCloseTo(250, 0);
      await page
        .locator("#sidebar")
        .getByRole("button")
        .filter({ hasText: "Local Saved pin" })
        .filter({ has: page.locator("strong") })
        .first()
        .click();
      await expect(page.locator("#conversation-title")).toContainText(
        "Local Saved pin",
      );
    } finally {
      await cdp.detach();
      await fixture.close();
    }
  });
});

test.describe("tablet split window", () => {
  test.use({
    hasTouch: true,
    userAgent:
      "Mozilla/5.0 (iPad; CPU OS 27_2 like Mac OS X) AppleWebKit/605.1.15 Version/27.2 Mobile/15E148 Safari/604.1",
  });
  test("An iPad mini 560x744 split window keeps its existing layout", async ({
    page,
    context,
  }) => {
    await page.setViewportSize({ width: 1200, height: 900 });
    const fixture = await openReview(page, context);
    try {
      await page.setViewportSize({ width: 560, height: 744 });
      await expect(page.locator(".workspace-frame")).not.toHaveAttribute(
        "data-pane-path",
      );
      await expect(page.locator(".duo-transcript-bar")).toHaveCount(0);
      await page.screenshot({
        path: test.info().outputPath("ipad-mini-split.png"),
      });
    } finally {
      await fixture.close();
    }
  });
});

test("Conversation navigation without an item closes Team and clears its saved state", async ({
  page,
  context,
}) => {
  await page.setViewportSize({ width: 1440, height: 900 });
  const fixture = await openReview(page, context);
  try {
    await page.locator("#team-toggle").click();
    await expect(page.locator("#team")).toBeVisible();
    await page.evaluate(() =>
      window.dispatchEvent(
        new CustomEvent("studio-navigate", {
          detail: { agentId: "overlap", section: "messages" },
        }),
      ),
    );
    await expect(page.locator("#team")).toBeHidden();
    expect(
      await page.evaluate(() =>
        JSON.parse(localStorage.getItem("codex-team-open") || "null"),
      ),
    ).toBe(false);
  } finally {
    await fixture.close();
  }
});
test("Closing the non-Duo Team drawer clears the saved desktop preference", async ({
  page,
  context,
}) => {
  await page.setViewportSize({ width: 1440, height: 900 });
  const fixture = await openReview(page, context);
  try {
    await page.locator("#team-toggle").click();
    await page.setViewportSize({ width: 800, height: 900 });
    await openDuoTeam(page);
    await expect(page.locator("#team")).toBeVisible();
    await page.keyboard.press("Escape");
    await expect(page.locator("#team")).toBeHidden();
    expect(
      await page.evaluate(() =>
        JSON.parse(localStorage.getItem("codex-team-open") || "null"),
      ),
    ).toBe(false);
    await page.setViewportSize({ width: 1440, height: 900 });
    await expect(page.locator("#team")).toBeHidden();
  } finally {
    await fixture.close();
  }
});
