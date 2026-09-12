import assert from "node:assert/strict";
import { createRequire } from "node:module";
import { execFileSync } from "node:child_process";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { resolve, join } from "node:path";
const root = resolve(import.meta.dirname, "../web");
const require = createRequire(join(root, "package.json"));
const { chromium, webkit } = require("playwright-core");
const { createServer } = await import(require.resolve("vite"));
const negative = process.env.NEGATIVE === "1";
const browserType = process.env.BROWSER === "webkit" ? webkit : chromium;
const entry = join(root, "account-error-fixture.tsx");
const failure = {
  message: "Provider request failed",
  code: "providerFailure",
  data: { message: "A nested provider reason", retryAfter: 17 },
  additionalDetails: ["Retain this field"],
};
const cacheDir = await mkdtemp(join(tmpdir(), "studio-account-errors-"));
const server = await createServer({
  configFile: false,
  root,
  cacheDir,
  server: { host: "127.0.0.1", port: 0 },
  plugins: [
    {
      name: "account-error-fixture",
      configureServer(server) {
        server.middlewares.use("/check", (_req, res) => {
          res.setHeader("Content-Type", "text/html");
          res.end(
            '<html><body><div id="root"></div><script type="module" src="/account-error-fixture.tsx"></script></body></html>',
          );
        });
      },
      resolveId(id) {
        if (id === "/account-error-fixture.tsx") return entry;
      },
      load(id) {
        if (negative && id === join(root, "src/components/AccountSignIn.tsx"))
          return execFileSync(
            "git",
            ["show", "f72c2ce:web/src/components/AccountSignIn.tsx"],
            { cwd: root, encoding: "utf8" },
          );
        if (id !== entry) return;
        return `import React,{useState} from 'react';import {createRoot} from 'react-dom/client';import {MantineProvider} from '@mantine/core';import '@mantine/core/styles.css';
import AccountSignIn from '/src/components/AccountSignIn.tsx';import Accounts from '/src/components/Accounts.tsx';import Workspace from '/src/components/Workspace.tsx';
const initial=${JSON.stringify(failure)};const agent={id:'lead',rootId:'lead',isLead:true,source:'managed',status:'idle',model:'gpt-6',name:'Fixture lead',created:1,cwd:'/fixture'};
localStorage.setItem('account-sign-in:fixture',JSON.stringify('request'));
function Harness(){const [error,setError]=useState(initial);const [surface,setSurface]=useState('signin');window.changeError=setError;window.showSurface=setSurface;
const state={scope:'fixture',data:{accounts:[{id:'default',label:'Fixture account',status:'ready',error}],defaultAccountKey:'default',logins:[{requestId:'request',accountKey:'default',status:'error',error}]},setData:()=>{},refresh:async()=>{},error:''};
const data={stateDir:'fixture',threads:[agent],runtime:{agents:[agent],requests:[],complaints:[],userMessages:[],userTasks:[]},board:{},nodes:[agent],edges:[],chats:[]};
return <MantineProvider><p data-shell>Saved conversation remains available</p>{surface==='signin'?<AccountSignIn state={state} opened={true}/>:surface==='accounts'?<Accounts state={state} accountKey='default' changeAccount={async()=>{}} onError={()=>{}}/>:<Workspace key={surface} opened={true} initialSection={surface} agent={agent} data={data} allRequests={[]} onClose={()=>window.showSurface('signin')} onSelect={()=>{}} refresh={async()=>{}} notify={()=>{}}/>}</MantineProvider>}
createRoot(document.getElementById('root')).render(<Harness/>);`;
      },
    },
  ],
});
let browser;
try {
  await server.listen();
  browser = await browserType.launch({
    headless: true,
    ...(browserType === chromium
      ? {
          executablePath:
            process.env.CHROME_BIN ||
            "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
        }
      : {}),
  });
  const page = await browser.newPage({
    viewport: { width: 1100, height: 850 },
  });
  const errors = [],
    mutations = [];
  page.on("pageerror", (error) => errors.push(error.message));
  let limitsHttpFailure = false;
  const limitsFailureBody = {
    error: failure,
    requestId: "limits-request",
    retryable: true,
  };
  await page.route("**/api/**", (route) => {
    const pathname = new URL(route.request().url()).pathname;
    if (route.request().method() !== "GET") mutations.push(pathname);
    if (pathname === "/api/limits" && limitsHttpFailure)
      return route.fulfill({ status: 503, json: limitsFailureBody });
    return route.fulfill({
      json:
        pathname === "/api/rules"
          ? {
              rules: [
                {
                  id: "rule",
                  agent: "lead",
                  name: "Rule diagnostic",
                  kind: "low_workers",
                  error: failure,
                  text: "Saved rule text",
                },
              ],
            }
          : pathname === "/api/capabilities"
            ? { errors: [failure], managed: [], native: [] }
            : pathname === "/api/changes"
              ? { scope: "chat", git: false, error: failure, files: [] }
              : pathname === "/api/limits"
                ? { accountKey: "default", error: failure, data: null }
                : {},
    });
  });
  await page.goto(`http://127.0.0.1:${server.httpServer.address().port}/check`);
  if (negative) {
    for (let i = 0; i < 100 && !errors.length; i++)
      await page.waitForTimeout(25);
    assert.ok(
      errors.some((error) =>
        /Objects are not valid as a React child|Minified React error #31/.test(
          error,
        ),
      ),
      "Original sign-in receipt renderer crashes on the structured payload",
    );
    console.log(
      "PASS negative control: original AccountSignIn throws React object-child error",
    );
  } else {
    async function details(expected, scope = page) {
      const button = scope
        .getByRole("button", { name: "Error details", exact: true })
        .first();
      await button.click();
      assert.deepEqual(
        JSON.parse(
          await scope
            .getByRole("button", { name: "Hide error details", exact: true })
            .first()
            .locator("..")
            .locator(":scope > span")
            .last()
            .innerText(),
        ),
        expected,
      );
      assert.equal(await page.locator("[data-shell]").count(), 1);
    }
    await page.getByText(failure.message, { exact: true }).waitFor();
    await details(failure);
    const changed = {
      message: "Updated provider error",
      code: "new-code",
      metadata: { region: "fixture" },
    };
    await page.evaluate((value) => window.changeError(value), changed);
    await page.getByText(changed.message, { exact: true }).waitFor();
    assert.match(await page.locator("[role=alert]").innerText(), /new-code/);
    await page.evaluate(() => window.showSurface("accounts"));
    await page
      .getByRole("button", { name: "Account: Fixture account" })
      .click();
    await details(changed);
    await page.getByText("Manage accounts", { exact: false }).click();
    await page.getByRole("dialog").waitFor();
    await details(changed);
    const limits = page.getByLabel("Limits for Fixture account");
    await limits.getByText(failure.message, { exact: true }).waitFor();
    await details(failure, limits);
    await page.keyboard.press("Escape");
    limitsHttpFailure = true;
    await page
      .getByRole("button", { name: "Account: Fixture account" })
      .click();
    await page.getByText("Manage accounts", { exact: false }).click();
    await limits.getByText(failure.message, { exact: true }).waitFor();
    await limits
      .getByRole("button", { name: "Error details", exact: true })
      .click();
    const httpDiagnostic = JSON.parse(
      await limits
        .getByRole("button", { name: "Hide error details", exact: true })
        .locator("..")
        .locator(":scope > span")
        .last()
        .innerText(),
    );
    assert.equal(httpDiagnostic.status, 503);
    assert.equal(httpDiagnostic.message, failure.message);
    assert.deepEqual(httpDiagnostic.details, limitsFailureBody);
    assert.equal(await page.locator("[data-shell]").count(), 1);
    await page.keyboard.press("Escape");
    for (const section of ["rules", "changes", "tools"]) {
      await page.evaluate((value) => window.showSurface(value), section);
      await page.getByText(failure.message, { exact: true }).waitFor();
      await details(failure);
    }
    assert.deepEqual(errors, []);
    assert.deepEqual(
      mutations,
      [],
      "Rendering error details never submits a form or repeats an operation",
    );
    console.log(
      `PASS ${browserType === chromium ? "Chromium" : "WebKit"}: sign-in, account, limits, rules, changes and tool diagnostics retain full structured fields without losing the shell`,
    );
  }
} finally {
  await browser?.close();
  await server.close();
  await rm(cacheDir, { recursive: true, force: true });
}
