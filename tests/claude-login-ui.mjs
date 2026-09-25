import assert from "node:assert/strict";
import { createRequire } from "node:module";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { resolve, join } from "node:path";
const root = resolve(import.meta.dirname, "../web");
const require = createRequire(join(root, "package.json"));
const { chromium } = require("playwright-core");
const { createServer } = await import(require.resolve("vite"));
const cacheDir = await mkdtemp(join(tmpdir(), "claude-login-ui-"));
const entry = join(root, "claude-login-fixture.tsx");
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
            '<div id="root"></div><script type="module" src="/claude-login-fixture.tsx"></script>',
          );
        });
      },
      resolveId(id) {
        if (id === "/claude-login-fixture.tsx") return entry;
      },
      load(id) {
        if (id === entry)
          return `import React from 'react';import {createRoot} from 'react-dom/client';import {MantineProvider} from '@mantine/core';import '@mantine/core/styles.css';import Accounts from '/src/components/Accounts.tsx';import ClaudeSignInNotice from '/src/components/ClaudeSignInNotice.tsx';const account={id:'claude',label:'Personal',email:'me@example.com',provider:'claude',status:'changed',error:"This profile's account changed. Restore its original login or add a separate profile."};const state={scope:'fixture',data:{accounts:[account],defaultAccountKey:'claude'},setData:()=>{},refresh:async()=>{},error:''};createRoot(document.getElementById('root')).render(<MantineProvider><ClaudeSignInNotice account={account} errors={[{message:"An unrelated earlier error"}]} onSignIn={key=>window.signInAccount=key}/><Accounts state={state} accountKey='claude' changeAccount={async()=>{}} onError={()=>{}}/></MantineProvider>);`;
      },
    },
  ],
});
let browser;
try {
  await server.listen();
  browser = await chromium.launch({
    headless: true,
    executablePath:
      process.env.CHROME_BIN ||
      "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
  });
  const page = await browser.newPage();
  const failures = [];
  page.on("pageerror", (e) => failures.push(e.message));
  let receipt = null;
  let starts = [];
  let codes = [];
  let lose = true;
  await page.route("**/api/**", async (route) => {
    const req = route.request();
    const url = new URL(req.url());
    const body = req.method() === "POST" ? req.postDataJSON() : {};
    if (url.pathname === "/api/accounts/claude/login") {
      if (req.method() === "POST") {
        starts.push(body.request_id);
        receipt = {
          requestId: body.request_id,
          accountKey: "claude",
          status: "pending",
          verificationUrl: "https://claude.ai/oauth/authorize?fixture=1",
        };
        if (lose) {
          lose = false;
          return route.abort();
        }
      }
      return route.fulfill({ json: receipt || { error: "Unknown request" } });
    }
    if (url.pathname.endsWith("/code")) {
      codes.push(body.code);
      receipt = { ...receipt, status: "ready", email: "me@example.com" };
      return route.fulfill({ json: receipt });
    }
    if (url.pathname.endsWith("/cancel")) {
      receipt = { ...receipt, status: "cancelled" };
      return route.fulfill({ json: receipt });
    }
    return route.fulfill({
      json: {
        accounts: [
          {
            id: "claude",
            label: "Personal",
            email: "me@example.com",
            provider: "claude",
            status: "signed_out",
          },
        ],
        defaultAccountKey: "claude",
      },
    });
  });
  const open = async () => {
    await page.getByRole("button", { name: "me@example.com" }).click();
    await page.getByRole("menuitem", { name: /Manage accounts/ }).click();
    await page
      .getByRole("button", { name: "Sign in again", exact: true })
      .click();
  };
  await page.goto(`http://127.0.0.1:${server.httpServer.address().port}/check`);
  await page.getByRole("button", {name:"Sign in to Claude", exact:true}).click();
  assert.equal(await page.evaluate(() => window.signInAccount), "claude");
  await open();
  await page
    .getByRole("button", { name: "Start sign-in", exact: true })
    .click();
  await page.getByRole("link", { name: "Open Claude sign-in" }).waitFor();
  const first = starts[0];
  await page.reload();
  await open();
  await page.getByRole("link", { name: "Open Claude sign-in" }).waitFor();
  assert.equal(starts.length, 1);
  await page.getByLabel("Authorization code").fill("secret-code");
  await page.getByRole("button", { name: "Submit code" }).click();
  await page
    .getByText("Signed in as me@example.com. You can retry the message.")
    .waitFor();
  assert.deepEqual(codes, ["secret-code"]);
  assert.equal(
    await page.evaluate(() =>
      JSON.stringify(localStorage).includes("secret-code"),
    ),
    false,
  );
  await page.reload();
  await open();
  await page.getByRole("button", { name: "Start new sign-in" }).click();
  await page.getByRole("link", { name: "Open Claude sign-in" }).waitFor();
  assert.notEqual(starts.at(-1), first);
  receipt.verificationUrl = "https://claude.ai.evil.example/oauth/authorize";
  await page.getByText("Claude returned an unsupported sign-in URL.").waitFor();
  assert.equal(
    await page.getByRole("link", { name: "Open Claude sign-in" }).count(),
    0,
  );
  await page.getByRole("button", { name: "Cancel sign-in" }).click();
  await page.getByText("Sign-in cancelled.", { exact: true }).waitFor();
  assert.deepEqual(failures, []);
  console.log(
    "PASS Claude account trigger, lost reply, reload, code secrecy, ready restart, unsafe URL, cancel",
  );
} finally {
  await browser?.close();
  await server.close();
  await rm(cacheDir, { recursive: true, force: true });
}
