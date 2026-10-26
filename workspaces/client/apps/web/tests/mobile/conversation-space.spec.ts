import {
  test,
  expect,
  readTestState,
  readFixtureSyncContract,
  stubEntityState,
} from "../playwright.ts";
import { mkdir, writeFile } from "node:fs/promises";
import { join } from "node:path";

test("phone conversation gives the transcript its viewport budget", async ({
  browser,
  readOnlyRust,
}, testInfo) => {
  test.setTimeout(120_000);
  const evidence = testInfo.outputPath("evidence");
  await mkdir(evidence, { recursive: true });
  console.log("Conversation space evidence:", evidence);
  const results = [];
  const origin = readOnlyRust.origin;
  const snapshot = await readTestState(origin);
  const agent = snapshot.threads.find((row) => row.name === "Other project");
  agent.status = "waiting";
  agent.inFlight = false;
  snapshot.runtime.sessionCostsByAgent = {
    [agent.id]: { rootId: agent.id, totalUSD: 0, pricingState: "ready" },
  };
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
        const view = structuredClone(snapshot);
        view.runtime.desktop = {
          ...view.runtime.desktop,
          diskSpace: warning
            ? {
                level: "warning",
                message:
                  "The Mac disk has only 13 GB free. Free disk space to keep agents running.",
              }
            : null,
        };
        await stubEntityState(page, view, contract);
        // Native transcript scopes continue to read the server's messages.
        await page.route("**/api/sync/pull**", (route) =>
          new URL(route.request().url()).searchParams.get("scope") ===
          "state:entities:v1"
            ? route.fallback()
            : route.continue(),
        );
        await readOnlyRust.open(page);
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
        await expect(page.locator("#messages")).toContainText("Separate task");
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
        await expect(
          activity.locator('[data-activity-id="conversation-space-monitor"]'),
        ).toBeVisible();
        await expect(
          page.locator(".simple-workspace-header"),
        ).not.toContainText(/Waiting|Working|Running|monitor/i);
        await expect(page.locator("#conversation-status")).toHaveCount(0);
        await expect(page.locator("#composer .execution-menu")).toHaveCount(1);
        await expect(
          page.locator(".conversation-composer-footer .execution-menu"),
        ).toHaveCount(0);
        await expect(
          page.getByRole("button", { name: "Dictation", exact: true }),
        ).toBeVisible();
        if (width < 760) {
          for (const target of [
            page.getByRole("button", { name: "Dictation", exact: true }),
            page.locator(".composer-tools-disclosure > summary"),
            page.locator("#composer .execution-summary-trigger"),
          ]) {
            const box = await target.boundingBox();
            expect(box.width).toBeGreaterThanOrEqual(44);
            expect(box.height).toBeGreaterThanOrEqual(44);
          }
          for (const target of [
            page.getByRole("button", { name: "Chat context", exact: true }),
            page.locator(".session-cost-summary"),
            page.getByRole("button", { name: "Account limits", exact: true }),
            ...(await page.locator(".account-limits-dot-target").all()),
          ]) {
            const box = await target.boundingBox();
            expect(box.width).toBeGreaterThanOrEqual(24);
            expect(box.height).toBeGreaterThanOrEqual(24);
          }
          await expect(page.locator(".account-limits-dots")).toBeVisible();
          await page
            .getByRole("button", { name: "Account limits", exact: true })
            .click();
          await expect(
            page.getByRole("region", { name: "Account limits details" }),
          ).toBeVisible();
          await page.keyboard.press("Escape");
          await expect(page.locator(".account-limits-popover")).toBeHidden();
          const footerFacts = await page
            .locator(".usage-footer")
            .evaluate((e) =>
              [
                ".context-use",
                ".session-cost-summary",
                ".account-limits-toggle",
              ].map((s) => e.querySelector(s).getBoundingClientRect().toJSON()),
            );
          for (let i = 1; i < footerFacts.length; i++)
            expect(
              footerFacts[i].left - footerFacts[i - 1].right,
            ).toBeGreaterThanOrEqual(8);
        }
        if (warning) {
          const notice = page.locator(".supervisor-recovery-notice");
          await expect(notice).toHaveAttribute("title", /13 GB free/);
          expect((await notice.boundingBox()).height).toBeLessThanOrEqual(44);
        } else
          await expect(page.locator(".supervisor-recovery-notice")).toHaveCount(
            0,
          );
        if (width < 760) {
          // Attach stays one tap away beside the summary at every phone width.
          const attach = page.getByRole("button", {
            name: "Attach files",
            exact: true,
          });
          await expect(attach).toHaveCount(1);
          await expect(attach).toBeVisible();
          const attachBox = await attach.boundingBox();
          expect(attachBox.width).toBeGreaterThanOrEqual(44);
          expect(attachBox.height).toBeGreaterThanOrEqual(44);
          // Only the lead provider remains beside its name on phones.
          const symbolRows = await page
            .locator("#composer .execution-symbols")
            .evaluateAll((nodes) =>
              nodes
                .filter((node) => node.getClientRects().length)
                .map((node) =>
                  [...node.children].map(
                    (child) => child.getBoundingClientRect().top,
                  ),
                ),
            );
          expect(symbolRows).toHaveLength(1);
          await expect(page.locator(".execution-summary-more")).toHaveText(
            "+2",
          );
          for (const tops of symbolRows)
            expect(Math.max(...tops) - Math.min(...tops)).toBeLessThanOrEqual(
              1,
            );
          const disclosure = page.locator(
            ".composer-tools-disclosure > summary",
          );
          await disclosure.click();
          const toolsBounds = await page
            .locator(".composer-tools-popover")
            .boundingBox();
          expect(toolsBounds.x).toBeGreaterThanOrEqual(0);
          expect(toolsBounds.x + toolsBounds.width).toBeLessThanOrEqual(width);

          // Both phone widths keep Queue in the open tools disclosure.
          for (const label of ["Queue for next turn", "Send message"]) {
            const button = page.getByRole("button", {
              name: label,
              exact: true,
            });
            const bounds = await button.boundingBox();
            expect(bounds.width).toBeGreaterThanOrEqual(44);
            expect(bounds.height).toBeGreaterThanOrEqual(44);
          }
          await expect(page.locator(".composer-queue-primary")).toBeHidden();
          await expect(
            page.locator(".composer-queue-disclosure"),
          ).toBeVisible();
          await disclosure.click();
          // The lead name remains visible and stays inside the trigger.
          const model = await page
            .locator(
              '.execution-summary-segment[data-role="lead"] .execution-model',
            )
            .boundingBox();
          const execution = await page
            .locator(".execution-summary-trigger")
            .boundingBox();
          expect(model).not.toBeNull();
          expect(model.x + model.width).toBeLessThanOrEqual(
            execution.x + execution.width,
          );
          await expect(
            page.locator(".execution-summary-trigger"),
          ).toHaveAttribute("aria-label", /Lead \S/);
          const controls = await page.locator("#composer").evaluate((e) =>
            [
              ...e.querySelectorAll(
                "button, .composer-tools-disclosure > summary",
              ),
            ]
              .filter(
                (b) =>
                  b.getClientRects().length &&
                  !b.closest(".composer-tools-popover"),
              )
              .map((b) => ({
                name: b.getAttribute("aria-label"),
                ...b.getBoundingClientRect().toJSON(),
              })),
          );
          for (let i = 0; i < controls.length; i++)
            for (let j = i + 1; j < controls.length; j++) {
              const a = controls[i],
                b = controls[j];
              expect(
                Math.min(a.right, b.right) - Math.max(a.left, b.left) > 0 &&
                  Math.min(a.bottom, b.bottom) - Math.max(a.top, b.top) > 0,
                JSON.stringify({ a, b }),
              ).toBe(false);
            }
        } else {
          await expect(page.locator(".composer-tools-disclosure")).toBeHidden();
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
      result.width < 760 ? 24 : 32,
    );
    expect(result.estimate.width).toBeGreaterThan(0);
    expect(
      Math.abs(result.activity.top - result.header.bottom),
    ).toBeLessThanOrEqual(1);
    expect(result.messages.top).toBeGreaterThanOrEqual(result.activity.bottom);
  }
});
