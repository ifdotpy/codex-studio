import { fixtureOrigin, waitForFrames } from "./browser.ts";
import { createFixtureViteServer as createServer } from "../playwright.ts";
import type {} from "./history-window-anchor-browser.fixture.tsx";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import type { Browser, Page } from "@playwright/test";
import { expect, test } from "../playwright.ts";

async function createHarness(
  browser: Browser,
  start: number,
  count: number,
  disableNativeAnchoring = true,
) {
  const root = fileURLToPath(new URL("../..", import.meta.url));
  const cacheDir = await mkdtemp(join(tmpdir(), "history-window-anchor-vite-"));
  const server = await createServer({
    configFile: false,
    root,
    cacheDir,
    optimizeDeps: {
      noDiscovery: true,
      include: [
        "react",
        "react/jsx-runtime",
        "react/jsx-dev-runtime",
        "react-dom",
        "react-dom/client",
        "dexie",
      ],
    },
    server: { host: "127.0.0.1", port: 0 },
    plugins: [
      {
        name: "history-window-anchor-audit",
        configureServer(vite) {
          vite.middlewares.use("/audit", (_request, response) => {
            response.setHeader("Content-Type", "text/html");
            response.end(
              '<div id="root"></div><script type="module" src="/tests/chat/history-window-anchor-browser.fixture.tsx"></script>',
            );
          });
        },
      },
    ],
  });
  const context = await browser.newContext({
    viewport: { width: 900, height: 700 },
  });
  const page = await context.newPage();
  await server.listen();
  await page.goto(
    `${fixtureOrigin(server)}/audit?start=${start}&count=${count}&disableNativeAnchoring=${disableNativeAnchoring}`,
  );
  await page.locator(`[data-message="message-${start}"]`).waitFor();
  return {
    page,
    async close() {
      await context.close();
      await server.close();
      await rm(cacheDir, { recursive: true, force: true });
    },
  };
}

async function expectMessageInViewport(page: Page, id: string) {
  await page.waitForFunction(
    (messageId) => {
      const root = document.querySelector("#messages");
      if (!root) throw new Error("Missing messages root");
      const message = Array.from(
        root.querySelectorAll<HTMLElement>("[data-message]"),
      ).find((candidate) => candidate.dataset.message === messageId);
      if (!message) return false;
      const bounds = root.getBoundingClientRect();
      const box = message.getBoundingClientRect();
      return (
        box.height > 0 && box.bottom > bounds.top && box.top < bounds.bottom
      );
    },
    id,
    { timeout: 5_000 },
  );
}

test("forward paging keeps the reading row mounted and at its viewport offset", async ({
  browser,
}) => {
  test.setTimeout(90_000);
  const { page, close } = await createHarness(browser, 0, 160, false);
  try {
    const anchor = page.locator('[data-message="message-19"]');
    await page.evaluate(() => window.historyAnchorFixture.scrollToMessage(20));
    await expect(anchor).toBeAttached();
    await waitForFrames(page);
    await page.evaluate(() =>
      window.historyAnchorFixture.commitViewportAnchor(),
    );
    await waitForFrames(page, 2);
    const originalOffset = await anchor.evaluate(
      (node) =>
        node.getBoundingClientRect().top -
        (document.querySelector("#messages")?.getBoundingClientRect().top ??
          Number.NaN),
    );
    expect(originalOffset).toBeCloseTo(-56, 0);
    await page.evaluate(() => window.historyAnchorFixture.appendForwardPage());
    await page.evaluate(async () => {
      for (let frame = 0; frame < 25; frame++)
        await new Promise<void>((resolve) =>
          requestAnimationFrame(() => resolve()),
        );
      window.historyAnchorFixture.releaseScroll();
    });
    const restored = await page.evaluate(() => {
      const root = document.querySelector("#messages");
      if (!root) throw new Error("Missing messages root");
      const message = document.querySelector('[data-message="message-19"]');
      return message
        ? {
            connected: !!message.closest("[data-history-row]"),
            offset:
              message.getBoundingClientRect().top -
              root.getBoundingClientRect().top,
          }
        : { connected: false, offset: Number.NaN };
    });
    expect(
      restored.connected,
      "the anchor row remains in the rendered window",
    ).toBe(true);
    expect(
      Math.abs(restored.offset - originalOffset),
      "the anchor returns to its viewport offset",
    ).toBeLessThan(2);
  } finally {
    await close();
  }
});

test("upward scrolling through prepends reaches the oldest row", async ({
  browser,
}) => {
  const { page, close } = await createHarness(browser, 240, 120);
  try {
    await page.evaluate(() => window.historyAnchorFixture.scrollToMessage(10));
    await waitForFrames(page);
    await page.evaluate(() =>
      window.historyAnchorFixture.commitViewportAnchor(),
    );
    await waitForFrames(page, 2);
    await page.evaluate(() => window.historyAnchorFixture.scrollToTopAsUser());
    await page.evaluate(() => window.historyAnchorFixture.prependPage(120));
    await waitForFrames(page);
    await page.evaluate(() => window.historyAnchorFixture.scrollToTopAsUser());
    await page.evaluate(() => window.historyAnchorFixture.prependPage(0));
    await expectMessageInViewport(page, "message-0");
  } finally {
    await close();
  }
});

test("an explicit jump to the oldest row is not undone by a prepend", async ({
  browser,
}) => {
  const { page, close } = await createHarness(browser, 120, 120);
  try {
    await page.evaluate(() => window.historyAnchorFixture.scrollToMessage(20));
    await waitForFrames(page);
    await page.evaluate(() =>
      window.historyAnchorFixture.commitViewportAnchor(),
    );
    await waitForFrames(page, 2);
    await page.evaluate(() => window.historyAnchorFixture.jumpToOldest());
    await waitForFrames(page, 25);
    await expectMessageInViewport(page, "message-0");
  } finally {
    await close();
  }
});
