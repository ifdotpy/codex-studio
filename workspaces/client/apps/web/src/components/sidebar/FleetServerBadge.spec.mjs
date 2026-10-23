import {
  test,
  expect,
  rustUiFixture,
  spawnFixture as spawn,
  readTestState,
} from "../../../tests/playwright.mjs";
import { mkdir, mkdtemp } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";

// The built renderer uses real API data and disposable fixture state.
test("every fleet strip shows its server without reducing long title width", async ({
  browser,
}) => {
  const root = await mkdtemp(join(tmpdir(), "studio-fleet-server-badge-"));
  const evidence =
    process.env.FLEET_BADGE_EVIDENCE || join(root, "screenshots");
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
    const origin = `http://127.0.0.1:${port}`;
    const state = await readTestState(origin);
    const titles = [
      "Review deployment permissions for all parallel projects and release environments",
      "Обновление интерфейса нескольких параллельных проектов и проверка навигации",
    ];
    for (const [index, agent] of state.runtime.agents.entries()) {
      const response = await fetch(`${origin}/api/rename`, {
        method: "POST",
        headers: {
          "Content-Type": "application/json",
          "X-Canvas-Token": state.token,
          Origin: origin,
        },
        body: JSON.stringify({
          id: agent.id,
          name: titles[index % titles.length],
        }),
      });
      expect(response.ok, await response.text()).toBe(true);
    }
    for (const scheme of ["light", "dark"]) {
      for (const width of [1440, 390]) {
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
        await page.goto(origin);
        await expect(page.locator("#composer")).toBeVisible();
        const sidebar = page.locator("#sidebar");
        if (!(await sidebar.isVisible()))
          await page.locator("#sidebar-toggle").click();
        await expect(sidebar).toBeInViewport({ ratio: 1 });
        const project = sidebar.locator(".project-tree-toggle").first();
        if ((await project.getAttribute("aria-expanded")) === "false")
          await project.click();
        const strips = sidebar.locator(".fleet-strip");
        await expect(strips).toHaveCount(3);
        const measurements = [];
        for (const strip of await strips.all()) {
          const badge = strip.locator(".fleet-server-badge");
          await expect(badge).toBeVisible();
          await expect(badge).toHaveText(/^[A-Z]{1,3}$/);
          await expect(badge).toHaveAccessibleName(/Server [A-Z]{1,3}/);
          const measurement = await strip.evaluate((element) => {
            const title = element.querySelector("strong");
            const badge = element.querySelector(".fleet-server-badge");
            const before = title.getBoundingClientRect();
            const bounds = badge.getBoundingClientRect();
            badge.style.display = "none";
            const withoutBadge = title.getBoundingClientRect().width;
            badge.style.removeProperty("display");
            return {
              title: title.textContent,
              width: before.width,
              withoutBadge,
              titleBottom: before.bottom,
              badgeTop: bounds.top,
              badgeRight: bounds.right,
              rowRight: element.getBoundingClientRect().right,
            };
          });
          expect(measurement.width).toBeCloseTo(measurement.withoutBadge, 2);
          expect(measurement.badgeTop).toBeGreaterThanOrEqual(
            measurement.titleBottom,
          );
          expect(measurement.badgeRight).toBeLessThanOrEqual(
            measurement.rowRight,
          );
          measurements.push(measurement);
        }
        expect(
          await page.evaluate(
            () => document.documentElement.scrollWidth <= innerWidth,
          ),
        ).toBe(true);
        if (width === 1440) await expect(sidebar).toHaveCSS("width", "288px");
        await page.evaluate(() => document.activeElement?.blur());
        await page.screenshot({
          path: join(evidence, `server-badge-real-${scheme}-${width}.png`),
        });
        console.log(JSON.stringify({ scheme, width, measurements, evidence }));
        await context.close();
      }
    }
  } finally {
    proc.kill("SIGTERM");
  }
});
