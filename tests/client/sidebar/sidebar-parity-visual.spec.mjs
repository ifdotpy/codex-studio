import { mkdir, appendFile, writeFile } from "node:fs/promises";
import { join } from "node:path";
import { test, expect } from "../playwright.mjs";
import { sidebarParityFixture } from "./sidebar-parity-fixture.mjs";

const output = "/tmp/one-sidebar-visual";
const modes = [
  { name: "light", colorScheme: "light", width: 1440, height: 1000 },
  { name: "dark", colorScheme: "dark", width: 1440, height: 1000 },
  { name: "mobile", colorScheme: "light", width: 390, height: 844 },
];

function inspectDOM(element, { omitHeader = false } = {}) {
  const header = omitHeader
    ? element.querySelector(":scope > .sidebar-header")
    : null;
  const visit = (node) => {
    if (node === header) return null;
    if (node.nodeType === Node.TEXT_NODE) return node.textContent;
    if (node.nodeType !== Node.ELEMENT_NODE) return null;
    const attributes = Object.fromEntries(
      [...node.attributes]
        .filter(({ name }) =>
          [
            "class",
            "role",
            "title",
            "type",
            "disabled",
            "aria-label",
            "aria-expanded",
            "aria-current",
            "placeholder",
            "d",
            "points",
            "x1",
            "x2",
            "y1",
            "y2",
            "cx",
            "cy",
            "r",
            "viewBox",
          ].includes(name),
        )
        .map(({ name, value }) => [name, value])
        .sort(([a], [b]) => a.localeCompare(b)),
    );
    return [
      node.tagName,
      attributes,
      [...node.childNodes].map(visit).filter((child) => child !== null),
    ];
  };
  return visit(element);
}

async function prepareCapture(page, sidebar) {
  await page.mouse.move(1, 1);
  await sidebar.evaluate(async (element) => {
    // Compare the same idle state after each filter edit.
    const active = element.ownerDocument.activeElement;
    if (element.contains(active) && active instanceof HTMLElement)
      active.blur();
    await element.ownerDocument.fonts.ready;
    const frame = () =>
      new Promise((resolve) => requestAnimationFrame(resolve));
    await frame();
    await Promise.all(
      element
        .getAnimations({ subtree: true })
        .filter(
          (animation) =>
            animation.effect?.getComputedTiming().iterations !== Infinity,
        )
        .map((animation) => animation.finished.catch(() => {})),
    );
    await frame();
  });
}

async function capture(page, sidebar, name) {
  await prepareCapture(page, sidebar);
  const buffer = await sidebar.screenshot({ animations: "disabled" });
  await writeFile(join(output, `${name}.png`), buffer);
  return buffer;
}

async function contactSheet(browser, name, left, right) {
  const context = await browser.newContext();
  try {
    const page = await context.newPage();
    await page.setContent(
      `<style>body{margin:0;background:#ddd;font:16px sans-serif}main{display:flex;width:max-content;gap:16px;padding:16px}figure{margin:0}figcaption{height:28px}img{display:block}</style><main><figure><figcaption>Classic</figcaption><img src="data:image/png;base64,${left.toString("base64")}"></figure><figure><figcaption>One list</figcaption><img src="data:image/png;base64,${right.toString("base64")}"></figure></main>`,
    );
    await page
      .locator("img")
      .evaluateAll((images) =>
        Promise.all(images.map((image) => image.decode())),
      );
    const size = await page.locator("main").boundingBox();
    await page.setViewportSize({
      width: Math.ceil(size.width),
      height: Math.ceil(size.height),
    });
    await page.screenshot({ path: join(output, `${name}.png`) });
  } finally {
    await context.close();
  }
}

