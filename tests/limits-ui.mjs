#!/usr/bin/env node
// Production UI and local fixture. Account variants replace only read responses.
import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { mkdtemp } from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { createRequire } from "node:module";
const skill = dirname(dirname(fileURLToPath(import.meta.url)));
const require = createRequire(join(skill, "web/package.json"));
const { chromium } = require("playwright-core");
const root = await mkdtemp(join(tmpdir(), "codex-limits-ui-"));
const fixture = spawn(
  "python3",
  ["-B", join(skill, "tests/simple-ui-fixture.py"), root],
  { stdio: ["ignore", "pipe", "pipe"] },
);
let log = "",
  browser;
fixture.stderr.on("data", (data) => (log += data));
try {
  const port = await new Promise((resolve, reject) => {
    fixture.stdout.once("data", (data) => resolve(Number(String(data).trim())));
    fixture.once("exit", () => reject(Error(log)));
  });
  const origin = `http://127.0.0.1:${port}`;
  const initial = await fetch(`${origin}/api/state`).then((response) =>
    response.json(),
  );
  const lead = initial.threads.find((agent) => agent.name === "Release lead");
  const now = Date.now() / 1000;
  const primary = {
    usedPercent: 42,
    windowDurationMins: 300,
    resetsAt: now + 3600,
  };
  const secondary = {
    usedPercent: 21,
    windowDurationMins: 10080,
    resetsAt: now + 86400,
  };
  let limits = {
    data: {
      rateLimits: {
        limitId: "codex",
        planType: "pro",
        primary,
        secondary,
        credits: { balance: "12.50" },
      },
    },
    at: now,
  };
  browser = await chromium.launch({
    executablePath:
      process.env.CHROME_BIN ||
      "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
    headless: true,
    args: ["--disable-extensions", "--no-first-run"],
  });
  const page = await browser.newPage({
    viewport: { width: 1440, height: 960 },
  });
  page.setDefaultTimeout(12000);
  await page.route("**/api/accounts", (route) =>
    route.fulfill({
      json: {
        defaultAccountKey: "default",
        accounts: [
          {
            id: "default",
            label: "Fixture",
            status: "ready",
            accountId: "test-account",
          },
        ],
      },
    }),
  );
  const errors = [];
  page.on("pageerror", (error) => errors.push(error.message));
  let failLimits = false;
  await page.route("**/api/limits", (route) =>
    failLimits
      ? route.fulfill({
          status: 503,
          json: { error: "Fixture quota refresh failed" },
        })
      : route.fulfill({ json: limits }),
  );
  let costs = {
    at: now,
    stale: false,
    refreshing: false,
    data: {
      todayUSD: 12.5,
      last30DaysUSD: 1234.56,
      sourceUpdatedAt: new Date().toISOString(),
      coverage: "unverified",
      unknownModels: [],
    },
  };
  let deferCosts = false,
    pendingCosts;
  await page.route("**/api/costs?*", (route) => {
    if (deferCosts) {
      pendingCosts = route;
      return;
    }
    return route.fulfill({ json: { ...costs, accountKey: "default" } });
  });
  await page.route("**/api/state", async (route) => {
    const response = await route.fetch();
    const state = await response.json();
    state.runtime.rateLimits = limits;
    await route.fulfill({ response, json: state });
  });
  const toggle = () =>
    page.getByRole("button", { name: "Account limits", exact: true });
  const details = () =>
    page.getByRole("region", { name: "Account limits details", exact: true });
  const load = async () => {
    await page.goto(origin);
    if (page.viewportSize().width <= 600)
      await page.locator("#sidebar-toggle").click();
    await page.locator(`[data-chat="${lead.id}"]`).click();
    await toggle().waitFor();
  };
  await load();
  assert.match(await toggle().innerText(), /5h 58% left · 7d 79% left/);
  assert.match(await page.locator("#usage-footer").innerText(), /Context 40%/);
  assert.match(
    await page.locator("#usage-footer").innerText(),
    /2 compactions/,
  );
  await toggle().click();
  await details().waitFor();
  await details().getByText("58% left", { exact: true }).waitFor();
  assert.equal(await details().getByRole("progressbar").count(), 2);
  assert.match(await details().innerText(), /7d/);
  assert.match(await details().innerText(), /Credits\s+12.50/);
  assert.match(await details().innerText(), /Updated/);
  assert.match(await details().innerText(), /Resets in 1h/);
  assert.equal(await details().locator("time[datetime]").count(), 2);
  assert.match(
    await details()
      .getByRole("region", { name: "Local cost estimates" })
      .innerText(),
    /\$12.50/,
  );
  assert.match(await details().innerText(), /\$1,234.56/);
  assert.match(await details().innerText(), /API cost estimate/);
  assert.doesNotMatch(
    await details().innerText(),
    /All local chats|All accounts/,
  );
  assert.doesNotMatch(
    await details().innerText(),
    /CodexBar|coverage unverified|ChatGPT bill|Costs as of/,
  );
  assert.ok(
    (await details().locator(".account-limit-group").boundingBox()).height <
      150,
    "quota pool remains compact",
  );
  await page.waitForFunction(
    () =>
      getComputedStyle(document.querySelector(".account-limits-popover"))
        .opacity === "1",
  );
  await page.screenshot({
    path: join(root, "limits-desktop.png"),
    animations: "disabled",
  });
  await page.clock.install();
  failLimits = true;
  const beforeQuotaFailure = await page.locator("#usage-footer").boundingBox();
  await details().getByRole("button", { name: "Refresh", exact: true }).click();
  await details()
    .getByText(/^Saved limits/)
    .waitFor();
  assert.match(
    await toggle().innerText(),
    /5h 58% left · 7d 79% left/,
    "a failed refresh retains confirmed quota values for this account",
  );
  assert.equal(await details().getByRole("progressbar").count(), 2);
  assert.deepEqual(
    await page.locator("#usage-footer").boundingBox(),
    beforeQuotaFailure,
    "refresh error cannot add a footer row",
  );
  failLimits = false;
  await page.clock.fastForward(30050);
  await details()
    .getByText(/^Saved limits/)
    .waitFor({ state: "hidden" });

  const resetCredit = {
    id: "credit-test",
    title: "Full reset",
    description: "Restore Codex limits.",
    resetType: "codexRateLimits",
    status: "available",
    expiresAt: now + 86400,
  };
  limits.data.accountId = "test-account";
  limits.data.rateLimitResetCredits = {
    availableCount: 3,
    credits: [resetCredit],
  };
  let resetOutcome = "uncertain";
  const resetRequests = [];
  let releaseReset;
  let holdReset = false;
  await page.route("**/api/limits/reset", async (route) => {
    resetRequests.push(route.request().postDataJSON());
    if (holdReset)
      await new Promise((resolve) => {
        releaseReset = resolve;
      });
    if (resetOutcome === "reset") {
      limits.data.rateLimitResetCredits = {
        availableCount: 2,
        credits: [{ ...resetCredit, status: "redeemed" }],
      };
    }
    await route.fulfill({
      json: {
        outcome: resetOutcome,
        error:
          resetOutcome === "uncertain"
            ? "Reset result uncertain. Refresh to check."
            : undefined,
      },
    });
  });
  await load();
  assert.match(await toggle().innerText(), /3 resets/);
  await toggle().click();
  const resetPanel = () =>
    details().getByRole("region", { name: "Limit reset credits" });
  await resetPanel()
    .getByRole("button", { name: "Apply reset", exact: true })
    .click();
  await resetPanel()
    .getByRole("button", { name: "Cancel", exact: true })
    .click();
  assert.equal(resetRequests.length, 0, "cancel never spends a credit");
  await resetPanel()
    .getByRole("button", { name: "Apply reset", exact: true })
    .click();
  await resetPanel()
    .getByRole("button", { name: "Use one reset credit", exact: true })
    .click();
  await resetPanel()
    .getByRole("alert")
    .getByText(/uncertain/)
    .waitFor();
  assert.equal(resetRequests.length, 1);
  assert.equal(resetRequests[0].account_id, "test-account");
  assert.equal(resetRequests[0].credit_id, "credit-test");
  assert.match(resetRequests[0].request_id, /^[0-9a-f-]{36}$/);
  resetOutcome = "reset";
  holdReset = true;
  await resetPanel()
    .getByRole("button", { name: "Use one reset credit", exact: true })
    .evaluate((button) => {
      button.click();
      button.click();
    });
  await page.waitForFunction(
    () =>
      document.querySelector(".account-reset-confirm button:last-child")
        ?.disabled === true,
  );
  for (let attempt = 0; attempt < 100 && !releaseReset; attempt++)
    await new Promise((resolve) => setTimeout(resolve, 10));
  assert.equal(
    resetRequests.length,
    2,
    "duplicate click starts only one request",
  );
  assert.equal(
    resetRequests[1].request_id,
    resetRequests[0].request_id,
    "uncertain retry reuses its receipt",
  );
  assert.match(
    await resetPanel().innerText(),
    /3 available/,
    "no optimistic credit count",
  );
  releaseReset();
  await resetPanel()
    .getByRole("status")
    .getByText(/Reset applied/)
    .waitFor();
  await resetPanel().getByText("2 available", { exact: true }).waitFor();
  assert.equal(
    await resetPanel()
      .getByRole("button", { name: "Apply reset", exact: true })
      .count(),
    0,
  );
  holdReset = false;
  for (const outcome of ["nothingToReset", "noCredit", "alreadyRedeemed"]) {
    resetOutcome = outcome;
    limits.data.rateLimitResetCredits = {
      availableCount: 3,
      credits: [resetCredit],
    };
    await load();
    await toggle().click();
    await resetPanel()
      .getByRole("button", { name: "Apply reset", exact: true })
      .click();
    await resetPanel()
      .getByRole("button", { name: "Use one reset credit", exact: true })
      .click();
    await resetPanel()
      .getByText(
        outcome === "nothingToReset"
          ? "No limits need a reset."
          : outcome === "noCredit"
            ? "This reset credit is no longer available."
            : "This reset credit was already used.",
        { exact: true },
      )
      .waitFor();
    assert.doesNotMatch(await resetPanel().innerText(), /Reset applied/);
  }
  resetOutcome = "reset";
  limits.data.rateLimitResetCredits = {
    availableCount: 3,
    credits: [
      resetCredit,
      { ...resetCredit, id: "credit-two" },
      { ...resetCredit, id: "credit-three" },
    ],
  };
  await load();
  await toggle().click();
  await resetPanel()
    .getByRole("button", { name: "Apply reset", exact: true })
    .first()
    .click();
  assert.equal(
    await resetPanel().locator("strong[title='Restore Codex limits.']").count(),
    3,
  );
  assert.equal(
    await resetPanel()
      .getByText("Restore Codex limits.", { exact: true })
      .count(),
    0,
  );
  await page.waitForFunction(
    () =>
      getComputedStyle(document.querySelector(".account-limits-popover"))
        .opacity === "1",
  );
  await page.screenshot({
    path: join(root, "limits-reset-credits.png"),
    animations: "disabled",
  });
  limits = {
    data: {
      rateLimitsByLimitId: {
        unrelated: {
          limitName: "GPT-5.3-Codex-Spark",
          primary: { ...primary, usedPercent: 1 },
        },
        codex: { limitId: "codex", primary },
        [lead.model]: {
          limitName: lead.model,
          primary: { ...primary, usedPercent: 92 },
          secondary,
        },
      },
    },
    at: now + 1,
  };
  await load();
  assert.match(await toggle().innerText(), /5h 8% left/);
  assert.doesNotMatch(await toggle().innerText(), /99%/);
  await toggle().click();
  await details().waitFor();
  assert.equal(await details().locator(".account-limit-group").count(), 3);
  await details().getByText("99% left", { exact: true }).waitFor();
  const names = await details()
    .locator(".account-limit-group > header > strong")
    .allTextContents();
  assert.equal(names[0], "Codex");
  assert.equal(names.at(-1), "GPT-5.3-Codex-Spark");
  await page.screenshot({
    path: join(root, "limits-pools-desktop.png"),
    animations: "disabled",
  });

  limits = {
    data: {
      rateLimitsByLimitId: {
        unrelated: { limitName: "Other model", primary },
        another: { limitName: "Another model", secondary },
      },
    },
    at: now + 2,
  };
  await load();
  assert.match(await toggle().innerText(), /2 pools/);
  assert.doesNotMatch(await toggle().innerText(), /58%/);

  limits = {
    data: {
      rateLimits: {
        limitId: "codex",
        primary: { windowDurationMins: 300 },
        secondary: { ...secondary, usedPercent: 99.5 },
        credits: {},
      },
    },
    at: now + 3,
  };
  await load();
  assert.match(await toggle().innerText(), /5h unavailable · 7d <1% left/);
  await toggle().click();
  await details().waitFor();
  assert.equal(await details().getByRole("progressbar").count(), 1);
  assert.match(await details().innerText(), /Reset time unavailable/);
  assert.match(
    await details().locator(".account-limit-credits").innerText(),
    /Unavailable/,
  );
  assert.doesNotMatch(await details().innerText(), /0%/);

  limits = {
    data: {
      rateLimits: {
        limitId: "codex",
        primary: { ...primary, resetsAt: now - 60 },
        secondary: { ...secondary, usedPercent: 100 },
      },
    },
    at: now + 4,
    error: "Account connection failed",
  };
  await load();
  assert.match(await toggle().innerText(), /5h refresh needed/);
  assert.match(await toggle().innerText(), /7d 0% left/);
  assert.doesNotMatch(await toggle().innerText(), /Update failed/);
  await toggle().click();
  await details().waitFor();
  await details().getByText("Awaiting update", { exact: true }).waitFor();
  assert.equal(await details().getByRole("progressbar").count(), 1);
  assert.doesNotMatch(await details().innerText(), /58%/);
  assert.match(await details().innerText(), /Saved limits/);
  assert.doesNotMatch(
    await details().innerText(),
    /Account connection failed|outcome unknown/,
  );

  costs = {
    at: now,
    stale: true,
    refreshing: false,
    error: "Scanner unavailable",
    data: {
      todayUSD: null,
      last30DaysUSD: null,
      coverage: "partial",
      unknownModels: ["unknown-astra"],
    },
  };
  limits = { data: null, at: now + 5 };
  await load();
  assert.match(await toggle().innerText(), /Unavailable/);
  await toggle().click();
  await details().waitFor();
  assert.equal(await details().getByRole("progressbar").count(), 0);
  assert.match(await details().innerText(), /has not supplied/);
  assert.match(await details().innerText(), /Estimate unavailable/);
  assert.doesNotMatch(
    await details().innerText(),
    /unknown-astra|Scanner unavailable/,
  );
  assert.doesNotMatch(await details().innerText(), /\$0\.00/);

  limits = {
    data: {
      rateLimitsByLimitId: {},
      rateLimits: {
        limitId: "codex",
        limitName: "Codex",
        primary,
        secondary,
        credits: { unlimited: true },
      },
    },
    at: now + 6,
  };
  await page.setViewportSize({ width: 780, height: 844 });
  await load();
  assert.match(await toggle().innerText(), /5h 58% left/);
  await toggle().click();
  await details().waitFor();
  await details().waitFor();
  await page.waitForFunction(
    () =>
      getComputedStyle(document.querySelector(".account-limits-popover"))
        .opacity === "1",
  );
  await page.screenshot({
    path: join(root, "limits-narrow-desktop.png"),
    animations: "disabled",
  });
  const box = await details().boundingBox();
  assert.ok(
    box &&
      box.x >= 0 &&
      box.x + box.width <= 780 &&
      box.y >= 0 &&
      box.y + box.height <= 844,
    "narrow desktop limits stay in viewport",
  );
  assert.equal(
    await page.evaluate(
      () => document.documentElement.scrollWidth > innerWidth,
    ),
    false,
    "narrow desktop page has no horizontal overflow",
  );
  assert.equal(
    await details().evaluate(
      (element) => element.scrollWidth > element.clientWidth,
    ),
    false,
    "narrow desktop limits have no horizontal overflow",
  );
  // Late cost data must not add a footer row or lift the composer.
  for (const width of [780, 1280]) {
    await page.setViewportSize({ width, height: 900 });
    deferCosts = true;
    pendingCosts = undefined;
    await load();
    await page.waitForTimeout(150);
    const before = await page.locator("#usage-footer").boundingBox();
    const composerBefore = await page.locator("#composer").boundingBox();
    for (let n = 0; n < 100 && !pendingCosts; n++)
      await new Promise((resolve) => setTimeout(resolve, 10));
    assert.ok(pendingCosts, "cost request is delayed");
    await pendingCosts.fulfill({
      json: {
        ...costs,
        accountKey: "default",
        error: null,
        data: { ...costs.data, todayUSD: 123456.78 },
      },
    });
    await page.locator(".account-cost-summary").waitFor();
    assert.deepEqual(
      await page.locator("#usage-footer").boundingBox(),
      before,
      `cost response preserves footer geometry at ${width}px`,
    );
    assert.deepEqual(
      await page.locator("#composer").boundingBox(),
      composerBefore,
      `cost response preserves composer geometry at ${width}px`,
    );
    const summaryBox = await toggle().boundingBox();
    assert.equal(summaryBox.height, 26, "quota trigger remains a single line");
    deferCosts = false;
  }
  assert.deepEqual(errors, []);
  console.log(
    JSON.stringify({
      ok: true,
      evidence: root,
      cases: [
        "visible summary",
        "context preserved",
        "active model",
        "multiple pools",
        "missing values",
        "nonzero floor",
        "expired windows",
        "zero remaining",
        "error",
        "empty data",
        "empty map fallback",
        "narrow desktop",
        "compact pools",
        "Codex before Spark",
        "reset countdown and exact time",
        "local cost scope and coverage",
        "unknown cost never zero",
        "reset confirmation and cancel",
        "reset account binding",
        "uncertain reset retry receipt",
        "reset duplicate click lock",
        "server credit count",
        "all reset outcomes",
        "delayed cost geometry at 780/1280px",
        "failed quota refresh preserves account values and geometry",
      ],
    }),
  );
} finally {
  await browser?.close();
  fixture.kill("SIGTERM");
}
