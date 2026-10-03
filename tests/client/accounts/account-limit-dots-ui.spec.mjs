#!/usr/bin/env node
import assert from "node:assert/strict";
import { mkdtemp } from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { test, expect, spawnFixture as spawn } from "../playwright.mjs";

test("account limit dots ui", async ({ browser: runnerBrowser }) => {
  const repo = dirname(
    dirname(dirname(dirname(fileURLToPath(import.meta.url)))),
  );
  const root = await mkdtemp(join(tmpdir(), "account-limit-dots-ui-"));
  const fixture = spawn(
    "python3",
    ["-B", join(repo, "tests/simple-ui-fixture.py"), root],
    { stdio: ["pipe", "pipe", "pipe"] },
  );
  let log = "",
    browser;
  const contexts = new Set();
  fixture.stderr.on("data", (data) => (log += data));
  try {
    const port = await new Promise((resolve, reject) => {
      fixture.stdout.once("data", (data) =>
        resolve(Number(String(data).trim())),
      );
      fixture.once("exit", () => reject(Error(log)));
    });
    const origin = `http://127.0.0.1:${port}`;
    const initial = await fetch(`${origin}/api/state`).then((response) =>
      response.json(),
    );
    const lead = initial.threads.find((agent) => agent.name === "Release lead");
    const now = Math.floor(Date.now() / 1000);
    // Each account except "outside" serves a member of this chat team.
    const teamKeys = [
      "yellow",
      "red",
      "unknown",
      "reset",
      "signedout",
      "no-weekly",
    ];
    const teamAccount = (agent) => {
      if (agent.id === lead.id) return "default";
      if (agent.rootId !== lead.id) return agent.accountKey;
      const workers = initial.threads.filter((item) => item.rootId === lead.id);
      return teamKeys[
        workers.findIndex((item) => item.id === agent.id) % teamKeys.length
      ];
    };
    const withTeamAccounts = (agents) =>
      agents?.forEach((agent) => {
        const key = teamAccount(agent);
        if (key) agent.accountKey = key;
      });
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
        label: "Status pending",
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
        id: "outside",
        label: "Other team account",
        email: "outside@example.test",
        provider: "codex",
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
    snapshots.reset.data.rateLimits.secondary.resetsAt = now - 1;
    const expected = {
      default: "green",
      yellow: "yellow",
      red: "red",
      reset: "green",
      signedout: "green",
      "no-weekly": "green",
    };
    browser = runnerBrowser;
    const screenshots = [];
    for (const [mobile, scheme] of [
      [false, "light"],
      [false, "dark"],
      [true, "light"],
      [true, "dark"],
    ]) {
      const context = await browser.newContext({
        viewport: mobile
          ? { width: 390, height: 844 }
          : { width: 1440, height: 960 },
        hasTouch: mobile,
        isMobile: mobile,
        colorScheme: scheme,
      });
      contexts.add(context);
      const page = await context.newPage();
      page.setDefaultTimeout(12000);
      const errors = [],
        reads = [];
      let unknownAvailable = true;
      page.on("pageerror", (error) => errors.push(error.message));
      await page.route("**/api/accounts", (route) =>
        route.fulfill({ json: { defaultAccountKey: "default", accounts } }),
      );
      await page.route(/\/api\/state(?:\?.*)?$/, async (route) => {
        const response = await route.fetch();
        const state = await response.json();
        state.runtime.rateLimits = snapshots.default;
        state.runtime.rateLimitsByAccount = snapshots;
        withTeamAccounts(state.threads);
        withTeamAccounts(state.runtime.agents);
        await route.fulfill({ response, json: state });
      });
      await page.route("**/api/sync/pull?*", async (route) => {
        // A closing page can dispose a pending long poll; nothing to patch then.
        let response, data;
        try {
          response = await route.fetch();
          data = await response.json();
        } catch {
          return;
        }
        for (const document of data.documents || []) {
          if (document.id.startsWith("entity:agent:")) {
            const record = JSON.parse(document.payload);
            if (record.value) withTeamAccounts([record.value]);
            document.payload = JSON.stringify(record);
            continue;
          }
          if (document.id !== "entity:workspace:current") continue;
          const record = JSON.parse(document.payload);
          record.value.rateLimits = snapshots.default;
          record.value.rateLimitsByAccount = snapshots;
          document.payload = JSON.stringify(record);
        }
        await route.fulfill({ response, json: data }).catch(() => {});
      });
      await page.route(/\/api\/limits(?:\?.*)?$/, (route) => {
        const query = new URL(route.request().url()).searchParams;
        const key = query.get("account_key") || "default";
        reads.push({ key, cached: query.get("cached") === "1" });
        return route.fulfill({
          json:
            key === "unknown" && unknownAvailable
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
      await page
        .waitForFunction(
          () =>
            document.querySelectorAll(".account-limits-dot-target").length ===
            7,
        )
        .catch(async (error) => {
          const keys = await dots.evaluateAll((elements) =>
            elements.map((element) => element.dataset.accountKey),
          );
          throw Error(`Dots ${JSON.stringify(keys)}: ${error.message}`);
        });
      await page
        .locator(
          '.account-limits-dot-target[data-account-key="yellow"] [data-color="yellow"]',
        )
        .waitFor();
      assert.equal(
        await dots.count(),
        7,
        "only accounts of this chat team appear",
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
        assert.equal(box.width, 12);
        assert.equal(box.height, 26);
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
      await page.waitForFunction(() =>
        document.querySelector(
          '.account-limits-dot-target[data-account-key="unknown"] [data-color="green"]',
        ),
      );
      assert.equal(
        await page.locator(".account-limits-dot[data-color='gray']").count(),
        0,
      );
      assert.match(
        await page
          .locator('.account-limits-dot-target[data-account-key="reset"]')
          .getAttribute("aria-label"),
        /Cached reset passed/,
      );
      assert.equal(
        await page.locator('[data-account-key="outside"]').count(),
        0,
      );
      assert.ok(!reads.some((read) => read.key === "outside" && read.cached));
      assert.ok(
        accounts
          .filter(
            (account) => !account.disconnected && account.id !== "outside",
          )
          .every((account) =>
            reads.some((read) => read.key === account.id && read.cached),
          ),
      );
      assert.ok(reads.every((read) => read.key === "default" || read.cached));
      // The dots share the footer row with Limits; the footer stays one row.
      const row = await page.evaluate(() => {
        const box = (selector) =>
          document.querySelector(selector).getBoundingClientRect();
        const dots = box(".account-limits-dots");
        const toggle = box(".account-limits-toggle");
        return {
          dots: dots.top + dots.height / 2,
          toggle: toggle.top + toggle.height / 2,
          footer: box("#usage-footer").height,
        };
      });
      // The dots may add at most 10 px to the footer height.
      const withoutDots = await page.evaluate(() => {
        const dots = document.querySelector(".account-limits-dots");
        dots.style.display = "none";
        const height = document
          .querySelector("#usage-footer")
          .getBoundingClientRect().height;
        dots.style.display = "";
        return height;
      });
      assert.ok(
        Math.abs(row.dots - row.toggle) <= 1,
        JSON.stringify({ mobile, ...row }),
      );
      assert.ok(
        row.footer - withoutDots <= 10,
        JSON.stringify({ withoutDots, ...row }),
      );
      await page.locator("#usage-footer").screenshot({
        path: join(root, `footer-${mobile ? 390 : 1440}-${scheme}.png`),
      });
      const prefix = `dots-${mobile ? 390 : 1440}-${scheme}`;
      const calmPath = join(root, `${prefix}.png`);
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
        path: join(root, `${prefix}-tooltip.png`),
        animations: "disabled",
      });
      screenshots.push(join(root, `${prefix}-tooltip.png`));
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
      // Escape belongs to the dropdown after its focus trap takes focus.
      await page.waitForFunction(() =>
        document
          .querySelector(".account-limits-popover")
          ?.contains(document.activeElement),
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
        reads.every((read) => read.key === "default" || read.cached),
        JSON.stringify(reads),
      );
      assert.equal(
        await page.evaluate(
          () => document.documentElement.scrollWidth > innerWidth,
        ),
        false,
      );
      await page.mouse.move(1, 1);
      await page.locator(".account-limits-dots").screenshot({
        path: join(root, `${prefix}-row.png`),
        animations: "disabled",
      });
      screenshots.push(join(root, `${prefix}-row.png`));
      snapshots.yellow = limits("yellow", 0);
      snapshots.yellow.at = now + 1;
      unknownAvailable = false;
      await page.reload();
      if (mobile) await page.locator("#sidebar-toggle").click();
      await page.locator(`[data-chat="${lead.id}"]`).click();
      await page
        .locator(
          '.account-limits-dot-target[data-account-key="yellow"] [data-color="green"]',
        )
        .waitFor();
      await page
        .locator(
          '.account-limits-dot-target[data-account-key="unknown"] [data-color="green"]',
        )
        .waitFor();
      snapshots.yellow = limits("yellow", 15);
      expect(errors).toEqual([]);
      await context.close();
      contexts.delete(context);
    }
    console.log(
      "Account dots UI passed: weekly bands, stale reset, no gray connected dots, keyboard, tabs, cached reads, persisted limits, and four viewport themes.",
    );
    console.log(JSON.stringify({ screenshots }));
  } finally {
    for (const context of contexts) {
      for (const page of context.pages())
        await page.unrouteAll({ behavior: "ignoreErrors" });
      await context.close();
    }
  }
});
