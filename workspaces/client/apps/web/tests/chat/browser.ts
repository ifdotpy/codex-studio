import type { BrowserContext, Page, TestInfo } from "@playwright/test";
import type { ViteDevServer } from "vite";
import { test } from "../playwright.ts";

export function trackBrowserContexts() {
  const initialContexts = new WeakMap<TestInfo, Set<BrowserContext>>();
  test.beforeEach(async ({ browser }, info) => {
    initialContexts.set(info, new Set(browser.contexts()));
  });
  test.afterEach(async ({ browser }, info) => {
    const initial = initialContexts.get(info);
    await Promise.all(
      browser
        .contexts()
        .filter((context) => !initial?.has(context))
        .map((context) => context.close()),
    );
  });
}

export function fixtureOrigin(server: ViteDevServer) {
  const address = server.httpServer?.address();
  if (!address || typeof address === "string")
    throw new Error("Fixture server has no TCP address");
  return `http://127.0.0.1:${address.port}`;
}

export async function waitForFrames(page: Page, count = 4) {
  await page.evaluate(
    (frames) =>
      new Promise<void>((resolve) => {
        const next = () =>
          frames-- > 0 ? requestAnimationFrame(next) : resolve();
        requestAnimationFrame(next);
      }),
    count,
  );
}
