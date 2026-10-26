import type { Page } from "@playwright/test";
import type { Account, AccountsState } from "../../src/components/Accounts.tsx";
import {
  required,
  fixtureUrl,
  fixtureAccount,
  fixtureAccounts,
  fixtureRequest,
  type FixtureRequest,
} from "./fixture-helpers.ts";
import type {} from "./accounts-cards.fixture.tsx";
import { createFixtureViteServer } from "../playwright.ts";
import { mkdtemp } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import {
  test,
  expect,
  API_SCHEMA_HASH_HEADER,
  readApiSchemaHash,
  apiSchemaHandshakeSse,
  protocol3SseEvent,
} from "../playwright.ts";

// Exercise the Accounts manager through its Studio settings host and native menus.
async function fixture(page: Page) {
  const root = join(import.meta.dirname, "../../");
  const evidence = await mkdtemp(join(tmpdir(), "studio-accounts-cards-"));
  const accounts: Account[] = (
    [
      {
        id: "personal",
        provider: "codex",
        label: "Personal",
        email: "personal@example.com",
        status: "ready",
        plan: "pro",
      },
      {
        id: "work",
        provider: "codex",
        label: "Work",
        email: "work@example.com",
        status: "ready",
        plan: "plus",
      },
      {
        id: "expired",
        provider: "codex",
        label: "Travel",
        email: "travel@example.com",
        status: "signedOut",
      },
      {
        id: "error",
        provider: "codex",
        label: "Second workspace",
        email: "another.long.account@example.com",
        status: "error",
        // @ts-expect-error Structured account error intentionally verifies diagnostic rendering beyond the wire string.
        error: {
          message: "The account needs attention.",
          code: "fixture-error",
          details: { reason: "Expired credentials" },
        },
      },
      {
        id: "claude",
        provider: "claude",
        label: "Claude personal",
        email: "claude@example.com",
        status: "ready",
        plan: "max",
      },
      {
        id: "claude-work",
        provider: "claude",
        label: "Claude work",
        email: "claude.work@example.com",
        status: "ready",
        plan: "pro",
      },
      {
        id: "claude-expired",
        provider: "claude",
        label: "Claude travel",
        email: "claude.travel@example.com",
        status: "signedOut",
      },
    ] satisfies Parameters<typeof fixtureAccount>[0][]
  ).map((account) =>
    fixtureAccount({
      ...account,
      // @ts-expect-error The structured error from the diagnostic negative case is intentionally outside the string wire contract.
      error: account.error,
      home: `/fixture/${account.id}`,
      source: account.provider,
      accountId: `native-${account.id}`,
    }),
  );
  let defaultAccountKey = "personal";
  let loseDeleteResponse = true;
  const bodies: FixtureRequest[] = [];
  const deleted = new Set<string>();
  const data = (): AccountsState =>
    fixtureAccounts({
      accounts,
      archivedAccounts: [],
      defaultAccountKey,
      logins: [],
      supportsDisconnect: true,
      supportsDelete: true,
    });
  await page.route("**/api/**", async (route) => {
    const url = new URL(route.request().url());
    const path = url.pathname;
    if (path === "/api/sync/identity")
      return route.fulfill({
        json: { workspaceId: "a".repeat(32) },
        headers: { [API_SCHEMA_HASH_HEADER]: readApiSchemaHash() },
      });
    if (path === "/api/sync/stream") {
      const resources: import("../playwright.ts").ResourceRef[] = JSON.parse(
        url.searchParams.get("resources") || "[]",
      );
      return route.fulfill({
        contentType: "text/event-stream",
        body: apiSchemaHandshakeSse(
          protocol3SseEvent("resources", {
            protocol: 4,
            workspaceId: "a".repeat(32),
            epoch: "cards",
            revision: 1,
            reason: "initial",
            resources,
            resourceVersions: resources.map((resource) => ({
              resource,
              revision: 1,
            })),
          }),
        ),
      });
    }
    if (route.request().method() === "POST") {
      const request = fixtureRequest(path, route.request().postDataJSON());
      bodies.push(request);
      const accountKey =
        "account_key" in request.body ? request.body.account_key : undefined;
      const account = accounts.find((account) => account.id === accountKey);
      if (request.path === "/api/accounts/default")
        defaultAccountKey = required(request.body.account_key);
      if (path === "/api/accounts/disconnect") {
        required(account).disconnected = true;
        if (defaultAccountKey === required(account).id)
          defaultAccountKey = "personal";
      }
      if (path === "/api/accounts/reconnect")
        required(account).disconnected = false;
      if (request.path === "/api/accounts/delete") {
        const body = request.body;
        if (!deleted.has(body.request_id)) {
          accounts.splice(accounts.indexOf(required(account)), 1);
          deleted.add(body.request_id);
        }
        if (loseDeleteResponse) {
          loseDeleteResponse = false;
          return route.abort("failed");
        }
      }
    }
    if (path === "/api/limits") {
      const key = required(url.searchParams.get("account_key"));
      const usedPercent =
        (
          {
            personal: 18,
            work: 85,
            claude: 100,
            "claude-work": 45,
          } satisfies Record<string, number>
        )[key] ?? 10;
      return route.fulfill({
        json: {
          accountKey: key,
          at: Date.now() / 1000,
          data: {
            accountId: `native-${key}`,
            rateLimits: {
              limitId: key.startsWith("claude") ? "claude" : "codex",
              primary: { usedPercent, windowDurationMins: 300 },
              secondary: { usedPercent: 5, windowDurationMins: 10080 },
            },
          },
        },
      });
    }
    return route.fulfill({
      json: path.startsWith("/api/accounts") ? data() : {},
    });
  });
  const server = await createFixtureViteServer({
    configFile: false,
    root,
    cacheDir: join(evidence, "cache"),
    server: { host: "127.0.0.1", port: 0 },
    optimizeDeps: {
      include: [
        "react",
        "react/jsx-runtime",
        "react/jsx-dev-runtime",
        "react-dom/client",
        "react-dom",
        "dexie",
      ],
    },
    plugins: [
      {
        name: "accounts-cards-fixture",
        configureServer(server) {
          server.middlewares.use("/fixture-data", (_req, res) => {
            res.setHeader("Content-Type", "application/json");
            res.end(JSON.stringify(data()));
          });
          server.middlewares.use("/check", (_req, res) => {
            res.setHeader("Content-Type", "text/html");
            res.end(
              '<div id="root"></div><script type="module" src="/tests/accounts/accounts-cards.fixture.tsx"></script>',
            );
          });
        },
      },
    ],
  });
  await server.listen();
  await page.goto(`${fixtureUrl(server)}/check`);
  return { evidence, bodies, close: () => server.close() };
}

