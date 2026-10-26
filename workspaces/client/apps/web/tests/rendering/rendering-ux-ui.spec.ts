#!/usr/bin/env node
import type { GetResult } from "../../src/apiTypes.ts";
// Isolated component browser checks. No server state or live model requests.
import type {} from "./rendering-ux-ui.fixture.tsx";
import assert from "node:assert/strict";
import { tmpdir } from "node:os";
import { rm, mkdtemp } from "node:fs/promises";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { test, expect } from "../playwright.ts";

test("rendering ux ui", async ({ page: runnerPage }) => {
  const root = fileURLToPath(new URL("../../../../../../", import.meta.url));
  const { createFixtureViteServer: createServer } =
    await import("../playwright.ts");
  const name = `.rendering-check-${process.pid}`;
  const artifacts = await mkdtemp(join(tmpdir(), "studio-rendering-theme-"));
  let server;
  try {
    server = await createServer({
      root: join(root, "workspaces/client/apps/web"),
      configFile: false,
      server: { host: "127.0.0.1", port: 0 },
      plugins: [
        {
          name: "rendering-check",
          configureServer(vite) {
            vite.middlewares.use(`/${name}.html`, (_request, response) => {
              response.setHeader("Content-Type", "text/html");
              response.end(
                `<div id="root"></div><script type="module" src="/tests/rendering/rendering-ux-ui.fixture.tsx"></script>`,
              );
            });
          },
        },
      ],
    });
    await server.listen();
    const page = runnerPage;
    const errors: string[] = [],
      external: string[] = [];
    page.on("pageerror", (e) => errors.push(e.message));
    page.on("request", (r) => {
      if (r.url().includes("external.invalid")) external.push(r.url());
    });
    const pixel =
      "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+aN1sAAAAASUVORK5CYII=";
    await page.route("**/api/file?**", (r) =>
      r.fulfill({
        json: {
          name: "pixel.png",
          mime: "image/png",
          base64: pixel,
        } satisfies GetResult<"/api/file">,
      }),
    );
    await page.route("https://external.invalid/**", (r) =>
      r.fulfill({ status: 404, body: "missing" }),
    );
    const baseUrl = server.resolvedUrls?.local[0];
    if (!baseUrl) throw new Error("Missing rendering fixture URL");
    await page.goto(`${baseUrl}${name}.html`);
    await page.getByText("Partial", { exact: true }).waitFor();
    await page.locator("#text .sentence-enter").count();
    await page.evaluate(() => {
      const span = [...document.querySelectorAll("#text span")].find(
        (n) => n.textContent === "First sentence. ",
      );
      if (!span) throw new Error("Missing first sentence span");
      window.retained = span;
      const range = document.createRange();
      range.selectNodeContents(span);
      const selection = window.getSelection();
      if (!selection) throw new Error("Missing text selection");
      selection.removeAllRanges();
      selection.addRange(range);
      window.setText("First sentence. Partial response continues");
    });
    await page
      .getByText("Partial response continues", { exact: true })
      .waitFor();
    assert.equal(await page.evaluate(() => window.retained.isConnected), true);
    assert.equal(
      await page.evaluate(() => window.getSelection()?.toString()),
      "First sentence. ",
    );
    for (const id of ["activity", "turn"]) {
      await page
        .locator(`#${id} .tool-card`)
        .first()
        .locator("summary")
        .first()
        .click();
      await page.locator(`#${id} .tool-body`).first().waitFor();
      await page
        .locator(`#${id} .tool-card`)
        .first()
        .evaluate((n) => (n.dataset.retained = "yes"));
    }
    await page.evaluate(() => window.setCount(3));
    for (const id of ["activity", "turn"]) {
      await page.locator(`#${id} .tool-card`).nth(2).waitFor();
      assert.equal(
        await page
          .locator(`#${id} .tool-card`)
          .first()
          .getAttribute("data-retained"),
        "yes",
      );
      assert.notEqual(
        await page.locator(`#${id} .tool-card`).first().getAttribute("open"),
        null,
      );
      const group = page.locator(`#${id} > details, #${id} .turn-work`).first();
      await group.locator(":scope > summary").click();
      await group.locator(":scope > summary").click();
      assert.notEqual(
        await page.locator(`#${id} .tool-card`).first().getAttribute("open"),
        null,
      );
    }
    await page.evaluate(() => window.setText("```python\nprint('partial"));
    await page
      .locator(".code-block code")
      .getByText("print('partial", { exact: false })
      .waitFor();
    await page.getByRole("button", { name: "Expand code" }).click();
    assert.equal(
      await page.locator(".code-block-content").getAttribute("data-expanded"),
      "true",
    );
    await page.evaluate(() =>
      Object.defineProperty(navigator, "clipboard", {
        configurable: true,
        value: {
          writeText: async () => {
            throw Error("denied");
          },
        },
      }),
    );
    await page.evaluate(() => {
      // Force the legacy fallback to fail too, independent of browser policy.
      document.execCommand = () => false;
    });
    await page.getByRole("button", { name: "Copy code", exact: true }).click();
    await page.getByText("Cannot copy. Select the code to copy it.").waitFor();
    await page.evaluate(() =>
      window.setText("```mermaid\nflowchart LR\nA-->B"),
    );
    await page
      .locator(".code-block code")
      .getByText("A-->B", { exact: false })
      .waitFor();
    assert.equal(await page.locator(".rich-preview").count(), 0);
    await page.evaluate(() =>
      window.setText("```mermaid\nflowchart LR\nA-->B\n```"),
    );
    await page.frameLocator(".rich-preview iframe").locator("svg").waitFor();
    await page
      .getByRole("button", { name: "Copy source", exact: true })
      .click();
    await page
      .getByText("Cannot copy. Open Source and select the text to copy it.")
      .waitFor();
    assert.equal(await page.locator(".rich-preview iframe").count(), 1);
    await page.evaluate(() =>
      window.setText(
        "![Remote](https://external.invalid/pixel.png)\n\n![Local](./pixel.png)",
      ),
    );
    await page
      .getByRole("button", { name: "Load image", exact: true })
      .waitFor();
    assert.deepEqual(external, [], "remote images require explicit load");
    await page
      .locator("#text")
      .getByRole("button", { name: "Preview Local" })
      .waitFor();
    await page.getByRole("button", { name: "Load image", exact: true }).click();
    await page.getByText("Cannot load this image.").waitFor();
    assert.equal(external.length, 1);
    for (const selector of [
      ".attachment-preview-button",
      ".message-attachments .image-preview-button",
      "#text .image-preview-button",
    ]) {
      await page.locator(selector).click();
      await page.getByRole("dialog").waitFor();
      await page.locator(".image-viewer").waitFor();
      await page.keyboard.press("Escape");
      await page.getByRole("dialog").waitFor({ state: "hidden" });
    }
    await page.evaluate(() =>
      window.setText(
        "```python\nprint('theme')\n```\n\n```html\n<p>Preview artifact</p>\n```",
      ),
    );
    await page.locator(".rich-preview iframe").waitFor();
    for (const theme of ["light", "dark"] as const) {
      await page.evaluate((value) => window.setTheme(value), theme);
      await page.waitForFunction(
        (value) =>
          document.documentElement.dataset.mantineColorScheme === value,
        theme,
      );
      const colors = await page.evaluate(() => {
        const read = (selector: string) => {
          const element = document.querySelector(selector);
          if (!element) throw new Error(`Missing ${selector}`);
          return getComputedStyle(element);
        };
        return {
          background: read(".rich-preview").backgroundColor,
          label: read(".rich-preview-label").color,
          codeBackground: read(".code-block").backgroundColor,
          codeText: read(".code-block").color,
        };
      });
      const luminance = (color: string) => {
        const channels = color.match(/[\d.]+/g);
        if (!channels || channels.length < 3)
          throw new Error(`Invalid color ${color}`);
        const rgb = channels
          .slice(0, 3)
          .map(Number)
          .map((n) => n / 255)
          .map((n) =>
            n <= 0.04045 ? n / 12.92 : ((n + 0.055) / 1.055) ** 2.4,
          );
        const [red = 0, green = 0, blue = 0] = rgb;
        return red * 0.2126 + green * 0.7152 + blue * 0.0722;
      };
      const contrast = (a: string, b: string) =>
        (Math.max(luminance(a), luminance(b)) + 0.05) /
        (Math.min(luminance(a), luminance(b)) + 0.05);
      assert.ok(
        contrast(colors.background, colors.label) >= 4.5,
        `${theme} preview label contrast`,
      );
      assert.ok(
        contrast(colors.codeBackground, colors.codeText) >= 4.5,
        `${theme} code contrast`,
      );
      assert.equal(luminance(colors.background) > 0.5, theme === "light");
      await page.screenshot({
        path: join(artifacts, `${theme}.png`),
        fullPage: true,
      });
    }
    console.log(`Theme screenshots: ${artifacts}`);
    expect(errors).toEqual([]);
    console.log(
      "PASS: partial text/code; selection; tool expansion; code controls; copy failure; image privacy, failure, and previews",
    );
  } finally {
    await server?.close();
    await rm(artifacts, { recursive: true, force: true });
  }
});
