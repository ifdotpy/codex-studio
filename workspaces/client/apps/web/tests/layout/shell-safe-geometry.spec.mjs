import { test, expect } from "../playwright.mjs";
import { readFileSync } from "node:fs";
const css = [
  "layout/layout-contract.generated.css",
  "studio-theme.css",
  "style.css",
  "workspace-layout.css",
  "servers/servers.css",
  "hooks/viewport-surface.css",
]
  .map((file) =>
    readFileSync(new URL(`../../src/${file}`, import.meta.url), "utf8").replace(
      /@import[^;]+;/g,
      "",
    ),
  )
  .join("\n");

test("combined shell reserves terminal height inside the safe content rectangle", async ({
  page,
  browserName,
}) => {
  test.skip(browserName !== "chromium", "Safe area emulation uses Chromium");
  await page.setViewportSize({ width: 951, height: 669 });
  const cdp = await page.context().newCDPSession(page);
  await cdp.send("Emulation.setSafeAreaInsetsOverride", {
    insets: { top: 0, right: 84, bottom: 34, left: 0 },
  });
  const rect = (selector) =>
    page.locator(selector).evaluate((node) => {
      const r = node.getBoundingClientRect();
      return {
        left: r.left,
        right: r.right,
        top: r.top,
        bottom: r.bottom,
        height: r.height,
      };
    });
  await page.setContent(
    `<style>${css}</style><div id="root" style="--terminal-dock-height:180px"><div class="multi-server-shell" data-shell-terminal="open" style="--shell-sidebar-width:240px;--shell-terminal-height:180px"><aside id="sidebar">Navigation</aside><main class="server-views"><div class="workspace-layout-row"><main class="workspace">Conversation</main></div><div class="terminal-dock" hidden></div></main><div class="shell-terminal-bar">Terminal</div></div></div>`,
  );
  expect(await rect(".multi-server-shell")).toEqual({
    left: 0,
    right: 867,
    top: 0,
    bottom: 635,
    height: 635,
  });
  expect(await rect(".workspace")).toMatchObject({
    left: 240,
    right: 867,
    top: 0,
    bottom: 455,
    height: 455,
  });
  expect(await rect("#sidebar")).toMatchObject({ bottom: 455 });
  await page.locator(".multi-server-shell").evaluate((node) => {
    node.dataset.shellTerminal = "closed";
    node.style.setProperty("--shell-terminal-height", "38px");
  });
  await page
    .locator("#root")
    .evaluate((node) =>
      node.style.setProperty("--terminal-dock-height", "38px"),
    );
  expect(await rect(".workspace")).toMatchObject({
    left: 240,
    right: 867,
    bottom: 635,
    height: 635,
  });
  expect(await rect(".shell-terminal-bar")).toMatchObject({
    left: 0,
    right: 240,
    bottom: 635,
    height: 38,
  });
  expect(await rect("#sidebar")).toMatchObject({ bottom: 597 });
});
