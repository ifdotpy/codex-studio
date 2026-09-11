#!/usr/bin/env node
// Test viewport geometry in a headless browser with isolated server state.
import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { mkdtemp } from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { createRequire } from "node:module";
const root = dirname(dirname(fileURLToPath(import.meta.url)));
const { chromium } = createRequire(join(root, "web/package.json"))(
  "playwright-core",
);
const state = await mkdtemp(join(tmpdir(), "codex-mobile-keyboard-"));
const fixture = spawn(
  "python3",
  ["-B", join(root, "tests/simple-ui-fixture.py"), state],
  { stdio: ["pipe", "pipe", "pipe"] },
);
let browser,
  log = "";
fixture.stderr.on("data", (chunk) => {
  log += chunk;
});
try {
  const port = await new Promise((resolve, reject) => {
    fixture.stdout.once("data", (chunk) =>
      resolve(Number(String(chunk).trim())),
    );
    fixture.once("exit", () => reject(new Error(log)));
  });
  browser = await chromium.launch({
    headless: true,
    executablePath:
      process.env.CHROME_BIN ||
      "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
  });
  const page = await browser.newPage({
    viewport: { width: 390, height: 844 },
    isMobile: true,
    hasTouch: true,
  });
  await page.addInitScript(() => {
    const viewport = new EventTarget();
    Object.assign(viewport, {
      width: 390,
      height: 844,
      offsetTop: 0,
      offsetLeft: 0,
      scale: 1,
    });
    Object.defineProperty(window, "visualViewport", {
      configurable: true,
      value: viewport,
    });
    window.setTestViewport = (patch, type = "resize") => {
      Object.assign(viewport, patch);
      viewport.dispatchEvent(new Event(type));
    };
    let retainedScroll = 0;
    const originalScroll = window.scrollTo.bind(window);
    Object.defineProperty(window, "scrollY", {
      configurable: true,
      get: () => retainedScroll,
    });
    window.scrollTo = (...args) => {
      retainedScroll = 0;
      originalScroll(...args);
    };
    window.retainTestScroll = (value) => {
      retainedScroll = value;
      window.dispatchEvent(new Event("scroll"));
    };
  });
  const errors = [];
  page.on("pageerror", (error) => errors.push(error.message));
  await page.goto(`http://127.0.0.1:${port}`);
  await page.locator("#composer").waitFor();
  const geometry = () =>
    page.evaluate(() => {
      const root = document.querySelector("#root").getBoundingClientRect();
      const composer = document
        .querySelector("#composer")
        .getBoundingClientRect();
      const footerNode = document.querySelector(".usage-footer");
      const footer = footerNode.getBoundingClientRect();
      const footerBottomSpace =
        parseFloat(getComputedStyle(footerNode).marginBottom) +
        parseFloat(
          getComputedStyle(document.querySelector("#conversation"))
            .paddingBottom,
        );
      return {
        top: root.top,
        height: root.height,
        bottom: root.bottom,
        composerBottom: composer.bottom,
        footerTop: footer.top,
        footerBottom: footer.bottom,
        footerBottomSpace,
        scrollTop: document.scrollingElement.scrollTop,
        scrollY,
        bodyOverflow: getComputedStyle(document.body).overflow,
        bottomInset: document.documentElement.style.getPropertyValue(
          "--mobile-safe-area-bottom",
        ),
      };
    });
  const settle = async (height, top) => {
    await page.waitForFunction(
      ({ height, top }) => {
        const box = document.querySelector("#root").getBoundingClientRect();
        return Math.abs(box.height - height) < 1 && Math.abs(box.top - top) < 1;
      },
      { height, top },
    );
    const box = await geometry();
    assert.equal(box.scrollY, 0);
    assert.equal(box.scrollTop, 0);
    assert.equal(box.bodyOverflow, "hidden");
    assert.ok(box.composerBottom <= box.bottom + 1, JSON.stringify(box));
    assert.ok(
      box.composerBottom <= box.footerTop + 1,
      `Composer overlaps usage: ${JSON.stringify(box)}`,
    );
    assert.ok(
      box.footerBottom <= box.bottom + 1,
      `Usage leaves the viewport: ${JSON.stringify(box)}`,
    );
    assert.ok(
      Math.abs(box.bottom - box.footerBottom - box.footerBottomSpace) <= 1,
      `No blank area below usage: ${JSON.stringify(box)}`,
    );
    return box;
  };
  await settle(844, 0);
  assert.equal(
    await page
      .locator('meta[name="apple-mobile-web-app-status-bar-style"]')
      .getAttribute("content"),
    "black",
  );
  const cdp = await page.context().newCDPSession(page);
  await cdp.send("Emulation.setSafeAreaInsetsOverride", {
    insets: { top: 59, bottom: 0, left: 0, right: 0 },
  });
  await page.evaluate(() =>
    window.setTestViewport({ height: 790, offsetTop: 12 }),
  );
  await settle(790, 12);
  assert.equal(
    await page
      .locator("#root")
      .evaluate((node) => getComputedStyle(node).paddingTop),
    "47px",
  );
  assert.ok(
    (await page.locator(".workspace-header").boundingBox()).y >= 59,
    "A partial viewport shift must not move the header under the status bar",
  );
  await page.locator("#message").fill("Keyboard geometry fixture");
  await page.evaluate(() =>
    window.setTestViewport({ height: 390, offsetTop: 54 }),
  );
  assert.equal((await settle(390, 54)).bottomInset, "0px");
  await page.evaluate(() => {
    window.retainTestScroll(150);
    window.setTestViewport({ offsetTop: 72 }, "scroll");
  });
  await settle(390, 72);
  await page.screenshot({
    path: join(state, "keyboard-open.png"),
    clip: { x: 0, y: 72, width: 390, height: 390 },
  });
  // Pinch zoom must not collapse the application to the magnified viewport.
  await page.evaluate(() =>
    window.setTestViewport({ scale: 2, height: 195, offsetTop: 140 }),
  );
  await page.evaluate(
    () =>
      new Promise((resolve) =>
        requestAnimationFrame(() => requestAnimationFrame(resolve)),
      ),
  );
  const zoomed = await geometry();
  assert.equal(zoomed.height, 390);
  assert.equal(zoomed.top, 72);
  await page.evaluate(() =>
    window.setTestViewport({ scale: 1, height: 844, offsetTop: 0 }),
  );
  assert.equal(
    (await settle(844, 0)).bottomInset,
    "env(safe-area-inset-bottom)",
  );
  await page.setViewportSize({ width: 640, height: 390 });
  await page.evaluate(() => {
    window.setTestViewport({ width: 640, height: 390, offsetTop: 0 });
    window.dispatchEvent(new Event("orientationchange"));
  });
  await settle(390, 0);
  await page.setViewportSize({ width: 390, height: 844 });
  await page.evaluate(() =>
    window.setTestViewport({ width: 390, height: 844, offsetTop: 0 }),
  );
  await settle(844, 0);
  // Desktop must regain its normal document layout and terminal space.
  await page.setViewportSize({ width: 1440, height: 960 });
  await page.waitForFunction(
    () =>
      getComputedStyle(document.querySelector("#root")).position !== "fixed" &&
      !document.documentElement.style.getPropertyValue(
        "--mobile-viewport-height",
      ),
  );
  assert.equal(
    await page.evaluate(() =>
      document.documentElement.style.getPropertyValue(
        "--mobile-viewport-height",
      ),
    ),
    "",
  );
  assert.equal(errors.length, 0, errors.join("\n"));
  console.log(
    `mobile-keyboard-ui: PASS (keyboard, offset, scroll reset, close, rotation, pinch, desktop). Screenshot: ${join(state, "keyboard-open.png")}`,
  );
} catch (error) {
  console.error(log);
  throw error;
} finally {
  await browser?.close();
  fixture.kill("SIGTERM");
}
