import {
  test,
  expect,
  spawnFixture,
  rustUiFixture,
  stubSessionCost,
} from "../playwright.mjs";
import { mkdtemp, mkdir, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";

test("conversation header stays compact and usage facts remain visible", async ({
  browser,
}, testInfo) => {
  test.setTimeout(90_000);
  const state = await mkdtemp(join(tmpdir(), "studio-header-footer-"));
  const evidence = join(
    process.env.CONVERSATION_HEADER_FOOTER_EVIDENCE || join(state, "evidence"),
    `repeat-${testInfo.repeatEachIndex}`,
  );
  await mkdir(evidence, { recursive: true });
  const fixture = spawnFixture(process.execPath, [rustUiFixture, state], {
    stdio: ["ignore", "pipe", "pipe"],
  });
  let log = "";
  fixture.stderr.on("data", (chunk) => (log += chunk));
  const results = [];
  const serverAlias = "MAC";
  try {
    const port = await new Promise((resolve, reject) => {
      fixture.stdout.once("data", (chunk) =>
        resolve(Number(String(chunk).trim())),
      );
      fixture.once("exit", () => reject(Error(log)));
    });
    for (const width of [1440, 1024, 390, 320])
      for (const scheme of ["light", "dark"]) {
        const page = await browser.newPage({
          viewport: { width, height: 900 },
          isMobile: width < 760,
          hasTouch: width < 760,
          colorScheme: scheme,
        });
        try {
          await page.addInitScript((alias) => {
            localStorage.setItem("studio-local-server-alias-v1", alias);
          }, serverAlias);
          await stubSessionCost(page);
          await page.goto(`http://127.0.0.1:${port}`);
          await page
            .getByLabel(
              page.viewportSize().width <= 760
                ? "Back to chats"
                : "Toggle conversations",
            )
            .waitFor();
          if (!(await page.locator("#sidebar").isVisible()))
            await page
              .getByLabel(
                page.viewportSize().width <= 760
                  ? "Back to chats"
                  : "Toggle conversations",
              )
              .click();
          await page
            .locator("[data-chat]")
            .filter({ hasText: "Release lead" })
            .first()
            .click();
          if (width < 760) await expect(page.locator("#sidebar")).toBeHidden();
          await expect(
            page.locator(".conversation-server .chat-server-alias"),
          ).toHaveText(serverAlias);
          await expect(page.locator(".conversation-server")).toHaveAttribute(
            "data-offline",
            "false",
          );
          await expect(
            page.locator(".conversation-server .chat-server-offline"),
          ).toHaveCount(0);
          await expect(page.locator("#conversation-title")).toHaveText(
            "Release lead",
          );
          await expect(page.locator("#conversation-status")).toHaveCount(0);
          await expect(page.locator("#messages")).toContainText(
            "Review the release",
          );
          const tools = page.locator(
            "#conversation-header-tools .conversation-header-tools-menu",
          );
          await expect(tools).toHaveAttribute(
            "data-compact",
            width === 1440 ? "no" : "yes",
          );
          await expect(
            tools.locator(".prompt-history-toggle-label"),
          ).toHaveText("1 / 1");
          if (width >= 760)
            await expect(
              page.locator('.account-limits-dot-target[aria-current="true"]'),
            ).toBeVisible();
          await expect(
            page.getByRole("button", { name: "Account limits", exact: true }),
          ).toBeVisible();
          await expect(page.locator(".session-cost-summary")).toHaveAttribute(
            "aria-busy",
            "false",
          );
          await expect(page.locator(".session-cost-summary")).toBeVisible();
          await expect(page.locator(".session-cost-summary")).toHaveAttribute(
            "title",
            /Session estimate: \$/,
          );
          await expect(
            page.locator(".session-cost-summary"),
          ).toHaveAccessibleName(/Session estimate: \$/);
          await page.evaluate(
            (s) =>
              document.documentElement.setAttribute(
                "data-mantine-color-scheme",
                s,
              ),
            scheme,
          );
          await page.evaluate(() => document.fonts.ready);
          const geometry = await page.evaluate(() => {
            const rect = (s) =>
              document.querySelector(s).getBoundingClientRect().toJSON();
            return {
              header: rect(".simple-workspace-header"),
              title: rect("#conversation-title"),
              server: rect(".conversation-server"),
              footer: rect(".usage-footer"),
              estimate: rect(".session-cost-summary"),
              dots: rect(".account-limits-dots"),
              dotTargets: [
                ...document.querySelectorAll(".account-limits-dot-target"),
              ].map((element) => element.getBoundingClientRect().toJSON()),
              limits: rect(".account-limits-toggle"),
              controls: [
                ...document.querySelectorAll(
                  ".usage-footer >*, .account-limits-toggle",
                ),
              ]
                .filter(
                  (e) =>
                    e.getClientRects().length &&
                    getComputedStyle(e).display !== "none",
                )
                .map((e) => e.getBoundingClientRect().toJSON()),
              teamStrip: !!document.querySelector(".conversation-team-strip"),
              overflow: document.documentElement.scrollWidth > innerWidth,
            };
          });
          const result = { width, scheme, ...geometry };
          results.push(result);
          await page.screenshot({
            path: join(evidence, `chrome-${width}-${scheme}.png`),
          });
          await page.locator(".simple-workspace-header").screenshot({
            path: join(evidence, `header-${width}-${scheme}.png`),
          });
          await page.locator(".usage-footer").screenshot({
            path: join(evidence, `footer-${width}-${scheme}.png`),
          });
          assertGeometry(result);
          const accountDot = page.locator(
            '.account-limits-dot-target[aria-current="true"]',
          );
          await accountDot.focus();
          await page.keyboard.press("Enter");
          await expect(
            page.getByRole("region", { name: "Account limits details" }),
          ).toBeVisible();
          await expect(accountDot).toHaveAttribute("aria-expanded", "true");
          await page.keyboard.press("Escape");
          await expect(page.locator(".account-limits-popover")).toBeHidden();
          await page
            .getByRole("button", { name: "Chat context", exact: true })
            .click();
          await expect(
            page.getByText("No context data yet.", { exact: true }),
          ).toBeVisible();
          await page.keyboard.press("Escape");
        } catch (error) {
          const failure = join(evidence, `failed-${width}-${scheme}.png`);
          console.error(
            `Header/footer ${width} ${scheme} failed:`,
            error instanceof Error ? error.stack : String(error),
          );
          // Timeout teardown may already have closed the page. Evidence must
          // never replace the assertion or operation that actually failed.
          await Promise.allSettled([
            page.screenshot({ path: failure }).then(() =>
              testInfo.attach(`Header/footer ${width} ${scheme}`, {
                path: failure,
              }),
            ),
            page
              .content()
              .then((html) =>
                writeFile(
                  join(evidence, `failed-${width}-${scheme}.html`),
                  html,
                ),
              ),
          ]);
          throw error;
        } finally {
          await page.context().close();
        }
      }
    console.log("Header/footer evidence:", evidence);
  } finally {
    await writeFile(
      join(evidence, "geometry.json"),
      JSON.stringify(results, null, 2),
    );
    await writeFile(join(evidence, "fixture.log"), log);
    console.log("Header/footer artifacts:", evidence);
    fixture.kill("SIGTERM");
  }
});

function assertGeometry(r) {
  expect(r.header.height, JSON.stringify(r)).toBeLessThanOrEqual(45);
  expect(r.teamStrip).toBe(false);
  expect(r.server.width).toBeGreaterThan(0);
  expect(r.server.left).toBeGreaterThanOrEqual(r.title.right);
  expect(r.limits.width).toBeGreaterThan(0);
  expect(r.dots.width).toBeGreaterThan(0);
  expect(r.dots.height).toBeGreaterThan(0);
  for (const [index, target] of r.dotTargets.entries()) {
    expect(target.width).toBeGreaterThanOrEqual(r.width < 760 ? 24 : 12);
    expect(target.height).toBeGreaterThanOrEqual(24);
    if (index)
      expect(target.left).toBeGreaterThanOrEqual(r.dotTargets[index - 1].right);
  }

  expect(r.estimate.width).toBeGreaterThan(0);
  const centers = r.controls.map((e) => e.top + e.height / 2);
  expect(
    Math.max(...centers) - Math.min(...centers),
    JSON.stringify(r),
  ).toBeLessThanOrEqual(1);
  expect(r.overflow).toBe(false);
}
