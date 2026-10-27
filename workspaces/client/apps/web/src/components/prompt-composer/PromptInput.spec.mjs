// Mount the production input by itself: no App or Conversation wrapper.
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { createServer } from "vite";
import { expect, test } from "../../../tests/playwright.mjs";

let cacheDir;
let server;

test.beforeAll(async () => {
  cacheDir = await mkdtemp(join(tmpdir(), "studio-prompt-input-test-"));
  server = await createServer({
    configFile: false,
    root: fileURLToPath(new URL("../../../", import.meta.url)),
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
  await server.listen();
});

test("isolated production PromptInput preserves its input behavior", async ({
  page,
}) => {
  const errors = [];
  page.setDefaultTimeout(10000);
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
  await page.goto(`${server.resolvedUrls.local[0]}prompt-input-test`);
  await page.evaluate(async () => {
    const { mount } =
      await import("/src/components/prompt-composer/PromptInput.test-entry.tsx");
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
  await expect(input).toHaveValue("hello");
  await expect
    .poll(() => page.evaluate(() => window.inputEvents.changes))
    .toBe(1);

  await input.press("Enter");
  await expect
    .poll(() => page.evaluate(() => window.inputEvents.sends))
    .toBe(1);
  await input.press("Shift+Enter");
  await expect
    .poll(() => page.evaluate(() => window.inputEvents.sends))
    .toBe(1);
  await page.evaluate(() => {
    document.querySelector("#message").dispatchEvent(
      new KeyboardEvent("keydown", {
        key: "Enter",
        bubbles: true,
        isComposing: true,
      }),
    );
  });
  await expect
    .poll(() => page.evaluate(() => window.inputEvents.sends))
    .toBe(1);

  await input.press("Tab");
  await expect
    .poll(() => page.evaluate(() => window.inputEvents.queues))
    .toBe(1);
  await page.evaluate(() => window.setInputState({ canSend: false }));
  await expect(input).toBeDisabled();
  const changesBeforeDisabledInput = await page.evaluate(
    () => window.inputEvents.changes,
  );
  await input.press("x").catch(() => {});
  await expect
    .poll(() => page.evaluate(() => window.inputEvents.changes))
    .toBe(changesBeforeDisabledInput);

  await page.evaluate(() =>
    window.setInputState({
      canSend: true,
      text: "x".repeat(12001),
      tooLong: true,
    }),
  );
  await expect(input).toHaveAttribute("aria-invalid", "true");

  await page.evaluate(() => {
    window.setInputState({ text: "$eli5", tooLong: false });
    const element = document.querySelector("#message");
    element.setSelectionRange(element.value.length, element.value.length);
    element.dispatchEvent(new Event("select", { bubbles: true }));
  });
  await page.getByRole("option", { name: /eli5/ }).waitFor();
  await input.press("Enter");
  await expect(input).toHaveValue("$eli5 ");
  await expect
    .poll(() => page.evaluate(() => window.inputEvents.sends))
    .toBe(1);
  expect(errors).toEqual([]);
});

test.afterAll(async () => {
  await server?.close();
  if (cacheDir) await rm(cacheDir, { recursive: true, force: true });
});

test("touch inputs keep the focus zoom font floor across layout widths", async ({
  browser,
}) => {
  for (const hasTouch of [true, false]) {
    const context = await browser.newContext({ hasTouch });
    try {
      const page = await context.newPage();
      await page.route("**/touch-input-test", (route) =>
        route.fulfill({
          contentType: "text/html",
          body: `<!doctype html><meta name="viewport" content="width=device-width, initial-scale=1">
            <div class="workspace">
              <aside id="sidebar"><input id="chat-search"></aside>
              <main id="conversation"><form id="composer"><div id="root"></div></form></main>
              <div id="controls"></div>
            </div>`,
        }),
      );
      await page.goto(`${server.resolvedUrls.local[0]}touch-input-test`);
      await page.evaluate(async () => {
        await import("/node_modules/@mantine/core/styles.css");
        await import("/src/style.css");
        await import("/src/studio-preferences.css");
        await import("/src/components/Analytics.css");
        const { mount, mountControls } =
          await import("/src/components/prompt-composer/PromptInput.test-entry.tsx");
        mount();
        mountControls();
      });
      await page.locator("#message").waitFor({ state: "attached" });
      await page.locator("#form-select").waitFor({ state: "attached" });
      for (const width of [390, 760, 761, 844, 1024, 1440]) {
        await page.setViewportSize({ width, height: 900 });
        for (const typography of ["original", "custom"]) {
          for (const fontSize of [12, 18, 24]) {
            await page.evaluate(
              ({ typography, fontSize }) => {
                const root = document.documentElement;
                root.dataset.studioTypography = typography;
                root.style.setProperty(
                  "--studio-main-font-size",
                  `${fontSize}px`,
                );
                root.style.setProperty(
                  "--studio-sidebar-font-size",
                  `${fontSize}px`,
                );
              },
              { typography, fontSize },
            );
            const floor = hasTouch || width <= 760;
            for (const id of [
              "message",
              "chat-search",
              "form-input",
              "form-textarea",
              "form-select",
              "worker-search",
              "analytics-select",
            ]) {
              const actual = await page
                .locator(`#${id}`)
                .evaluate((node) =>
                  parseFloat(getComputedStyle(node).fontSize),
                );
              const preferred =
                typography === "custom" &&
                ["message", "chat-search"].includes(id)
                  ? fontSize
                  : id === "message"
                    ? 14
                    : 12;
              expect(
                actual,
                JSON.stringify({ id, width, hasTouch, typography, fontSize }),
              ).toBe(floor ? Math.max(16, preferred) : preferred);
            }
          }
        }
      }
    } finally {
      await context.close();
    }
  }
});
