import { fixtureOrigin } from "./performance-fixture.ts";
import type {} from "./scroll-restart.fixture.tsx";
import assert from "node:assert/strict";
import { fileURLToPath } from "node:url";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test, browserExecutablePath } from "../playwright.ts";

test("Scroll restart", async () => {
  test.setTimeout(180_000);
  const testRepo = fileURLToPath(
    new URL("../../../../../../", import.meta.url),
  );
  // Exercise the real scroll hook across a browser reload and delayed history load.
  const repo = testRepo;
  const { chromium, webkit } = await import("playwright-core");
  const browserType = process.env.BROWSER === "webkit" ? webkit : chromium;
  const { createServer } = await import("vite");
  const cache = await mkdtemp(join(tmpdir(), "studio-scroll-restart-"));
  const server = await createServer({
    configFile: false,
    root: join(repo, "workspaces/client/apps/web"),
    cacheDir: cache,
    optimizeDeps: {
      noDiscovery: true,
      include: [
        "react",
        "react/jsx-runtime",
        "react/jsx-dev-runtime",
        "react-dom/client",
        "dexie",
      ],
    },
    server: { host: "127.0.0.1", port: 0, hmr: false },
    plugins: [
      {
        name: "scroll-restart",
        resolveId(id) {
          if (id === "/scroll-restart.fixture.tsx")
            return fileURLToPath(
              new URL("./scroll-restart.fixture.tsx", import.meta.url),
            );
        },
      },
    ],
  });
  let browser;
  try {
    await server.listen();
    browser = await browserType.launch({
      headless: true,
      ...(browserType === chromium
        ? { executablePath: browserExecutablePath }
        : {}),
    });
    const page = await browser.newPage();
    await page.clock.install();
    const errors: string[] = [];
    page.on("pageerror", (error) => errors.push(error.message));
    await page.route("**/check", (route) =>
      route.fulfill({
        contentType: "text/html",
        body: "<!doctype html><title>Scroll recovery</title>",
      }),
    );
    const mount = async () => {
      await page.goto(fixtureOrigin(server) + "/check");
      await page.evaluate(async () =>
        (await import("./scroll-restart.fixture.tsx")).mount(),
      );
      await page.waitForFunction(() => !!window.scrollRestartFixture);
    };
    const ready = async () => {
      await page.evaluate(() => window.scrollRestartFixture.setReady(true));
      await page.waitForFunction(
        () => document.querySelectorAll("[data-message]").length === 80,
      );
    };
    await mount();
    await ready();
    await page.evaluate(() => {
      window.scrollRestartFollowChanges = 0;
      window.scrollRestartFollowUnsubscribe =
        window.scrollRestartFixture.position.subscribeFollow(() => {
          window.scrollRestartFollowChanges++;
        });
      const root = document.getElementById("scroll");
      if (!root) throw new Error("Missing scroll fixture");
      root.dispatchEvent(new WheelEvent("wheel", { deltaY: -1 }));
      root.scrollTop = 900;
      root.dispatchEvent(new Event("scroll", { bubbles: true }));
    });
    await page.waitForFunction(
      () =>
        !window.scrollRestartFixture.position.getFollow() &&
        window.scrollRestartFollowChanges > 0,
    );
    assert.equal(
      await page.evaluate(() =>
        window.scrollRestartFixture.position.getFollow(),
      ),
      false,
    );
    assert.ok(await page.evaluate(() => window.scrollRestartFollowChanges > 0));
    await page.evaluate(() => window.scrollRestartFollowUnsubscribe());
    await mount();
    assert.equal(
      await page.evaluate(() =>
        window.scrollRestartFixture.position.getFollow(),
      ),
      false,
    );
    await ready();
    await page.waitForFunction(
      () => document.getElementById("scroll")?.scrollTop === 900,
    );
    // Expire the hook's recent-input window, including performance.now().
    await page.clock.fastForward(650);
    await page.evaluate(() => {
      window.scrollRestartMessageMeasurements = 0;
      const original = Element.prototype.getBoundingClientRect;
      Element.prototype.getBoundingClientRect = function (...args) {
        if (this.matches("[data-message]"))
          window.scrollRestartMessageMeasurements++;
        return original.apply(this, args);
      };
    });
    await page.evaluate(() => window.scrollRestartFixture.position.remember());
    await page.evaluate(() => (window.scrollRestartMessageMeasurements = 0));
    for (let revision = 1; revision <= 20; revision++) {
      await page.evaluate(
        (value) => window.scrollRestartFixture.redraw(value),
        revision,
      );
      await page.waitForFunction(
        (value) =>
          document.getElementById("scroll")?.dataset.revision === String(value),
        revision,
      );
    }
    const messageMeasurements = await page.evaluate(
      () => window.scrollRestartMessageMeasurements,
    );
    assert.ok(
      messageMeasurements < 100,
      `Unrelated renders reuse the visible anchor without rescanning earlier messages (measured ${messageMeasurements})`,
    );
    assert.equal(
      await page.locator("#scroll").evaluate((node) => node.scrollTop),
      900,
    );
    await page.evaluate(() =>
      window.scrollRestartFixture.select("workspace-b:agent:lead"),
    );
    await page.waitForFunction(
      () => document.getElementById("scroll")?.scrollTop === 4500,
    );
    await page.evaluate(() =>
      window.scrollRestartFixture.select("workspace-a:agent:lead"),
    );
    await page.waitForFunction(
      () => document.getElementById("scroll")?.scrollTop === 900,
    );
    await page.evaluate(() =>
      window.scrollRestartFixture.position.setFollow(true),
    );
    await mount();
    await ready();
    await page.waitForFunction(
      () =>
        window.scrollRestartFixture.position.getFollow() &&
        document.getElementById("scroll")?.scrollTop === 4500,
    );
    assert.deepEqual(errors, []);
    console.log(
      "PASS: reload retains reading position, delayed history does not overwrite it, workspace isolation, Latest remains selected after reload",
    );
  } finally {
    await browser?.close();
    await server.close();
    await rm(cache, { recursive: true, force: true });
  }
});