test("Accounts provider cards preserve menus, confirmations, exact retries and responsive limits", async ({
  page,
}) => {
  test.setTimeout(90_000);
  const errors: string[] = [];
  page.on("pageerror", (error) => errors.push(error.message));
  const f = await fixture(page);
  try {
    const settings = page.getByRole("dialog", {
      name: "Studio settings",
      exact: true,
    });
    await expect(settings.locator("[data-account]")).toHaveCount(7);
    await expect(
      settings.getByRole("heading", { name: "Codex", exact: true }),
    ).toBeVisible();
    await expect(
      settings.getByRole("heading", { name: "Claude", exact: true }),
    ).toBeVisible();
    await expect(
      settings.locator(
        '.accounts-provider[aria-label="Codex accounts"] [data-account]',
      ),
    ).toHaveCount(4);
    await expect(
      settings.locator(
        '.accounts-provider[aria-label="Claude accounts"] [data-account]',
      ),
    ).toHaveCount(3);
    const personal = settings.locator('[data-account="personal"]');
    const work = settings.locator('[data-account="work"]');
    const claude = settings.locator('[data-account="claude"]');
    await expect(
      personal.getByRole("meter", { name: "Remaining limit" }),
    ).toHaveAttribute("aria-valuenow", "82");
    await expect(
      work.getByRole("meter", { name: "Remaining limit" }),
    ).toHaveAttribute("aria-valuenow", "15");
    await expect(
      claude.getByRole("meter", { name: "Remaining limit" }),
    ).toHaveAttribute("aria-valuenow", "0");
    await expect(
      settings
        .locator('[data-account="expired"]')
        .locator(".account-card-heading")
        .getByText("Sign in needed", { exact: true }),
    ).toBeVisible();
    const failure = settings.locator('[data-account="error"]');
    await expect(
      failure
        .locator(".account-card-heading")
        .getByText("Error", { exact: true }),
    ).toBeVisible();
    await failure.getByRole("button", { name: /^Limit details for/ }).click();
    await failure
      .getByRole("button", { name: "Error details", exact: true })
      .click();
    await expect(failure).toContainText("Expired credentials");
    await failure
      .getByRole("button", { name: "Hide error details", exact: true })
      .click();
    await failure.getByRole("button", { name: /^Limit details for/ }).click();
    await expect(
      personal
        .locator(".account-card-heading")
        .getByText("Application default", { exact: true }),
    ).toBeVisible();
    for (const [color, width] of [
      ["dark", 1440],
      ["light", 1440],
      ["dark", 390],
    ] as const) {
      await page.setViewportSize({ width, height: 1000 });
      await page.evaluate(
        (color) => window.accountsCardsFixture.setTheme(color),
        color,
      );
      await expect(page.locator("html")).toHaveAttribute(
        "data-mantine-color-scheme",
        color,
      );
      const a = required(await personal.boundingBox());
      const b = required(await work.boundingBox());
      expect(b.y).toBeGreaterThan(a.y);
      expect(b.x).toBe(a.x);
      expect(a.height).toBeLessThanOrEqual(80);
      expect(b.height).toBeLessThanOrEqual(80);
      const windows = personal.locator(".account-window-grid");
      await expect(windows).not.toBeAttached();
      await expect(
        personal.getByRole("textbox", {
          name: "Group of personal@example.com",
        }),
      ).toBeHidden();
      await personal
        .getByRole("button", { name: /^Limit details for/ })
        .click();
      await expect(
        personal.getByRole("textbox", {
          name: "Group of personal@example.com",
        }),
      ).toBeVisible();
      expect(
        await personal
          .locator(".account-window-meter")
          .first()
          .evaluate((element) => element.getBoundingClientRect().height),
      ).toBe(3);
      await work.getByRole("button", { name: /^Limit details for/ }).click();
      await expect(personal.locator(".account-expanded")).not.toBeAttached();
      await expect(work.locator(".account-expanded")).toBeVisible();
      await work.getByRole("button", { name: /^Limit details for/ }).click();
      expect(
        await page.evaluate(
          () => document.documentElement.scrollWidth <= innerWidth,
        ),
      ).toBe(true);
      await page.screenshot({
        path: join(f.evidence, `accounts-${color}-${width}.png`),
        animations: "disabled",
      });
    }
    await page.setViewportSize({ width: 1440, height: 1000 });
    const actions = async (
      row: import("@playwright/test").Locator,
      email: string,
    ) => {
      await expect(page.getByRole("menu")).not.toBeVisible();
      await row
        .getByRole("button", { name: `Actions for ${email}`, exact: true })
        .click();
      await expect(page.getByRole("menu")).toBeVisible();
    };
    const trigger = work.getByRole("button", {
      name: "Actions for work@example.com",
      exact: true,
    });
    await trigger.focus();
    await trigger.press("Enter");
    const menu = page.getByRole("menu");
    await expect(menu.getByRole("menuitem")).toHaveText([
      "Set application default",
      "Sign in again",
      "Disconnect account",
      "Delete account",
    ]);
    await expect(menu.getByRole("menuitem").nth(0)).toBeFocused();
    await page.keyboard.press("ArrowDown");
    await expect(menu.getByRole("menuitem").nth(1)).toBeFocused();
    await page.keyboard.press("Escape");
    await expect(menu).not.toBeVisible();
    await expect(trigger).toBeFocused();
    await page.keyboard.press("Escape");
    await expect(settings).toBeVisible();
    await actions(work, "work@example.com");
    await page
      .getByRole("menuitem", {
        name: "Use work@example.com by default",
        exact: true,
      })
      .click();
    await expect(
      work
        .locator(".account-card-heading")
        .getByText("Application default", { exact: true }),
    ).toBeVisible();
    expect(f.bodies.at(-1)).toEqual({
      path: "/api/accounts/default",
      body: { account_key: "work" },
    });
    await actions(
      settings.locator('[data-account="expired"]'),
      "travel@example.com",
    );
    await expect(
      page.getByRole("menuitem", {
        name: "Use travel@example.com by default",
        exact: true,
      }),
    ).toHaveAttribute("data-disabled", "true");
    await page.keyboard.press("Escape");
    await actions(work, "work@example.com");
    await page
      .getByRole("menuitem", { name: "Disconnect account", exact: true })
      .click();
    const disconnect = page.getByRole("dialog", {
      name: "Disconnect account",
      exact: true,
    });
    await expect(disconnect).toContainText(
      "The application default changes to personal@example.com.",
    );
    await disconnect
      .getByRole("button", { name: "Cancel", exact: true })
      .click();
    expect(f.bodies).toHaveLength(1);
    await actions(work, "work@example.com");
    await page
      .getByRole("menuitem", { name: "Disconnect account", exact: true })
      .click();
    await disconnect
      .getByRole("button", { name: "Disconnect account", exact: true })
      .click();
    await work.getByRole("button", { name: /^Limit details for/ }).click();
    await expect(
      work.getByText("Hidden from new chats.", { exact: true }),
    ).toBeVisible();
    await actions(work, "work@example.com");
    await page
      .getByRole("menuitem", { name: "Reconnect account", exact: true })
      .click();
    await expect(
      work.getByText("Hidden from new chats.", { exact: true }),
    ).not.toBeVisible();
    expect(f.bodies.slice(-2).map((request) => request.path)).toEqual([
      "/api/accounts/disconnect",
      "/api/accounts/reconnect",
    ]);
    await actions(
      settings.locator('[data-account="expired"]'),
      "travel@example.com",
    );
    await page
      .getByRole("menuitem", { name: "Sign in again", exact: true })
      .click();
    const signIn = page.getByRole("dialog", {
      name: "Sign in to Codex · travel@example.com",
      exact: true,
    });
    await expect(signIn).toBeVisible();
    await signIn.getByRole("button", { name: "Close", exact: true }).click();
    await actions(claude, "claude@example.com");
    await page
      .getByRole("menuitem", { name: "Sign in again", exact: true })
      .click();
    const claudeSignIn = page.getByRole("dialog", {
      name: "Sign in to Claude · Claude personal",
      exact: true,
    });
    await expect(claudeSignIn).toBeVisible();
    await claudeSignIn.getByLabel("Close", { exact: true }).click();
    await actions(work, "work@example.com");
    await page
      .getByRole("menuitem", { name: "Delete account", exact: true })
      .click();
    const deletion = page.getByRole("dialog", {
      name: "Delete account",
      exact: true,
    });
    await deletion
      .getByRole("button", { name: "Delete account", exact: true })
      .click();
    await expect(deletion.getByRole("alert")).toBeVisible();
    await deletion
      .getByRole("button", { name: "Delete account", exact: true })
      .click();
    await expect(deletion).not.toBeVisible();
    await expect(work).toHaveCount(0);
    const deletes = f.bodies.filter(
      (request) => request.path === "/api/accounts/delete",
    );
    expect(deletes).toHaveLength(2);
    expect(deletes[0]).toEqual(deletes[1]);
    expect(required(deletes[0]).body.request_id).toMatch(/^[0-9a-f-]{36}$/);
    expect(errors).toEqual([]);
    console.log(`Accounts card screenshots: ${f.evidence}`);
  } finally {
    await f.close();
  }
});
