import { test, expect } from "../playwright.ts";

test("desktop conversation header keeps title and actions on one row", async ({
  page,
  readOnlyRust,
}) => {
  test.setTimeout(90_000);
  await page.setViewportSize({ width: 1440, height: 900 });
  await readOnlyRust.open(page);
  await page
    .locator("[data-chat]")
    .filter({ hasText: "Other project" })
    .first()
    .click();
  await expect(page.locator("#conversation-title")).toHaveText("Other project");
  await expect(
    page.locator("#conversation-header-tools .prompt-navigation-compact"),
  ).toBeVisible();
  const header = page.locator(".simple-workspace-header");
  const geometry = await header.evaluate((element) => {
    const rect = (selector: string) => {
      const node = element.querySelector(selector);
      if (!node) throw new Error(`Missing header element ${selector}`);
      return node.getBoundingClientRect();
    };
    return [
      "#sidebar-toggle",
      ".conversation-heading",
      "#conversation-header-tools",
      "#chat-settings-toggle",
      "#messages-toggle",
      '[aria-label="Chat actions"]',
    ].map((selector) => ({ selector, ...rect(selector) }));
  });
  const centers = geometry.map((item) => item.y + item.height / 2);
  expect(
    Math.max(...centers) - Math.min(...centers),
    JSON.stringify(geometry),
  ).toBeLessThanOrEqual(1);
  for (let index = 1; index < geometry.length; index++) {
    const previous = geometry[index - 1];
    const current = geometry[index];
    if (!previous || !current) throw new Error("Missing header geometry");
    expect(current.x, JSON.stringify(geometry)).toBeGreaterThanOrEqual(
      previous.right,
    );
  }
  await expect(page.locator("#conversation-status")).toHaveCount(0);
  const context = page.getByRole("button", {
    name: "Chat context",
    exact: true,
  });
  await expect(context).toHaveText("Context");
  await expect(context.getByRole("progressbar")).toHaveCount(0);
  await page.screenshot({
    path: test.info().outputPath("real-header-1440.png"),
  });
});
