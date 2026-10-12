import { test, expect } from "../playwright.mjs";
import { sidebarParityFixture } from "./sidebar-parity-fixture.mjs";

// In the combined window the terminal panel of the selected server view sits under the shell
// sidebar while closed, and uses the whole window width while open.
test("the combined window keeps the closed terminal bar under the sidebar and opens it full width", async ({
  page,
  context,
}) => {
  test.setTimeout(90000);
  await page.setViewportSize({ width: 1440, height: 900 });
  const f = await sidebarParityFixture(context);
  try {
    const sidebar = await f.open(page);
    const shell = page.locator(".multi-server-shell");
    const bar = page.locator(".shell-terminal-bar");
    await expect(bar).toBeVisible();
    await expect(shell).toHaveAttribute("data-shell-terminal", "closed");
    const frame = page.frameLocator('iframe[title="Studio on Local"]');
    // The view's own closed bar gives way to the shell's.
    await expect(frame.locator(".terminal-dock")).toBeHidden();
    const closed = await page.evaluate(() => ({
      bar: document
        .querySelector(".shell-terminal-bar")
        .getBoundingClientRect()
        .toJSON(),
      sidebar: document
        .querySelector(".multi-server-shell > #sidebar")
        .getBoundingClientRect()
        .toJSON(),
      height: innerHeight,
    }));
    expect(closed.bar.left).toBe(0);
    expect(closed.bar.bottom).toBe(closed.height);
    expect(Math.round(closed.bar.width)).toBe(Math.round(closed.sidebar.width));
    expect(Math.round(closed.sidebar.bottom)).toBe(Math.round(closed.bar.top));

    await bar
      .getByRole("button", { name: "Show terminals", exact: true })
      .click();
    await expect(shell).toHaveAttribute("data-shell-terminal", "open");
    const dock = frame.locator(".terminal-dock.is-open");
    await expect(dock).toBeVisible();
    await expect(bar).toHaveCount(0);
    await expect
      .poll(() =>
        page.evaluate(() => {
          const views = document
            .querySelector(".server-views")
            .getBoundingClientRect();
          const side = document
            .querySelector(".multi-server-shell > #sidebar")
            .getBoundingClientRect();
          return {
            left: views.left,
            width: views.width,
            sideBottom: Math.round(side.bottom),
          };
        }),
      )
      .toMatchObject({ left: 0, width: 1440 });
    const open = await dock.evaluate((node) => ({
      dock: node.getBoundingClientRect().toJSON(),
      padding: getComputedStyle(document.querySelector("#root")).paddingLeft,
      width: innerWidth,
      height: innerHeight,
    }));
    // The panel spans the window; the chat keeps clear of the sidebar above it.
    expect(open.dock.left).toBe(0);
    expect(open.dock.width).toBe(open.width);
    expect(Math.round(parseFloat(open.padding))).toBe(
      Math.round(closed.sidebar.width),
    );
    const sideBottom = await page.evaluate(() =>
      Math.round(
        document
          .querySelector(".multi-server-shell > #sidebar")
          .getBoundingClientRect().bottom,
      ),
    );
    expect(sideBottom).toBe(Math.round(open.dock.top));
    await expect(sidebar.locator(".project-tree-toggle").first()).toBeVisible();

    await dock
      .getByRole("button", { name: "Hide terminals", exact: true })
      .click();
    await expect(shell).toHaveAttribute("data-shell-terminal", "closed");
    await expect(bar).toBeVisible();
  } finally {
    await f.close();
  }
});
