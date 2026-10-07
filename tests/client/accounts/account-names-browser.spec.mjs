import { createRequire } from "node:module";
import { mkdtemp } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test, expect } from "../playwright.mjs";

async function fixture(page, mode) {
  page.on("pageerror", (error) =>
    console.error("Account fixture:", error.message),
  );
  const root = join(import.meta.dirname, "../../../web");
  const require = createRequire(join(root, "package.json"));
  const { createServer } = await import(require.resolve("vite"));
  const evidence = await mkdtemp(join(tmpdir(), "studio-account-names-"));
  const entry = join(root, "account-names-fixture.tsx");
  const accounts = [
    {
      id: "default",
      home: "/fixture/codex",
      source: "Codex",
      provider: "codex",
      label: "Personal",
      email: "personal@example.com",
      status: "signedOut",
    },
    {
      id: "work",
      home: "/fixture/claude",
      source: "Claude Code",
      provider: "claude",
      label: "Work 1",
      email: "work@example.com",
      status: "signedOut",
    },
  ];
  const data = () => ({
    accounts,
    archivedAccounts: [],
    defaultAccountKey: "default",
    logins: [],
  });
  const bodies = [];
  let loseResponse = false;
  await page.route("**/api/**", async (route) => {
    const path = new URL(route.request().url()).pathname;
    if (path === "/api/sync/identity")
      return route.fulfill({ status: 404, json: {} });
    if (path === "/api/accounts/name") {
      const body = route.request().postDataJSON();
      bodies.push(body);
      accounts.find((account) => account.id === body.account_key).label =
        body.label.trim();
      if (loseResponse) {
        loseResponse = false;
        return route.abort("failed");
      }
    }
    if (path === "/api/claude/profiles") {
      bodies.push(route.request().postDataJSON());
      return route.fulfill({ json: data() });
    }
    await route.fulfill({
      json:
        path === "/api/accounts" || path === "/api/accounts/name" ? data() : {},
    });
  });
  const server = await createServer({
    configFile: false,
    root,
    cacheDir: join(evidence, "cache"),
    server: { host: "127.0.0.1", port: 0 },
    optimizeDeps: {
      noDiscovery: false,
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
        name: "account-names-fixture",
        configureServer(server) {
          server.middlewares.use("/check", (_req, res) => {
            res.setHeader("Content-Type", "text/html");
            res.end(
              '<div id="root"></div><script type="module" src="/account-names-fixture.tsx"></script>',
            );
          });
        },
        resolveId(id) {
          if (id === "/account-names-fixture.tsx") return entry;
        },
        load(id) {
          if (id !== entry) return;
          return `import React,{useState}from'react';import{createRoot}from'react-dom/client';import{MantineProvider,Button}from'@mantine/core';import Accounts from'/src/components/Accounts.tsx';import{theme}from'/src/theme.ts';import'@mantine/core/styles.css';import'/src/style.css';import'/src/appearance.css';
const initial=${JSON.stringify(data())};const mode=${JSON.stringify(mode)};
function Fixture(){const[data,setData]=useState(initial);const[custom,setCustom]=useState(false);const[color,setColor]=useState('dark');window.setTheme=setColor;window.setCustom=setCustom;
const state={data,setData,error:'',refresh:async()=>data};const ready={...state,data:{...data,accounts:data.accounts.map(a=>({...a,status:'ready'}))}};
return <MantineProvider theme={theme} forceColorScheme={color}><main style={{padding:16,width:'100%',maxWidth:760,overflow:'auto'}}>{mode.startsWith('names')?<Accounts state={state} managerOnly={mode==='names'} onError={()=>{}}/>:<Accounts state={ready} agent={{id:'lead',isLead:true,empty:false,threadId:'thread',provider:'codex'}} accountKey="default" onError={()=>{}} renderPicker={custom?((selectAccount,disabled)=><><Button disabled={disabled} onClick={()=>selectAccount('work')}>Pick Work</Button><Button onClick={()=>selectAccount('unknown')}>Pick unknown</Button></>):undefined}/>}</main></MantineProvider>};createRoot(document.getElementById('root')).render(<Fixture/>);`;
        },
      },
    ],
  });
  await server.listen();
  await page.goto(`http://127.0.0.1:${server.httpServer.address().port}/check`);
  return {
    evidence,
    bodies,
    loseNextResponse: () => {
      loseResponse = true;
    },
    close: () => server.close(),
  };
}

