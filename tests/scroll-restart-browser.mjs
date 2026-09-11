// Exercise the real scroll hook across a browser reload and delayed history load.
import assert from "node:assert/strict";
import { createRequire } from "node:module";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, dirname } from "node:path";
import { fileURLToPath } from "node:url";
const repo = dirname(dirname(fileURLToPath(import.meta.url)));
const require = createRequire(join(repo, "web/package.json"));
const { chromium, webkit } = require("playwright-core");
const browserType = process.env.BROWSER === "webkit" ? webkit : chromium;
const { createServer } = await import(require.resolve("vite"));
const cache = await mkdtemp(join(tmpdir(), "studio-scroll-restart-"));
const server = await createServer({
  configFile: false,
  root: join(repo, "web"),
  cacheDir: cache,
  optimizeDeps: { include: ["react", "react-dom/client"] },
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
            const position = useConversationScroll(id, ready);
            window.fixture = {select, setReady, position};
            return React.createElement("div", {id: "scroll", ref: position.scroll, onScroll: position.onScroll, style: {height: 300, overflow: "auto"}},
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
      browserType === chromium
        ? process.env.CHROME_BIN ||
          "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome"
        : undefined,
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
    const root = document.getElementById("scroll");
    root.dispatchEvent(new WheelEvent("wheel", { deltaY: -1 }));
    root.scrollTop = 900;
    root.dispatchEvent(new Event("scroll", { bubbles: true }));
  });
  await page.waitForFunction(() => !window.fixture.position.follow);
  await mount();
  assert.equal(
    await page.evaluate(() => window.fixture.position.follow),
    false,
  );
  await ready();
  await page.waitForFunction(
    () => document.getElementById("scroll").scrollTop === 900,
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
      window.fixture.position.follow &&
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
