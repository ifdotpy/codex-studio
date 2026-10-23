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
}) => {
  test.setTimeout(90_000);
  const state = await mkdtemp(join(tmpdir(), "studio-header-footer-"));
  const evidence =
    process.env.CONVERSATION_HEADER_FOOTER_EVIDENCE || join(state, "evidence");
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
          const sidebarServer = page
            .locator("[data-chat]")
            .filter({ hasText: "Release lead" })
            .first()
            .locator(".chat-server-line");
          // Default-server rows omit their redundant badge; the local alias is
          // still the shared source used by both the sidebar and the header.
          const sidebarAlias = (await sidebarServer.count())
            ? await sidebarServer.locator(".chat-server-alias").textContent()
            : await page.evaluate(() =>
                localStorage.getItem("studio-local-server-alias-v1"),
              );
          const sidebarOffline = await sidebarServer
            .locator(".chat-server-offline")
            .count();
          if (width < 760) await expect(page.locator("#sidebar")).toBeHidden();
          await expect(
            page.locator(".conversation-server .chat-server-alias"),
          ).toHaveText(sidebarAlias);
          await expect(
            page.locator(".conversation-server .chat-server-offline"),
          ).toHaveCount(sidebarOffline);
          await expect(page.locator("#conversation-title")).toHaveText(
            "Release lead",
          );
          await expect(page.locator(".session-cost-summary")).toHaveAttribute(
            "aria-busy",
            "false",
          );
          await page.evaluate(
            (s) =>
              document.documentElement.setAttribute(
                "data-mantine-color-scheme",
                s,
              ),
            scheme,
          );
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
                  width: label?.clientWidth,
                  scrollWidth: label?.scrollWidth,
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
          results.push({ width, scheme, ...geometry });
          await page.screenshot({
            path: join(evidence, `chrome-${width}-${scheme}.png`),
          });
        } finally {
          await page.context().close();
        }
      }
    await writeFile(
      join(evidence, "geometry.json"),
      JSON.stringify(results, null, 2),
    );
    for (const r of results) {
      expect(r.header.height, JSON.stringify(r)).toBeLessThanOrEqual(45);
      expect(r.teamStrip).toBe(false);
      expect(r.server.width).toBeGreaterThan(0);
      expect(r.server.left).toBeGreaterThanOrEqual(r.status.right);
      if (r.statusLabel.visible)
        expect(r.statusLabel.scrollWidth).toBeLessThanOrEqual(
          r.statusLabel.width + 1,
        );
      expect(
        Math.abs(
          r.title.top + r.title.height / 2 - r.status.top - r.status.height / 2,
        ),
      ).toBeLessThanOrEqual(1);
      expect(r.dots.width).toBeGreaterThan(0);
      expect(r.dots.height).toBeGreaterThan(0);
      expect(r.estimate.width).toBeGreaterThanOrEqual(r.width === 320 ? 0 : 1);
      if (r.width === 320) expect(r.estimate.width).toBe(0);
      const centers = r.controls.map((e) => e.top + e.height / 2);
      expect(
        Math.max(...centers) - Math.min(...centers),
        JSON.stringify(r),
      ).toBeLessThanOrEqual(1);
      expect(r.overflow).toBe(false);
    }
    console.log("Header/footer evidence:", evidence);
  } finally {
    fixture.kill("SIGTERM");
  }
});
