import assert from "node:assert/strict";
import { fileURLToPath } from "node:url";
import { createRequire } from "node:module";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test, browserExecutablePath } from "../playwright.mjs";

test("Scroll restart", async () => {
  test.setTimeout(180_000);
  const testRepo = fileURLToPath(
    new URL("../../../../../../", import.meta.url),
  );
  // Exercise the real scroll hook across a browser reload and delayed history load.
  const repo = testRepo;
  const require = createRequire(
    join(repo, "workspaces/client/apps/web/package.json"),
  );
  const { chromium, webkit } = require("playwright-core");
  const browserType = process.env.BROWSER === "webkit" ? webkit : chromium;
  const { createServer } = await import(require.resolve("vite"));
  const cache = await mkdtemp(join(tmpdir(), "studio-scroll-restart-"));
  const server = await createServer({
    configFile: false,
    root: join(repo, "web"),
    cacheDir: cache,
    optimizeDeps: {
      noDiscovery: true,
      include: ["react", "react-dom/client"],
    },
    server: { host: "127.0.0.1", port: 0, hmr: false },
    plugins: [
      {
        name: "scroll-restart",
        resolveId(id) {
          if (id === "virtual:scroll-restart") return "\0" + id;
        },
        load(id) {
          if (id !== "\0virtual:scroll-restart") return;
          return `
          import React, {useState} from "react";
          import {createRoot} from "react-dom/client";
          import {useConversationScroll} from "/src/components/useConversationScroll.ts";
          export function mount() {
            createRoot(document.body).render(React.createElement(function Harness() {
              const [id, select] = useState("workspace-a:agent:lead");
              const [ready, setReady] = useState(false);
              const [revision, redraw] = useState(0);
              const position = useConversationScroll(id, ready);
              window.fixture = {select, setReady, position, redraw};
              return React.createElement("div", {id: "scroll", "data-revision": revision, ref: position.scroll, onScroll: position.onScroll, style: {height: 300, overflow: "auto"}},
                React.createElement("div", {ref: position.content}, ...Array.from({length: ready ? 80 : 0}, (_, index) =>
                  React.createElement("p", {key: index, "data-message": "message-"+index, style: {height: 60, margin: 0}}, "Message " + index))));
            }));
          }
        `;
        },
      },
    ],
  });
  let browser;
  try {
    await server.listen();
    browser = await browserType.launch({
      headless: true,
      executablePath:
        browserType === chromium ? browserExecutablePath : undefined,
    });
    const page = await browser.newPage();
    const errors = [];
    page.on("pageerror", (error) => errors.push(error.message));
    await page.route("**/check", (route) =>
      route.fulfill({
        contentType: "text/html",
        body: "<!doctype html><title>Scroll recovery</title>",
      }),
    );
    const mount = async () => {
      await page.goto(server.resolvedUrls.local[0] + "check");
      await page.evaluate(async () =>
        (await import("/@id/virtual:scroll-restart")).mount(),
      );
      await page.waitForFunction(() => !!window.fixture);
    };
    const ready = async () => {
      await page.evaluate(() => window.fixture.setReady(true));
      await page.waitForFunction(
        () => document.querySelectorAll("[data-message]").length === 80,
      );
    };
    await mount();
    await ready();
    await page.evaluate(() => {
      window.followChanges = 0;
      window.followUnsubscribe = window.fixture.position.subscribeFollow(() => {
        window.followChanges++;
      });
      const root = document.getElementById("scroll");
      root.dispatchEvent(new WheelEvent("wheel", { deltaY: -1 }));
      root.scrollTop = 900;
      root.dispatchEvent(new Event("scroll", { bubbles: true }));
    });
    await page.waitForFunction(
      () => !window.fixture.position.getFollow() && window.followChanges > 0,
    );
    assert.equal(
      await page.evaluate(() => window.fixture.position.getFollow()),
      false,
    );
    assert.ok(await page.evaluate(() => window.followChanges > 0));
    await page.evaluate(() => window.followUnsubscribe());
    await mount();
    assert.equal(
      await page.evaluate(() => window.fixture.position.getFollow()),
      false,
    );
    await ready();
    await page.waitForFunction(
      () => document.getElementById("scroll").scrollTop === 900,
    );
    // Let native scroll delivery and the hook's 600 ms user-input window settle
    // before counting layout reads caused by unrelated React renders.
    await page.waitForTimeout(650);
    await page.evaluate(() => {
      window.messageMeasurements = 0;
      const original = Element.prototype.getBoundingClientRect;
      Element.prototype.getBoundingClientRect = function (...args) {
        if (this.matches("[data-message]")) window.messageMeasurements++;
        return original.apply(this, args);
      };
    });
    await page.evaluate(() => window.fixture.position.remember());
    await page.evaluate(() => (window.messageMeasurements = 0));
    for (let revision = 1; revision <= 20; revision++) {
      await page.evaluate((value) => window.fixture.redraw(value), revision);
      await page.waitForFunction(
        (value) =>
          document.getElementById("scroll").dataset.revision === String(value),
        revision,
      );
    }
    const messageMeasurements = await page.evaluate(
      () => window.messageMeasurements,
    );
    assert.ok(
      messageMeasurements < 100,
      `Unrelated renders reuse the visible anchor without rescanning earlier messages (measured ${messageMeasurements})`,
    );
    assert.equal(
      await page.locator("#scroll").evaluate((node) => node.scrollTop),
      900,
    );
    await page.evaluate(() => window.fixture.select("workspace-b:agent:lead"));
    await page.waitForFunction(
      () => document.getElementById("scroll").scrollTop === 4500,
    );
    await page.evaluate(() => window.fixture.select("workspace-a:agent:lead"));
    await page.waitForFunction(
      () => document.getElementById("scroll").scrollTop === 900,
    );
    await page.evaluate(() => window.fixture.position.setFollow(true));
    await mount();
    await ready();
    await page.waitForFunction(
      () =>
        window.fixture.position.getFollow() &&
        document.getElementById("scroll").scrollTop === 4500,
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
