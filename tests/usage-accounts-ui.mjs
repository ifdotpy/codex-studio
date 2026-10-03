#!/usr/bin/env node
import assert from "node:assert/strict";
import { createRequire } from "node:module";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const repo = dirname(dirname(fileURLToPath(import.meta.url)));
const require = createRequire(join(repo, "web/package.json"));
const { chromium } = require("playwright-core");
const { createServer } = await import(require.resolve("vite"));
const cache = await mkdtemp(join(tmpdir(), "usage-accounts-vite-"));
const entry = join(repo, "web/__usage-accounts-fixture.jsx");
const now = Math.floor(Date.now() / 1000);
let resetPayload;
const source = `
import React,{useState} from 'react';
import {createRoot} from 'react-dom/client';
import {MantineProvider} from '@mantine/core';
import '@mantine/core/styles.css';
import Usage from '/src/components/Usage.tsx';
const own={id:'chat',rootId:'chat',accountKey:'own',source:'managed',model:'gpt-6-astra',status:'completed',isLead:true,created:1,contextUsage:{tokens:10,window:100}};
const ownLimits={accountKey:'own',at:${now},data:{accountId:'own-account',rateLimits:{limitId:'codex',primary:{usedPercent:20,resetsAt:${now + 3600}}}}};
const teamLimits={accountKey:'team',at:${now},data:{accountId:'team-account',rateLimits:{limitId:'codex',primary:{usedPercent:92,resetsAt:${now + 3600}}},rateLimitResetCredits:{availableCount:1,credits:[{id:'team-credit',title:'Team reset',status:'available',resetType:'codexRateLimits'}]}}};
window.reloadCalls=[];
function Fixture(){
 const [multi,setMulti]=useState(location.search.includes('multi'));
 const [teamReady,setTeamReady]=useState(false);
 const accounts=[{key:'own',label:'Own account',email:'own@example.test',accountId:'own-account',limits:ownLimits,loading:false,reload:(force=false)=>{window.reloadCalls.push(['own',force]);}},...(multi?[{key:'team',label:'Team account',email:'team@example.test',provider:'openai',accountId:'team-account',limits:teamReady?teamLimits:null,loading:false,reload:(force=false)=>{window.reloadCalls.push(['team',force]);if(!teamReady)setTeamReady(true);}}]:[])];
 return <MantineProvider><Usage agent={own} stateDir='/fixture' limits={ownLimits} accountLabel='own@example.test' reload={()=>window.reloadCalls.push(['fallback',true])} opened={true} onChange={()=>{}} limitsLoading={false} accounts={accounts}/></MantineProvider>;
}
createRoot(document.getElementById('root')).render(<Fixture/>);`;
const server = await createServer({
  configFile: false,
  cacheDir: cache,
  root: join(repo, "web"),
  server: { host: "127.0.0.1", port: 0 },
  plugins: [
    {
      name: "usage-accounts-fixture",
      resolveId(id) {
        if (id === "/__usage-accounts-fixture.jsx") return entry;
      },
      load(id) {
        if (id === entry) return source;
      },
      configureServer(vite) {
        vite.middlewares.use((req, res, next) => {
          if (req.url?.startsWith("/api/")) {
            res.setHeader("Content-Type", "application/json");
            if (req.url.startsWith("/api/session-cost")) {
              res.end(
                JSON.stringify({
                  pricingState: "ready",
                  totalUSD: 0,
                  rootId: "chat",
                  claudeHistoryIncomplete: true,
                }),
              );
              return;
            }
            if (req.url.startsWith("/api/costs")) {
              const key =
                new URL(req.url, "http://localhost").searchParams.get(
                  "account_key",
                ) || "default";
              res.end(
                JSON.stringify({
                  accountKey: key,
                  data: { todayUSD: 0, last30DaysUSD: 0 },
                }),
              );
              return;
            }
            if (req.url.startsWith("/api/limits/reset")) {
              let body = "";
              req.on("data", (part) => (body += part));
              req.on("end", () => {
                resetPayload = JSON.parse(body);
                res.end(JSON.stringify({ outcome: "reset" }));
              });
              return;
            }
          }
          if (req.url?.split("?")[0] !== "/") return next();
          res.setHeader("Content-Type", "text/html");
          res.end(
            '<div id="root"></div><script type="module" src="/__usage-accounts-fixture.jsx"></script>',
          );
        });
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
  const page = await browser.newPage({
    viewport: { width: 1100, height: 900 },
  });
  const errors = [];
  page.on("pageerror", (error) => errors.push(error.message));
  await page.goto(server.resolvedUrls.local[0]);
  // Notes stay in the tooltip; the footer shows only the estimate and a marker.
  await page
    .locator(".session-cost-summary")
    .filter({ hasText: "*" })
    .waitFor();
  assert.match(
    await page.locator(".session-cost-summary").getAttribute("title"),
    /Earlier Claude totals may be incomplete/,
  );
  assert.doesNotMatch(
    await page.locator(".session-cost-summary").innerText(),
    /incomplete|Unpriced/,
  );
  const _toggle = page.getByRole("button", {
    name: "Account limits",
    exact: true,
  });
  await page.getByRole("region", { name: "Account limits details" }).waitFor();
  assert.equal(
    await page.getByRole("tab").count(),
    0,
    "one account has no tabs",
  );
  assert.match(
    await page.locator(".account-limits-panel").innerText(),
    /80%\s*left/,
  );
  await page.getByRole("button", { name: "Refresh", exact: true }).click();
  await page.waitForFunction(() =>
    window.reloadCalls.some(([key, force]) => key === "own" && force),
  );

  await page.goto(server.resolvedUrls.local[0] + "?multi");
  const ownTab = page.getByRole("tab", {
    name: "own@example.test account limits",
  });
  const teamTab = page.getByRole("tab", {
    name: "team@example.test, openai account limits",
  });
  await ownTab.waitFor();
  assert.equal(
    await ownTab.getAttribute("aria-selected"),
    "true",
    "chat account is selected first",
  );
  assert.match(
    await page.locator('[role="tabpanel"]').innerText(),
    /80%\s*left/,
  );
  await ownTab.press("ArrowRight");
  assert.equal(
    await teamTab.getAttribute("aria-selected"),
    "true",
    "ArrowRight selects the next account tab",
  );
  await page.waitForFunction(() =>
    window.reloadCalls.some(([key, force]) => key === "team" && !force),
  );
  await page.getByText("8% left", { exact: false }).waitFor();
  assert.match(
    await page.getByRole("region", { name: "Limit reset credits" }).innerText(),
    /Team reset/,
  );
  await page
    .getByRole("button", { name: "Account limits", exact: true })
    .waitFor();
  assert.equal(
    await page
      .getByRole("button", { name: "Account limits" })
      .getAttribute("title"),
    "Low allowance: team@example.test",
  );
  await page.getByRole("button", { name: "Refresh", exact: true }).click();
  await page.waitForFunction(() =>
    window.reloadCalls.some(([key, force]) => key === "team" && force),
  );
  await page.getByRole("button", { name: "Apply reset", exact: true }).click();
  await page
    .getByRole("button", { name: "Use one reset credit", exact: true })
    .click();
  await page.getByText("Reset applied.", { exact: true }).waitFor();
  const reset = resetPayload;
  assert.ok(reset, "reset request reached the fixture server");
  assert.equal(reset.account_id, "team-account");
  assert.equal(reset.credit_id, "team-credit");
  assert.deepEqual(errors, []);
  console.log(
    "Usage account UI: single account, tabs, lazy load, refresh, low allowance, and account-bound reset passed.",
  );
} finally {
  await browser?.close();
  await server.close();
  await rm(cache, { recursive: true, force: true });
}
