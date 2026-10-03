#!/usr/bin/env node
import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { mkdtemp } from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { createRequire } from "node:module";

const repo = dirname(dirname(fileURLToPath(import.meta.url)));
const require = createRequire(join(repo, "web/package.json"));
const { chromium } = require("playwright-core");
const root = await mkdtemp(join(tmpdir(), "account-limit-dots-ui-"));
const fixture = spawn(
  "python3",
  ["-B", join(repo, "tests/simple-ui-fixture.py"), root],
  { stdio: ["pipe", "pipe", "pipe"] },
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
  const now = Math.floor(Date.now() / 1000);
  const accounts = [
    {
      id: "default",
      label: "Own account",
      email: "own@example.test",
      provider: "codex",
      status: "ready",
    },
    {
      id: "yellow",
      label: "Claude low",
      email: "low@example.test",
      provider: "claude",
      status: "ready",
    },
    {
      id: "red",
      label: "Codex low",
      email: "red@example.test",
      provider: "codex",
      status: "ready",
    },
    {
      id: "unknown",
      label: "Not loaded",
      email: "unknown@example.test",
      provider: "claude",
      status: "ready",
    },
    {
      id: "reset",
      label: "Reset first",
      email: "reset@example.test",
      provider: "claude",
      status: "ready",
    },
    {
      id: "signedout",
      label: "Signed out",
      email: "signedout@example.test",
      provider: "codex",
      status: "signed_out",
    },
    {
      id: "no-weekly",
      label: "No weekly data",
      email: "weekly@example.test",
      provider: "claude",
      status: "ready",
    },
    {
      id: "disconnected",
      label: "Disconnected",
      provider: "codex",
      status: "ready",
      disconnected: true,
    },
  ];
  const limits = (accountKey, usedPercent, elapsed = 1) => ({
    accountKey,
    at: now,
    data: {
      rateLimits: {
        limitId:
          accounts.find((account) => account.id === accountKey)?.provider ||
          "codex",
        primary: {
          usedPercent: 100,
          windowDurationMins: 300,
          resetsAt: now + 3600,
        },
        secondary: {
          usedPercent,
          windowDurationMins: 10080,
          resetsAt: now + (7 - elapsed) * 86400,
        },
      },
    },
  });
  const snapshots = {
    default: limits("default", 10),
    yellow: limits("yellow", 15),
    red: limits("red", 20),
    reset: limits("reset", 80, 6),
    signedout: limits("signedout", 10),
    "no-weekly": {
      accountKey: "no-weekly",
      at: now,
      data: {
        rateLimits: {
          limitId: "claude",
          primary: {
            usedPercent: 20,
            windowDurationMins: 300,
            resetsAt: now + 3600,
          },
        },
      },
    },
  };
  const expected = {
    default: "green",
    yellow: "yellow",
    red: "red",
    unknown: "gray",
    reset: "green",
    signedout: "gray",
    "no-weekly": "gray",
  };
  browser = await chromium.launch({
    executablePath:
      process.env.CHROME_BIN ||
      "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
    headless: true,
    args: ["--disable-extensions", "--no-first-run"],
  });
  const screenshots = [];
  for (const mobile of [false, true]) {
    const context = await browser.newContext({
      viewport: mobile
        ? { width: 390, height: 844 }
        : { width: 1440, height: 960 },
      hasTouch: mobile,
      isMobile: mobile,
    });
    const page = await context.newPage();
    page.setDefaultTimeout(12000);
    const errors = [],
      reads = [];
    page.on("pageerror", (error) => errors.push(error.message));
    await page.route("**/api/accounts", (route) =>
      route.fulfill({ json: { defaultAccountKey: "default", accounts } }),
    );
    await page.route(/\/api\/state(?:\?.*)?$/, async (route) => {
      const response = await route.fetch();
      const state = await response.json();
      state.runtime.rateLimits = snapshots.default;
      state.runtime.rateLimitsByAccount = snapshots;
      await route.fulfill({ response, json: state });
    });
    await page.route("**/api/sync/pull?*", async (route) => {
      const response = await route.fetch();
      const data = await response.json();
      for (const document of data.documents || []) {
        if (document.id !== "entity:workspace:current") continue;
        const record = JSON.parse(document.payload);
        record.value.rateLimits = snapshots.default;
        record.value.rateLimitsByAccount = snapshots;
        document.payload = JSON.stringify(record);
      }
      await route.fulfill({ response, json: data });
    });
    await page.route(/\/api\/limits(?:\?.*)?$/, (route) => {
      const key =
        new URL(route.request().url()).searchParams.get("account_key") ||
        "default";
      reads.push(key);
      return route.fulfill({
        json:
          key === "unknown"
            ? limits("unknown", 10)
            : snapshots[key] || { accountKey: key, at: now, data: null },
      });
    });
    await page.route("**/api/costs?*", (route) =>
      route.fulfill({
        json: {
          accountKey:
            new URL(route.request().url()).searchParams.get("account_key") ||
            "default",
          data: {},
        },
      }),
    );
    await page.route("**/api/session-cost?*", (route) =>
      route.fulfill({
        json: { pricingState: "ready", totalUSD: 0.42, rootId: lead.id },
      }),
    );
    await page.goto(origin);
    if (mobile) await page.locator("#sidebar-toggle").click();
    await page.locator(`[data-chat="${lead.id}"]`).click();
    const dots = page.locator(".account-limits-dot-target");
    await page.waitForFunction(
      () =>
        document.querySelectorAll(".account-limits-dot-target").length === 7,
    );
    await page
      .locator(
        '.account-limits-dot-target[data-account-key="yellow"] [data-color="yellow"]',
      )
      .waitFor();
    assert.equal(
      await dots.count(),
      7,
      "all connected accounts appear, including accounts outside this chat team",
    );
    const order = await dots.evaluateAll((elements) =>
      elements.map((element) => element.dataset.accountKey),
    );
    for (const [key, color] of Object.entries(expected)) {
      const target = page.locator(
        `.account-limits-dot-target[data-account-key="${key}"]`,
      );
      assert.equal(
        await target.locator("span").getAttribute("data-color"),
        color,
      );
      const box = await target.boundingBox();
      assert.ok(
        box.width >= 44 && box.height >= 44,
        "touch area is at least 44 pixels",
      );
      assert.ok(
        box.x >= 0 && box.x + box.width <= (mobile ? 390 : 1440),
        "the touch area fits the viewport",
      );
      const dotBox = await target.locator("span").boundingBox();
      assert.equal(dotBox.width, 6);
      assert.equal(dotBox.height, 6);
    }
    assert.equal(
      await page
        .locator('.account-limits-dot-target[aria-current="true"]')
        .getAttribute("data-account-key"),
      "default",
    );
    assert.equal(
      await page
        .locator('[data-current="true"]')
        .evaluate((element) => getComputedStyle(element).outlineStyle),
      "solid",
    );
    assert.ok(
      reads.every((key) => key === "default"),
      "the dots do not read additional accounts on mount",
    );
    const calmPath = join(root, mobile ? "dots-390.png" : "dots-desktop.png");
    await page.screenshot({ path: calmPath, animations: "disabled" });
    screenshots.push(calmPath);
    const red = page.locator(
      '.account-limits-dot-target[data-account-key="red"]',
    );
    await dots.first().focus();
    for (let index = 0; index < order.indexOf("red"); index++) {
      await page.keyboard.press("Tab");
    }
    assert.equal(
      await red.evaluate((element) => document.activeElement === element),
      true,
    );
    const tooltip = page
      .getByRole("tooltip")
      .filter({ hasText: "red@example.test" });
    await tooltip.waitFor();
    await page
      .getByRole("tooltip")
      .filter({ hasText: "low@example.test" })
      .waitFor({ state: "hidden" });
    assert.match(
      await tooltip.innerText(),
      /Codex low.*red@example.test.*Codex/s,
    );
    assert.match(
      await tooltip.innerText(),
      /weekly: 80% left.*Resets.*About 4\.0 days left at the current rate/s,
    );
    assert.doesNotMatch(await tooltip.innerText(), /5h|\b0% left/);
    const before = await page.locator("#usage-footer").boundingBox();
    await page.screenshot({
      path: join(
        root,
        mobile ? "dots-390-tooltip.png" : "dots-desktop-tooltip.png",
      ),
      animations: "disabled",
    });
    screenshots.push(
      join(root, mobile ? "dots-390-tooltip.png" : "dots-desktop-tooltip.png"),
    );
    await red.press("Enter");
    const tabs = page.getByRole("tab");
    await page
      .getByRole("region", { name: "Account limits details", exact: true })
      .waitFor();
    assert.match(
      await page.locator('[role="tab"][aria-selected="true"]').innerText(),
      /red@example.test/,
    );
    assert.deepEqual(
      (await tabs.allTextContents()).map(
        (text) =>
          accounts.find(
            (account) => account.email && text.startsWith(account.email),
          )?.id,
      ),
      order,
    );
    await page.keyboard.press("Escape");
    await page
      .getByRole("region", { name: "Account limits details", exact: true })
      .waitFor({ state: "hidden" });
    assert.equal(
      await red.evaluate((element) => document.activeElement === element),
      true,
      "Escape returns focus to the account dot",
    );
    for (const key of order) {
      const target = page.locator(
        `.account-limits-dot-target[data-account-key="${key}"]`,
      );
      if (mobile) await target.tap();
      else await target.click();
      await page
        .getByRole("region", { name: "Account limits details", exact: true })
        .waitFor();
      assert.equal(
        await tabs.nth(order.indexOf(key)).getAttribute("aria-selected"),
        "true",
        `the ${key} dot selects its own tab`,
      );
      if (key === "unknown") {
        await target.locator('[data-color="green"]').waitFor();
      }
      assert.match(
        await page.locator('[role="tab"][aria-selected="true"]').innerText(),
        new RegExp(
          accounts
            .find((account) => account.id === key)
            .email.replaceAll(".", "\\."),
        ),
      );
      await page.keyboard.press("Escape");
      await page
        .getByRole("region", { name: "Account limits details", exact: true })
        .waitFor({ state: "hidden" });
    }
    assert.equal(
      await page
        .locator('.account-limits-dot-target[aria-current="true"]')
        .getAttribute("data-account-key"),
      "default",
      "panel selection does not move the current-chat marker",
    );
    const after = await page.locator("#usage-footer").boundingBox();
    assert.deepEqual(
      after,
      before,
      "panel selection preserves the footer geometry",
    );
    assert.ok(
      !reads.some((key) =>
        ["yellow", "red", "reset", "signedout", "no-weekly"].includes(key),
      ),
      "cached account dots and panel tabs do not cause new provider reads",
    );
    assert.equal(
      await page.evaluate(
        () => document.documentElement.scrollWidth > innerWidth,
      ),
      false,
    );
    await page.mouse.move(1, 1);
    await page.locator(".account-limits-dots").screenshot({
      path: join(root, mobile ? "dots-390-row.png" : "dots-desktop-row.png"),
      animations: "disabled",
    });
    screenshots.push(
      join(root, mobile ? "dots-390-row.png" : "dots-desktop-row.png"),
    );
    assert.deepEqual(errors, []);
    await context.close();
  }
  console.log(
    "Account dots UI passed: weekly bands, reset first, gray states, current marker, tooltip, keyboard, tabs, cached reads, and 390 px touch targets.",
  );
  console.log(JSON.stringify({ screenshots }));
} finally {
  await browser?.close();
  fixture.kill("SIGTERM");
}
