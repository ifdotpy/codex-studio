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
          await page.addInitScript(() => {
            localStorage.setItem("studio-local-server-alias-v1", "MAC");
          });
          await stubSessionCost(page);
          await page.goto(`http://127.0.0.1:${port}`);
          await page.getByLabel("Toggle conversations").waitFor();
          if (!(await page.locator("#sidebar").isVisible()))
            await page.getByLabel("Toggle conversations").click();
          await page
            .locator("[data-chat]")
            .filter({ hasText: "Release lead" })
            .first()
            .click();
          const sidebarBadge = page
            .locator("[data-chat]")
            .filter({ hasText: "Release lead" })
            .first()
            .locator(".fleet-server-badge");
          const sidebarAlias = await sidebarBadge.textContent();
          const sidebarOffline =
            (await sidebarBadge.getAttribute("data-offline")) === "true";
          if (width < 760) await expect(page.locator("#sidebar")).toBeHidden();
          await expect(
            page.locator(".conversation-server .chat-server-alias"),
          ).toHaveText(sidebarAlias);
          await expect(page.locator(".conversation-server")).toHaveAttribute(
            "data-offline",
            String(sidebarOffline),
          );
          await expect(
            page.locator(".conversation-server .chat-server-offline"),
          ).toHaveCount(sidebarOffline ? 1 : 0);
          await expect(page.locator("#conversation-title")).toHaveText(
            "Release lead",
          );
          // Opening a completed chat commits read state and loads its transcript
          // independently of the cost request. Measure the post-open header.
          await expect(
            page.locator("#conversation-status"),
          ).toHaveAccessibleName("Read");
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
              status: rect("#conversation-status"),
              server: rect(".conversation-server"),
              statusLabel: (() => {
                const label = document.querySelector(
                  ".conversation-status-label",
                );
                return {
                  visible: !!label?.getClientRects().length,
                  width: label?.parentElement.clientWidth,
                  scrollWidth: label?.parentElement.scrollWidth,
                  dotWidth: document
                    .querySelector(".conversation-status-dot")
                    .getBoundingClientRect().width,
                  accessible: document
                    .querySelector("#conversation-status")
                    .getAttribute("aria-label"),
                };
              })(),
              footer: rect(".usage-footer"),
              estimate: rect(".session-cost-summary"),
              dots: rect(".account-limits-dots"),
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
          assertGeometry(result);
        } catch (error) {
          const failure = join(evidence, `failed-${width}-${scheme}.png`);
          await page.screenshot({ path: failure }).catch(() => {});
          await writeFile(
            join(evidence, `failed-${width}-${scheme}.html`),
            await page.content(),
          );
          await testInfo.attach(`Header/footer ${width} ${scheme}`, {
            path: failure,
          });
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
  expect(r.server.left).toBeGreaterThanOrEqual(r.status.right);
  expect(r.statusLabel.accessible).toBeTruthy();
  if (r.statusLabel.visible)
    expect(r.statusLabel.scrollWidth, JSON.stringify(r)).toBeLessThanOrEqual(
      r.statusLabel.width + 1,
    );
  else expect(r.statusLabel.dotWidth).toBeGreaterThan(0);
  expect(
    Math.abs(
      r.title.top + r.title.height / 2 - r.status.top - r.status.height / 2,
    ),
  ).toBeLessThanOrEqual(1);
  expect(r.dots.width).toBeGreaterThan(0);
  expect(r.dots.height).toBeGreaterThan(0);
  expect(r.estimate.width).toBeGreaterThan(0);
  const centers = r.controls.map((e) => e.top + e.height / 2);
  expect(
    Math.max(...centers) - Math.min(...centers),
    JSON.stringify(r),
  ).toBeLessThanOrEqual(1);
  expect(r.overflow).toBe(false);
}
