import { createRequire } from "node:module";
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
} from "../playwright.mjs";

// Exercise the Accounts manager through its Studio settings host and native menus.
async function fixture(page) {
  const root = join(import.meta.dirname, "../../");
  const require = createRequire(join(root, "package.json"));
  const { createServer } = await import(require.resolve("vite"));
  const evidence = await mkdtemp(join(tmpdir(), "studio-accounts-cards-"));
  const entry = join(root, "accounts-cards-fixture.tsx");
  const accounts = [
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
  ].map((account) => ({
    ...account,
    home: `/fixture/${account.id}`,
    source: account.provider,
    accountId: `native-${account.id}`,
  }));
  let defaultAccountKey = "personal";
  let loseDeleteResponse = true;
  const bodies = [];
  const deleted = new Set();
  const data = () => ({
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
      const resources = JSON.parse(url.searchParams.get("resources") || "[]");
      return route.fulfill({
        contentType: "text/event-stream",
        body: apiSchemaHandshakeSse(
          protocol3SseEvent("resources", {
            protocol: 3,
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
      const body = route.request().postDataJSON();
      bodies.push({ path, body });
      const account = accounts.find(
        (account) => account.id === body.account_key,
      );
      if (path === "/api/accounts/default")
        defaultAccountKey = body.account_key;
      if (path === "/api/accounts/disconnect") {
        account.disconnected = true;
        if (defaultAccountKey === account.id) defaultAccountKey = "personal";
      }
      if (path === "/api/accounts/reconnect") account.disconnected = false;
      if (path === "/api/accounts/delete") {
        if (!deleted.has(body.request_id)) {
          accounts.splice(accounts.indexOf(account), 1);
          deleted.add(body.request_id);
        }
        if (loseDeleteResponse) {
          loseDeleteResponse = false;
          return route.abort("failed");
        }
      }
    }
    if (path === "/api/limits") {
      const key = url.searchParams.get("account_key");
      const usedPercent =
        { personal: 18, work: 85, claude: 100, "claude-work": 45 }[key] ?? 10;
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
  const server = await createServer({
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
          server.middlewares.use("/check", (_req, res) => {
            res.setHeader("Content-Type", "text/html");
            res.end(
              '<div id="root"></div><script type="module" src="/accounts-cards-fixture.tsx"></script>',
            );
          });
        },
        resolveId(id) {
          if (id === "/accounts-cards-fixture.tsx") return entry;
        },
        load(id) {
          if (id !== entry) return;
          return `import React,{useState} from 'react';import{createRoot}from'react-dom/client';import{MantineProvider,Modal,Tabs}from'@mantine/core';import Accounts from'/src/components/Accounts.tsx';import{theme,modalSizes}from'/src/theme.ts';import'@mantine/core/styles.css';import'/src/style.css';import'/src/appearance.css';
        function Fixture(){const[data,setData]=useState(${JSON.stringify(data())});const[color,setColor]=useState('dark');const[blocked,setBlocked]=useState(false);window.setTheme=setColor;return <MantineProvider theme={theme} forceColorScheme={color}><Modal opened title="Studio settings" size={modalSizes.settings} onClose={()=>{}} closeOnEscape={!blocked} transitionProps={{duration:0}} classNames={{body:'studio-settings-body'}}><div className="studio-settings-panel"><Tabs defaultValue="accounts" className="studio-settings-tabs"><Tabs.List aria-label="Studio settings"><Tabs.Tab value="accounts">Accounts</Tabs.Tab><Tabs.Tab value="appearance">Appearance</Tabs.Tab></Tabs.List><Tabs.Panel value="accounts" pt="md"><section className="settings-group" aria-label="Studio accounts"><Accounts managerOnly state={{data,setData,error:'',scope:'cards',refresh:async()=>data}} onError={()=>{}} onModalOpenChange={setBlocked}/></section></Tabs.Panel></Tabs></div></Modal></MantineProvider>};createRoot(document.getElementById('root')).render(<Fixture/>);`;
        },
      },
    ],
  });
  await server.listen();
  await page.goto(`http://127.0.0.1:${server.httpServer.address().port}/check`);
  return { evidence, bodies, close: () => server.close() };
}

test("Accounts provider cards preserve menus, confirmations, exact retries and responsive limits", async ({
  page,
}) => {
  test.setTimeout(90_000);
  const errors = [];
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
        .getByText("Sign in needed", { exact: true }),
    ).toBeVisible();
    const failure = settings.locator('[data-account="error"]');
    await expect(failure.getByText("Error", { exact: true })).toBeVisible();
    await failure
      .getByRole("button", { name: "Error details", exact: true })
      .click();
    await expect(failure).toContainText("Expired credentials");
    await failure
      .getByRole("button", { name: "Hide error details", exact: true })
      .click();
    await expect(
      personal.getByText("Application default", { exact: true }),
    ).toBeVisible();
    for (const [color, width] of [
      ["dark", 1440],
      ["light", 1440],
      ["dark", 390],
    ]) {
      await page.setViewportSize({ width, height: 1000 });
      await page.evaluate((color) => window.setTheme(color), color);
      await expect(page.locator("html")).toHaveAttribute(
        "data-mantine-color-scheme",
        color,
      );
      const a = await personal.boundingBox();
      const b = await work.boundingBox();
      if (width === 390) {
        expect(b.y).toBeGreaterThan(a.y);
        expect(b.x).toBe(a.x);
      } else {
        expect(b.y).toBe(a.y);
        expect(b.x).toBeGreaterThan(a.x);
      }
      expect(
        await personal
          .locator(".account-limit-bar")
          .evaluate((element) => element.getBoundingClientRect().height),
      ).toBe(3);
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
    const actions = async (row, email) => {
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
    await page.keyboard.press("Escape");
    await expect(trigger).toBeFocused();
    await actions(work, "work@example.com");
    await page
      .getByRole("menuitem", {
        name: "Use work@example.com by default",
        exact: true,
      })
      .click();
    await expect(
      work.getByText("Application default", { exact: true }),
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
      name: "Sign in to travel@example.com",
      exact: true,
    });
    await expect(signIn).toBeVisible();
    await signIn.getByRole("button", { name: "Close", exact: true }).click();
    await actions(claude, "claude@example.com");
    await page
      .getByRole("menuitem", { name: "Sign in again", exact: true })
      .click();
    const claudeSignIn = page.getByRole("dialog", {
      name: "Sign in to Claude",
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
    expect(deletes[0].body.request_id).toMatch(/^[0-9a-f-]{36}$/);
    expect(errors).toEqual([]);
    console.log(`Accounts card screenshots: ${f.evidence}`);
  } finally {
    await f.close();
  }
});
