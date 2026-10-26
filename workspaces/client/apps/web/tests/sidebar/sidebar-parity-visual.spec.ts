import { requiredFixtureValue } from "../servers/browser-fixture.ts";
import type { Page, Locator, Browser, FrameLocator } from "playwright";
import { mkdir, appendFile, writeFile } from "node:fs/promises";
import { join } from "node:path";
import { test, expect } from "../playwright.ts";
import { sidebarParityFixture } from "./sidebar-parity-fixture.ts";

let output: string;
const modes = [
  { name: "light", colorScheme: "light", width: 1440, height: 1000 },
  { name: "dark", colorScheme: "dark", width: 1440, height: 1000 },
  { name: "mobile", colorScheme: "light", width: 390, height: 844 },
] as const;

type DomTree = string | null | [string, Record<string, string>, DomTree[]];
function inspectDOM(element: Element, { omitHeader = false } = {}) {
  const header = omitHeader
    ? element.querySelector(":scope > .sidebar-header")
    : null;
  const visit = (node: Node): DomTree => {
    if (node === header) return null;
    if (node.nodeType === Node.TEXT_NODE) return node.textContent;
    if (!(node instanceof Element)) return null;
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
        .map(({ name, value }): [string, string] => [name, value])
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

async function prepareCapture(page: Page, sidebar: Locator) {
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
      element.ownerDocument
        .getAnimations()
        .filter(
          (animation) =>
            animation.effect?.getComputedTiming().iterations !== Infinity,
        )
        .map((animation) => animation.finished.catch(() => {})),
    );
    await frame();
  });
}

async function capture(page: Page, sidebar: Locator, name: string) {
  await prepareCapture(page, sidebar);
  const buffer = await sidebar.screenshot({ animations: "disabled" });
  await writeFile(join(output, `${name}.png`), buffer);
  return buffer;
}

async function contactSheet(
  browser: Browser,
  name: string,
  left: Buffer,
  right: Buffer,
) {
  const context = await browser.newContext();
  try {
    const page = await context.newPage();
    await page.setContent(
      `<style>body{margin:0;background:#ddd;font:16px sans-serif}main{display:flex;width:max-content;gap:16px;padding:16px}figure{margin:0}figcaption{height:28px}img{display:block}</style><main><figure><figcaption>Classic</figcaption><img src="data:image/png;base64,${left.toString("base64")}"></figure><figure><figcaption>One list</figcaption><img src="data:image/png;base64,${right.toString("base64")}"></figure></main>`,
    );
    await page.locator("img").evaluateAll((images) =>
      Promise.all(
        images.map((image) => {
          if (!(image instanceof HTMLImageElement))
            throw new Error("Expected image");
          return image.decode();
        }),
      ),
    );
    const size = await page.locator("main").boundingBox();
    await page.setViewportSize({
      width: Math.ceil(requiredFixtureValue(size).width),
      height: Math.ceil(requiredFixtureValue(size).height),
    });
    await page.screenshot({ path: join(output, `${name}.png`) });
  } finally {
    await context.close();
  }
}

