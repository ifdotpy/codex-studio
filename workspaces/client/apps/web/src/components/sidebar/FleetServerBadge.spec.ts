import { waitForFixturePort } from "../../../tests/fixture-readiness.ts";
import {
  test,
  expect,
  rustUiFixture,
  spawnFixture as spawn,
  readTestState,
} from "../../../tests/playwright.ts";
import { mkdir, mkdtemp } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";

// The built renderer uses real API data and disposable fixture state.
test("default-server strips maximize title width and reveal actions on hover or focus", async ({
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
      FIXTURE_WORKER_COUNT: "5",
    },
  });
  let errors = "";
  proc.stderr?.on("data", (chunk) => {
    errors += chunk;
  });
  try {
    const port = await waitForFixturePort(proc, { diagnostics: () => errors });
    const origin = `http://127.0.0.1:${port}`;
    const state = await readTestState(origin);
    const lead = state.runtime.agents.find(
      (agent) => agent.name === "Release lead",
    );
    if (!lead) throw new Error("Missing release lead");
    const titles = [
      "Review deployment permissions for all parallel projects and release environments",
      "Обновление интерфейса нескольких параллельных проектов и проверка навигации",
    ];
    for (const [index, agent] of state.runtime.agents
      .filter((agent) => agent.isLead)
      .entries()) {
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
    for (const scheme of ["light", "dark"] as const) {
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
          await expect(strip.locator(".fleet-server-badge")).toHaveCount(0);
          await expect(strip).toHaveCSS("box-shadow", "none");
          const measurement = await strip.evaluate((element) => {
            const title = element.querySelector("strong");
            if (!title) throw new Error("Missing fleet title");
            return {
              title: title.textContent,
              width: title.getBoundingClientRect().width,
              rowWidth: element.getBoundingClientRect().width,
            };
          });
          expect(measurement.width).toBeGreaterThan(
            measurement.rowWidth * 0.65,
          );
          measurements.push(measurement);
        }
        expect(
          await page.evaluate(
            () => document.documentElement.scrollWidth <= innerWidth,
          ),
        ).toBe(true);
        if (width === 1440) {
          const sidebarWidth = await sidebar.evaluate(
            (node) => node.getBoundingClientRect().width,
          );
          expect(sidebarWidth).toBeCloseTo(18 * 1.1 * 16, 1);
          const idle = strips
            .filter({ has: page.locator('.chat-row[aria-current="false"]') })
            .last();
          const menu = idle.locator(".row-actions");
          await page.mouse.move(1400, 900);
          await page.evaluate(
            () =>
              document.activeElement instanceof HTMLElement &&
              document.activeElement.blur(),
          );
          await expect(menu).toHaveCSS("opacity", "0");
          const idleTitle = await idle.locator("strong").boundingBox();
          if (!idleTitle) throw new Error("Missing idle title bounds");
          const idleTitleWidth = idleTitle.width;
          await idle.hover();
          await expect(menu).toHaveCSS("opacity", "1");
          expect(
            (await idle.locator("strong").boundingBox())?.width,
          ).toBeLessThan(idleTitleWidth);
          const titleBounds = await idle.locator("strong").boundingBox();
          const menuBounds = await menu.boundingBox();
          if (!titleBounds || !menuBounds)
            throw new Error("Missing menu bounds");
          expect(titleBounds.x + titleBounds.width).toBeLessThanOrEqual(
            menuBounds.x,
          );
          await page.mouse.move(1400, 900);
          await menu.focus();
          await expect(menu).toHaveCSS("opacity", "1");
          await page.keyboard.press("Enter");
          await expect(page.getByRole("menu")).toBeVisible();
          await page.keyboard.press("Escape");
        }
        await page.evaluate(
          () =>
            document.activeElement instanceof HTMLElement &&
            document.activeElement.blur(),
        );
        await page.screenshot({
          path: join(evidence, `server-badge-real-${scheme}-${width}.png`),
        });
        // The same built renderer also hosts Team at desktop and phone widths.
        await sidebar
          .locator(`.sidebar-project [data-chat="${lead.id}"]`)
          .click();
        if (width === 1440) {
          await page.locator("#team-toggle").click();
        } else {
          await page
            .getByRole("button", { name: "Chat actions", exact: true })
            .click();
          await page
            .getByRole("menuitem", { name: "Team", exact: true })
            .click();
        }
        const team = page.getByRole("complementary", {
          name: "Team",
          exact: true,
        });
        await expect(team).toBeVisible();
        await team
          .getByRole("button", { name: "Finished (5)", exact: true })
          .click();
        await expect(team.locator(".worker-entry").first()).toBeVisible();
        for (const worker of await team.locator(".worker-entry").all())
          await expect(worker).toHaveCSS("box-shadow", "none");
        await expect(team).toBeInViewport({ ratio: 1 });
        await page.screenshot({
          path: join(evidence, `team-real-${scheme}-${width}.png`),
        });
        console.log(JSON.stringify({ scheme, width, measurements, evidence }));
        await context.close();
      }
    }
  } finally {
    proc.kill("SIGTERM");
  }
});
