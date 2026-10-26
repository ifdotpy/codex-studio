import { fixtureOrigin } from "../misc-fixture.ts";
import { usageFixtureServer } from "./fixture-server.ts";
// Current quota recovery and immutable history through real React components.

import assert from "node:assert/strict";
import type { LimitSnapshot } from "./limit-recovery-state-ui.fixture.tsx";
import type { GetResult } from "../../src/apiTypes.ts";
import { test } from "../playwright.ts";

test("Limit Recovery State Ui", async ({
  browser: _testBrowser,
  context: _testContext,
  page: testPage,
}) => {
  const now = Math.floor(Date.now() / 1000);

  const server = await usageFixtureServer("limit-recovery-state-ui");
  try {
    await server.listen();
    const page = testPage;
    await page.setViewportSize({ width: 1200, height: 900 });
    await page.addInitScript((clock) => {
      Date.now = () => clock * 1000;
    }, now);
    const errors: string[] = [],
      writes: string[] = [];
    page.on("pageerror", (error) => errors.push(error.message));
    await page.route("**/api/**", async (route) => {
      if (route.request().method() !== "GET")
        writes.push(route.request().url());
      const path = new URL(route.request().url()).pathname;
      await route.fulfill({
        json:
          path === "/api/costs"
            ? { accountKey: "work", error: "Isolated cost fixture" }
            : {},
      });
    });
    await page.goto(fixtureOrigin(server) + `?now=${now}`);
    await page.locator(".account-limits-panel").waitFor();
    const originalError = await page.locator("#saved-error").textContent();
    const assertBanner = async (visible: boolean) => {
      await page.locator(".account-limits-panel").waitFor();
      await page.waitForFunction(
        (expected) =>
          !!document.querySelector("#current .native-error") === expected &&
          !!document.querySelector(
            ".account-limits-panel .account-limit-recovery",
          ) === expected,
        visible,
      );
      assert.equal(
        await page.locator("#history .native-limit-notice").count(),
        1,
      );
      assert.match(
        (await page.locator("#history").textContent()) ?? "",
        /Historical quota failure/,
      );
      assert.equal(
        await page.locator("#saved-error").textContent(),
        originalError,
      );
    };
    const apply = async (
      label: string,
      limits:
        | (GetResult<"/api/limits"> & { stale?: boolean; loading?: boolean })
        | null,
      visible: boolean,
      identity: Partial<Omit<LimitSnapshot, "label" | "limits">> = {},
    ) => {
      if (limits?.eligibility)
        limits = {
          ...limits,
          eligibility: {
            ...limits.eligibility,
            agentId: identity.agentId || "lumina",
          },
        };
      await page.evaluate(
        (serialized) => window.limitRecoveryFixture.applySerialized(serialized),
        JSON.stringify({ label, limits, ...identity } satisfies LimitSnapshot),
      );
      await page.locator(`main[data-case="${label}"]`).waitFor();
      await assertBanner(visible);
    };
    await assertBanner(true);
    const available = {
      accountKey: "work",
      at: now,
      eligibility: {
        agentId: "lumina",
        accountKey: "work",
        provider: "codex",
        model: "gpt-6-astra",
        canRun: true,
        allowed: true,
        fresh: true,
        checkedAt: now,
        validUntil: now + 5,
        resetAt: null,
        buckets: [{ id: "codex", overageAllowed: false }],
        overage: "unavailable",
      },
      data: {
        ordinaryUsageAllowed: true,
        rateLimits: {
          planType: "pro",
          rateLimitReachedType: null,
          primary: { usedPercent: 14, resetsAt: now + 3600 },
          secondary: { usedPercent: 35, resetsAt: now + 86400 },
        },
      },
    } satisfies GetResult<"/api/limits">;
    const cases: [string, Parameters<typeof apply>[1]][] = [
      ["another-account", { ...available, accountKey: "another" }],
      ["stale", { ...available, stale: true }],
      ["expired", { ...available, at: now - 301 }],
      ["older-than-error", { ...available, at: now - 20 }],
      ["same-time-as-error", { ...available, at: now - 10 }],
      ["future", { ...available, at: now + 60 }],
      ["loading", { ...available, loading: true }],
      ["read-error", { ...available, error: "Offline" }],
      [
        "unknown",
        {
          ...available,
          eligibility: undefined,
          data: { rateLimits: available.data.rateLimits },
        },
      ],
      ["no-snapshot", null],
      [
        "usage-denied",
        {
          ...available,
          eligibility: {
            ...available.eligibility,
            allowed: false,
            canRun: false,
          },
          data: { ...available.data, ordinaryUsageAllowed: false },
        },
      ],
      [
        "exhausted",
        {
          ...available,
          eligibility: {
            ...available.eligibility,
            allowed: false,
            canRun: false,
          },
          data: {
            ...available.data,
            rateLimits: {
              primary: { usedPercent: 100, resetsAt: now + 3600 },
            },
          },
        },
      ],
    ];
    for (const [label, limits] of cases) {
      await apply(label, limits, true, { agentId: label });
      await apply(label + "-recovered", available, false, { agentId: label });
    }
    await apply("recovered-episode", available, false);
    await apply("recovered-now-stale", { ...available, stale: true }, false);
    await apply(
      "recovered-read-error",
      { ...available, error: "Offline" },
      false,
    );
    await apply("recovered-cache-aged", { ...available, at: now - 301 }, false);
    const stale = { ...available, stale: true };
    await apply("same-episode-new-turn-identity", stale, false, {
      turnId: "later-turn",
      lastCompletedTurn: "completed-turn",
    });
    await apply("recovered-component-remount", stale, false, { mountKey: 1 });
    await apply("other-account-after-recovery", stale, true, {
      accountKey: "another",
      mountKey: 2,
    });
    await apply("other-thread-after-recovery", stale, true, {
      threadId: "another-thread",
      mountKey: 3,
    });
    assert.deepEqual(
      await page.evaluate(() => window.limitRecoveryFixture.actions),
      [],
    );
    await page.evaluate(
      (snapshot) => {
        sessionStorage.setItem("fixture-initial", JSON.stringify(snapshot));
      },
      { label: "recovered-page-reload", limits: stale },
    );
    await page.reload();
    await page.locator('main[data-case="recovered-page-reload"]').waitFor();
    await assertBanner(false);
    await apply("new-quota-error", available, true, { episode: now });
    assert.deepEqual(writes, []);
    assert.deepEqual(
      await page.evaluate(() => window.limitRecoveryFixture.actions),
      [],
    );
    assert.deepEqual(errors, []);
    console.log(
      JSON.stringify({
        passed: true,
        retainedCases: cases.length,
        currentBannerClearsWithoutSend: true,
        usageNoticeClears: true,
        historicalErrorPreserved: true,
        recoveredEpisodeStaysClear: true,
        remountAndReloadKeepRecovery: true,
        sameEpisodeTurnChangesKeepRecovery: true,
        savedRecoveryAccountAndThreadScoped: true,
        newQuotaErrorVisible: true,
      }),
    );
  } finally {
    await server.close();
  }
});
