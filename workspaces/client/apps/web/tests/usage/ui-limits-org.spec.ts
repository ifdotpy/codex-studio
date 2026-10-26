import { fixtureUrl, usageFixtureServer } from "./fixture-server.ts";
// Hidden browser fixture for the production limits and organization components.
// Screenshots stay outside the checkout.
import type { GetResult } from "../../src/apiTypes.ts";
import assert from "node:assert/strict";
import { mkdir } from "node:fs/promises";
import { tmpdir } from "node:os";
import { resolve, join } from "node:path";
import { test, expect } from "../playwright.ts";

test("Rust model decisions and safe Claude organizations at 320 and 1200 pixels", async ({
  browser,
}) => {
  test.setTimeout(120_000);
  const output =
    process.env.UI_LIMITS_ORG_SCREENSHOTS ||
    join(tmpdir(), "studio-ui-limits-org");
  await mkdir(output, { recursive: true });

  const server = await usageFixtureServer("ui-limits-org", {
    html: '<!doctype html><html style="height:auto;overflow:auto"><head><meta name="viewport" content="width=device-width,initial-scale=1"></head><body style="position:static;height:auto;overflow:auto"><div id="root" style="position:static;display:block;height:auto;overflow:visible"></div><script type="module" src="/tests/usage/ui-limits-org.fixture.tsx"></script></body></html>',
  });
  try {
    await server.listen();
    for (const width of [320, 1200]) {
      const page = await browser.newPage({ viewport: { width, height: 1000 } });
      page.setDefaultTimeout(10_000);
      const errors: string[] = [];
      page.on("pageerror", (error) => errors.push(error.message));
      await page.emulateMedia({ colorScheme: "light" });
      const now = 1_800_000_000;
      await page.clock.install({ time: now * 1000 });
      let mode: NonNullable<
        GetResult<"/api/limits">["eligibility"]
      >["overage"] = "rejected";
      const requests: { agent: string | null; account: string | null }[] = [];
      await page.route("**/api/**", async (route) => {
        if (route.request().url().includes("/api/limits")) {
          const query = new URL(route.request().url()).searchParams;
          requests.push({
            agent: query.get("agent"),
            account: query.get("account_key"),
          });
          await route.fulfill({
            json: {
              accountKey: "claude",
              at: now,
              data: {
                rateLimits: {
                  limitId: "claude",
                  primary: { usedPercent: 100, resetsAt: now + 3600 },
                },
              },
              eligibility: {
                agentId: "worker",
                accountKey: "claude",
                model: "sonnet",
                provider: "claude",
                allowed: mode !== "rejected",
                canRun: mode !== "rejected",
                fresh: true,
                checkedAt: now,
                validUntil: now + 60,
                resetAt: mode === "rejected" ? now + 3600 : null,
                buckets: [
                  { id: "claude", overageAllowed: mode !== "rejected" },
                  { id: "claude-sonnet", overageAllowed: mode !== "rejected" },
                ],
                overage: mode,
              },
            } satisfies GetResult<"/api/limits">,
          });
        } else
          await route.fulfill({ json: { items: [], accounts: [], cost: 0 } });
      });
      await page.goto(fixtureUrl(server));
      // The chat notice is one plain line; the details stay on the limits page.
      await page
        .locator("[data-notice]")
        .getByText(/^Limit reached\. Resets at /)
        .waitFor({ timeout: 10_000 });
      const notice = await page.locator("[data-notice]").innerText();
      for (const detail of [
        /Applicable buckets/,
        /Overage/,
        /Current eligibility is unknown/,
        /Cannot run now/,
      ])
        assert.doesNotMatch(notice, detail);
      assert.equal(
        await page.locator("[data-notice] .model-limit-eligibility").count(),
        0,
      );
      await expect(
        page
          .locator("[data-notice]")
          .getByRole("button", { name: "Refresh", exact: true }),
      ).toBeVisible();
      await page.screenshot({
        path: resolve(output, `notice-rejected-${width}.png`),
        fullPage: true,
        animations: "disabled",
      });
      await page.emulateMedia({ colorScheme: "dark" });
      await expect(page.locator("html")).toHaveAttribute(
        "data-mantine-color-scheme",
        "dark",
      );
      await page.screenshot({
        path: resolve(output, `notice-rejected-dark-${width}.png`),
        fullPage: true,
        animations: "disabled",
      });
      await page.emulateMedia({ colorScheme: "light" });
      await expect(page.locator("html")).toHaveAttribute(
        "data-mantine-color-scheme",
        "light",
      );
      await page
        .getByRole("button", { name: "Account limits", exact: true })
        .click();
      await page
        .locator(".account-limits-panel")
        .getByText("Model eligibility details", { exact: true })
        .click();
      const eligibility = page.locator(
        ".account-limits-panel .model-limit-eligibility",
      );
      await expect(eligibility).toContainText(
        "Applicable buckets: claude, claude-sonnet",
      );
      assert.doesNotMatch(await eligibility.innerText(), /claude-opus/);
      await expect(eligibility).toContainText("Cannot run now");
      await expect(
        page.locator(".account-limits-panel .model-limit-eligibility"),
      ).toContainText("Overage rejected");
      assert.ok(requests.length >= 2);
      assert.ok(
        requests.every(
          ({ agent, account }) => agent === "worker" && account === "claude",
        ),
      );
      await expect(page.locator(".account-limits-popover")).toHaveCSS(
        "opacity",
        "1",
      );
      const panelBox = await page
        .locator(".account-limits-popover")
        .boundingBox();
      assert.ok(panelBox);
      assert.ok(
        panelBox.x >= 0 &&
          panelBox.y >= 0 &&
          panelBox.x + panelBox.width <= width,
      );
      await page.screenshot({
        path: resolve(output, `limits-rejected-${width}.png`),
        fullPage: true,
        animations: "disabled",
      });
      for (const [value, label] of [
        ["allowed", "Overage allowed"],
        ["in_use", "Overage in use"],
      ] as const) {
        mode = value;
        await page.clock.fastForward(1000);
        await page.evaluate(() => window.limitsOrgFixture.refresh());
        await page
          .getByText(label, { exact: true })
          .first()
          .waitFor({ timeout: 10_000 });
        assert.equal(
          await page.locator("[data-notice] .native-error").count(),
          0,
        );
        await page.screenshot({
          path: resolve(output, `limits-${value}-${width}.png`),
          fullPage: true,
          animations: "disabled",
        });
      }
      await page.clock.fastForward(61_000);
      await expect(
        page.locator(".account-limits-panel .model-limit-eligibility"),
      ).toContainText("Current eligibility is unknown");
      await page
        .getByRole("button", { name: "Show accounts", exact: true })
        .click();
      await page
        .getByText(/Signed in to a different organization:/)
        .waitFor({ timeout: 10_000 });
      await page
        .getByRole("button", { name: "Sign in to Claude", exact: true })
        .first()
        .click();
      assert.equal(
        await page.evaluate(() => window.limitsOrgFixture.signedInAccount),
        "claude",
      );
      await page
        .getByRole("button", {
          name: "Limit details for research@example.test",
          exact: true,
        })
        .click();
      await expect(
        page.locator('[data-account="claude"] .claude-organization').first(),
      ).toBeVisible();
      assert.ok((await page.locator(".claude-organization").count()) >= 2);
      assert.equal(
        await page.evaluate(
          () => document.documentElement.scrollWidth > innerWidth,
        ),
        false,
      );
      await page.screenshot({
        path: resolve(output, `accounts-${width}.png`),
        fullPage: true,
        animations: "disabled",
      });
      assert.deepEqual(errors, []);
      await page.close();
    }
    console.log(
      `PASS: limits, overage and organization at 320/1200; screenshots: ${output}`,
    );
  } finally {
    await server.close();
  }
});