test("accounts names save, clear, keys, exact retry, and themes", async ({
  page,
}) => {
  test.setTimeout(90_000);
  const f = await fixture(page, "names");
  try {
    const row = page.locator('[data-account="default"]');
    await row
      .getByRole("button", { name: "Edit name for personal@example.com" })
      .click();
    const input = row.getByRole("textbox", {
      name: "Name for personal@example.com",
    });
    await expect(input).toHaveAttribute("placeholder", "personal");
    await input.fill("  Work 2  ");
    await input.press("Enter");
    await expect(
      row.getByRole("button", { name: "Edit name for personal@example.com" }),
    ).toHaveText("Work 2");
    await row
      .getByRole("button", { name: "Edit name for personal@example.com" })
      .click();
    await input.fill("Discard");
    await input.press("Escape");
    expect(f.bodies).toHaveLength(1);
    await row
      .getByRole("button", { name: "Edit name for personal@example.com" })
      .click();
    await input.fill("x".repeat(33));
    await row.getByRole("button", { name: "Save", exact: true }).click();
    await expect(row.getByText("Use at most 32 characters.")).toBeVisible();
    expect(f.bodies).toHaveLength(1);
    await input.fill("");
    f.loseNextResponse();
    await input.press("Enter");
    await expect(row.locator(".mantine-TextInput-error")).toBeVisible();
    await row.getByRole("button", { name: "Save", exact: true }).click();
    await expect(
      row.getByRole("button", { name: "Edit name for personal@example.com" }),
    ).toHaveText("personal");
    expect(f.bodies[1]).toEqual(f.bodies[2]);
    const claude = page.locator('[data-account="work"]');
    await claude
      .getByRole("button", { name: "Edit name for work@example.com" })
      .click();
    await claude
      .getByRole("textbox", { name: "Name for work@example.com" })
      .fill("Claude 1");
    await claude.getByRole("button", { name: "Save", exact: true }).click();
    await expect(
      claude.getByRole("button", { name: "Edit name for work@example.com" }),
    ).toHaveText("Claude 1");
    await claude.getByText("Configure Claude", { exact: true }).click();
    await claude
      .getByRole("button", { name: "Save Claude profile", exact: true })
      .click();
    await expect(
      claude.getByText("Claude profile saved.", { exact: true }),
    ).toBeVisible();
    expect(f.bodies.at(-1)).not.toHaveProperty("label");
    await expect(
      claude.getByRole("button", { name: "Edit name for work@example.com" }),
    ).toHaveText("Claude 1");
    await claude.getByText("Configure Claude", { exact: true }).click();
    for (const [theme, width] of [
      ["dark", 1280],
      ["light", 1280],
      ["dark", 390],
      ["light", 390],
    ]) {
      await page.setViewportSize({ width, height: 1000 });
      await page.evaluate((value) => window.setTheme(value), theme);
      await expect(page.locator("html")).toHaveAttribute(
        "data-mantine-color-scheme",
        theme,
      );
      expect(
        await page.evaluate(
          () => document.documentElement.scrollWidth <= innerWidth,
        ),
      ).toBe(true);
      await page.locator(".accounts-list").screenshot({
        path: join(f.evidence, `accounts-${theme}-${width}.png`),
        animations: "disabled",
      });
      if (width === 390 && theme === "dark") {
        await claude
          .getByRole("button", { name: "Edit name for work@example.com" })
          .click();
        await expect(
          claude.getByRole("textbox", { name: "Name for work@example.com" }),
        ).toBeVisible();
        expect(
          await page.evaluate(
            () => document.documentElement.scrollWidth <= innerWidth,
          ),
        ).toBe(true);
        await page.locator(".accounts-list").screenshot({
          path: join(f.evidence, "accounts-edit-dark-390.png"),
          animations: "disabled",
        });
        await claude
          .getByRole("button", { name: "Cancel", exact: true })
          .click();
      }
    }
    console.log(`Account name screenshots: ${f.evidence}`);
  } finally {
    await f.close();
  }
});

test("accounts renderPicker uses the menu transfer confirmation", async ({
  page,
}) => {
  test.setTimeout(90_000);
  const f = await fixture(page, "picker");
  try {
    await page.getByTestId("account-picker").click();
    await page
      .getByRole("menuitem")
      .filter({ hasText: "work@example.com" })
      .click();
    const dialog = page.getByRole("dialog", {
      name: "Transfer this chat",
      exact: true,
    });
    await expect(dialog).toBeVisible();
    const menuText = await dialog.innerText();
    await dialog.getByRole("button", { name: "Cancel", exact: true }).click();
    await page.evaluate(() => window.setCustom(true));
    await page
      .getByRole("button", { name: "Pick unknown", exact: true })
      .click();
    await expect(dialog).not.toBeVisible();
    await page.getByRole("button", { name: "Pick Work", exact: true }).click();
    await expect(dialog).toBeVisible();
    expect(await dialog.innerText()).toBe(menuText);
    expect(f.bodies).toHaveLength(0);
  } finally {
    await f.close();
  }
});

test("accounts Name Escape keeps the manager open", async ({ page }) => {
  test.setTimeout(90_000);
  const f = await fixture(page, "names-modal");
  try {
    await page.getByTestId("account-picker").click();
    await page.getByRole("menuitem", { name: /Manage accounts/ }).click();
    const manager = page.getByRole("dialog", { name: "Accounts", exact: true });
    await manager
      .getByRole("button", { name: "Edit name for personal@example.com" })
      .click();
    await manager
      .getByRole("textbox", { name: "Name for personal@example.com" })
      .press("Escape");
    await expect(manager).toBeVisible();
    await expect(
      manager.getByRole("button", {
        name: "Edit name for personal@example.com",
      }),
    ).toBeVisible();
    expect(f.bodies).toHaveLength(0);
  } finally {
    await f.close();
  }
});
