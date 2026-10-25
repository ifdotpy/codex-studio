import {
  test,
  expect,
  rustUiFixture,
  spawnFixture as spawn,
} from "../../../tests/playwright.mjs";
import { mkdir, mkdtemp } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";

async function focusGeometry(page) {
  return page.locator(":focus").evaluate((element) => {
    const style = getComputedStyle(element);
    const rect = element.getBoundingClientRect();
    const root = getComputedStyle(document.documentElement);
    const rem = parseFloat(root.fontSize);
    const focusWidth =
      parseFloat(root.getPropertyValue("--studio-focus-ring-width")) * rem;
    const focusInset =
      parseFloat(root.getPropertyValue("--studio-focus-ring-inset")) * rem;
    const width = parseFloat(style.outlineWidth);
    const offset = parseFloat(style.outlineOffset);
    const ring = {
      left: rect.left - width - offset,
      right: rect.right + width + offset,
      top: rect.top - width - offset,
      bottom: rect.bottom + width + offset,
    };
    const strip = element.closest(".fleet-strip");
    const stripStyle = strip && getComputedStyle(strip);
    const ancestors = [];
    for (let node = element; node; node = node.parentElement)
      ancestors.push(node);
    const canvas = document.createElement("canvas");
    canvas.width = canvas.height = 1;
    const context = canvas.getContext("2d");
    context.fillStyle = "white";
    context.fillRect(0, 0, 1, 1);
    for (const node of [...ancestors].reverse()) {
      context.fillStyle = getComputedStyle(node).backgroundColor;
      context.fillRect(0, 0, 1, 1);
    }
    const background = [...context.getImageData(0, 0, 1, 1).data].slice(0, 3);
    context.clearRect(0, 0, 1, 1);
    context.fillStyle = style.outlineColor;
    context.fillRect(0, 0, 1, 1);
    const foreground = [...context.getImageData(0, 0, 1, 1).data].slice(0, 3);
    const luminance = (rgb) =>
      rgb
        .map((channel) => {
          const value = channel / 255;
          return value <= 0.04045
            ? value / 12.92
            : ((value + 0.055) / 1.055) ** 2.4;
        })
        .reduce(
          (sum, value, index) => sum + value * [0.2126, 0.7152, 0.0722][index],
          0,
        );
    const values = [luminance(background), luminance(foreground)].sort(
      (a, b) => b - a,
    );
    const clips = ancestors.slice(1).filter((node) => {
      const bounds = node.getBoundingClientRect();
      const css = getComputedStyle(node);
      return (
        (/(hidden|clip|auto|scroll)/.test(css.overflowX) &&
          (ring.left < bounds.left - 0.5 || ring.right > bounds.right + 0.5)) ||
        (/(hidden|clip|auto|scroll)/.test(css.overflowY) &&
          (ring.top < bounds.top - 0.5 || ring.bottom > bounds.bottom + 0.5))
      );
    });
    return {
      name: element.getAttribute("aria-label") || element.textContent.trim(),
      className: element.className,
      tag: element.tagName,
      keyboard: element.matches(":focus-visible"),
      outline: style.outlineStyle,
      width,
      offset,
      focusWidth,
      focusInset,
      shadow: style.boxShadow,
      color: style.outlineColor,
      primary: getComputedStyle(document.documentElement)
        .getPropertyValue("--mantine-primary-color-filled")
        .trim(),
      ring,
      stripShadow: stripStyle?.boxShadow,
      clipped: clips.length > 0,
      clips: clips.map((node) => ({
        className: node.className,
        bounds: node.getBoundingClientRect().toJSON(),
      })),
      contrast: (values[0] + 0.05) / (values[1] + 0.05),
      scope: element.closest("#sidebar")
        ? "sidebar"
        : element.closest(".fleet-menu")
          ? "menu"
          : "outside",
      priority: !!element.closest(".fleet-attention"),
      state: strip?.dataset.studioState,
      duplicates: ancestors
        .slice(1)
        .filter((node) => node.matches(":focus-visible")).length,
    };
  });
}
async function checkFocus(page) {
  const geometry = await focusGeometry(page);
  expect(geometry.keyboard, geometry.name).toBe(true);
  expect(geometry.outline, geometry.name).toBe("solid");
  expect(geometry.width, geometry.name).toBe(geometry.focusWidth);
  expect(geometry.offset, geometry.name).toBe(
    -geometry.focusWidth - geometry.focusInset,
  );
  expect(geometry.shadow, geometry.name).toBe("none");
  expect(geometry.clipped, JSON.stringify(geometry)).toBe(false);
  expect(geometry.contrast, geometry.name).toBeGreaterThanOrEqual(3);
  expect(geometry.duplicates, geometry.name).toBe(0);
  if (geometry.stripShadow != null)
    expect(geometry.stripShadow, geometry.name).toBe("none");
  return geometry;
}

