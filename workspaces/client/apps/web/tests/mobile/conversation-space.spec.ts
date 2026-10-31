import assert from "node:assert/strict";
import type { EntityFixtureState, TestStateView } from "../playwright.ts";
import {
  test,
  expect,
  readTestState,
  readFixtureSyncContract,
  stubEntityState,
  handleEntitySyncFixtureRequest,
} from "../playwright.ts";
import { mkdir, writeFile } from "node:fs/promises";
import { join } from "node:path";
import { createServer } from "node:http";

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
  const snapshot: EntityFixtureState & Pick<TestStateView, "threads"> =
    await readTestState(origin);
  snapshot.runtime.desktop = await (
    await fetch(new URL("/api/desktop", origin))
  ).json();
  const agent = snapshot.threads.find((row) => row.name === "Other project")!;
  agent.status = "waiting";
  agent.inFlight = false;
  snapshot.runtime.sessionCostsByAgent = {
    [agent.id]: {
      rootId: agent.id,
      totalUSD: 0,
      pricingState: "ready",
    },
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
    for (const scheme of ["light", "dark"] as const) {
      const page = await browser.newPage({
        viewport: { width, height },
        isMobile: width < 760,
        hasTouch: width < 760,
        colorScheme: scheme,
      });
      let stream: ReturnType<typeof createServer> | undefined;
      try {
        const view = structuredClone(snapshot);
        assert.ok(view.runtime.desktop);

        view.runtime.desktop = {
          ...view.runtime.desktop!,
          diskSpace: warning
            ? {
                level: "warning",
                message:
                  "The device has only 13 GB free. Free disk space to keep agents running.",
              }
            : null,
        };
        const fixtureStream = createServer(async (request, response) => {
          response.setHeader("Access-Control-Allow-Origin", "*");
          response.setHeader("Access-Control-Allow-Headers", "*");
          if (request.method === "OPTIONS") {
            response.writeHead(204).end();
            return;
          }
          let body = "";
          if (request.method === "POST")
            for await (const chunk of request) body += chunk;
          if (
            !handleEntitySyncFixtureRequest(request, response, {
              snapshot: view,
              workspaceId: contract.identity.workspaceId,
              versionedResources: true,
              ...(body ? { subscriptionUpdate: JSON.parse(body) } : {}),
            })
          )
            response.writeHead(404).end();
        });
        stream = fixtureStream;
        await new Promise<void>((resolve) =>
          fixtureStream.listen(0, "127.0.0.1", resolve),
        );
        const streamPort = (
          fixtureStream.address() as import("node:net").AddressInfo
        ).port;
        await stubEntityState(page, view, contract);
        for (const route of [
          "**/api/sync/stream?*",
          "**/api/sync/subscriptions",
        ])
          await page.route(route, (request) => {
            const url = new URL(request.request().url());
            return request.continue({
              url: `http://127.0.0.1:${streamPort}${url.pathname}${url.search}`,
            });
          });
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
            page.viewportSize()!.width <= 760
              ? "Back to chats"
              : "Toggle conversations",
          )
          .waitFor();
        if (!(await page.locator("#sidebar").isVisible()))
          await page
            .getByLabel(
              page.viewportSize()!.width <= 760
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
            const box = (await target.boundingBox())!;
            expect(box.width).toBeGreaterThanOrEqual(44);
            expect(box.height).toBeGreaterThanOrEqual(44);
          }
          for (const target of [
            page.getByRole("button", { name: "Chat context", exact: true }),
            page.locator(".session-cost-summary"),
            page.getByRole("button", { name: "Account limits", exact: true }),
            ...(await page.locator(".account-limits-dot-target").all()),
          ]) {
            const box = (await target.boundingBox())!;
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
              ].map(
                (s) =>
                  e
                    .querySelector(s)!
                    .getBoundingClientRect()
                    .toJSON() as DOMRect,
              ),
            );
          for (let i = 1; i < footerFacts.length; i++) {
            const current = footerFacts[i],
              previous = footerFacts[i - 1];
            assert.ok(current && previous);
            expect(current.left - previous.right).toBeGreaterThanOrEqual(8);
          }
        }
        if (warning) {
          const notice = page.locator(".supervisor-recovery-notice");
          await expect(notice).toHaveAttribute("title", /13 GB free/);
          expect((await notice.boundingBox())!.height).toBeLessThanOrEqual(44);
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
          const attachBox = (await attach.boundingBox())!;
          expect(attachBox.width).toBeGreaterThanOrEqual(44);
          expect(attachBox.height).toBeGreaterThanOrEqual(44);
          // The lead stays visible; the control names all roles accessibly.
          await expect(
            page.locator('.execution-summary-segment[data-role="lead"]'),
          ).toBeVisible();
          for (const role of ["worker", "review"])
            await expect(
              page.locator(`.execution-summary-segment[data-role="${role}"]`),
            ).toBeHidden();
          await expect(
            page.locator(".execution-summary-trigger"),
          ).toHaveAccessibleName(/Workers.*Review/);
          const disclosure = page.locator(
            ".composer-tools-disclosure > summary",
          );
          await disclosure.click();
          const toolsBounds = (await page
            .locator(".composer-tools-popover")
            .boundingBox())!;
          expect(toolsBounds.x).toBeGreaterThanOrEqual(0);
          expect(toolsBounds.x + toolsBounds.width).toBeLessThanOrEqual(width);

          // Both phone widths keep Queue in the open tools disclosure.
          for (const label of ["Queue for next turn", "Send message"]) {
            const button = page.getByRole("button", {
              name: label,
              exact: true,
            });
            const bounds = (await button.boundingBox())!;
            expect(bounds.width).toBeGreaterThanOrEqual(44);
            expect(bounds.height).toBeGreaterThanOrEqual(44);
          }
          await expect(page.locator(".composer-queue-primary")).toBeHidden();
          await expect(
            page.locator(".composer-queue-disclosure"),
          ).toBeVisible();
          await disclosure.click();
          // The lead name remains visible and stays inside the trigger.
          const model = (await page
            .locator(
              '.execution-summary-segment[data-role="lead"] .execution-model',
            )
            .boundingBox())!;
          const execution = (await page
            .locator(".execution-summary-trigger")
            .boundingBox())!;
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
          const rect = (selector: string) =>
            document
              .querySelector(selector)!
              .getBoundingClientRect()
              .toJSON() as DOMRect;
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
        if (stream?.listening)
          await new Promise<void>((resolve, reject) =>
            stream!.close((error) => (error ? reject(error) : resolve())),
          );
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
