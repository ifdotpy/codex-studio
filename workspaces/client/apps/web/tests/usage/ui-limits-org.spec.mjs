// Hidden browser fixture for the production limits and organization components.
// Screenshots stay outside the checkout.
import assert from "node:assert/strict";
import { mkdtemp, mkdir, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { resolve, join } from "node:path";
import { fileURLToPath } from "node:url";
import { createServer } from "vite";
import { test, expect } from "../playwright.mjs";

test("Rust model decisions and safe Claude organizations at 320 and 1200 pixels", async ({
  browser,
}) => {
  test.setTimeout(120_000);
  const root = fileURLToPath(new URL("../../", import.meta.url));
  const output =
    process.env.UI_LIMITS_ORG_SCREENSHOTS ||
    join(tmpdir(), "studio-ui-limits-org");
  await mkdir(output, { recursive: true });
  const cache = await mkdtemp(join(tmpdir(), "studio-ui-limits-org-vite-"));
  const entry = join(root, "__ui-limits-org.jsx");
  const source = `
import React,{useState} from 'react';
import {createRoot} from 'react-dom/client';
import {MantineProvider,Button} from '@mantine/core';
import '@mantine/core/styles.css';
import Usage from '/src/components/Usage.tsx';
import {NativeError} from '/src/components/NativeNotice.tsx';
import Accounts from '/src/components/Accounts.tsx';
import ClaudeProfile from '/src/components/ClaudeProfile.tsx';
import AccountSignInNotice from '/src/components/AccountSignInNotice.tsx';
import '/src/components/accounts.css';
import '/src/style.css';
import '/src/appearance.css';
import {theme} from '/src/theme.ts';
const expected={name:'Research organization for the selected Claude profile',idSuffix:'12345678'};
const observed={name:'Another organization with a separate subscription',idSuffix:'87654321'};
const account={id:'claude',label:'Research',email:'research@example.test',provider:'claude',status:'changed',organization:expected,organizationMismatch:{expected,observed}};
const now=Date.now()/1000;
const agent={id:'worker',rootId:'worker',epoch:1,isLead:true,status:'failed',accountKey:'claude',provider:'claude',model:'sonnet',threadId:'thread',nativeLimitErrorAt:now-20,error:{codexErrorInfo:'rateLimitExceeded',message:'Usage limit reached'}};
function Fixture(){const[screen,setScreen]=useState('limits');const[at,setAt]=useState(now);const[limitsOpen,setLimitsOpen]=useState(false);
window.refreshFixture=()=>setAt(Date.now()/1000);const limits={accountKey:'claude',at,eligibility:{agentId:'another-worker',accountKey:'claude',provider:'claude',model:'opus',allowed:true,canRun:true,fresh:true,checkedAt:now,validUntil:now+3600,resetAt:null,buckets:[],overage:'unavailable'},data:{rateLimits:{limitId:'claude',primary:{usedPercent:100,resetsAt:now+3600}},rateLimitsByLimitId:{'claude-opus':{limitName:'Opus',secondary:{usedPercent:100,resetsAt:now+7200}},'claude-sonnet':{limitName:'Sonnet',secondary:{usedPercent:20,resetsAt:now+3600}}}}};
return <MantineProvider theme={theme} defaultColorScheme="auto"><main style={{padding:12,maxWidth:900,margin:'auto',minWidth:0}}>
<Button onClick={()=>setScreen(screen==='limits'?'accounts':'limits')}>{screen==='limits'?'Show accounts':'Show limits'}</Button>
{screen==='limits'?<><div data-notice><NativeError agent={agent} limits={limits}/></div><div style={{position:'fixed',bottom:12,right:12}} data-usage><Usage agent={agent} stateDir='fixture' limits={limits} accounts={[{key:'claude',provider:'claude',label:'Research',limits,reload:async()=>window.refreshFixture()}]} accountLabel='Research' opened={limitsOpen} onChange={setLimitsOpen} reload={async()=>window.refreshFixture()}/></div></>:<>
<AccountSignInNotice account={account} errors={[]} onSignIn={key=>window.signedInAccount=key}/>
<Accounts managerOnly state={{data:{accounts:[account],archivedAccounts:[],defaultAccountKey:'claude',defaultAccountGroups:[],logins:[]},setData:()=>{},refresh:async()=>{},error:'',scope:'fixture'}} onError={()=>{}}/>
<ClaudeProfile account={account} onSaved={()=>{}} expanded/></>}
</main></MantineProvider>}
createRoot(document.getElementById('root')).render(<Fixture/>);`;
  const server = await createServer({
    configFile: false,
    root,
    cacheDir: cache,
    server: { host: "127.0.0.1", port: 0 },
    plugins: [
      {
        name: "ui-limits-org-fixture",
        resolveId(id) {
          if (id === "/__ui-limits-org.jsx") return entry;
        },
        load(id) {
          if (id === entry) return source;
        },
        configureServer(vite) {
          vite.middlewares.use((req, res, next) => {
            if (req.url !== "/") return next();
            res.setHeader("Content-Type", "text/html");
            res.end(
              '<!doctype html><html style="height:auto;overflow:auto"><head><meta name="viewport" content="width=device-width,initial-scale=1"></head><body style="position:static;height:auto;overflow:auto"><div id="root" style="position:static;display:block;height:auto;overflow:visible"></div><script type="module" src="/__ui-limits-org.jsx"></script></body></html>',
            );
          });
        },
      },
    ],
  });
  try {
    await server.listen();
    for (const width of [320, 1200]) {
      const page = await browser.newPage({ viewport: { width, height: 1000 } });
      page.setDefaultTimeout(10_000);
      const errors = [];
      page.on("pageerror", (error) => errors.push(error.message));
      await page.emulateMedia({ colorScheme: "light" });
      const now = 1_800_000_000;
      await page.clock.install({ time: now * 1000 });
      let mode = "rejected";
      const requests = [];
      await page.route("**/api/**", async (route) => {
        if (route.request().url().includes("/api/limits")) {
          const query = new URL(route.request().url()).searchParams;
          requests.push({
            agent: query.get("agent"),
            account: query.get("account_key"),
          });
          await route.fulfill({
            json: {
              accountKey: "claude",
              at: now,
              data: {
                rateLimits: {
                  limitId: "claude",
                  primary: { usedPercent: 100, resetsAt: now + 3600 },
                },
              },
              eligibility: {
                agentId: "worker",
                accountKey: "claude",
                model: "sonnet",
                provider: "claude",
                allowed: mode !== "rejected",
                canRun: mode !== "rejected",
                fresh: true,
                checkedAt: now,
                validUntil: now + 60,
                resetAt: mode === "rejected" ? now + 3600 : null,
                buckets: [
                  { id: "claude", overageAllowed: mode !== "rejected" },
                  { id: "claude-sonnet", overageAllowed: mode !== "rejected" },
                ],
                overage: mode,
              },
            },
          });
        } else
          await route.fulfill({ json: { items: [], accounts: [], cost: 0 } });
      });
      await page.goto(server.resolvedUrls.local[0]);
      await page
        .locator("[data-notice]")
        .getByText("Overage rejected", { exact: true })
        .waitFor({ timeout: 10_000 });
      const eligibility = page.locator(
        "[data-notice] .model-limit-eligibility",
      );
      assert.match(
        await eligibility.innerText(),
        /Applicable buckets: claude, claude-sonnet/,
      );
      assert.doesNotMatch(await eligibility.innerText(), /claude-opus/);
      assert.match(await eligibility.innerText(), /Cannot run now/);
      await page.screenshot({
        path: resolve(output, `notice-rejected-${width}.png`),
        fullPage: true,
        animations: "disabled",
      });
      await page.emulateMedia({ colorScheme: "dark" });
      await expect(page.locator("html")).toHaveAttribute(
        "data-mantine-color-scheme",
        "dark",
      );
      await page.screenshot({
        path: resolve(output, `notice-rejected-dark-${width}.png`),
        fullPage: true,
        animations: "disabled",
      });
      await page.emulateMedia({ colorScheme: "light" });
      await expect(page.locator("html")).toHaveAttribute(
        "data-mantine-color-scheme",
        "light",
      );
      await page
        .getByRole("button", { name: "Account limits", exact: true })
        .click();
      await expect(
        page.locator(".account-limits-panel .model-limit-eligibility"),
      ).toContainText("Cannot run now");
      await expect(
        page.locator(".account-limits-panel .model-limit-eligibility"),
      ).toContainText("Overage rejected");
      assert.ok(requests.length >= 2);
      assert.ok(
        requests.every(
          ({ agent, account }) => agent === "worker" && account === "claude",
        ),
      );
      await expect(page.locator(".account-limits-popover")).toHaveCSS(
        "opacity",
        "1",
      );
      const panelBox = await page
        .locator(".account-limits-popover")
        .boundingBox();
      assert.ok(
        panelBox.x >= 0 &&
          panelBox.y >= 0 &&
          panelBox.x + panelBox.width <= width,
      );
      await page.screenshot({
        path: resolve(output, `limits-rejected-${width}.png`),
        fullPage: true,
        animations: "disabled",
      });
      for (const [value, label] of [
        ["allowed", "Overage allowed"],
        ["in_use", "Overage in use"],
      ]) {
        mode = value;
        await page.clock.fastForward(1000);
        await page.evaluate(() => window.refreshFixture());
        await page
          .getByText(label, { exact: true })
          .first()
          .waitFor({ timeout: 10_000 });
        assert.equal(
          await page.locator("[data-notice] .native-error").count(),
          0,
        );
        await page.screenshot({
          path: resolve(output, `limits-${value}-${width}.png`),
          fullPage: true,
          animations: "disabled",
        });
      }
      await page.clock.fastForward(61_000);
      await expect(
        page.locator(".account-limits-panel .model-limit-eligibility"),
      ).toContainText("Current eligibility is unknown");
      await page
        .getByRole("button", { name: "Show accounts", exact: true })
        .click();
      await page
        .getByText(/Signed in to a different organization:/)
        .waitFor({ timeout: 10_000 });
      await page
        .getByRole("button", { name: "Sign in to Claude", exact: true })
        .first()
        .click();
      assert.equal(await page.evaluate(() => window.signedInAccount), "claude");
      await page
        .getByRole("button", {
          name: "Limit details for research@example.test",
          exact: true,
        })
        .click();
      await expect(
        page.locator('[data-account="claude"] .claude-organization').first(),
      ).toBeVisible();
      assert.ok((await page.locator(".claude-organization").count()) >= 2);
      assert.equal(
        await page.evaluate(
          () => document.documentElement.scrollWidth > innerWidth,
        ),
        false,
      );
      await page.screenshot({
        path: resolve(output, `accounts-${width}.png`),
        fullPage: true,
        animations: "disabled",
      });
      assert.deepEqual(errors, []);
      await page.close();
    }
    console.log(
      `PASS: limits, overage and organization at 320/1200; screenshots: ${output}`,
    );
  } finally {
    await server.close();
    await rm(cache, { recursive: true, force: true });
  }
});
