import { fixtureOrigin } from "../../tests/misc-fixture.ts";
import type {} from "./visualActivity.browser.fixture.tsx";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { test, expect } from "../../tests/playwright.ts";

test("hidden visual rows stop timers and rate renders and resume current values", async ({
  browser,
}) => {
  test.setTimeout(60_000);
  const { createFixtureViteServer: createServer } =
    await import("../../tests/playwright.ts");
  const cacheDir = await mkdtemp(join(tmpdir(), "studio-visual-activity-"));
  const server = await createServer({
    configFile: false,
    cacheDir,
    root: fileURLToPath(new URL("../..", import.meta.url)),
    server: { host: "127.0.0.1", port: 0 },
    plugins: [
      {
        name: "visual-activity-check",
        configureServer(server) {
          server.middlewares.use((request, response, next) => {
            if (request.url !== "/visual-activity-check") return next();
            response.setHeader("Content-Type", "text/html");
            response.end(`<!doctype html><title>Visual activity check</title>
              <style>body{margin:0}.card{display:flex;gap:12px;height:40px;width:600px}#far{margin-top:2000px}</style>
              <div id="root"></div>
              <script type="module" src="/src/hooks/visualActivity.browser.fixture.tsx"></script>`);
          });
        },
      },
    ],
  });
  const page = await browser.newPage({ viewport: { width: 800, height: 600 } });
  const errors: string[] = [];
  page.on("pageerror", (error) => errors.push(error.message));
  await page.addInitScript(() => {
    window.visualActivityFixture = {
      commits: {},
      rateInterests: new Set(),
      rate: () => {},
      workerRate: () => {},
      unmount: () => {},
      visualMetrics: { ticks: 0, observers: 0 },
      activeVisualTimers: () => 0,
      testingHidden: false,
      setTestingHidden: () => {},
    };
    const timers = new Map<number, boolean>();
    const timerWindow: Window = window;
    const interval = timerWindow.setInterval.bind(timerWindow);
    const clear = timerWindow.clearInterval.bind(timerWindow);
    window.visualActivityFixture.visualMetrics = { ticks: 0, observers: 0 };
    window.visualActivityFixture.activeVisualTimers = () => timers.size;
    timerWindow.setInterval = (
      callback: TimerHandler,
      delay?: number,
      ...args: unknown[]
    ) => {
      if (delay !== 1000) return interval(callback, delay, ...args);
      const id = interval(() => {
        window.visualActivityFixture.visualMetrics.ticks++;
        if (typeof callback === "function") callback(...args);
      }, delay);
      timers.set(id, true);
      return id;
    };
    timerWindow.clearInterval = (id) => {
      if (id !== undefined) timers.delete(id);
      clear(id);
    };
    const Observer = window.IntersectionObserver;
    window.IntersectionObserver = class extends Observer {
      active: boolean;
      constructor(...args: ConstructorParameters<typeof Observer>) {
        super(...args);
        window.visualActivityFixture.visualMetrics.observers++;
        this.active = true;
      }
      disconnect() {
        if (this.active) window.visualActivityFixture.visualMetrics.observers--;
        this.active = false;
        super.disconnect();
      }
    };
    window.visualActivityFixture.testingHidden = false;
    Object.defineProperty(document, "hidden", {
      configurable: true,
      get: () => window.visualActivityFixture.testingHidden,
    });
    window.visualActivityFixture.setTestingHidden = (hidden) => {
      window.visualActivityFixture.testingHidden = hidden;
      document.dispatchEvent(new Event("visibilitychange"));
    };
  });
  try {
    await server.listen();
    await page.goto(`${fixtureOrigin(server)}/visual-activity-check`);
    await expect(page.locator("#near .token-rate")).toHaveAttribute(
      "data-visual-active",
      "true",
    );
    await expect(page.locator("#far .token-rate")).toHaveAttribute(
      "data-visual-active",
      "false",
    );
    await expect(page.locator("#closed .token-rate")).toHaveAttribute(
      "data-visual-active",
      "false",
    );
    await expect
      .poll(() =>
        page.evaluate(() => window.visualActivityFixture.activeVisualTimers()),
      )
      .toBe(1);
    expect(
      await page.evaluate(
        () => window.visualActivityFixture.visualMetrics.observers,
      ),
    ).toBe(1);
    expect(
      await page.evaluate(
        () => window.visualActivityFixture.rateInterests.size,
      ),
    ).toBe(2);
    expect(
      await page
        .locator("#far .chat-status svg")
        .evaluate((node) => getComputedStyle(node).animationPlayState),
    ).toBe("paused");
    expect(
      await page
        .locator("#near .chat-status svg")
        .evaluate((node) => getComputedStyle(node).animationPlayState),
    ).toBe("running");
    await page.evaluate(() => {
      window.visualActivityFixture.commits = {};
      for (let i = 0; i < 100; i++) {
        window.visualActivityFixture.rate("far", 30 + i);
        window.visualActivityFixture.rate("closed", 30 + i);
      }
      window.visualActivityFixture.rate("empty", 42);
    });
    await expect(page.locator("#empty .token-rate")).toHaveText("42 tok/s");
    expect(
      await page.evaluate(() => window.visualActivityFixture.commits.far || 0),
    ).toBe(0);
    expect(
      await page.evaluate(
        () => window.visualActivityFixture.commits.closed || 0,
      ),
    ).toBe(0);
    expect(await page.locator("#team .token-rate").boundingBox()).toBeNull();
    await page.evaluate(() => window.visualActivityFixture.workerRate(43));
    await expect(page.locator("#team .token-rate")).toHaveText("43 tok/s");
    await expect(page.locator("#team .token-rate")).toHaveAttribute(
      "data-visual-active",
      "true",
    );
    await page.locator("#fold summary").click();
    await expect(page.locator("#closed .token-rate")).toHaveText("129 tok/s");
    expect(
      await page.evaluate(
        () => window.visualActivityFixture.rateInterests.size,
      ),
    ).toBe(3);
    await expect
      .poll(() =>
        page.evaluate(() => window.visualActivityFixture.activeVisualTimers()),
      )
      .toBe(2);
    await page.evaluate(() =>
      window.visualActivityFixture.setTestingHidden(true),
    );
    await expect
      .poll(() =>
        page.evaluate(() => window.visualActivityFixture.activeVisualTimers()),
      )
      .toBe(0);
    expect(
      await page.evaluate(
        () => window.visualActivityFixture.rateInterests.size,
      ),
    ).toBe(0);
    const ticks = await page.evaluate(
      () => window.visualActivityFixture.visualMetrics.ticks,
    );
    await page.waitForTimeout(1200);
    expect(
      await page.evaluate(
        () => window.visualActivityFixture.visualMetrics.ticks,
      ),
    ).toBe(ticks);
    await page.evaluate(() => {
      window.visualActivityFixture.rate("near", 88);
      window.visualActivityFixture.setTestingHidden(false);
    });
    await expect(page.locator("#near .token-rate")).toHaveAttribute(
      "data-rate",
      "88",
    );
    await expect
      .poll(() =>
        page.evaluate(() => window.visualActivityFixture.activeVisualTimers()),
      )
      .toBe(2);
    await page.evaluate(() => window.scrollTo(0, document.body.scrollHeight));
    await expect(page.locator("#far .token-rate")).toHaveAttribute(
      "data-visual-active",
      "true",
    );
    await expect(page.locator("#far .token-rate")).toHaveText("129 tok/s");
    expect(
      await page.evaluate(
        () => window.visualActivityFixture.rateInterests.size,
      ),
    ).toBe(1);
    await expect
      .poll(() =>
        page.evaluate(() => window.visualActivityFixture.activeVisualTimers()),
      )
      .toBe(1);
    await expect
      .poll(async () => {
        const text = await page.locator("#far .reasoning-time").textContent();
        return Number.parseInt(text ?? "") >= 4;
      })
      .toBe(true);
    await page.evaluate(() => window.visualActivityFixture.unmount());
    expect(
      await page.evaluate(() =>
        window.visualActivityFixture.activeVisualTimers(),
      ),
    ).toBe(0);
    expect(
      await page.evaluate(
        () => window.visualActivityFixture.rateInterests.size,
      ),
    ).toBe(0);
    expect(
      await page.evaluate(
        () => window.visualActivityFixture.visualMetrics.observers,
      ),
    ).toBe(0);
    // Without an observer, keep visible-tab updates active after hide/resume.
    // There is no scroll observer to reactivate an offscreen row in this mode.
    await page.addInitScript(() => {
      Reflect.deleteProperty(window, "IntersectionObserver");
    });
    await page.reload();
    await expect
      .poll(() =>
        page.evaluate(() => window.visualActivityFixture.activeVisualTimers()),
      )
      .toBe(2);
    await page.evaluate(() =>
      window.visualActivityFixture.setTestingHidden(true),
    );
    await expect
      .poll(() =>
        page.evaluate(() => window.visualActivityFixture.activeVisualTimers()),
      )
      .toBe(0);
    await page.evaluate(() => {
      window.visualActivityFixture.rate("far", 91);
      window.visualActivityFixture.setTestingHidden(false);
    });
    await expect
      .poll(() =>
        page.evaluate(() => window.visualActivityFixture.activeVisualTimers()),
      )
      .toBe(2);
    await page.evaluate(() => window.scrollTo(0, document.body.scrollHeight));
    await expect(page.locator("#far .token-rate")).toHaveAttribute(
      "data-visual-active",
      "true",
    );
    await expect(page.locator("#far .token-rate")).toHaveText("91 tok/s");
    await page.locator("#fold summary").click();
    await expect
      .poll(() =>
        page.evaluate(() => window.visualActivityFixture.activeVisualTimers()),
      )
      .toBe(3);
    await page.evaluate(() => window.visualActivityFixture.unmount());
    expect(
      await page.evaluate(() =>
        window.visualActivityFixture.activeVisualTimers(),
      ),
    ).toBe(0);
    expect(errors).toEqual([]);
  } finally {
    await page.close();
    await server.close();
    await rm(cacheDir, { recursive: true, force: true });
  }
});