test("sidebar keyboard focus is consistent, unclipped and visible without state rails", async ({
  browser,
}) => {
  test.setTimeout(90000);
  const root = await mkdtemp(join(tmpdir(), "studio-sidebar-focus-"));
  const evidence = test.info().outputPath("screenshots");
  await mkdir(evidence, { recursive: true });
  const proc = spawn(process.execPath, [rustUiFixture, root], {
    stdio: ["pipe", "pipe", "pipe"],
    env: {
      ...process.env,
      FIXTURE_SEED: "question",
      FIXTURE_HOLD: "lead,other",
      FIXTURE_WORKER_COUNT: "0",
    },
  });
  let errors = "";
  proc.stderr.on("data", (chunk) => {
    errors += chunk;
  });
  try {
    const port = await new Promise((resolve, reject) => {
      proc.stdout.once("data", (chunk) =>
        resolve(Number(String(chunk).trim())),
      );
      proc.once("exit", () => reject(new Error(errors)));
    });
    for (const scheme of ["light", "dark"])
      for (const width of [1440, 390, 320]) {
        const context = await browser.newContext({
          colorScheme: scheme,
          viewport: { width, height: 960 },
        });
        const page = await context.newPage();
        await page.addInitScript(
          (scheme) =>
            localStorage.setItem("mantine-color-scheme-value", scheme),
          scheme,
        );
        await page.goto(`http://127.0.0.1:${port}`);
        await expect(page.locator("#composer")).toBeVisible();
        if (!(await page.locator("#sidebar").isVisible()))
          await page.locator("#sidebar-toggle").click();
        await expect(page.locator("#sidebar")).toBeVisible();
        await expect(page.locator("#sidebar")).toBeInViewport({ ratio: 1 });
        const project = page.locator("#sidebar .project-tree-toggle").first();
        if ((await project.getAttribute("aria-expanded")) === "false")
          await project.click();
        await page.keyboard.press("Tab");
        await page.locator("#sidebar .brand").focus();
        const visited = [];
        for (let index = 0; index < 60; index++) {
          const geometry = await focusGeometry(page);
          if (
            geometry.scope !== "sidebar" ||
            (index > 0 && geometry.className.includes("brand"))
          )
            break;
          visited.push(await checkFocus(page));
          if (geometry.priority && geometry.className.includes("chat-row"))
            await page.screenshot({
              path: join(evidence, `focus-needs-you-${scheme}-${width}.png`),
            });
          if (geometry.tag === "INPUT")
            await page.screenshot({
              path: join(evidence, `focus-filter-${scheme}-${width}.png`),
            });
          if (
            geometry.state === "working" &&
            geometry.className.includes("chat-row")
          )
            await page.screenshot({
              path: join(evidence, `focus-working-${scheme}-${width}.png`),
            });
          await page.keyboard.press("Tab");
        }
        for (const kind of [
          "sidebar-search",
          "sidebar-new-chat",
          "sidebar-shared-chat",
          "project-tree-toggle",
          "project-tree-action",
          "chat-row",
          "row-actions",
        ])
          expect(
            visited.some((item) => item.className.includes(kind)),
            kind,
          ).toBe(true);
        expect(visited.some((item) => item.tag === "INPUT")).toBe(true);
        expect(
          visited.some(
            (item) => item.priority && item.className.includes("chat-row"),
          ),
        ).toBe(true);
        expect(
          visited.some(
            (item) =>
              item.state === "working" && item.className.includes("chat-row"),
          ),
        ).toBe(true);
        for (const [menuIndex, target] of [
          page.locator("#sidebar .row-actions").first(),
          page
            .locator("#sidebar .project-tree-action[aria-haspopup=menu]")
            .first(),
          page.getByRole("button", { name: "Project list options" }),
        ].entries()) {
          await target.scrollIntoViewIfNeeded();
          await target.focus();
          await checkFocus(page);
          await page.keyboard.press("Enter");
          await expect(page.getByRole("menu")).toBeVisible();
          await expect(page.getByRole("menu")).toHaveCSS("opacity", "1");
          await expect(
            page.getByRole("menu").locator("[data-autofocus]"),
          ).toBeFocused();
          await page.keyboard.press("ArrowDown");
          await expect(page.locator('[role="menuitem"]:focus')).toBeVisible();
          const names = new Set();
          for (let index = 0; index < 20; index++) {
            const geometry = await checkFocus(page);
            expect(geometry.scope).toBe("menu");
            if (names.has(geometry.name)) break;
            names.add(geometry.name);
            if (index === 0)
              await page.screenshot({
                path: join(
                  evidence,
                  `focus-menu-${menuIndex}-${scheme}-${width}-${visited.length}.png`,
                ),
              });
            await page.keyboard.press("ArrowDown");
          }
          await page.keyboard.press("Escape");
          await expect(page.getByRole("menu")).toBeHidden();
          // Escape also dismisses the phone drawer; reopen it for the next menu.
          if (width < 760) {
            await expect(page.locator("#sidebar")).toBeHidden();
            await page.locator("#sidebar-toggle").click();
            await expect(page.locator("#sidebar")).toBeInViewport({ ratio: 1 });
            await page.keyboard.press("Tab");
          }
        }
        await page.locator("#sidebar .project-tree-toggle").first().click();
        expect(
          await page
            .locator("#sidebar .project-tree-toggle")
            .first()
            .evaluate((node) => ({
              keyboard: node.matches(":focus-visible"),
              ring: getComputedStyle(node).outlineStyle,
            })),
        ).toEqual({ keyboard: false, ring: "none" });
        console.log(
          JSON.stringify({
            scheme,
            width,
            evidence,
            controls: visited.length,
            minContrast: Math.min(...visited.map((item) => item.contrast)),
          }),
        );
        await context.close();
      }
  } finally {
    proc.kill("SIGTERM");
  }
});
