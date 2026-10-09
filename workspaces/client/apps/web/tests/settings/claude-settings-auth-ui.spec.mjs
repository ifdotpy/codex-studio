import assert from "node:assert/strict";
import { createRequire } from "node:module";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { resolve, join } from "node:path";

import { test } from "../playwright.mjs";

const browserContextsByTest = new WeakMap();
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

test("claude settings auth ui", async ({ browser: _browser }) => {
  test.setTimeout(120_000);
  const root = resolve(import.meta.dirname, "../../../web");
  const require = createRequire(join(root, "package.json"));
  const { createServer } = await import(require.resolve("vite"));
  const cacheDir = await mkdtemp(join(tmpdir(), "claude-settings-auth-"));
  const entry = join(root, "claude-settings-auth-fixture.tsx");
  const server = await createServer({
    configFile: false,
    root,
    cacheDir,
    server: { host: "127.0.0.1", port: 0 },
    plugins: [
      {
        name: "fixture",
        configureServer(server) {
          server.middlewares.use("/check", (_req, res) => {
            res.setHeader("Content-Type", "text/html");
            res.end(
              '<div id="root"></div><script type="module" src="/claude-settings-auth-fixture.tsx"></script>',
            );
          });
        },
        resolveId(id) {
          if (id === "/claude-settings-auth-fixture.tsx") return entry;
        },
        load(id) {
          if (id !== entry) return;
          return `import React,{useState} from 'react';
          import {createRoot} from 'react-dom/client';
          import {MantineProvider} from '@mantine/core';
          import '@mantine/core/styles.css';
          import {ClaudeSettings} from '/src/components/ClaudeSettings.tsx';
          import ClaudeSignIn from '/src/components/ClaudeSignIn.tsx';
          const agent={id:'chat',accountKey:'claude-work',provider:'claude',status:'idle',model:'sonnet'};
          function Fixture(){
            const [account,setAccount]=useState({id:'claude-work',label:'Work',email:'work@example.com',provider:'claude',status:'signedOut'});
            const [login,setLogin]=useState(false);
            return <MantineProvider>
              <ClaudeSettings agent={agent} account={account} onSignIn={key=>{window.signInAccount=key;setLogin(true)}}/>
              {login&&<ClaudeSignIn account={account} scope='settings-fixture' onClose={()=>setLogin(false)} onReady={()=>{setAccount({...account,status:'ready'});setLogin(false)}}/>}
            </MantineProvider>;
          }
          createRoot(document.getElementById('root')).render(<Fixture/>);`;
        },
      },
    ],
  });
  let browser;
  try {
    await server.listen();
    browser = _browser;
    const page = await browser.newPage({
      viewport: { width: 390, height: 844 },
    });
    const errors = [];
    page.on("pageerror", (error) => errors.push(error.message));
    let authenticated = false;
    let receipt;
    let settingsReads = 0;
    let modelReads = 0;
    const loginStarts = [];
    const mutations = [];
    await page.route("**/api/**", async (route) => {
      const req = route.request();
      const url = new URL(req.url());
      const body = req.method() === "POST" ? req.postDataJSON() : {};
      if (url.pathname === "/api/claude/session") {
        assert.equal(body.id, "chat");
        if (body.action !== "state") mutations.push(body);
        else settingsReads++;
        if (!authenticated)
          return route.fulfill({
            status: 400,
            json: { error: "Sign in with claude auth login first" },
          });
        return route.fulfill({
          json: {
            settings: { permissionMode: "acceptEdits", thinking: true },
            turns: [],
          },
        });
      }
      if (url.pathname === "/api/models") {
        modelReads++;
        assert.equal(authenticated, true);
        assert.equal(url.searchParams.get("account_key"), "claude-work");
        return route.fulfill({ json: { data: [{ model: "sonnet" }] } });
      }
      if (url.pathname === "/api/accounts/claude/login") {
        if (req.method() === "POST") {
          loginStarts.push(body);
          receipt = {
            requestId: body.login_id,
            accountKey: body.account_key,
            status: "pending",
            verificationUrl: "https://claude.ai/oauth/authorize?fixture=1",
          };
        }
        return route.fulfill({ json: receipt || { error: "Unknown request" } });
      }
      if (url.pathname === "/api/accounts/claude/login/code") {
        authenticated = true;
        receipt = { ...receipt, status: "ready", email: "work@example.com" };
        return route.fulfill({ json: receipt });
      }
      return route.fulfill({ json: {} });
    });
    await page.goto(
      `http://127.0.0.1:${server.httpServer.address().port}/check`,
    );
    await page
      .getByRole("alert")
      .filter({ hasText: "Sign in to Claude to load these settings." })
      .waitFor();
    assert.equal(await page.getByLabel("Permission mode").isDisabled(), true);
    assert.equal(await page.getByLabel("Extended thinking").isDisabled(), true);
    assert.equal(modelReads, 0);
    await page.getByText("Claude needs sign-in. work@example.com").waitFor();
    await page
      .getByRole("button", { name: "Sign in to Claude", exact: true })
      .click();
    assert.equal(
      await page.evaluate(() => window.signInAccount),
      "claude-work",
    );
    await page
      .getByRole("button", { name: "Start sign-in", exact: true })
      .click();
    await page.getByRole("link", { name: "Open Claude sign-in" }).waitFor();
    assert.equal(loginStarts.length, 1);
    assert.equal(loginStarts[0].account_key, "claude-work");
    await page.getByLabel("Authorization code").fill("fixture-code");
    await page.getByRole("button", { name: "Submit code" }).click();
    await page.waitForFunction(
      () =>
        !document.querySelector('select[aria-label="Permission mode"]')
          ?.disabled &&
        [...document.querySelectorAll("select")].some(
          (select) => select.value === "acceptEdits" && !select.disabled,
        ),
    );
    assert.equal(
      await page.getByLabel("Permission mode").inputValue(),
      "acceptEdits",
    );
    await page.waitForFunction(() =>
      [...document.querySelectorAll('input[type="checkbox"]')].some(
        (input) => !input.disabled,
      ),
    );
    assert.equal(
      await page.getByLabel("Extended thinking").isDisabled(),
      false,
    );
    assert.equal(
      await page
        .getByRole("button", { name: "Sign in to Claude", exact: true })
        .count(),
      0,
    );
    assert.equal(settingsReads, 2);
    assert.equal(modelReads, 1);
    assert.deepEqual(mutations, []);
    assert.deepEqual(errors, []);

    // A chat without a native thread can return stored settings after sign-out.
    const emptyChat = await browser.newPage();
    const emptyChatRequests = [];
    await emptyChat.route("**/api/**", async (route) => {
      const req = route.request();
      const url = new URL(req.url());
      if (url.pathname === "/api/claude/session") {
        emptyChatRequests.push(req.postDataJSON());
        return route.fulfill({
          json: {
            settings: { permissionMode: "acceptEdits", thinking: true },
            turns: [],
          },
        });
      }
      assert.notEqual(url.pathname, "/api/models");
      return route.fulfill({ json: {} });
    });
    await emptyChat.goto(
      `http://127.0.0.1:${server.httpServer.address().port}/check`,
    );
    await emptyChat.waitForFunction(
      () =>
        document
          .querySelector('[aria-label="Claude settings"]')
          ?.getAttribute("aria-busy") === "false",
    );
    await emptyChat
      .getByText("Claude needs sign-in. work@example.com")
      .waitFor();
    assert.equal(
      await emptyChat.getByLabel("Permission mode").inputValue(),
      "acceptEdits",
    );
    assert.equal(
      await emptyChat.getByLabel("Permission mode").isDisabled(),
      true,
    );
    assert.equal(
      await emptyChat.getByLabel("Extended thinking").isDisabled(),
      true,
    );
    await emptyChat.getByText("Advanced", { exact: true }).click();
    assert.equal(
      await emptyChat.getByLabel("Auto-compact token limit").isDisabled(),
      true,
    );
    assert.deepEqual(
      emptyChatRequests.map((request) => request.action),
      ["state"],
    );
    await emptyChat.close();
    console.log(
      "PASS Claude settings sign-out, pinned account login, disabled writes, model load and settings reload after sign-in",
    );
  } finally {
    await server.close();
    await rm(cacheDir, { recursive: true, force: true });
  }
});
