import { usageSessionCost, usageAccountCost } from "./usage-fixture.ts";
import type { BrowserContext, TestInfo } from "@playwright/test";
import type { ServerResponse } from "node:http";
import type { PostBody, PostResult } from "../../src/apiTypes.ts";
import { fixturePostBody } from "./fixture-server.ts";
import { fixtureUrl, usageFixtureServer } from "./fixture-server.ts";
import assert from "node:assert/strict";

import {
  test,
  apiSchemaHandshakeSse,
  API_SCHEMA_HASH_HEADER,
  readApiSchemaHash,
} from "../playwright.ts";

const browserContextsByTest = new WeakMap<TestInfo, Set<BrowserContext>>();
test.beforeEach(async ({ browser }, testInfo) => {
  browserContextsByTest.set(testInfo, new Set(browser.contexts()));
});
test.afterEach(async ({ browser }, testInfo) => {
  const initialContexts = browserContextsByTest.get(testInfo) ?? new Set();
  await Promise.all(
    browser
      .contexts()
      .filter((context) => !initialContexts.has(context))
      .map((context) => context.close()),
  );
});

test("usage accounts ui", async ({ browser: _browser }) => {
  test.setTimeout(120_000);

  const now = Math.floor(Date.now() / 1000);
  const workspaceId = "abcdef0123456789abcdef0123456789";
  const epoch = "usage-accounts-fixture";
  type ResourceStream = { response: ServerResponse; resources: unknown[] };
  const streams = new Set<ResourceStream>();
  const reads = { costs: 0, sessionCost: 0 };
  let revision = 0;
  const writeResources = (
    stream: ResourceStream,
    reason: string,
    resources = stream.resources,
  ) => {
    if (reason !== "initial") revision++;
    const event = {
      protocol: 4,
      workspaceId,
      epoch,
      revision,
      reason,
      resources,
    };
    assert.ok(
      Array.isArray(event.resources) && Number.isFinite(event.revision),
    );
    stream.response.write(
      `event: resources\ndata: ${JSON.stringify(event)}\n\n`,
    );
  };
  const captured: { reset?: PostBody<"/api/limits/reset"> } = {};

  const server = await usageFixtureServer("usage-accounts-ui", {
    middleware(req, res, next) {
      if (req.url?.startsWith("/api/"))
        res.setHeader(API_SCHEMA_HASH_HEADER, readApiSchemaHash());
      if (req.url?.startsWith("/api/sync/identity")) {
        res.setHeader("Content-Type", "application/json");
        res.end(JSON.stringify({ workspaceId }));
        return;
      }
      if (req.url?.startsWith("/api/sync/stream")) {
        const stream = {
          response: res,
          resources: JSON.parse(
            new URL(req.url, "http://localhost").searchParams.get(
              "resources",
            ) || "[]",
          ),
        };
        res.writeHead(200, {
          "Content-Type": "text/event-stream",
          "Cache-Control": "no-cache",
          Connection: "keep-alive",
        });
        res.write(apiSchemaHandshakeSse());
        streams.add(stream);
        writeResources(stream, "initial");
        res.on("close", () => streams.delete(stream));
        return;
      }
      if (req.url?.startsWith("/api/")) {
        res.setHeader("Content-Type", "application/json");
        if (req.url.startsWith("/api/session-cost")) {
          reads.sessionCost++;
          res.end(JSON.stringify(usageSessionCost()));
          return;
        }
        if (req.url.startsWith("/api/costs")) {
          reads.costs++;
          const key =
            new URL(req.url, "http://localhost").searchParams.get(
              "account_key",
            ) || "default";
          res.end(JSON.stringify(usageAccountCost(key)));
          return;
        }
        if (req.url.startsWith("/api/limits/reset")) {
          let body = "";
          req.on("data", (part) => (body += part));
          req.on("end", () => {
            captured.reset = fixturePostBody<"/api/limits/reset">(
              JSON.parse(body),
            );
            res.end(
              JSON.stringify({
                outcome: "reset",
                account_id: captured.reset.account_id,
                account_key: captured.reset.account_key ?? "team",
                credit_id: captured.reset.credit_id,
                request_id: captured.reset.request_id,
                limits: { accountKey: "team" },
              } satisfies PostResult<"/api/limits/reset">),
            );
          });
          return;
        }
      }

      next();
    },
  });
  let browser;
  try {
    await server.listen();
    browser = _browser;
    const page = await browser.newPage({
      viewport: { width: 1100, height: 900 },
    });
    await page.clock.install();
    const errors: string[] = [];
    page.on("pageerror", (error) => errors.push(error.message));
    await page.goto(fixtureUrl(server) + `?now=${now}`);
    // Notes stay in the tooltip; the footer shows only the estimate and a marker.
    await page
      .locator(".session-cost-summary")
      .filter({ hasText: "*" })
      .waitFor({ state: "attached" });
    assert.match(
      (await page.locator(".session-cost-summary").getAttribute("title")) ?? "",
      /Earlier Claude totals may be incomplete/,
    );
    assert.doesNotMatch(
      await page.locator(".session-cost-summary").innerText(),
      /incomplete|Unpriced/,
    );
    assert.deepEqual(
      reads,
      { costs: 0, sessionCost: 0 },
      "the component consumes cost projections without legacy resource GETs",
    );
    const _toggle = page.getByRole("button", {
      name: "Account limits",
      exact: true,
    });
    await page
      .getByRole("region", { name: "Account limits details" })
      .waitFor();
    assert.equal(
      await page.getByRole("tab").count(),
      1,
      "one account has one detail tile",
    );
    const ownLimitsPanel = page.locator(".account-limits-panel");
    await ownLimitsPanel.waitFor();
    assert.match(await ownLimitsPanel.innerText(), /80%\s*left/);
    await page.getByRole("button", { name: "Refresh", exact: true }).click();
    await page.waitForFunction(() =>
      window.usageAccountsFixture.reloadCalls.some(
        ([key, force]) => key === "own" && force,
      ),
    );

    await page.goto(fixtureUrl(server) + `?multi&now=${now}`);
    const ownTab = page.getByRole("tab", {
      name: "own@example.test account limits",
    });
    const teamTab = page.getByRole("tab", {
      name: "team@example.test, openai account limits",
    });
    await ownTab.waitFor();
    assert.equal(
      await ownTab.getAttribute("aria-selected"),
      "true",
      "chat account is selected first",
    );
    // The detail panel and the account tile state the same remaining limit.
    const accountPanel = page.locator("#usage-account-panel");
    const selectedOwnLimit = accountPanel.getByText("80% left", {
      exact: true,
    });
    await selectedOwnLimit.waitFor();
    assert.equal(await selectedOwnLimit.isVisible(), true);
    assert.equal(await selectedOwnLimit.textContent(), "80% left");
    assert.equal(
      await ownTab.locator(".account-remaining").textContent(),
      "80% left",
    );
    await ownTab.press("ArrowRight");
    assert.equal(
      await teamTab.getAttribute("aria-selected"),
      "true",
      "ArrowRight selects the next account tile",
    );
    await page.waitForFunction(() =>
      window.usageAccountsFixture.reloadCalls.some(
        ([key, force]) => key === "team" && !force,
      ),
    );
    const selectedTeamLimit = accountPanel.getByText("8% left", {
      exact: false,
    });
    await selectedTeamLimit.waitFor();
    assert.equal(await selectedTeamLimit.isVisible(), true);
    const teamResetCredits = page.getByRole("region", {
      name: "Limit reset credits",
    });
    await teamResetCredits.waitFor();
    assert.match(await teamResetCredits.innerText(), /Team reset/);
    await page
      .getByRole("button", { name: "Account limits", exact: true })
      .waitFor();
    assert.equal(
      await page
        .getByRole("button", { name: "Account limits" })
        .getAttribute("title"),
      "Low allowance: team@example.test",
    );
    await page.getByRole("button", { name: "Refresh", exact: true }).click();
    await page.waitForFunction(() =>
      window.usageAccountsFixture.reloadCalls.some(
        ([key, force]) => key === "team" && force,
      ),
    );
    await page
      .getByRole("button", { name: "Apply reset", exact: true })
      .click();
    await page
      .getByRole("button", { name: "Use one reset credit", exact: true })
      .click();
    const resetStatus = page.getByText("Reset applied.", { exact: true });
    await resetStatus.waitFor();
    assert.equal(await resetStatus.textContent(), "Reset applied.");
    const reset = captured.reset;
    assert.ok(reset, "reset request reached the fixture server");
    assert.equal(reset.account_id, "team-account");
    assert.equal(reset.credit_id, "team-credit");
    const ownTile = page.getByRole("tab", {
      name: "own@example.test account limits",
      exact: true,
    });
    assert.equal(
      await ownTile.getAttribute("aria-current"),
      "true",
      "detail selection keeps the current chat marker",
    );
    assert.equal(
      await ownTile.locator(".account-limits-tile-name").innerText(),
      "Own account",
    );
    const teamMeter = teamTab.getByRole("meter");
    assert.equal(await teamMeter.getAttribute("aria-valuenow"), "8");
    assert.equal(
      await teamMeter
        .locator("span")
        .evaluate((element) =>
          element instanceof HTMLElement ? element.style.background : "",
        ),
      "var(--studio-state-error)",
    );
    await page
      .getByRole("radio", { name: "Claude", exact: true })
      .locator("..")
      .locator("label")
      .click();
    const claudeTile = page.getByRole("tab", {
      name: "claude@example.test, claude account limits",
      exact: true,
    });
    await claudeTile.waitFor();
    assert.equal(
      await page.getByRole("tab").count(),
      1,
      "the provider shows only its own accounts",
    );
    assert.equal(await claudeTile.getAttribute("aria-selected"), "true");
    assert.equal(await claudeTile.getAttribute("aria-current"), null);
    const claudeMeter = claudeTile.getByRole("meter");
    assert.equal(await claudeMeter.getAttribute("aria-valuenow"), "50");
    assert.equal(
      await claudeMeter
        .locator("span")
        .evaluate((element) =>
          element instanceof HTMLElement ? element.style.background : "",
        ),
      "var(--studio-state-answer)",
    );
    await page.getByRole("button", { name: "Refresh", exact: true }).click();
    await page.waitForFunction(() =>
      window.usageAccountsFixture.reloadCalls.some(
        ([key, force]) => key === "claude" && force,
      ),
    );
    await page
      .getByRole("radio", { name: "Codex", exact: true })
      .locator("..")
      .locator("label")
      .click();
    await ownTile.waitFor();
    assert.equal(await ownTile.getAttribute("aria-current"), "true");
    assert.equal(await ownTile.getAttribute("aria-selected"), "true");
    assert.equal(
      await ownTile
        .getByRole("meter")
        .locator("span")
        .evaluate((element) =>
          element instanceof HTMLElement ? element.style.background : "",
        ),
      "var(--studio-state-unread)",
    );
    const unnamedTile = page.getByRole("tab", {
      name: "Account name unavailable, codex account limits",
      exact: true,
    });
    assert.equal(
      await unnamedTile.locator(".account-limits-tile-name").innerText(),
      "Account name unavailable",
    );
    assert.equal(
      await unnamedTile.getByRole("meter").getAttribute("aria-valuenow"),
      "15",
    );
    assert.equal(
      await unnamedTile
        .getByRole("meter")
        .locator("span")
        .evaluate((element) =>
          element instanceof HTMLElement ? element.style.background : "",
        ),
      "var(--studio-state-answer)",
    );
    await unnamedTile.click();
    assert.equal(await unnamedTile.getAttribute("aria-selected"), "true");
    assert.equal(await ownTile.getAttribute("aria-current"), "true");
    assert.equal(
      await page
        .locator(
          ".account-limits-detail-panel[data-active] .account-limits-selected-name strong",
        )
        .innerText(),
      "Account name unavailable",
    );

    // The five-account edge fixture reserves the taller mounted provider panel.
    for (const width of [320, 390, 1440])
      for (const scheme of ["light", "dark"]) {
        const page = await browser.newPage({
          viewport: { width, height: 844 },
          isMobile: width < 760,
          hasTouch: width < 760,
        });
        page.on("pageerror", (error) => errors.push(error.message));
        await page.goto(
          fixtureUrl(server) + `?geometry&scheme=${scheme}&now=${now}`,
        );
        assert.equal(
          await page.locator(".context-use").innerText(),
          "Context 10%",
        );
        await page.evaluate(() => document.fonts.ready);
        const dots = await page
          .locator(".account-limits-dot-target .account-limits-dot")
          .evaluateAll((elements) =>
            elements.map((element) => ({
              width: element.getBoundingClientRect().width,
              height: element.getBoundingClientRect().height,
              radius: getComputedStyle(element).borderRadius,
              color: element.getAttribute("data-color"),
              background: getComputedStyle(element).backgroundColor,
            })),
          );
        const palette: Record<string, string> =
          scheme === "light"
            ? {
                green: "rgb(52, 134, 109)",
                yellow: "rgb(166, 124, 33)",
                red: "rgb(179, 81, 81)",
                gray: "rgb(139, 144, 154)",
              }
            : {
                green: "rgb(103, 173, 145)",
                yellow: "rgb(213, 173, 82)",
                red: "rgb(204, 119, 119)",
                gray: "rgb(133, 139, 150)",
              };
        assert.equal(dots.length, 5);
        for (const dot of dots) {
          assert.equal(dot.width, 6);
          assert.equal(dot.height, 6);
          assert.equal(dot.radius, "50%");
          assert.equal(
            dot.background,
            dot.color === null ? undefined : palette[dot.color],
          );
        }
        const targets = await page
          .locator(".account-limits-dot-target")
          .evaluateAll((elements) =>
            elements.map((element) => element.getBoundingClientRect().toJSON()),
          );
        for (const [index, target] of targets.entries()) {
          assert.ok(
            target.width >= (width < 760 ? 24 : 12),
            "allowance dots have usable hit targets",
          );
          assert.ok(target.height >= 24);
          if (index)
            assert.ok(
              target.left >= (targets[index - 1]?.right ?? 0),
              "allowance targets do not overlap",
            );
        }
        const footerControls = await page
          .locator(
            ".context-use, .session-cost-summary, .account-limits-dots, .account-limits-toggle",
          )
          .evaluateAll((elements) =>
            elements
              .map((element) => ({
                name: element.className,
                ...element.getBoundingClientRect().toJSON(),
              }))
              .sort((a, b) => a.left - b.left),
          );
        for (const [index, control] of footerControls.entries()) {
          assert.ok(
            control.left >= 0 && control.right <= width,
            JSON.stringify({ width, footerControls }),
          );
          if (index)
            assert.ok(
              control.left >= (footerControls[index - 1]?.right ?? 0),
              JSON.stringify({ width, footerControls }),
            );
        }
        const footer = await page.locator(".usage-footer").boundingBox();
        assert.ok(footer);
        assert.ok(
          footer.height <= (width < 760 ? 24 : 32),
          "usage footer preserves its row height",
        );
        console.log(
          "Usage footer geometry:",
          JSON.stringify({
            width,
            scheme,
            height: footer.height,
            targetWidth: targets[0]?.width,
            targetHeight: targets[0]?.height,
          }),
        );
        const popover = page.locator(".account-limits-popover");
        await page
          .getByRole("button", { name: "Apply reset", exact: true })
          .waitFor();
        await page.waitForFunction(
          () =>
            getComputedStyle(
              document.querySelector(".account-limits-popover") ??
                document.body,
            ).opacity === "1",
        );
        await page.evaluate(
          () =>
            new Promise((resolve) =>
              requestAnimationFrame(() =>
                requestAnimationFrame(() => resolve(undefined)),
              ),
            ),
        );
        assert.doesNotMatch(await popover.innerText(), /Session estimate/);
        assert.match(await popover.innerText(), /Estimated API cost/);
        const before = await popover.boundingBox();
        assert.equal(await page.getByRole("tab").count(), 3);
        const assertFits = async () => {
          assert.deepEqual(
            await popover.evaluate((element) => {
              const panel = element.querySelector(".account-limits-panel");
              if (!panel) throw new Error("Limits panel is missing");
              return {
                clipped: panel.scrollHeight > panel.clientHeight + 1,
                scrollbars: [...element.querySelectorAll("*")]
                  .filter(
                    (e) =>
                      ["auto", "scroll"].includes(
                        getComputedStyle(e).overflowY,
                      ) && e.scrollHeight > e.clientHeight + 1,
                  )
                  .map((e) => e.className),
              };
            }),
            { clipped: false, scrollbars: [] },
          );
          const rect = await popover.boundingBox();
          assert.ok(rect);
          assert.ok(
            rect.y >= 0 && rect.y + rect.height <= 845,
            "all limits fit in the viewport",
          );
        };
        await assertFits();
        await page
          .getByRole("radio", { name: "Claude", exact: true })
          .locator("..")
          .locator("label")
          .click();
        await page.getByRole("link", { name: "Open free resets" }).waitFor();
        assert.equal(await page.getByRole("tab").count(), 2);
        const after = await popover.boundingBox();
        assert.ok(before);
        assert.ok(after);
        assert.equal(
          after.height,
          before.height,
          "switching providers preserves natural panel height",
        );
        assert.equal(
          after.y,
          before.y,
          "switching providers preserves popover position",
        );
        await assertFits();
        await page
          .getByRole("radio", { name: "Codex", exact: true })
          .locator("..")
          .locator("label")
          .click();
        await page
          .getByRole("button", { name: "Apply reset", exact: true })
          .waitFor();
        assert.equal((await popover.boundingBox())?.height, before.height);
        const claudeDot = page.locator(
          '.account-limits-dot-target[data-account-key="claude-main"]',
        );
        await claudeDot.focus();
        await page.keyboard.press("Enter");
        await page.getByRole("link", { name: "Open free resets" }).waitFor();
        assert.equal(await claudeDot.getAttribute("aria-expanded"), "true");
        assert.equal(
          await page
            .locator('.account-limits-dot-target[aria-current="true"]')
            .getAttribute("data-account-key"),
          "codex-main",
        );
        await page.context().close();
      }

    assert.deepEqual(errors, []);
    console.log(
      "Usage account UI: single account, tabs, lazy load, refresh, low allowance, account-bound reset, and stable five-account popovers at 320/390/1440 light/dark passed.",
    );
  } finally {
    await server.close();
  }
});
