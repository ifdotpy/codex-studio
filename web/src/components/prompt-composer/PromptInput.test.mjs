#!/usr/bin/env node
// Mount the production input by itself: no App or Conversation wrapper.
import assert from "node:assert/strict";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { createRequire } from "node:module";
import { join } from "node:path";
import { createServer } from "vite";

const require = createRequire(
  new URL("../../../package.json", import.meta.url),
);
const { chromium } = require("playwright-core");
const cacheDir = await mkdtemp(join(tmpdir(), "studio-prompt-input-test-"));
const server = await createServer({
  configFile: false,
  root: new URL("../../../", import.meta.url).pathname,
  cacheDir,
  optimizeDeps: {
    noDiscovery: true,
    include: [
      "react",
      "react/jsx-runtime",
      "react/jsx-dev-runtime",
      "react-dom/client",
      "@mantine/core",
    ],
  },
  server: { host: "127.0.0.1", port: 0, hmr: false },
});
let browser;

try {
  await server.listen();
  browser = await chromium.launch({
    headless: true,
    executablePath: process.env.CHROME_BIN || "/usr/bin/chromium",
  });
  const page = await browser.newPage();
  page.setDefaultTimeout(10000);
  const errors = [];
  page.on("console", (message) => {
    if (message.type() === "error") errors.push(message.text());
  });
  page.on("pageerror", (error) => errors.push(error.message));
  await page.route("**/api/skills?*", (route) =>
    route.fulfill({
      json: {
        skills: [
          { name: "eli5", description: "Simple explanation", path: "/eli5" },
          { name: "doc1", description: "Durable docs", path: "/doc1" },
        ],
        errors: [],
      },
    }),
  );
  await page.route("**/prompt-input-test", (route) =>
    route.fulfill({
      contentType: "text/html",
      body: "<!doctype html><html><head></head><body><div id='root'></div></body></html>",
    }),
  );
  await page.goto(server.resolvedUrls.local[0] + "prompt-input-test");
  await page.evaluate(async () => {
    const { mount } = await import(
      "/src/components/prompt-composer/PromptInput.test-entry.tsx"
    );
    mount();
  });
  await page
    .locator("#message")
    .waitFor()
    .catch(async () => {
      throw new Error(
        `isolated PromptInput did not mount: ${await page.locator("body").innerHTML()} ${errors.join("\n")}`,
      );
    });

  const input = page.getByRole("combobox", { name: "Message" });
  await input.fill("hello");
  assert.equal(await input.inputValue(), "hello");
  assert.equal(await page.evaluate(() => window.inputEvents.changes), 1);

  await input.press("Enter");
  assert.equal(await page.evaluate(() => window.inputEvents.sends), 1);
  await input.press("Shift+Enter");
  assert.equal(await page.evaluate(() => window.inputEvents.sends), 1);
  await page.evaluate(() => {
    document.querySelector("#message").dispatchEvent(
      new KeyboardEvent("keydown", {
        key: "Enter",
        bubbles: true,
        isComposing: true,
      }),
    );
  });
  assert.equal(await page.evaluate(() => window.inputEvents.sends), 1);

  await input.press("Tab");
  assert.equal(await page.evaluate(() => window.inputEvents.queues), 1);
  await page.evaluate(() => window.setInputState({ canSend: false }));
  await page.waitForFunction(
    () => document.querySelector("#message")?.disabled === true,
  );
  assert.equal(await input.isDisabled(), true);
  const changesBeforeDisabledInput = await page.evaluate(
    () => window.inputEvents.changes,
  );
  await input.press("x").catch(() => {});
  assert.equal(
    await page.evaluate(() => window.inputEvents.changes),
    changesBeforeDisabledInput,
  );

  await page.evaluate(() =>
    window.setInputState({
      canSend: true,
      text: "x".repeat(12001),
      tooLong: true,
    }),
  );
  await page.waitForFunction(
    () =>
      document.querySelector("#message")?.getAttribute("aria-invalid") ===
      "true",
  );
  assert.equal(await input.getAttribute("aria-invalid"), "true");

  await page.evaluate(() => {
    window.setInputState({ text: "$eli5", tooLong: false });
    const element = document.querySelector("#message");
    element.setSelectionRange(element.value.length, element.value.length);
    element.dispatchEvent(new Event("select", { bubbles: true }));
  });
  await page.getByRole("option", { name: /eli5/ }).waitFor();
  await input.press("Enter");
  await page.waitForFunction(
    () => document.querySelector("#message")?.value === "$eli5 ",
  );
  assert.equal(await page.evaluate(() => window.inputEvents.sends), 1);
  assert.deepEqual(errors, []);
  console.log(
    "PASS: isolated production PromptInput covers controlled/programmatic updates, length/disabled state, Enter/Shift+Enter/Tab, IME, and skill insertion",
  );
} finally {
  await browser?.close();
  await server.close();
  await rm(cacheDir, { recursive: true, force: true });
}
