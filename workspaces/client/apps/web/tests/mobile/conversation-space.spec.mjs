import {
  test,
  expect,
  spawnFixture,
  rustUiFixture,
  stubSessionCost,
  readTestState,
  readFixtureSyncContract,
  stubEntityState,
} from "../playwright.mjs";
import { mkdtemp, mkdir, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";

test("phone conversation gives the transcript its viewport budget", async ({
  browser,
}) => {
  test.setTimeout(120_000);
  const state = await mkdtemp(join(tmpdir(), "studio-conversation-space-"));
  const evidence =
    process.env.CONVERSATION_SPACE_EVIDENCE || join(state, "evidence");
  await mkdir(evidence, { recursive: true });
  console.log("Conversation space evidence:", evidence);
  const fixture = spawnFixture(process.execPath, [rustUiFixture, state], {
    stdio: ["ignore", "pipe", "pipe"],
    env: { FIXTURE_HOLD: "other" },
  });
  let log = "";
  fixture.stderr.on("data", (chunk) => {
    log += chunk;
  });
  const results = [];
  try {
    const port = await new Promise((resolve, reject) => {
      fixture.stdout.once("data", (chunk) =>
        resolve(Number(String(chunk).trim())),
      );
      fixture.once("exit", () => reject(new Error(log)));
    });
    const origin = `http://127.0.0.1:${port}`;
    const snapshot = await readTestState(origin);
    const agent = snapshot.threads.find((row) => row.name === "Other project");
    // One deterministic active monitor exercises the real activity-strip caller.
    // All other entity values and transcript reads come from the isolated server.
    snapshot.runtime.monitors = [
      ...(snapshot.runtime.monitors || []),
      {
        id: "conversation-space-monitor",
        agent: agent.id,
        status: "running",
        command: "Check the release build",
        created: Date.now() / 1000,
      },
    ];
    const contract = await readFixtureSyncContract(origin);
    for (const { width, height, minimum, warning = false } of [
      { width: 390, height: 844, minimum: 0.75 },
      { width: 320, height: 568, minimum: 0.6 },
      { width: 1440, height: 900, minimum: 0.7 },
      { width: 390, height: 844, minimum: 0.7, warning: true },
    ]) {
      for (const scheme of ["light", "dark"]) {
        const page = await browser.newPage({
          viewport: { width, height },
          isMobile: width < 760,
          hasTouch: width < 760,
          colorScheme: scheme,
        });
        try {
          await stubSessionCost(page);
          await stubEntityState(page, snapshot, contract);
          // Native transcript scopes continue to read the server's messages.
          await page.route("**/api/sync/pull**", (route) =>
            new URL(route.request().url()).searchParams.get("scope") ===
            "state:entities:v1"
              ? route.fallback()
              : route.continue(),
          );
          await page.route(
            (url) => url.pathname === "/api/desktop",
            async (route) => {
              const response = await route.fetch();
              const data = await response.json();
              await route.fulfill({
                json: {
                  ...data,
                  diskSpace: warning
                    ? {
                        ...data.diskSpace,
                        level: "warning",
                        message:
                          "The Mac disk has only 13 GB free. Free disk space to keep agents running.",
                      }
                    : null,
                },
              });
            },
          );
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
            .filter({ hasText: "Other project" })
            .first()
            .click();
          if (width < 760) await expect(page.locator("#sidebar")).toBeHidden();
          await expect(page.locator("#conversation-title")).toHaveText(
            "Other project",
          );
          await expect(page.locator("#messages")).toContainText(
            "Separate task",
          );
          await expect(page.locator(".session-cost-summary")).toHaveAttribute(
            "aria-busy",
            "false",
          );
          await expect(page.locator(".session-cost-summary")).toBeVisible();
          await expect(
            page.locator(".session-cost-summary"),
          ).toHaveAccessibleName(/Session estimate: \$/);
          await page.evaluate(
            (value) =>
              document.documentElement.setAttribute(
                "data-mantine-color-scheme",
                value,
              ),
            scheme,
          );
          const activity = page.getByRole("region", {
            name: "Session activity",
          });
          await expect(activity).toBeVisible();
          await expect(page.locator("#composer .execution-menu")).toHaveCount(
            1,
          );
          await expect(
            page.locator(".conversation-composer-footer .execution-menu"),
          ).toHaveCount(0);
          await expect(
            page.getByRole("button", { name: "Dictation", exact: true }),
          ).toBeVisible();
          if (warning) {
            const notice = page.locator(".supervisor-recovery-notice");
            await expect(notice).toHaveAttribute("title", /13 GB free/);
            expect((await notice.boundingBox()).height).toBeLessThanOrEqual(44);
          } else
            await expect(
              page.locator(".supervisor-recovery-notice"),
            ).toHaveCount(0);
          if (width < 760) {
            await expect(
              page.getByRole("button", { name: "Attach files", exact: true }),
            ).toHaveCount(0);
            const disclosure = page.locator(
              ".composer-tools-disclosure > summary",
            );
            await disclosure.click();
            await expect(
              page.getByRole("button", { name: "Attach files", exact: true }),
            ).toHaveCount(1);
            const toolsBounds = await page
              .locator(".composer-tools-popover")
              .boundingBox();
            expect(toolsBounds.x).toBeGreaterThanOrEqual(0);
            expect(toolsBounds.x + toolsBounds.width).toBeLessThanOrEqual(
              width,
            );
            if (width > 320) await disclosure.click();
            for (const label of ["Queue for next turn", "Send message"]) {
              const button = page.getByRole("button", {
                name: label,
                exact: true,
              });
              const bounds = await button.boundingBox();
              expect(bounds.width).toBeGreaterThanOrEqual(44);
              expect(bounds.height).toBeGreaterThanOrEqual(44);
            }
            if (width <= 320) {
              await expect(
                page.locator(".composer-queue-primary"),
              ).toBeHidden();
              await expect(
                page.locator(".composer-queue-disclosure"),
              ).toBeVisible();
              await disclosure.click();
            }
          } else {
            await expect(
              page.locator(".composer-tools-disclosure"),
            ).toBeHidden();
            await expect(
              page.getByRole("button", { name: "Attach files", exact: true }),
            ).toBeVisible();
          }
          const geometry = await page.evaluate(() => {
            const rect = (selector) =>
              document.querySelector(selector).getBoundingClientRect().toJSON();
            return {
              header: rect(".simple-workspace-header"),
              messages: rect("#messages"),
              activity: rect(".session-activity"),
              composer: rect("#composer"),
              footer: rect(".conversation-composer-footer"),
              estimate: rect(".session-cost-summary"),
              footerHeight: rect(".usage-footer").height,
              overflow: document.documentElement.scrollWidth > innerWidth,
            };
          });
          const result = {
            width,
            height,
            scheme,
            minimum,
            warning,
            ratio: geometry.messages.height / height,
            ...geometry,
          };
          results.push(result);
          await expect(page.locator("#toast")).toBeHidden({ timeout: 10_000 });
          await page.mouse.move(0, 0);
          await page.screenshot({
            path: join(
              evidence,
              `active-${width}-${height}-${scheme}${warning ? "-warning" : ""}.png`,
            ),
          });
        } finally {
          await page.context().close();
        }
      }
    }
    await writeFile(
      join(evidence, "geometry.json"),
      JSON.stringify(results, null, 2),
    );
    console.log(
      JSON.stringify(
        results.map(
          ({ width, height, scheme, ratio, header, composer, footer }) => ({
            width,
            height,
            scheme,
            ratio,
            header: header.height,
            composer: composer.height,
            footer: footer.height,
          }),
        ),
      ),
    );
    for (const result of results) {
      expect(result.ratio, JSON.stringify(result)).toBeGreaterThanOrEqual(
        result.minimum,
      );
      expect(result.overflow).toBe(false);
      expect(result.footerHeight).toBeLessThanOrEqual(
        result.width < 760 ? 20 : 32,
      );
      expect(result.estimate.width).toBeGreaterThan(0);
      expect(
        Math.abs(result.activity.top - result.header.bottom),
      ).toBeLessThanOrEqual(1);
      expect(result.messages.top).toBeGreaterThanOrEqual(
        result.activity.bottom,
      );
    }
  } finally {
    await writeFile(join(evidence, "fixture.log"), log);
    fixture.kill("SIGTERM");
  }
});
