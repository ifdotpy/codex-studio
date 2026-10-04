import assert from "node:assert/strict";
import { fileURLToPath } from "node:url";
import { createRequire } from "node:module";
import { execFileSync } from "node:child_process";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test, browserExecutablePath } from "../playwright.mjs";
import { isResourceChangeEvent } from "../../../web/src/generated/stream-validators.js";

test("Account settings errors", async () => {
  test.setTimeout(45_000);
  const root = fileURLToPath(new URL("../../../web/", import.meta.url));
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
  const workspaceId = "0123456789abcdef0123456789abcdef";
  const resourceStreams = new Set();
  let resourceRevision = 0;
  const writeResourceEvent = (stream, reason, resources = stream.resources) => {
    if (reason !== "initial") resourceRevision++;
    const event = {
      protocol: 3,
      workspaceId,
      epoch: "account-errors-fixture",
      revision: resourceRevision,
      reason,
      resources,
    };
    assert.ok(isResourceChangeEvent(event));
    stream.response.write(
      `event: resources\ndata: ${JSON.stringify(event)}\n\n`,
    );
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
          server.middlewares.use("/api/sync/identity", (_req, res) => {
            res.setHeader("Content-Type", "application/json");
            res.end(JSON.stringify({ workspaceId }));
          });
          server.middlewares.use("/api/sync/stream", (req, res) => {
            const stream = {
              response: res,
              resources: JSON.parse(
                new URL(req.url || "/", "http://localhost").searchParams.get(
                  "resources",
                ) || "[]",
              ),
            };
            res.writeHead(200, {
              "Content-Type": "text/event-stream",
              "Cache-Control": "no-cache",
              Connection: "keep-alive",
            });
            resourceStreams.add(stream);
            writeResourceEvent(stream, "initial");
            res.on("close", () => resourceStreams.delete(stream));
          });
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
  import AccountSignIn from '/src/components/AccountSignIn.tsx';import Accounts from '/src/components/Accounts.tsx';import Workspace from '/src/components/shell/Workspace.tsx';
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
            executablePath: browserExecutablePath,
          }
        : {}),
    });
    const page = await browser.newPage({
      viewport: { width: 1100, height: 850 },
    });
    page.setDefaultTimeout(5000);
    const errors = [],
      mutations = [];
    page.on("pageerror", (error) => errors.push(error.message));
    let limitsHttpFailure = false;
    let limitsRecovered = false;
    let limitsReads = 0;
    const limitsFailureBody = {
      error: failure,
      requestId: "limits-request",
      retryable: true,
    };
    await page.route("**/api/**", (route) => {
      const pathname = new URL(route.request().url()).pathname;
      if (pathname === "/api/sync/identity" || pathname === "/api/sync/stream")
        return route.continue();
      if (pathname === "/api/limits") limitsReads++;
      if (route.request().method() !== "GET") mutations.push(pathname);
      if (pathname === "/api/limits" && limitsHttpFailure)
        return route.fulfill({ status: 503, json: limitsFailureBody });
      if (pathname === "/api/limits" && limitsRecovered)
        return route.fulfill({
          json: {
            accountKey: "default",
            at: Date.now() / 1000,
            data: {
              rateLimits: {
                limitId: "codex",
                planType: "pro",
                primary: { usedPercent: 20, windowDurationMins: 300 },
                secondary: {
                  usedPercent: 20,
                  windowDurationMins: 10080,
                },
              },
            },
          },
        });
      return route.fulfill({
        json:
          pathname === "/api/accounts"
            ? {
                accounts: [],
                archivedAccounts: [],
                defaultAccountKey: "default",
                logins: [],
              }
            : pathname === "/api/rules"
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
                    ? {
                        accountKey: "default",
                        error: failure.message,
                        data: null,
                      }
                    : {},
      });
    });
    await page.goto(
      `http://127.0.0.1:${server.httpServer.address().port}/check`,
    );
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
              .textContent(),
          ),
          expected,
        );
        assert.equal(await page.locator("[data-shell]").count(), 1);
      }
      await page
        .getByText(failure.message, { exact: true })
        .waitFor({ state: "attached" });
      await details(failure);
      const changed = {
        message: "Updated provider error",
        code: "new-code",
        metadata: { region: "fixture" },
      };
      await page.evaluate((value) => window.changeError(value), changed);
      await page
        .getByText(changed.message, { exact: true })
        .waitFor({ state: "attached" });
      assert.match(
        await page.locator("[role=alert]").textContent(),
        /new-code/,
      );
      await page.evaluate(() => window.showSurface("accounts"));
      await page
        .getByRole("button", { name: "Account: Fixture account" })
        .click();
      await details(changed);
      await page
        .getByRole("menuitem", { name: /Manage accounts/ })
        .click({ force: true });
      await page.getByRole("dialog").waitFor();
      await details(changed);
      const limits = page.getByLabel("Limits for Fixture account");
      await limits
        .getByText(failure.message, { exact: true })
        .waitFor({ state: "attached" });
      await page.keyboard.press("Escape");
      limitsHttpFailure = true;
      await page
        .getByRole("button", { name: "Account: Fixture account" })
        .click();
      await page
        .getByRole("menuitem", { name: /Manage accounts/ })
        .click({ force: true });
      await limits
        .getByText("Provider request failed")
        .waitFor({ state: "attached" });
      assert.equal(await page.locator("[data-shell]").count(), 1);
      limitsHttpFailure = false;
      limitsRecovered = true;
      const readsBeforeChange = limitsReads;
      const recoveredRead = page.waitForResponse(
        (response) =>
          new URL(response.url()).pathname === "/api/limits" &&
          response.status() === 200,
        { timeout: 5000 },
      );
      assert.ok(
        [...resourceStreams].some((stream) =>
          stream.resources.some(
            (resource) =>
              resource.kind === "limits" && resource.accountKey === "default",
          ),
        ),
        "the active limits resource is subscribed",
      );
      for (const stream of resourceStreams) {
        writeResourceEvent(
          stream,
          "change",
          stream.resources.filter(
            (resource) =>
              resource.kind === "limits" && resource.accountKey === "default",
          ),
        );
      }
      await recoveredRead;
      assert.equal(limitsReads, readsBeforeChange + 1);
      assert.equal(
        await limits.getByText(/80%\s*left/).count(),
        2,
        "typed notification displays recovered primary and weekly limits",
      );
      assert.equal(
        await limits.getByText(failure.message, { exact: true }).count(),
        0,
        "a successful typed resource reread clears the previous limit error",
      );
      const readsBeforeReconnect = limitsReads;
      const reconnectRead = page.waitForResponse(
        (response) =>
          new URL(response.url()).pathname === "/api/limits" &&
          response.status() === 200,
        { timeout: 5000 },
      );
      for (const stream of resourceStreams)
        writeResourceEvent(stream, "reconnect");
      await reconnectRead;
      assert.equal(limitsReads, readsBeforeReconnect + 1);
      assert.equal(await limits.getByText(/80%\s*left/).count(), 2);
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
});
