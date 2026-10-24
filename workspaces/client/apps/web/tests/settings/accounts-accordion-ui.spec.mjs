import { test, expect } from "../playwright.mjs";
import { fixture } from "../servers/multi-server-fixture.mjs";
import { accountLimitFixtures } from "../../src/components/AccountLimits.fixtures.ts";

async function keyboardCycle(page, region) {
  const targets = await region.evaluate((root) => {
    const elements = [
      ...root.querySelectorAll(
        "button,a[href],input,select,textarea,[tabindex]",
      ),
    ].filter(
      (element) =>
        element.tabIndex >= 0 &&
        !element.matches(":disabled,input[type=radio]:not(:checked)") &&
        element.getClientRects().length &&
        getComputedStyle(element).visibility === "visible",
    );
    return {
      count: elements.length,
      ids: elements.map((element) => element.id).filter(Boolean),
    };
  });
  // Browser-owned traversal must visit every identified control and wrap in both directions.
  for (const key of ["Tab", "Shift+Tab"]) {
    const visited = new Set();
    for (let step = 0; step < targets.count + 2; step++) {
      await page.keyboard.press(key);
      const active = await region.evaluate((root) => ({
        contained: root.contains(document.activeElement),
        id: document.activeElement?.id,
      }));
      expect(active.contained).toBe(true);
      visited.add(active.id);
    }
    for (const id of targets.ids)
      expect(visited.has(id), `${key} reaches ${id}`).toBe(true);
  }
}

