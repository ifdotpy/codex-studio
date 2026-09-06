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
  const errors = [];
  page.on("pageerror", (error) => errors.push(error.message));
  await page.route("**/api/limits", (route) => route.fulfill({ json: limits }));
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
  await page.waitForFunction(
    () =>
      getComputedStyle(document.querySelector(".account-limits-popover"))
        .opacity === "1",
  );
  await page.screenshot({
    path: join(root, "limits-desktop.png"),
    animations: "disabled",
  });

  limits = {
    data: {
      rateLimitsByLimitId: {
        unrelated: {
          limitName: "Other model",
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
  assert.match(await toggle().innerText(), /Update failed/);
  await toggle().click();
  await details().waitFor();
  await details().getByText("Awaiting update", { exact: true }).waitFor();
  assert.equal(await details().getByRole("progressbar").count(), 1);
  assert.doesNotMatch(await details().innerText(), /58%/);
  assert.match(await details().innerText(), /Account connection failed/);

  limits = { data: null, at: now + 5 };
  await load();
  assert.match(await toggle().innerText(), /Unavailable/);
  await toggle().click();
  await details().waitFor();
  assert.equal(await details().getByRole("progressbar").count(), 0);
  assert.match(await details().innerText(), /has not supplied/);

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
  await page.setViewportSize({ width: 390, height: 844 });
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
    path: join(root, "limits-mobile.png"),
    animations: "disabled",
  });
  const box = await details().boundingBox();
  assert.ok(
    box &&
      box.x >= 0 &&
      box.x + box.width <= 390 &&
      box.y >= 0 &&
      box.y + box.height <= 844,
    "mobile limits stay in viewport",
  );
  assert.equal(
    await page.evaluate(
      () => document.documentElement.scrollWidth > innerWidth,
    ),
    false,
    "mobile page has no horizontal overflow",
  );
  assert.equal(
    await details().evaluate(
      (element) => element.scrollWidth > element.clientWidth,
    ),
    false,
    "mobile limits have no horizontal overflow",
  );
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
        "mobile",
      ],
    }),
  );
} finally {
  await browser?.close();
  fixture.kill("SIGTERM");
}
