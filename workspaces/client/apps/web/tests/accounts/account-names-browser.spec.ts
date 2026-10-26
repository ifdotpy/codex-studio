import { join } from "node:path";
import { test, expect, createFixtureViteServer } from "../playwright.ts";

async function fixture(page) {
  page.on("pageerror", (error) =>
    console.error("Account fixture:", error.message),
  );
  const root = join(import.meta.dirname, "../../");
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
  const server = await createFixtureViteServer({
    configFile: false,
    root,
    server: { host: "127.0.0.1", port: 0 },
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
          return `import React,{useState}from'react';import{createRoot}from'react-dom/client';import{MantineProvider}from'@mantine/core';import Accounts from'/src/components/Accounts.tsx';import{theme}from'/src/theme.ts';import'@mantine/core/styles.css';import'/src/style.css';import'/src/appearance.css';
const initial=${JSON.stringify(data())};
function Fixture(){const[data,setData]=useState(initial);const[color,setColor]=useState('dark');window.setTheme=setColor;
const state={data,setData,error:'',refresh:async()=>data};
return <MantineProvider theme={theme} forceColorScheme={color}><main style={{padding:16,width:'100%',maxWidth:760,overflow:'auto'}}><Accounts state={state} managerOnly onError={()=>{}}/></main></MantineProvider>};createRoot(document.getElementById('root')).render(<Fixture/>);`;
        },
      },
    ],
  });
  try {
    await server.listen();
    await page.goto(
      `http://127.0.0.1:${server.httpServer.address().port}/check`,
    );
  } catch (error) {
    await server.close();
    throw error;
  }
  return {
    bodies,
    loseNextResponse: () => {
      loseResponse = true;
    },
    close: () => server.close(),
  };
}

test("accounts names save, clear, keys, exact retry, and themes", async ({
  page,
}, testInfo) => {
  test.setTimeout(90_000);
  const f = await fixture(page);
  try {
    const row = page.locator('[data-account="default"]');
    await row
      .getByRole("button", { name: "Limit details for personal@example.com" })
      .click();
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
    await expect(row.locator(".mantine-TextInput-error")).toHaveText(
      "Use at most 32 characters.",
    );
    await expect(
      row.getByRole("button", { name: "Save", exact: true }),
    ).toBeDisabled();
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
      .getByRole("button", { name: "Limit details for work@example.com" })
      .click();
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
        await claude
          .getByRole("button", { name: "Cancel", exact: true })
          .click();
      }
    }
  } finally {
    try {
      if (testInfo.status !== testInfo.expectedStatus)
        await testInfo.attach("accounts", {
          body: await page.screenshot(),
          contentType: "image/png",
        });
    } finally {
      await f.close();
    }
  }
});

test("accounts Name Escape keeps the inline manager visible", async ({
  page,
}) => {
  test.setTimeout(90_000);
  const f = await fixture(page);
  try {
    const manager = page.locator(".accounts-manager-inline");
    await manager
      .getByRole("button", { name: "Limit details for personal@example.com" })
      .click();
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
