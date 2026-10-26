import { fixtureOrigin, trackBrowserContexts } from "./browser.ts";
import assert from "node:assert/strict";
import { createFixtureViteServer as createServer } from "../playwright.ts";
import type {} from "./session-activity-browser.fixture.tsx";
import { mkdtemp } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { test } from "../playwright.ts";

trackBrowserContexts();

test("session activity browser", async ({ browser: _browser }) => {
  test.setTimeout(120_000);
  const root = join(import.meta.dirname, "../../");
  const cacheDir = await mkdtemp(join(tmpdir(), "studio-session-activity-"));
  const server = await createServer({
    configFile: false,
    root,
    cacheDir,
    server: { host: "127.0.0.1", port: 0 },
    optimizeDeps: {
      noDiscovery: true,
      include: [
        "react",
        "react/jsx-runtime",
        "react/jsx-dev-runtime",
        "react-dom/client",
        "react-dom",
      ],
    },
    plugins: [
      {
        name: "session-activity-fixture",
        configureServer(server) {
          server.middlewares.use("/check", (_req, res) => {
            res.setHeader("Content-Type", "text/html");
            res.end(
              '<style>:root{--surface:#1b1b20;--text:#e9e9ee;--muted:#9696a5;--line:#ffffff0d}body{margin:0;font:14px system-ui;color:var(--text);background:var(--surface)}header,main{padding:16px}*{box-sizing:border-box}</style><div id="root"></div><script type="module" src="/tests/chat/session-activity-browser.fixture.tsx"></script>',
            );
          });
        },
      },
    ],
  });
  let browser;
  try {
    await server.listen();
    browser = _browser;
    const page = await browser.newPage({
      viewport: { width: 1280, height: 900 },
    });
    const errors: string[] = [];
    page.on("pageerror", (error) => errors.push(error.message));
    await page.goto(`${fixtureOrigin(server)}/check`);
    const strip = page.getByRole("region", { name: "Session activity" });
    await strip.waitFor();
    assert.equal(await strip.locator("[data-activity-id]").count(), 1);
    await strip.getByText("webcrypto-globals", { exact: true }).waitFor();
    assert.equal(
      await strip.getByText("Running command", { exact: true }).count(),
      0,
    );
    await strip.getByText("14h 0m", { exact: true }).waitFor();
    assert.equal(await strip.locator("code").count(), 0);
    assert(
      await strip.evaluate((node) => node.getBoundingClientRect().height <= 34),
    );
    await page.setViewportSize({ width: 390, height: 844 });
    assert(
      await strip.evaluate((node) => node.getBoundingClientRect().height <= 34),
    );
    await page.screenshot({
      path: join(cacheDir, "session-activity-compact.png"),
    });
    await strip.locator('[data-activity-id="long-command"]').click();
    assert.deepEqual(
      await page.evaluate(() => window.sessionActivityFixture.opened[0]),
      await page.evaluate(() => window.sessionActivityFixture.activities[0]),
    );
    await strip
      .getByRole("button", { name: "Show 3 more", exact: true })
      .click();
    assert.equal(await strip.locator("[data-activity-id]").count(), 4);
    for (const id of ["monitor-one", "worker-one", "tool-two"])
      await strip.locator(`[data-activity-id="${id}"]`).click();
    assert.deepEqual(
      await page.evaluate(() =>
        window.sessionActivityFixture.opened.map((item) => item.id),
      ),
      ["long-command", "monitor-one", "worker-one", "tool-two"],
    );
    await page.setViewportSize({ width: 390, height: 844 });
    assert(
      await page.evaluate(
        () => document.documentElement.scrollWidth <= innerWidth,
      ),
    );
    assert(
      await strip.evaluate((node) => node.scrollWidth <= node.clientWidth),
    );
    await page.screenshot({
      path: join(cacheDir, "session-activity-mobile.png"),
    });
    await page.evaluate(() =>
      window.sessionActivityFixture.setActivities(
        Array.from({ length: 40 }, (_, index) => ({
          ...(() => {
            const item = window.sessionActivityFixture.activities[index % 4];
            if (!item) throw new Error("Missing activity");
            return item;
          })(),
          id: `many-${index}`,
        })),
      ),
    );
    assert(
      await strip
        .getByRole("list")
        .evaluate(
          (node) =>
            node.clientHeight <= 240 &&
            node.clientHeight <= innerHeight * 0.3 &&
            node.scrollHeight > node.clientHeight,
        ),
    );
    await strip.getByRole("button", { name: "Show less", exact: true }).click();
    assert.equal(await strip.locator("[data-activity-id]").count(), 1);
    await page.evaluate(() => {
      window.sessionActivityFixture.time += 120000;
      document.dispatchEvent(new Event("visibilitychange"));
    });
    await strip.getByText("14h 2m", { exact: true }).waitFor();
    await page.evaluate(() =>
      window.sessionActivityFixture.setActivities(
        window.sessionActivityFixture.activities.slice(0, 1),
      ),
    );
    assert.equal(await strip.getByRole("button", { name: /Show/ }).count(), 0);
    await page.evaluate(() => window.sessionActivityFixture.setActivities([]));
    await strip.waitFor({ state: "detached" });
    assert.deepEqual(errors, []);
    console.log(
      `PASS session activity: ${test.info().project.use.browserName}, owners, command, elapsed, exact actions, one compact row, expand, 390px, empty state`,
    );
    console.log("Screenshot:", join(cacheDir, "session-activity-mobile.png"));
  } finally {
    await server.close();
  }
});