// The classic shell has a server switcher above the iframe. Compare every
// common visible pixel below the expected shell header, without changing the UI.
async function equalContentPixels(browser, left, right, bounds) {
  const context = await browser.newContext();
  try {
    const page = await context.newPage();
    const result = await page.evaluate(
      async ({ sources, bounds }) => {
        const images = await Promise.all(
          sources.map(async (source) => {
            const image = new Image();
            image.src = source;
            await image.decode();
            return image;
          }),
        );
        const width = images[0].width;
        const height = Math.min(
          ...images.map(
            (image, index) =>
              Math.min(image.height, bounds[index].bottom) - bounds[index].top,
          ),
        );
        const pixels = images.map((image, index) => {
          const canvas = document.createElement("canvas");
          canvas.width = width;
          canvas.height = height;
          const ctx = canvas.getContext("2d");
          ctx.drawImage(
            image,
            0,
            bounds[index].top,
            width,
            height,
            0,
            0,
            width,
            height,
          );
          return ctx.getImageData(0, 0, width, height).data;
        });
        let differences = 0;
        let maxDelta = 0;
        for (let index = 0; index < pixels[0].length; index += 4) {
          let delta = 0;
          for (let channel = 0; channel < 4; channel++)
            delta = Math.max(
              delta,
              Math.abs(pixels[0][index + channel] - pixels[1][index + channel]),
            );
          if (delta) differences++;
          maxDelta = Math.max(maxDelta, delta);
        }
        return {
          widths: images.map((image) => image.width),
          width,
          height,
          differences,
          maxDelta,
        };
      },
      {
        sources: [left, right].map(
          (buffer) => `data:image/png;base64,${buffer.toString("base64")}`,
        ),
        bounds,
      },
    );
    expect(result.widths[1]).toBe(result.widths[0]);
    expect(result.height).toBeGreaterThan(0);
    // The iframe and shell can use different compositing paths. Layout and
    // computed colors remain exact; only small raster channel differences pass.
    expect(result.maxDelta, JSON.stringify(result)).toBeLessThanOrEqual(24);
    return result;
  } finally {
    await context.close();
  }
}

function inspectLayout(element) {
  const root = element.getBoundingClientRect();
  const top = element
    .querySelector(".sidebar-header")
    .getBoundingClientRect().bottom;
  const selectors =
    ".sidebar-nav,.projects-heading,input,.project-tree-heading,.sidebar-row,.project-show-more,.project-show-all,.row-copy strong,.chat-server-line";
  return [...element.querySelectorAll(selectors)].map((node) => {
    const rect = node.getBoundingClientRect();
    const style = getComputedStyle(node);
    return {
      tag: node.tagName,
      class: node.className,
      x: rect.left - root.left,
      y: rect.top - top,
      width: rect.width,
      height: rect.height,
      color: style.color,
      background: style.backgroundColor,
      font: style.font,
      border: style.border,
    };
  });
}

async function menus(view, sidebar) {
  const results = {};
  for (const name of [
    "Actions for Local Saved pin",
    "Options for project Home project",
    "Options for folder Local Parent",
    "Options for team Local Saved team",
  ]) {
    const button = sidebar.getByRole("button", { name, exact: true });
    await button.locator("..").hover();
    await button.click();
    const menu = view.getByRole("menu");
    await expect(menu).toBeVisible();
    results[name] = await menu.evaluate(inspectDOM);
    await button.click();
    await expect(menu).toBeHidden();
  }
  return results;
}

