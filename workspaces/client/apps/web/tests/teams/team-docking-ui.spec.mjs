import {
  test,
  expect,
  spawnFixture,
  rustUiFixture,
  readTestState,
  stubEntityState,
  readFixtureSyncContract,
} from "../playwright.mjs";
import { mkdtemp, mkdir, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";

test("Team docks beside readable chats and uses a modal sheet below that width", async ({
  page,
}) => {
  test.setTimeout(120_000);
  page.setDefaultTimeout(10000);
  const state = await mkdtemp(join(tmpdir(), "team-docking-"));
  const evidence =
    process.env.TEAM_DOCKING_EVIDENCE ||
    join(tmpdir(), "redesign", "team-v4", "docking");
  await mkdir(evidence, { recursive: true });
  const fixture = spawnFixture(process.execPath, [rustUiFixture, state], {
    stdio: ["ignore", "pipe", "pipe"],
  });
  const port = await new Promise((resolve, reject) => {
    fixture.stdout.once("data", (chunk) =>
      resolve(Number(String(chunk).trim())),
    );
    fixture.once("exit", () =>
      reject(new Error("Team fixture exited before startup")),
    );
  });
  const origin = `http://127.0.0.1:${port}`;
  await stubEntityState(
    page,
    await readTestState(origin),
    await readFixtureSyncContract(origin),
  );
  // Keep the roster fixed while native transcript scopes read fixture messages.
  await page.route("**/api/sync/pull**", (route) =>
    new URL(route.request().url()).searchParams.get("scope") ===
    "state:entities:v1"
      ? route.fallback()
      : route.continue(),
  );
  await page.setViewportSize({ width: 1440, height: 900 });
  await page.goto(`http://127.0.0.1:${port}`);
  await page
    .locator(".project-chats [data-chat]")
    .filter({ hasText: "Release lead" })
    .click();
  await page.locator("#message").fill("Review the release");
  const results = [];
  for (const width of [1440, 1280, 1024, 768, 390]) {
    for (const sidebarOpen of width > 760 ? [true, false] : [false]) {
      await page.setViewportSize({ width, height: 900 });
      const sidebar = page.locator("#sidebar");
      if ((await sidebar.isVisible()) !== sidebarOpen)
        await page
          .getByRole("button", { name: "Toggle conversations", exact: true })
          .click();
      await expect(sidebar).toBeVisible({ visible: sidebarOpen });
      for (const scheme of ["light", "dark"]) {
        await page.emulateMedia({ colorScheme: scheme });
        const contentRatio = scheme === "dark" ? 0.6 : 1;
        await page.evaluate((ratio) => {
          document.documentElement.dataset.studioContentLayout = "custom";
          document.documentElement.style.setProperty(
            "--studio-content-width-ratio",
            String(ratio),
          );
        }, contentRatio);
        if (await page.locator("#team-toggle").isVisible()) {
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
        const frameWidth = await page
          .locator(".workspace-frame")
          .evaluate((node) => node.getBoundingClientRect().width);
        const dockThreshold = await page.evaluate(() => {
          const probe = document.createElement("div");
          probe.style.width =
            "calc(var(--studio-form-max) + var(--studio-team-size))";
          document.body.append(probe);
          const width = probe.getBoundingClientRect().width;
          probe.remove();
          return width;
        });
        const docked = frameWidth >= dockThreshold;
        if (docked) {
          await expect(
            page.getByRole("dialog", { name: "Team", exact: true }),
          ).toHaveCount(0);
          await expect
            .poll(async () =>
              page.locator(".workspace").evaluate((workspace) => {
                const panel = workspace.parentElement.querySelector("#team");
                if (!panel) return false;
                const edge = panel.getBoundingClientRect().left;
                return [
                  ...workspace.querySelectorAll(
                    "#composer, #composer button, .conversation-composer-footer, .workspace-header",
                  ),
                ]
                  .filter((node) => node.getBoundingClientRect().width > 0)
                  .every((node) => node.getBoundingClientRect().right <= edge);
              }),
            )
            .toBe(true);
          const column = await page
            .locator(".workspace")
            .evaluate((workspace) => ({
              width: workspace.getBoundingClientRect().width,
              gutter: parseFloat(
                getComputedStyle(workspace).getPropertyValue("--chat-gutter"),
              ),
              composer: workspace
                .querySelector("#composer")
                .getBoundingClientRect().width,
            }));
          expect(
            Math.abs(
              column.composer -
                (column.width - 2 * column.gutter) * contentRatio,
            ),
          ).toBeLessThan(1);
        } else {
          const dialog = page.getByRole("dialog", {
            name: "Team",
            exact: true,
          });
          await expect(dialog).toHaveAttribute("aria-modal", "true");
          await expect(page.locator(".mantine-Drawer-overlay")).toBeVisible();
          await team.getByRole("button", { name: "Close team" }).focus();
          await page.keyboard.press("Shift+Tab");
          await expect
            .poll(() =>
              dialog.evaluate((node) => node.contains(document.activeElement)),
            )
            .toBe(true);
        }
        await expect
          .poll(() =>
            team.evaluate((node) => {
              const rect = node.getBoundingClientRect();
              const content = node.closest(".mantine-Drawer-content");
              return (
                rect.left >= 0 &&
                rect.right <= innerWidth &&
                (!content ||
                  new DOMMatrix(getComputedStyle(content).transform).isIdentity)
              );
            }),
          )
          .toBe(true);
        results.push({
          width,
          sidebarOpen,
          scheme,
          contentRatio,
          frameWidth,
          docked,
        });
        await page.screenshot({
          path: join(
            evidence,
            `${width}-${sidebarOpen ? "sidebar" : "no-sidebar"}-${scheme}.png`,
          ),
        });
        if (docked)
          await team.getByRole("button", { name: "Close team" }).click();
        else await page.keyboard.press("Escape");
        await expect(team).toBeHidden();
        await expect(page.locator("#message")).toHaveValue(
          "Review the release",
        );
      }
    }
  }
  await writeFile(
    join(evidence, "metrics.json"),
    JSON.stringify(results, null, 2),
  );
});