// The classic shell has a server switcher above the iframe. Compare every
// common visible pixel below the expected shell header, without changing the UI.
async function equalContentPixels(
  browser: Browser,
  left: Buffer,
  right: Buffer,
  bounds: { top: number; bottom: number }[],
) {
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
        const width = (
          images[0] ??
          (() => {
            throw new Error("Missing fixture value");
          })()
        ).width;
        const height = Math.min(
          ...images.map(
            (image, index) =>
              Math.min(
                image.height,
                (
                  bounds[index] ??
                  (() => {
                    throw new Error("Missing fixture value");
                  })()
                ).bottom,
              ) -
              (
                bounds[index] ??
                (() => {
                  throw new Error("Missing fixture value");
                })()
              ).top,
          ),
        );
        const pixels = images.map((image, index) => {
          const canvas = document.createElement("canvas");
          canvas.width = width;
          canvas.height = height;
          const ctx = canvas.getContext("2d");
          (
            ctx ??
            (() => {
              throw new Error("Missing fixture value");
            })()
          ).drawImage(
            image,
            0,
            (
              bounds[index] ??
              (() => {
                throw new Error("Missing fixture value");
              })()
            ).top,
            width,
            height,
            0,
            0,
            width,
            height,
          );
          return (
            ctx ??
            (() => {
              throw new Error("Missing fixture value");
            })()
          ).getImageData(0, 0, width, height).data;
        });
        let differences = 0;
        let maxDelta = 0;
        for (
          let index = 0;
          index <
          (
            pixels[0] ??
            (() => {
              throw new Error("Missing fixture value");
            })()
          ).length;
          index += 4
        ) {
          let delta = 0;
          for (let channel = 0; channel < 4; channel++)
            delta = Math.max(
              delta,
              Math.abs(
                ((pixels[0] ??
                  (() => {
                    throw new Error("Missing fixture value");
                  })())[index + channel] ??
                  (() => {
                    throw new Error("Missing fixture value");
                  })()) -
                  ((pixels[1] ??
                    (() => {
                      throw new Error("Missing fixture value");
                    })())[index + channel] ??
                    (() => {
                      throw new Error("Missing fixture value");
                    })()),
              ),
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

function inspectLayout(element: Element) {
  const root = element.getBoundingClientRect();
  const header = element.querySelector(".sidebar-header");
  if (!header) throw new Error("Missing fixture header");
  const top = header.getBoundingClientRect().bottom;
  const selectors =
    ".sidebar-header,.projects-heading,input,.project-tree-heading,.sidebar-row,.project-show-more,.project-show-all,.row-copy strong,.chat-server-line";
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

async function menus(view: Page | FrameLocator, sidebar: Locator) {
  const results: Record<string, DomTree> = {};
  for (const name of [
    "Actions for Local Saved pin",
    "Options for project Home project",
    "Options for folder Local Parent",
    "Options for team Local Saved team",
  ] as const) {
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
  test.beforeAll(async ({ browser: _browser }, testInfo) => {
    output = testInfo.outputPath("sidebar-visual");
    await mkdir(output, { recursive: true });
    await writeFile(
      join(output, "index.md"),
      "# One sidebar screenshots\n\nClassic is on the left. One list is on the right.\n\n",
    );
  });

  for (const mode of modes) {
    for (const paired of [false, true] as const) {
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
          for (const combined of [false, true] as const) {
            const navigation = combined ? "combined" : "classic";
            const page = await context.newPage();
            page.on("pageerror", (error) => console.error(error.message));
            page.setDefaultTimeout(10000);
            const sidebar = await fixture.open(page, { combined });
            const surface =
              !combined && paired
                ? page.frameLocator('iframe[title="Studio on Local"]')
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
                name: "Show old (6)",
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
                  (
                    element.querySelector(".sidebar-header") ??
                    (() => {
                      throw new Error("Missing fixture value");
                    })()
                  ).getBoundingClientRect().bottom - root.top,
                ),
                // Keep the partially rasterized clip edge outside the comparison.
                bottom:
                  Math.floor(
                    (
                      element.querySelector("#chat-list") ??
                      (() => {
                        throw new Error("Missing fixture value");
                      })()
                    ).getBoundingClientRect().bottom - root.top,
                  ) - 1,
              };
            });
            if (paired) {
              await expect(
                sidebar.getByRole("button", {
                  name: "Studio settings",
                  exact: true,
                }),
              ).toHaveCount(1);
              // The header shows no unread count; rows mark unread chats.
              await expect(
                sidebar.locator(
                  '.sidebar-header [aria-label$=" unread chats"]',
                ),
              ).toHaveCount(0);
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
            ).toBeVisible();
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
            requiredFixtureValue(views[0]).screenshot,
            requiredFixtureValue(views[1]).screenshot,
          );
          await contactSheet(
            browser,
            `${scenario}-${mode.name}-archived-comparison`,
            requiredFixtureValue(views[0]).archivedScreenshot,
            requiredFixtureValue(views[1]).archivedScreenshot,
          );
          expect(requiredFixtureValue(views[1]).dom).toEqual(
            requiredFixtureValue(views[0]).dom,
          );
          expect(requiredFixtureValue(views[1]).menuDOM).toEqual(
            requiredFixtureValue(views[0]).menuDOM,
          );
          expect(requiredFixtureValue(views[1]).archivedDOM).toEqual(
            requiredFixtureValue(views[0]).archivedDOM,
          );
          let pixelReport = "";
          if (paired) {
            expect(requiredFixtureValue(views[1]).layout).toEqual(
              requiredFixtureValue(views[0]).layout,
            );
            expect(requiredFixtureValue(views[1]).archivedLayout).toEqual(
              requiredFixtureValue(views[0]).archivedLayout,
            );
            const activePixels = await equalContentPixels(
              browser,
              requiredFixtureValue(views[0]).screenshot,
              requiredFixtureValue(views[1]).screenshot,
              views.map((view) => view.bounds),
            );
            const archivedPixels = await equalContentPixels(
              browser,
              requiredFixtureValue(views[0]).archivedScreenshot,
              requiredFixtureValue(views[1]).archivedScreenshot,
              views.map((view) => view.bounds),
            );
            pixelReport = ` Active: ${activePixels.differences} pixels, max channel delta ${activePixels.maxDelta}. Archived: ${archivedPixels.differences} pixels, max channel delta ${archivedPixels.maxDelta}.`;
          } else {
            expect(
              requiredFixtureValue(views[1]).screenshot.equals(
                requiredFixtureValue(views[0]).screenshot,
              ),
            ).toBe(true);
            expect(
              requiredFixtureValue(views[1]).archivedScreenshot.equals(
                requiredFixtureValue(views[0]).archivedScreenshot,
              ),
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