test("Studio Accounts is exclusive and keeps keyboard focus inside settings and limits", async ({
  page,
}) => {
  test.setTimeout(120000);
  const accounts = accountLimitFixtures.map((item, index) => ({
    id: index ? item.key : "default",
    accountId: item.accountId || item.key,
    label: item.label,
    email: item.email,
    provider: item.provider,
    plan: item.plan,
    status: "ready",
    isDefault: index === 0,
    home: `/fixture/${item.key}`,
    source: "fixture",
  }));
  const limits = Object.fromEntries(
    accounts.map((account, index) => [
      account.id,
      {
        ...accountLimitFixtures[index].limits,
        accountKey: account.id,
        ...(accountLimitFixtures[index].limits.data
          ? {
              data: {
                ...accountLimitFixtures[index].limits.data,
                accountId: account.accountId,
              },
            }
          : {}),
      },
    ]),
  );
  const local = await fixture("Workstation", false, {
    accounts,
    handle: ({ url, json }) => {
      if (url.pathname === "/api/limits") {
        json(limits[url.searchParams.get("account_key") || "default"]);
        return true;
      }
      if (url.pathname === "/api/costs") {
        json({
          accountKey: url.searchParams.get("account_key") || "default",
          data: { todayUSD: 4.2, last30DaysUSD: 28.35 },
        });
        return true;
      }
      if (url.pathname === "/api/session-cost") {
        json({ rootId: "overlap", pricingState: "ready", totalUSD: 1.25 });
        return true;
      }
    },
  });
  local.snapshot.runtime.rateLimitsByAccount = limits;
  local.snapshot.runtime.rateLimits = limits.default;
  const lead = local.snapshot.threads[0];
  local.setChats(
    accounts.map((account, index) => ({
      ...lead,
      id: index ? `worker-${index}` : "overlap",
      rootId: "overlap",
      isLead: index === 0,
      accountKey: account.id,
      provider: account.provider,
      status: index ? "running" : "waiting",
      inFlight: index > 0,
    })),
  );
  try {
    for (const width of [320, 390, 768, 1440])
      for (const scheme of ["light", "dark"]) {
        await page.setViewportSize({ width, height: 844 });
        await page.addInitScript(
          (value) => localStorage.setItem("mantine-color-scheme-value", value),
          scheme,
        );
        await page.goto(local.origin);
        await page.locator("#message").waitFor();
        // A fresh phone page opens the chats board above the chat.
        if (width <= 760)
          await page.locator('#sidebar [data-chat="overlap"]').first().click();
        await page
          .getByRole("button", { name: "Account limits", exact: true })
          .click();
        const popover = page.locator(".account-limits-popover");
        await expect(popover).toBeVisible();
        await popover
          .getByRole("button", { name: "Refresh", exact: true })
          .focus();
        await keyboardCycle(page, popover);
        await page
          .getByRole("radio", { name: "Claude", exact: true })
          .locator("..")
          .locator("label")
          .click();
        await keyboardCycle(page, popover);
        await page.keyboard.press("Escape");
        if (width < 761)
          await page
            .getByRole("button", {
              name: /^(Servers and chats|Back to chats)$/,
            })
            .click();
        await page
          .getByRole("button", { name: "Studio settings", exact: true })
          .click();
        const dialog = page.getByRole("dialog", {
          name: "Studio settings",
          exact: true,
        });
        if (
          await dialog
            .getByRole("button", { name: "Back", exact: true })
            .isVisible()
        )
          await dialog
            .getByRole("button", { name: "Back", exact: true })
            .click();
        await dialog
          .getByRole("tab", { name: "Accounts", exact: true })
          .click();
        const rows = dialog.locator(".server-account-row");
        await expect(rows).toHaveCount(5);
        await expect(dialog.locator(".account-expanded")).toHaveCount(0);
        const heights = await rows.evaluateAll((elements) =>
          elements.map((element) => element.getBoundingClientRect().height),
        );
        expect(
          heights.every((height) => height <= (width === 1440 ? 44 : 80)),
        ).toBe(true);
        const names = await rows
          .locator(".account-identity > strong")
          .evaluateAll((elements) =>
            elements.map((element) => ({
              width: element.getBoundingClientRect().width,
              full: element.scrollWidth,
              eightCh: (() => {
                const context = document
                  .createElement("canvas")
                  .getContext("2d");
                context.font = getComputedStyle(element).font;
                return context.measureText("00000000").width;
              })(),
            })),
          );
        expect(names.every((name) => name.width >= name.eightCh - 1)).toBe(
          true,
        );
        if (width === 320) {
          const alignment = await rows.first().evaluate((row) => {
            const name = row.querySelector(".account-identity > strong");
            const plan = row.querySelector(".account-plan");
            return Math.abs(
              name.getBoundingClientRect().x -
                plan.getBoundingClientRect().x -
                parseFloat(getComputedStyle(plan).paddingLeft),
            );
          });
          expect(alignment).toBeLessThanOrEqual(1);
        }
        await dialog
          .getByRole("button", { name: "Close", exact: true })
          .focus();
        await keyboardCycle(page, dialog);
        for (let index = 0; index < 5; index++) {
          await rows
            .nth(index)
            .getByRole("button", { name: /^Limit details for/ })
            .click();
          await expect(dialog.locator(".account-expanded")).toHaveCount(1);
          await expect(
            rows
              .nth(index)
              .getByRole("heading", { name: "Limits", exact: true }),
          ).toBeVisible();
          await expect(
            rows
              .nth(index)
              .getByRole("heading", { name: "Account", exact: true }),
          ).toBeVisible();
          await dialog
            .getByRole("button", { name: "Close", exact: true })
            .focus();
          await keyboardCycle(page, dialog);
        }
        await expect(
          dialog.getByRole("button", { name: "Apply reset", exact: true }),
        ).toHaveCount(0);
        const throttle = rows
          .last()
          .getByRole("button", { name: /^Turn start limits/ });
        if (await throttle.count()) {
          await throttle.first().click();
          await expect(
            rows.last().getByRole("button", { name: "Save turn start limits" }),
          ).toBeVisible();
          await keyboardCycle(page, dialog);
        }
        if (
          await dialog
            .getByRole("button", { name: "Back", exact: true })
            .isVisible()
        )
          await dialog
            .getByRole("button", { name: "Back", exact: true })
            .click();
        await dialog.getByRole("tab", { name: "Servers", exact: true }).click();
        const heading = dialog.getByRole("heading", {
          name: "Servers",
          exact: true,
          level: 3,
        });
        const headingBox = await heading.boundingBox();
        const headingLine = await heading.evaluate((element) =>
          parseFloat(getComputedStyle(element).lineHeight),
        );
        expect(headingBox.height).toBeLessThanOrEqual(headingLine + 1);
        const invitation = await dialog
          .getByRole("button", { name: "Add with an invitation", exact: true })
          .boundingBox();
        if (width === 320)
          expect(invitation.y).toBeGreaterThanOrEqual(
            headingBox.y + headingBox.height,
          );
        await dialog
          .getByRole("button", { name: "Edit", exact: true })
          .first()
          .click();
        await expect(
          dialog
            .getByRole("textbox", { name: "Server name", exact: true })
            .first(),
        ).toBeVisible();
        await keyboardCycle(page, dialog);
        await dialog
          .getByRole("button", { name: "Edit", exact: true })
          .first()
          .click();
        await expect(
          dialog.getByRole("textbox", { name: "Server name", exact: true }),
        ).toHaveCount(0);
        await keyboardCycle(page, dialog);
        expect(
          await dialog.evaluate(
            (element) => element.scrollWidth <= element.clientWidth + 1,
          ),
        ).toBe(true);
        if (
          await dialog
            .getByRole("button", { name: "Back", exact: true })
            .isVisible()
        )
          await dialog
            .getByRole("button", { name: "Back", exact: true })
            .click();
        await dialog
          .getByRole("tab", { name: "Appearance", exact: true })
          .click();
        const more = dialog.getByRole("button", { name: "More", exact: true });
        await expect(more).toHaveAttribute("aria-expanded", "false");
        await expect(
          dialog.getByRole("combobox", { name: "Studio text style" }),
        ).toHaveCount(0);
        await keyboardCycle(page, dialog);
        await more.click();
        await expect(
          dialog.getByRole("combobox", { name: "Studio text style" }),
        ).toBeVisible();
        await keyboardCycle(page, dialog);
        await more.click();
        await expect(
          dialog.getByRole("combobox", { name: "Studio text style" }),
        ).toHaveCount(0);
        await keyboardCycle(page, dialog);
      }
  } finally {
    await local.close();
  }
});
