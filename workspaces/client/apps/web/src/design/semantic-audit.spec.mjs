import {
  test,
  expect,
  rustUiFixture,
  spawnFixture as spawn,
} from "../../tests/playwright.ts";
import { computedContrast } from "./computedContrast.ts";
import { mkdtemp, mkdir, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";

test("Semantic colors and coarse modal/menu targets in the actual renderer", async ({
  browser,
}, testInfo) => {
  test.setTimeout(120000);
  const root = await mkdtemp(join(tmpdir(), "studio-semantic-audit-"));
  const artifacts =
    process.env.REDESIGN_AUDIT_DIR || testInfo.outputPath("semantic-audit");
  await mkdir(artifacts, { recursive: true });
  const proc = spawn(process.execPath, [rustUiFixture, root], {
    stdio: ["pipe", "pipe", "pipe"],
    env: {
      ...process.env,
      FIXTURE_WORKER_COUNT: "2",
      FIXTURE_AGENT_STEPS: JSON.stringify({
        "Release lead": [
          { fail: "The provider connection failed. Your input is retained." },
        ],
        "Other project": [
          {
            say: "Which scope should I review?",
            item: "audit-question",
            extra: {
              questions: [
                { title: "Which scope?", options: ["One file", "All files"] },
              ],
            },
          },
        ],
      }),
    },
  });
  let log = "";
  proc.stderr.on("data", (chunk) => {
    log += chunk;
  });
  const port = await new Promise((resolve, reject) => {
    proc.stdout.once("data", (chunk) => resolve(Number(String(chunk).trim())));
    proc.once("exit", () => reject(new Error(log)));
  });
  const evidence = [];
  try {
    for (const width of [1440, 390])
      for (const scheme of ["light", "dark"]) {
        const context = await browser.newContext({
          viewport: { width, height: 900 },
          hasTouch: width === 390,
          colorScheme: scheme,
          reducedMotion: "reduce",
        });
        const page = await context.newPage();
        page.setDefaultTimeout(10000);
        page.on("pageerror", (error) =>
          console.error("renderer", error.message),
        );
        const prefix = `${width}-${scheme}`;
        const auditText = async (selector) => {
          const values = [];
          for (const element of await page.locator(selector).all()) {
            if (!(await element.isVisible())) continue;
            const result = await element.evaluate(computedContrast);
            expect(
              result.ratio,
              `${prefix} ${selector}: ${JSON.stringify(result)}`,
            ).toBeGreaterThanOrEqual(4.5);
            values.push(result);
          }
          expect(values.length, selector).toBeGreaterThan(0);
          return values;
        };
        try {
          await page.goto(`http://127.0.0.1:${port}`);
          await page.locator(".workspace").waitFor();
          if (
            width === 390 &&
            !(await page
              .locator("[data-chat]")
              .filter({ hasText: "Release lead" })
              .first()
              .isVisible())
          )
            await page
              .getByRole("button", {
                name: "Toggle conversations",
                exact: true,
              })
              .click();
          await page
            .locator("[data-chat]")
            .filter({ hasText: "Release lead" })
            .first()
            .click();
          await expect(
            page.locator(".native-error > span").first(),
          ).toBeVisible();
          const notices = await auditText(
            ".native-error > span, .native-notice.error summary",
          );
          // The compact header hides the Team label and its count at phone widths.
          const counts =
            width === 1440 ? await auditText(".team-button-count") : [];
          if (width === 390)
            await expect(page.locator(".team-button-count")).toBeHidden();
          await page.screenshot({
            path: join(artifacts, `${prefix}-notices.png`),
          });
          if (width === 390)
            await page
              .getByRole("button", {
                name: "Toggle conversations",
                exact: true,
              })
              .click();
          await page
            .locator("[data-chat]")
            .filter({ hasText: "Other project" })
            .first()
            .click();
          await expect(
            page.locator('.question-card [data-variant="filled"]').first(),
          ).toBeVisible();
          const primary = await auditText(
            '.question-card [data-variant="filled"]',
          );
          await page.screenshot({
            path: join(artifacts, `${prefix}-primary.png`),
          });
          await page
            .getByRole("button", { name: "Chat actions", exact: true })
            .click();
          await expect(page.getByRole("menuitem").first()).toBeVisible();
          const menu = await page.getByRole("menuitem").evaluateAll((items) =>
            items.map((item) => ({
              label: item.textContent,
              width: item.getBoundingClientRect().width,
              height: item.getBoundingClientRect().height,
            })),
          );
          expect(menu.length).toBeGreaterThan(0);
          if (width === 390)
            for (const target of menu) {
              expect
                .soft(target.height, target.label)
                .toBeGreaterThanOrEqual(44);
              expect
                .soft(target.width, target.label)
                .toBeGreaterThanOrEqual(44);
            }
          await page.screenshot({
            path: join(artifacts, `${prefix}-menu.png`),
          });
          await page
            .getByRole("menuitem", { name: "Chat settings", exact: true })
            .click();
          const close = page.locator(".mantine-Modal-close:visible").last();
          await expect(close).toBeVisible();
          const target = await close.boundingBox();
          if (width === 390) {
            expect(target.width).toBeGreaterThanOrEqual(44);
            expect(target.height).toBeGreaterThanOrEqual(44);
          }
          await page.screenshot({
            path: join(artifacts, `${prefix}-modal.png`),
          });
          evidence.push({
            width,
            scheme,
            notices,
            counts,
            primary,
            menu,
            close: target,
          });
        } catch (error) {
          await page.screenshot({
            path: join(artifacts, `${prefix}-failure.png`),
          });
          console.error(await page.locator("body").innerText());
          throw error;
        } finally {
          await context.close();
        }
      }
    await writeFile(
      join(artifacts, "computed-audit.json"),
      JSON.stringify(evidence, null, 2),
    );
    console.log(JSON.stringify(evidence));
  } finally {
    proc.kill("SIGTERM");
  }
});
