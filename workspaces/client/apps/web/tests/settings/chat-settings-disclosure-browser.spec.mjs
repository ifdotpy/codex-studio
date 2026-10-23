import { mkdtemp, mkdir, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { execFileSync } from "node:child_process";
import {
  API_SCHEMA_HASH_HEADER,
  readApiSchemaHash,
  apiSchemaHandshakeSse,
  test,
  expect,
  createFixtureViteServer,
} from "../playwright.mjs";

test("Chat settings uses native agent disclosures, distinct accounts and one phone scroll", async ({
  page,
}) => {
  test.setTimeout(120000);
  const root = join(import.meta.dirname, "../..");
  const cacheDir = await mkdtemp(join(tmpdir(), "execution-disclosures-"));
  const entry = join(root, "execution-disclosures-fixture.tsx");
  const server = await createFixtureViteServer({
    root,
    configFile: false,
    cacheDir,
    server: { host: "127.0.0.1", port: 0, hmr: false },
    plugins: [
      {
        name: "execution-disclosures",
        configureServer(server) {
          server.middlewares.use("/check", (_req, res) => {
            res.setHeader("Content-Type", "text/html");
            res.end(
              '<!doctype html><meta name="viewport" content="width=device-width,initial-scale=1"><div id="root"></div><script type="module" src="/execution-disclosures-fixture.tsx"></script>',
            );
          });
        },
        resolveId(id) {
          if (id === "/execution-disclosures-fixture.tsx") return entry;
        },
        load(id) {
          if (process.env.EXECUTION_BASELINE === "1") {
            const relative = id.startsWith(root + "/")
              ? id.slice(root.length + 1)
              : "";
            if (
              [
                "src/components/agents/ChatSettingsDialog.tsx",
                "src/components/agents/ExecutionSettings.tsx",
                "src/components/agents/AccountExclusionsControl.tsx",
                "src/components/agents/SubagentConcurrencyControl.tsx",
                "src/components/BrowserAccessNotice.tsx",
              ].includes(relative)
            )
              return execFileSync(
                "git",
                ["show", `28d51987:workspaces/client/apps/web/${relative}`],
                { encoding: "utf8" },
              );
          }
          if (id !== entry) return;
          return `import React,{useState} from 'react';import{createRoot}from'react-dom/client';import{flushSync}from'react-dom';import{MantineProvider}from'@mantine/core';import'@mantine/core/styles.css';import'/src/style.css';import'/src/appearance.css';import'/src/design/state.css';import{theme}from'/src/theme.ts';import{ChatSettingsDialog}from'/src/components/agents/ChatSettingsDialog.tsx';
const names=['gpt-6-astra','gpt-6.1-sol','gpt-6-luna','gpt-5.6-sol','gpt-5.6-luna','gpt-5.6-terra','gpt-6-sol'];const models=names.map((model,index)=>({model,provider:'codex',isDefault:!index,description:'Coding and reasoning for '+model,supportedReasoningEfforts:['low','medium','high','xhigh','max','ultra'].map(reasoningEffort=>({reasoningEffort})),serviceTiers:[{id:'priority'}],availableAccessPrograms:{cyber:['standard','daybreakBlue']}}));const accounts=Array.from({length:5},(_,i)=>({id:i?'account-'+i:'default',label:'Work '+(i+1),email:'operator'+(i+1)+'@example.com',status:'ready',provider:'codex'}));const accountsData={accounts,archivedAccounts:[],defaultAccountKey:'default',defaultAccountGroups:[],logins:[],supportsDelete:true,supportsDisconnect:true};const initial={id:'lead',accountKey:'default',provider:'codex',name:'Lead',source:'managed',model:names[0],effort:'high',isLead:true,status:'idle',created:1,yoloMode:false,subagentConcurrencyVersion:2,concurrency:8,agentModeRevision:1,workerDefaults:{model:'gpt-6-luna',effort:'high',delegationDepth:1},reviewDefaults:{model:null,effort:null}};
window.calls=[];const nativeFetch=window.fetch;window.fetch=async(input,options)=>{const url=input instanceof Request?input.url:input;options=input instanceof Request?{method:input.method,body:input.method==='GET'?undefined:await input.clone().text()}:options;const u=new URL(url,location.origin);let result;if(u.pathname==='/api/limits'){const key=u.searchParams.get('account_key');result={accountKey:key,at:Date.now()/1000,data:{rateLimits:{primary:{usedPercent:20,windowDurationMins:300,resetsAt:Date.now()/1000+3600}}}}}else if(u.pathname==='/api/models')result={data:models};else if(u.pathname==='/api/desktop')result={browser:{enabled:false,reason:'Browser access is disabled for this account.'}};else if(u.pathname==='/api/accounts')result=accountsData;else if(u.pathname==='/api/conversation'){const body=JSON.parse(options.body);window.calls.push(body);let next={...window.agent};if(body.worker_defaults)next.workerDefaults={...next.workerDefaults,model:body.worker_defaults.model,effort:body.worker_defaults.effort,delegationDepth:body.worker_defaults.delegation_depth,accountKey:body.worker_defaults.account_key,fastMode:body.worker_defaults.fast_mode};else if(body.review_defaults)next.reviewDefaults=body.review_defaults;else if(body.excluded_account_keys)next.excludedAccountKeys=body.excluded_account_keys;else if('subagent_concurrency' in body){next.concurrency=body.subagent_concurrency;next.agentModeRevision++}else if('yolo_mode' in body)next.yoloMode=body.yolo_mode;else Object.assign(next,{model:body.model,effort:body.effort,fastMode:body.fast_mode,daybreakEnabled:body.daybreak_enabled});window.updateAgent(next);result=next;}if(!result)return nativeFetch(input,options);return new Response(JSON.stringify(result),{headers:{'Content-Type':'application/json'}})};
function Fixture(){const[agent,setAgent]=useState(initial),[opened,setOpened]=useState(true),[accountOpen,setAccountOpen]=useState(false),[mainOpen,setMainOpen]=useState(false),[workerOpen,setWorkerOpen]=useState(false),[reviewOpen,setReviewOpen]=useState(false);window.agent=agent;window.updateAgent=value=>flushSync(()=>setAgent(value));const state={data:accountsData,setData:()=>{},error:'',refresh:async()=>accountsData,scope:'fixture'};return <MantineProvider theme={theme} forceColorScheme={new URLSearchParams(location.search).get('theme')||'light'}><ChatSettingsDialog settingsOpen={opened} setSettingsOpen={setOpened} accountModalOpen={accountOpen} accountModalOpenRef={{current:accountOpen}} mainSettingsOpen={mainOpen} workerSettingsOpen={workerOpen} reviewSettingsOpen={reviewOpen} setMainSettingsOpen={setMainOpen} setWorkerSettingsOpen={setWorkerOpen} setReviewSettingsOpen={setReviewOpen} setAccountModalOpen={setAccountOpen} updateAccountModalOpen={setAccountOpen} agent={agent} lead={agent} accounts={state} agentModels={{models,loading:false,error:'',retry:()=>{}}} data={{stateDir:'execution-preview',runtime:{agents:[]}}} workspaceId="1234567890abcdef1234567890abcdef" accountKey="default" refresh={async()=>{}} notify={()=>{}} creating={false} newChat={()=>{}} run={fn=>fn()} submitNativeAction={async()=>{}} project={()=>{}} selectedAccount={accounts[0]} setClaudeLoginKey={()=>{}} /></MantineProvider>};createRoot(document.getElementById('root')).render(<Fixture/>);`;
        },
      },
    ],
  });
  await server.listen();
  try {
    await page.route("**/api/sync/identity", (route) =>
      route.fulfill({
        headers: { [API_SCHEMA_HASH_HEADER]: readApiSchemaHash() },
        json: {
          workspaceId: "1234567890abcdef1234567890abcdef",
          syncProtocol: 2,
        },
      }),
    );
    await page.route("**/api/sync/stream?**", (route) => {
      const resources = JSON.parse(
        new URL(route.request().url()).searchParams.get("resources") || "[]",
      );
      return route.fulfill({
        contentType: "text/event-stream",
        body: apiSchemaHandshakeSse(
          `event: resources\ndata: ${JSON.stringify({ protocol: 3, workspaceId: "1234567890abcdef1234567890abcdef", epoch: "fixture", revision: 1, reason: "initial", resources })}\n\n`,
        ),
      });
    });
    const url = server.resolvedUrls.local[0] + "check";
    await page.setViewportSize({ width: 390, height: 844 });
    await page.goto(url);
    const modal = page.getByRole("dialog", {
      name: "Chat settings",
      exact: true,
    });
    await expect(modal).toBeVisible();
    if (process.env.EXECUTION_SCREENSHOTS) {
      await mkdir(process.env.EXECUTION_SCREENSHOTS, { recursive: true });
      for (const width of [320, 390, 768, 1440])
        for (const theme of ["light", "dark"]) {
          await page.setViewportSize({
            width,
            height: width === 320 ? 568 : 844,
          });
          await page.goto(url + "?theme=" + theme);
          await expect(modal).toBeVisible();
          if (process.env.EXECUTION_BASELINE === "1") {
            await modal.evaluate((node) => {
              for (let el = node; el; el = el.parentElement) el.scrollTop = 0;
            });
            await page.screenshot({
              path: join(
                process.env.EXECUTION_SCREENSHOTS,
                `before-chat-settings-${width}-${theme}.png`,
              ),
              fullPage: true,
            });
          } else {
            for (const role of ["orchestrator", "worker", "review"]) {
              await modal
                .locator(`details[data-role="${role}"] > summary`)
                .click();
              await modal.evaluate((node) => {
                for (let el = node; el; el = el.parentElement) el.scrollTop = 0;
              });
              await page.screenshot({
                path: join(
                  process.env.EXECUTION_SCREENSHOTS,
                  `after-chat-settings-${role}-${width}-${theme}.png`,
                ),
                fullPage: true,
              });
            }
            await modal
              .locator('details[data-role="review"] > summary')
              .click();
            await modal.locator(".execution-accounts > summary").click();
            await modal.locator(".execution-accounts").scrollIntoViewIfNeeded();
            await page.screenshot({
              path: join(
                process.env.EXECUTION_SCREENSHOTS,
                `after-chat-settings-accounts-${width}-${theme}.png`,
              ),
              fullPage: true,
            });
          }
          expect(
            await page.evaluate(
              () => document.documentElement.scrollWidth <= innerWidth,
            ),
          ).toBe(true);
        }
      if (process.env.EXECUTION_BASELINE === "1") return;
      await page.setViewportSize({ width: 390, height: 844 });
      await page.goto(url);
    }
    await expect(modal.locator('details[name="agent"]')).toHaveCount(3);
    await expect(
      modal.getByRole("radiogroup", { name: "Agent role" }),
    ).toHaveCount(0);
    await modal.locator('details[data-role="orchestrator"] > summary').click();
    await expect(
      modal
        .getByRole("radiogroup", { name: "Main agent model", exact: true })
        .getByRole("radio"),
    ).toHaveCount(7);
    await modal.locator('details[data-role="worker"] > summary').click();
    await expect(
      modal.locator('details[data-role="orchestrator"]'),
    ).not.toHaveAttribute("open", "");
    const workers = modal.locator('details[data-role="worker"]');
    await expect(
      workers
        .getByRole("radiogroup", {
          name: "Default subagent model",
          exact: true,
        })
        .getByRole("radio", { name: "Same as main agent", exact: true }),
    ).toBeVisible();
    await workers
      .getByRole("radiogroup", {
        name: "Default subagent reasoning",
        exact: true,
      })
      .getByText("Low", { exact: true })
      .click();
    await page.waitForFunction(() =>
      window.calls.some((call) => call.worker_defaults?.effort === "low"),
    );
    await modal
      .getByRole("combobox", { name: "Delegation depth", exact: true })
      .selectOption("2");
    await page.waitForFunction(() =>
      window.calls.some((call) => call.worker_defaults?.delegation_depth === 2),
    );
    await modal
      .getByRole("textbox", { name: "Subagent parallelism", exact: true })
      .fill("0");
    await modal.getByRole("button", { name: "Apply", exact: true }).click();
    await page.waitForFunction(() =>
      window.calls.some((call) => call.subagent_concurrency === 0),
    );
    await modal.locator(".execution-accounts > summary").click();
    await modal.getByRole("checkbox", { name: "Work 2", exact: true }).click();
    await page.waitForFunction(() =>
      window.calls.some((call) =>
        call.excluded_account_keys?.includes("account-1"),
      ),
    );
    await expect(modal.locator(".execution-accounts > summary")).toContainText(
      "4 of 5 enabled",
    );
    await expect(
      modal.getByRole("checkbox", { name: "Work 1", exact: true }),
    ).toBeDisabled();
    await expect(
      modal.getByText("Browser access is disabled for this account.", {
        exact: true,
      }),
    ).toBeVisible();
    await expect(modal.getByRole("dialog")).toHaveCount(0);
    await expect(
      modal.getByRole("button", { name: "Compact", exact: true }),
    ).toHaveCount(0);
    await expect(
      modal.getByRole("button", { name: "Review", exact: true }),
    ).toHaveCount(0);
    expect(
      await modal
        .locator(".model-options")
        .evaluateAll((nodes) =>
          nodes.every((node) => getComputedStyle(node).overflowY === "visible"),
        ),
    ).toBe(true);
    expect(
      await page.evaluate(
        () => document.documentElement.scrollWidth <= innerWidth,
      ),
    ).toBe(true);
  } finally {
    await server.close();
    await rm(cacheDir, { recursive: true, force: true });
  }
});
