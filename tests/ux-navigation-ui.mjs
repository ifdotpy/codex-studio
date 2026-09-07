#!/usr/bin/env node
// Production bundle, isolated backend, no model calls.
import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { mkdtemp } from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { createRequire } from "node:module";
const repo = dirname(dirname(fileURLToPath(import.meta.url)));
const { chromium } = createRequire(join(repo, "web/package.json"))(
  "playwright-core",
);
const evidence = await mkdtemp(join(tmpdir(), "studio-ux-navigation-"));
const fixture = spawn(
  "python3",
  ["-B", join(repo, "tests/simple-ui-fixture.py"), evidence],
  { stdio: ["ignore", "pipe", "pipe"] },
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
  const origin = `http://127.0.0.1:${port}`;
  const snapshot = await (await fetch(origin + "/api/state")).json();
  browser = await chromium.launch({
    headless: true,
    executablePath:
      process.env.CHROME_BIN ||
      "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
  });
  const page = await browser.newPage({
    viewport: { width: 1440, height: 960 },
  });
  page.setDefaultTimeout(12000);
  const errors = [],
    actions = [];
  page.on("pageerror", (error) => errors.push(error.message));
  await page.route("**/api/action", (route) => {
    actions.push(route.request().postDataJSON());
    return route.fulfill({ json: { status: "accepted" } });
  });
  await page.goto(origin);
  await page.locator("#message").waitFor();
  await page.locator(".chat-row").filter({ hasText: "Release lead" }).click();
  await page.reload();
  await page.waitForFunction(
    () =>
      document.querySelector("#conversation-title")?.textContent ===
      "Release lead",
  );
  assert.equal(
    await page.evaluate(
      (stateDir) =>
        JSON.parse(localStorage.getItem(`codex-desktop-opened:${stateDir}`)),
      snapshot.stateDir,
    ),
    snapshot.threads.find((agent) => agent.name === "Release lead").id,
  );
  await page.locator('input[type="file"]').setInputFiles({
    name: "review-notes.txt",
    mimeType: "text/plain",
    buffer: Buffer.from("A review attachment"),
  });
  await page
    .getByRole("button", { name: "Remove review-notes.txt", exact: true })
    .waitFor();
  await page.locator("#message").fill("/review");
  await page.locator("#send").click();
  await page
    .getByRole("status")
    .filter({ hasText: "Commands cannot include files" })
    .waitFor();
  assert.equal(await page.locator("#message").inputValue(), "/review");
  assert.equal(actions.length, 0);
  await page
    .getByRole("button", { name: "Remove review-notes.txt", exact: true })
    .click();
  await page.locator("#message").fill("/review keep these instructions");
  await page.locator("#send").click();
  await page
    .getByRole("status")
    .filter({ hasText: "Use the command without additional text" })
    .waitFor();
  assert.equal(actions.length, 0);
  assert.equal(
    await page.locator("#message").inputValue(),
    "/review keep these instructions",
  );
  await page.locator("#message").fill("/review");
  await page.locator("#send").click();
  await page.waitForFunction(
    () => document.querySelector("#message").value === "",
  );
  assert.equal(actions.length, 1);
  assert.equal(actions[0].action, "review");

  const mobile = await browser.newPage({
    viewport: { width: 390, height: 844 },
    isMobile: true,
    hasTouch: true,
  });
  mobile.on("pageerror", (error) => errors.push(error.message));
  await mobile.addInitScript(() => {
    if (window.top !== window) return;
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
    window.setTestViewport = (patch) => {
      Object.assign(viewport, patch);
      viewport.dispatchEvent(new Event("resize"));
    };
  });
  await mobile.goto(origin);
  await mobile.locator("#message").waitFor();
  const fits = async (selector, height, top) => {
    await mobile.waitForFunction(
      ({ selector, height, top }) => {
        const element = document.querySelector(selector);
        if (!element) return false;
        for (let node = element; node; node = node.parentElement) {
          if (Number(getComputedStyle(node).opacity) < 1) return false;
        }
        const rect = element.getBoundingClientRect();
        return (
          rect.height > 0 &&
          rect.top >= top - 1 &&
          rect.bottom <= top + height + 1
        );
      },
      { selector, height, top },
    );
  };
  for (const width of [320, 390, 760]) {
    await mobile.setViewportSize({ width, height: 844 });
    await mobile.evaluate(
      (width) => window.setTestViewport({ width, height: 390, offsetTop: 54 }),
      width,
    );
    await mobile.getByLabel("Toggle conversations", { exact: true }).click();
    await fits(".mantine-Drawer-content", 390, 54);
    await fits("#sidebar", 390, 54);
    await mobile.getByLabel("Close conversations", { exact: true }).click();
    await mobile
      .getByRole("button", { name: "Chat settings", exact: true })
      .click();
    await fits(".mantine-Modal-content", 390, 54);
    const dialog = mobile.getByRole("dialog", {
      name: "Chat settings",
      exact: true,
    });
    await dialog
      .getByRole("button", { name: "Agent chats", exact: true })
      .click();
    await fits(".mantine-Drawer-content", 390, 54);
    await mobile
      .getByRole("dialog", { name: "Agent chats", exact: true })
      .getByRole("button", { name: "Close", exact: true })
      .click();
    await mobile.evaluate(() =>
      window.setTestViewport({ height: 844, offsetTop: 0 }),
    );
  }
  await mobile.setViewportSize({ width: 390, height: 844 });
  await mobile
    .getByRole("button", { name: "Chat settings", exact: true })
    .click();
  await mobile.evaluate(() =>
    window.setTestViewport({ height: 390, offsetTop: 54 }),
  );
  await fits(".mantine-Modal-content", 390, 54);
  await mobile.screenshot({
    path: join(evidence, "settings-keyboard.png"),
    clip: { x: 0, y: 54, width: 390, height: 390 },
  });
  await mobile.keyboard.press("Escape");
  await mobile.setViewportSize({ width: 1440, height: 960 });
  await mobile.locator(".workspace-shortcuts").waitFor();
  assert.equal(
    await mobile.evaluate(() =>
      document.documentElement.style.getPropertyValue(
        "--mobile-viewport-height",
      ),
    ),
    "",
  );
  assert.deepEqual(errors, []);
  console.log(
    `PASS: desktop selection, commands retain files/text, keyboard overlays at 320/390/760px. Screenshot: ${join(evidence, "settings-keyboard.png")}`,
  );
} catch (error) {
  console.error(log);
  throw error;
} finally {
  await browser?.close();
  fixture.kill("SIGTERM");
}