test.describe("sidebar visible parity", () => {
  test.describe.configure({ mode: "default" });
  test.beforeAll(async () => {
    await mkdir(output, { recursive: true });
    await writeFile(
      join(output, "index.md"),
      "# One sidebar screenshots\n\nClassic is on the left. One list is on the right.\n\n",
    );
  });

  for (const mode of modes) {
    for (const paired of [false, true]) {
      const scenario = paired ? "single-shell" : "single";
      test(`${scenario} equality and ${mode.name} screenshots`, async ({
        browser,
      }) => {
        test.setTimeout(90000);
        const context = await browser.newContext({
          viewport: { width: mode.width, height: mode.height },
          colorScheme: mode.colorScheme,
        });
        const fixture = await sidebarParityFixture(context, {
          paired,
          remoteEmpty: true,
        });
        const views = [];
        try {
          for (const combined of [false, true]) {
            const navigation = combined ? "combined" : "classic";
            const page = await context.newPage();
            page.on("pageerror", (error) => console.error(error.message));
            page.setDefaultTimeout(10000);
            const sidebar = await fixture.open(page, { combined });
            const surface =
              !combined && paired
                ? page.frameLocator('iframe[title="Studio on This computer"]')
                : page;
            await expect(sidebar).toBeVisible();
            await expect(sidebar.locator("[data-chat]")).toHaveCount(7);
            const rows = await sidebar
              .locator(".row-copy strong")
              .allTextContents();
            expect(rows).toEqual([
              "Local Saved shared chat",
              "Local Saved pin",
              "Local Root B",
              "Local Root A",
              "Local Approval",
              "Local Read answer",
              "Local Running",
              "Local Unread",
            ]);
            await expect(sidebar.locator(".chat-pin")).toHaveCount(1);
            await expect(sidebar.locator(".peer-team-toggle")).toHaveAttribute(
              "aria-expanded",
              "false",
            );
            await expect(
              sidebar.getByRole("button", {
                name: "Show more (6)",
                exact: true,
              }),
            ).toBeVisible();
            await prepareCapture(page, sidebar);
            const dom = await sidebar.evaluate(inspectDOM, {
              omitHeader: paired,
            });
            const layout = await sidebar.evaluate(inspectLayout);
            const bounds = await sidebar.evaluate((element) => {
              const root = element.getBoundingClientRect();
              return {
                top: Math.round(
                  element
                    .querySelector(".sidebar-header")
                    .getBoundingClientRect().bottom - root.top,
                ),
                // Keep the partially rasterized clip edge outside the comparison.
                bottom:
                  Math.floor(
                    element.querySelector("#chat-list").getBoundingClientRect()
                      .bottom - root.top,
                  ) - 1,
              };
            });
            if (paired) {
              await expect(
                sidebar.getByRole("button", {
                  name: "Studio settings",
                  exact: true,
                }),
              ).toHaveCount(combined ? 1 : 0);
              await expect(
                sidebar.locator(
                  '.sidebar-header [aria-label$=" unread chats"]',
                ),
              ).toHaveCount(combined ? 1 : 0);
            }
            const screenshot = await capture(
              page,
              sidebar,
              `${scenario}-${mode.name}-${navigation}`,
            );
            const menuDOM = await menus(surface, sidebar);
            await sidebar
              .getByRole("button", { name: "Project list options" })
              .click();
            await surface
              .getByRole("menuitem", { name: "Show archived chats" })
              .click();
            await expect(
              sidebar.getByTitle("Local Archived", { exact: true }),
            ).toHaveCount(0);
            await sidebar
              .getByLabel("Filter projects and chats")
              .fill("Archived");
            await expect(
              sidebar.getByTitle("Local Archived", { exact: true }),
            ).toBeVisible();
            await prepareCapture(page, sidebar);
            const archivedDOM = await sidebar.evaluate(inspectDOM, {
              omitHeader: paired,
            });
            const archivedLayout = await sidebar.evaluate(inspectLayout);
            const archivedScreenshot = await capture(
              page,
              sidebar,
              `${scenario}-${mode.name}-${navigation}-archived`,
            );
            views.push({
              dom,
              bounds,
              layout,
              archivedLayout,
              screenshot,
              menuDOM,
              archivedDOM,
              archivedScreenshot,
            });
          }
          await contactSheet(
            browser,
            `${scenario}-${mode.name}-comparison`,
            views[0].screenshot,
            views[1].screenshot,
          );
          await contactSheet(
            browser,
            `${scenario}-${mode.name}-archived-comparison`,
            views[0].archivedScreenshot,
            views[1].archivedScreenshot,
          );
          expect(views[1].dom).toEqual(views[0].dom);
          expect(views[1].menuDOM).toEqual(views[0].menuDOM);
          expect(views[1].archivedDOM).toEqual(views[0].archivedDOM);
          let pixelReport = "";
          if (paired) {
            expect(views[1].layout).toEqual(views[0].layout);
            expect(views[1].archivedLayout).toEqual(views[0].archivedLayout);
            const activePixels = await equalContentPixels(
              browser,
              views[0].screenshot,
              views[1].screenshot,
              views.map((view) => view.bounds),
            );
            const archivedPixels = await equalContentPixels(
              browser,
              views[0].archivedScreenshot,
              views[1].archivedScreenshot,
              views.map((view) => view.bounds),
            );
            pixelReport = ` Active: ${activePixels.differences} pixels, max channel delta ${activePixels.maxDelta}. Archived: ${archivedPixels.differences} pixels, max channel delta ${archivedPixels.maxDelta}.`;
          } else {
            expect(views[1].screenshot.equals(views[0].screenshot)).toBe(true);
            expect(
              views[1].archivedScreenshot.equals(views[0].archivedScreenshot),
            ).toBe(true);
          }
          await appendFile(
            join(output, "index.md"),
            `- [${scenario} ${mode.name}](${scenario}-${mode.name}-comparison.png), [archived](${scenario}-${mode.name}-archived-comparison.png). ${paired ? "Exact DOM, menus, layout boxes and computed colors below the header. The shell adds Settings and an unread count. Pixel comparison uses the common scroll viewport and a maximum channel delta of 24." : "Exact full DOM, menus and PNG equality."}${pixelReport}\n`,
          );
          expect(fixture.calls).toEqual({ local: [], remote: [] });
          expect(fixture.local.writes).toEqual([]);
          expect(fixture.remote.writes).toEqual([]);
        } finally {
          await context.close();
          await fixture.close();
        }
      });
    }

    test(`two-server ${mode.name} screenshots include remote folders and aliases`, async ({
      browser,
    }) => {
      test.setTimeout(90000);
      const context = await browser.newContext({
        viewport: { width: mode.width, height: mode.height },
        colorScheme: mode.colorScheme,
      });
      const fixture = await sidebarParityFixture(context);
      try {
        const classic = await context.newPage();
        const classicSidebar = await fixture.open(classic, {
          combined: false,
          owner: "remote",
        });
        const classicScreenshot = await capture(
          classic,
          classicSidebar,
          `two-${mode.name}-classic-remote`,
        );
        const combined = await context.newPage();
        const sidebar = await fixture.open(combined);
        await expect(
          sidebar.getByTitle("Remote Saved pin", { exact: true }),
        ).toBeVisible();
        await expect(
          sidebar.getByTitle("Local Saved pin", { exact: true }),
        ).toBeVisible();
        await expect(sidebar.locator(".sidebar-project")).toHaveCount(1);
        const remoteRow = sidebar
          .locator(".sidebar-row")
          .filter({ hasText: "Remote Saved pin" });
        await expect(remoteRow.locator(".chat-server-line")).toHaveText("REM");
        const remoteFolder = sidebar
          .locator(
            ".project-folder > .project-tree-heading > .project-tree-toggle",
          )
          .filter({ hasText: "Remote Parent" });
        await expect(remoteFolder).toBeVisible();
        await expect(remoteFolder.locator(".chat-server-line")).toHaveText(
          "REM",
        );
        const screenshot = await capture(
          combined,
          sidebar,
          `two-${mode.name}-combined`,
        );
        await contactSheet(
          browser,
          `two-${mode.name}-comparison`,
          classicScreenshot,
          screenshot,
        );
        const remoteNested = sidebar
          .locator(
            ".project-folder > .project-tree-heading > .project-tree-toggle",
          )
          .filter({ hasText: "Remote Nested" });
        await remoteNested.scrollIntoViewIfNeeded();
        await expect(remoteFolder).toBeInViewport();
        await expect(remoteNested).toBeInViewport();
        const folderScreenshot = await capture(
          combined,
          sidebar,
          `two-${mode.name}-remote-folders`,
        );
        await contactSheet(
          browser,
          `two-${mode.name}-folders-comparison`,
          classicScreenshot,
          folderScreenshot,
        );
        await appendFile(
          join(output, "index.md"),
          `- [Two servers ${mode.name}](two-${mode.name}-comparison.png). Classic remote on the left; both servers on the right. Remote rows and folders show REM. [Remote folders](two-${mode.name}-folders-comparison.png) includes the lower rows.\n`,
        );
        expect(fixture.calls).toEqual({ local: [], remote: [] });
        expect(fixture.local.writes).toEqual([]);
        expect(fixture.remote.writes).toEqual([]);
      } finally {
        await context.close();
        await fixture.close();
      }
    });
  }
});
