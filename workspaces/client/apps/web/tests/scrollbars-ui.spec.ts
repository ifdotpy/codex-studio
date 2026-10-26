import { test, expect } from "./playwright.ts";
import type {} from "./scrollbars-ui.fixture.tsx";
import { build } from "esbuild";
import { fileURLToPath } from "node:url";
import { browserExecutablePath } from "./browser-executable.ts";

test.use({
  launchOptions: async ({ browserName }, use) => {
    const launchOverrides =
      browserName === "chromium"
        ? {
            executablePath: browserExecutablePath,
            ignoreDefaultArgs: ["--hide-scrollbars"],
          }
        : {};
    await use(launchOverrides);
  },
});

const root = fileURLToPath(new URL("../", import.meta.url));

// Exercise the real CSS cascade and interactive Mantine component without a backend.
test("native panes and Mantine share scrollbar styling in both themes", async ({
  page,
}, testInfo) => {
  const bundle = await build({
    absWorkingDir: root,
    entryPoints: [
      fileURLToPath(new URL("./scrollbars-ui.fixture.tsx", import.meta.url)),
    ],
    bundle: true,
    write: false,
    outdir: testInfo.outputPath("scrollbar-bundle"),
    define: { "process.env.NODE_ENV": '"production"' },
  });
  await page.setContent(
    '<div id="fixture" style="display:flex;gap:24px;padding:24px"></div>',
  );
  const css = bundle.outputFiles.find((file) => file.path.endsWith(".css"));
  const javascript = bundle.outputFiles.find((file) =>
    file.path.endsWith(".js"),
  );
  if (!css || !javascript) throw new Error("Missing scrollbar fixture bundle");
  await page.addStyleTag({
    content: css.text,
  });
  await page.addScriptTag({
    content: javascript.text,
  });
  const viewport = page.locator(".mantine-ScrollArea-viewport");
  const verticalTrack = page.locator(
    '.mantine-ScrollArea-scrollbar[data-orientation="vertical"]',
  );
  const thumb = verticalTrack.locator(".mantine-ScrollArea-thumb");
  await expect(thumb).toBeVisible();
  await expect(thumb).toHaveCSS("width", "6px");
  await expect(thumb).toHaveCSS("border-radius", "12px");
  await expect(verticalTrack).toHaveCSS("width", "12px");
  await expect(
    page.locator(
      '.mantine-ScrollArea-scrollbar[data-orientation="horizontal"] > .mantine-ScrollArea-thumb',
    ),
  ).toHaveCSS("height", "6px");

  for (const scheme of ["light", "dark"]) {
    await page
      .locator("html")
      .evaluate(
        (element, value) =>
          element.setAttribute("data-mantine-color-scheme", value),
        scheme,
      );
    const expectedColor =
      scheme === "light"
        ? "rgba(24, 36, 71, 0.094)"
        : "rgba(255, 255, 255, 0.094)";
    for (const selector of ["#messages", ".chat-scroll", "textarea"]) {
      await expect(page.locator(selector)).toHaveCSS("scrollbar-width", "thin");
      await expect(page.locator(selector)).toHaveCSS(
        "scrollbar-color",
        `${expectedColor} rgba(0, 0, 0, 0)`,
      );
    }
    await expect(viewport).toHaveCSS("scrollbar-width", "none");
    await expect(thumb).toHaveCSS("background-color", expectedColor);
    await verticalTrack.hover();
    await expect(verticalTrack).toHaveCSS(
      "background-color",
      "rgba(0, 0, 0, 0)",
    );
    await expect(thumb).toHaveCSS("background-color", expectedColor);
    await page.mouse.down();
    await expect(verticalTrack).toHaveCSS(
      "background-color",
      "rgba(0, 0, 0, 0)",
    );
    await expect(thumb).toHaveCSS("background-color", expectedColor);
    await page.mouse.up();
    await expect(page.locator(".mantine-ScrollArea-corner")).toHaveCSS(
      "background-color",
      "rgba(0, 0, 0, 0)",
    );
    await testInfo.attach(`${scheme}-computed`, {
      body: JSON.stringify(
        await page.evaluate(() => window.scrollbarFixture.computedStyles()),
        null,
        2,
      ),
      contentType: "application/json",
    });
    await testInfo.attach(`${scheme}-scrollbars`, {
      body: await page.screenshot(),
      contentType: "image/png",
    });
  }

  await page.locator(".chat-scroll").hover();
  await page.mouse.wheel(0, 150);
  await expect
    .poll(() =>
      page.locator(".chat-scroll").evaluate((element) => element.scrollTop),
    )
    .toBeGreaterThan(0);
  await page.locator("#messages").focus();
  await page.keyboard.press("ArrowDown");
  await expect
    .poll(() =>
      page.locator("#messages").evaluate((element) => element.scrollTop),
    )
    .toBeGreaterThan(0);
  const box = await thumb.boundingBox();
  if (!box) throw new Error("Scrollbar thumb has no bounding box");
  await page.mouse.move(box.x + box.width / 2, box.y + box.height / 2);
  await page.mouse.down();
  await page.mouse.move(box.x + box.width / 2, box.y + box.height / 2 + 60);
  await page.mouse.up();
  await expect
    .poll(() => viewport.evaluate((element) => element.scrollTop))
    .toBeGreaterThan(0);
});
