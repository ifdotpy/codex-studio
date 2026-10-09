import {
  API_SCHEMA_HASH_HEADER,
  readApiSchemaHash,
  apiSchemaHandshakeSse,
} from "../playwright.mjs";
import assert from "node:assert/strict";
import { createRequire } from "node:module";
import { mkdtemp, mkdir, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test, expect } from "../playwright.mjs";
import { chooseSetupValue } from "../../setup-controls.mjs";

// Production components with isolated HTTP responses. No live account or model request.
test("unified agent picker roles, accounts and project/shared flows", async ({
  browser,
}) => {
  test.setTimeout(180000);
  const root = join(import.meta.dirname, "../../../web");
  const require = createRequire(join(root, "package.json"));
  const { createServer } = await import(require.resolve("vite"));
  const cacheDir = await mkdtemp(join(tmpdir(), "studio-unified-picker-vite-"));
  const output =
    process.env.PICKER_SCREENSHOTS ||
    join(tmpdir(), "studio-unified-picker-shots");
  await mkdir(output, { recursive: true });
  const entry = join(root, "unified-picker-fixture.tsx");
  const server = await createServer({
    configFile: false,
    root,
    cacheDir,
    server: { host: "127.0.0.1", port: 0 },
    optimizeDeps: {
      noDiscovery: true,
      include: [
        "react",
        "react/jsx-runtime",
        "react/jsx-dev-runtime",
        "react-dom/client",
        "react-dom",
        "@mantine/core",
        "@mantine/hooks",
        "lucide-react",
        "rxdb",
        "rxdb/plugins/storage-dexie",
        "rxdb/plugins/leader-election",
        "rxdb/plugins/replication",
      ],
    },
    plugins: [
      {
        name: "unified-picker-fixture",
        configureServer(server) {
          server.middlewares.use("/check", (_req, res) => {
            res.setHeader("Content-Type", "text/html");
            res.end(
              '<meta name="viewport" content="width=device-width,initial-scale=1"><div id="root"></div><script type="module" src="/unified-picker-fixture.tsx"></script>',
            );
          });
        },
        resolveId(id) {
          if (id === "/unified-picker-fixture.tsx") return entry;
        },
        load(id) {
          if (id !== entry) return;
          return `import React,{useState} from 'react';import {createRoot} from 'react-dom/client';import {flushSync} from 'react-dom';import {MantineProvider} from '@mantine/core';import '@mantine/core/styles.css';import {theme} from '/src/theme.ts';import '/src/style.css';import {ExecutionSettings} from '/src/components/agents/ExecutionSettings.tsx';import {SettingsRow} from '/src/components/ui/primitives.tsx';import '/src/appearance.css';import {AccountTiles} from '/src/components/AccountTiles.tsx';import ProjectAccount from '/src/components/ProjectAccount.tsx';import SharedChatCreate from '/src/components/SharedChatCreate.tsx';
      const accounts=Array.from({length:7},(_,i)=>({id:'account-'+i,home:'/fixture/'+i,label:i<3?'Work '+(i+1):'Personal '+(i-2),email:'fixture'+i+'@example.com',source:'fixture',provider:i<4?'codex':'claude',status:'ready'}));
      const codex=['gpt-6-astra','gpt-6-luna'].map((model,i)=>({model,displayName:i?'Luna':'Astra',isDefault:!i,provider:'codex',defaultReasoningEffort:'medium',supportedReasoningEfforts:['low','medium','high'].map(reasoningEffort=>({reasoningEffort})),serviceTiers:i?[]:[{id:'priority'}],availableAccessPrograms:{cyber:i?['standard']:['standard','daybreakBlue']}}));
      const claudeRow=(model,displayName,extra)=>({model,displayName,provider:'claude',defaultReasoningEffort:'medium',supportedReasoningEfforts:['low','medium','high'].map(reasoningEffort=>({reasoningEffort})),serviceTiers:[],...extra});const claude=[claudeRow('default','Claude · Default (recommended)',{isDefault:true,resolvedModel:'claude-opus-5-5'}),claudeRow('opus','Claude · Opus 5.5',{resolvedModel:'claude-opus-5-5'}),claudeRow('claude-sonnet-5-5','Claude Sonnet 5.5',{})];
      const initial={id:'lead',accountKey:'account-0',provider:'codex',name:'Lead',source:'managed',model:'gpt-6-astra',effort:'medium',fastMode:false,daybreakEnabled:false,isLead:true,status:'idle',created:1,yoloMode:false,nextTurnSettingsSupported:true,workerDefaults:{model:'gpt-6-luna',effort:'high',fastMode:false},reviewDefaults:{model:null,effort:null}};
      window.calls=[];const nativeFetch=window.fetch;window.fetch=async(input,options)=>{const url=input instanceof Request?input.url:input;options=input instanceof Request?{method:input.method,body:input.method==='GET'?undefined:await input.clone().text()}:options;const u=new URL(url,location.origin);let result;if(u.pathname==='/api/limits'){const key=u.searchParams.get('account_key'),i=Number(key?.split('-')[1]);result={accountKey:key,at:Date.now()/1000,data:{rateLimits:{limitId:i<4?'codex':'claude',primary:{usedPercent:[20,60,95,100,25,70,100][i],windowDurationMins:300,resetsAt:Date.now()/1000+3600}}}};}else if(u.pathname==='/api/models'){result={data:u.searchParams.get('workers')?'1'===u.searchParams.get('workers')?[...codex,...claude]:codex:Number(u.searchParams.get('account_key')?.split('-')[1])>=4?claude:codex};}else if(options?.method==='POST'){const body=JSON.parse(options.body);window.calls.push({endpoint:u.pathname,...body});if(u.pathname==='/api/conversation'){const next={...window.agent};if(body.worker_defaults)next.workerDefaults={model:body.worker_defaults.model,effort:body.worker_defaults.effort,fastMode:body.worker_defaults.fast_mode,daybreakEnabled:body.worker_defaults.daybreak_enabled,accountKey:body.worker_defaults.account_key};else if(body.review_defaults)next.reviewDefaults=body.review_defaults;else Object.assign(next,{model:body.model,effort:body.effort,fastMode:body.fast_mode,daybreakEnabled:body.daybreak_enabled});window.updateAgent(next);result=next;}else if(u.pathname==='/api/agents/account-transfer'){result={id:body.request_id,leadId:body.id,scope:body.scope,status:'completed',targetAccountKey:body.account_key,total:0,moved:0,completed:0};window.updateAgent({...window.agent,workerDefaults:{accountKey:body.account_key,model:Number(body.account_key.split('-')[1])>=4?claude[0].model:codex[0].model,effort:'medium',fastMode:false}});}else if(u.pathname==='/api/projects')result={id:'project',path:'/fixture/project',name:'Project',created:1,accountKey:body.account_key,accountKeys:body.account_keys,accountRevision:1};else if(u.pathname==='/api/peer-teams')result={room:{id:'shared'}};}if(result===undefined)return nativeFetch(url,options);return new Response(JSON.stringify(result),{headers:{'Content-Type':'application/json'}});};
      function Fixture(){const[agent,setAgent]=useState(initial),[scene,setScene]=useState('picker'),[scheme,setScheme]=useState('dark'),[account,setAccount]=useState('account-0');window.agent=agent;window.updateAgent=value=>flushSync(()=>setAgent(value));window.scene=value=>flushSync(()=>setScene(value));window.scheme=value=>flushSync(()=>setScheme(value));window.reset=()=>flushSync(()=>{setAgent(initial);setScene('picker');window.calls=[]});const data={stateDir:'fixture',threads:[],runtime:{rooms:[],projects:[{id:'project',path:'/fixture/project',name:'Project',created:1}]}};return <MantineProvider theme={theme} forceColorScheme={scheme}><main style={{padding:20,maxWidth:720,margin:'0 auto'}}><h2>Agent setup</h2>{scene==='picker'?<ExecutionSettings agent={agent} accounts={accounts} catalog={{models:agent.provider==='claude'?claude:codex,loading:false,error:'',retry:()=>{}}} onAccountChange={key=>{window.calls.push({path:'account-selection',key});const provider=accounts.find(a=>a.id===key).provider;setAgent({...agent,accountKey:key,provider,model:provider==='claude'?claude[0].model:codex[0].model,effort:'medium'});}} refresh={async()=>{}}/>:scene==='rows'?<div className="chat-settings-panel chat-settings-rows">{['orchestrator','worker','review'].map(role=><SettingsRow key={role} label={role==='orchestrator'?'Model':role==='worker'?'Workers':'Review'}><ExecutionSettings settingsRow initialRole={role} agent={agent} accounts={accounts} catalog={{models:agent.provider==='claude'?claude:codex,loading:false,error:'',retry:()=>{}}} onAccountChange={()=>{}} refresh={async()=>{}}/></SettingsRow>)}</div>:scene==='chat-account'?<AccountTiles label="Account" accounts={accounts} value={account} onChange={setAccount}/>:scene==='project'?<ProjectAccount path="/fixture/project" project={{id:'project',path:'/fixture/project',name:'Project',created:1,accountKey:'account-0',accountKeys:accounts.map(a=>a.id)}} defaultAccountKey="account-0" accounts={{accounts,defaultAccountKey:'account-0'}} saved={async()=>{}}/>:<SharedChatCreate data={data} accounts={{accounts,defaultAccountKey:'account-0'}} initialPath="/fixture/project" refresh={async()=>{}} created={id=>{window.created=id}}/>}</main></MantineProvider>};createRoot(document.getElementById('root')).render(<Fixture/>);`;
        },
      },
    ],
  });
  const results = [];
  const contexts = [];
  try {
    await server.listen();
    const url = server.resolvedUrls.local[0] + "check";
    for (const [profile, width, scheme] of [
      ["dark", 1440, "dark"],
      ["light", 1440, "light"],
      ["mobile", 390, "dark"],
    ]) {
      const context = await browser.newContext({
        viewport: { width, height: 900 },
        colorScheme: scheme,
      });
      contexts.push(context);
      const page = await context.newPage();
      const errors = [];
      page.on("pageerror", (error) => {
        errors.push(error.message);
        console.error(error.message);
      });
      page.on("console", (message) => {
        if (message.type() === "error") console.error(message.text());
      });
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
      await page.route("**/api/sync/identity", (route) =>
        route.fulfill({
          headers: { [API_SCHEMA_HASH_HEADER]: readApiSchemaHash() },
          json: {
            workspaceId: "1234567890abcdef1234567890abcdef",
            syncProtocol: 2,
          },
        }),
      );
      await page.goto(url);
      await page.waitForFunction(
        () => typeof window.scheme === "function",
        null,
        { timeout: 30000 },
      );
      await page.evaluate((scheme) => window.scheme(scheme), scheme);
      const trigger = page.getByRole("button", {
        name: "Main agent settings",
        exact: true,
      });
      await expect(trigger).toContainText("Astra 6 · Medium · Work 1");
      assert.equal(
        await trigger
          .locator(".execution-selected")
          .evaluate((node) => node.scrollWidth <= node.clientWidth),
        true,
      );
      await page.screenshot({
        path: join(output, `${profile}-composer-trigger.png`),
      });
      await page
        .getByRole("button", { name: "Main agent settings", exact: true })
        .click();
      await expect(
        page.getByRole("listbox", { name: "Main agent model" }),
      ).toBeVisible();
      const capture = async (scene) => {
        await page.screenshot({
          path: join(output, `${profile}-${scene}.png`),
        });
        if (process.env.PICKER_CLIP_DETECTOR) {
          const { detectTextClipping } = await import(
            process.env.PICKER_CLIP_DETECTOR
          );
          results.push(
            ...(await page.evaluate(detectTextClipping, { scene, profile })),
          );
        }
      };
      await expect(page.locator('[role="meter"]').first()).toHaveAttribute(
        "aria-valuenow",
        "80",
      );
      await expect(
        page.locator('[data-account-key="account-3"]'),
      ).toHaveAttribute("data-exhausted", "true");
      await capture("orchestrator-codex");
      await chooseSetupValue(
        page.getByLabel("Main agent reasoning", { exact: true }),
        "high",
      );
      await page.waitForFunction(() =>
        window.calls.some((call) => call.effort === "high"),
      );
      await page
        .getByRole("button", { name: "Fast mode", exact: true })
        .click();
      await page.waitForFunction(() =>
        window.calls.some((call) => call.fast_mode === true),
      );
      await page.getByRole("button", { name: "Worker", exact: true }).click();
      await expect(
        page.getByRole("listbox", { name: "Default subagent model" }),
      ).toBeVisible();
      await capture("worker-codex");
      await chooseSetupValue(
        page.getByLabel("Subagent account", { exact: true }),
        "account-4",
      );
      await page.waitForFunction(() =>
        window.calls.some(
          (call) =>
            call.scope === "subagents" && call.account_key === "account-4",
        ),
      );
      await expect(
        page.getByRole("option", {
          name: "Opus 5.5 recommended",
          exact: true,
        }),
      ).toBeVisible();
      await capture("worker-claude");
      assert.equal(
        await page
          .getByRole("button", { name: "Daybreak", exact: true })
          .count(),
        0,
      );
      assert.equal(
        await page
          .getByRole("button", { name: "Fast mode", exact: true })
          .count(),
        0,
      );
      await page.getByRole("button", { name: "Review", exact: true }).click();
      await expect(
        page.getByText("Uses the caller account", { exact: true }),
      ).toBeVisible();
      assert.equal(
        await page.getByLabel("Subagent account", { exact: true }).count(),
        0,
      );
      assert.equal(
        await page.getByRole("option", { name: /Sonnet/ }).count(),
        0,
      );
      // Restore automatic workers, so review defaults have the Codex catalog accepted by the backend.
      await page.evaluate(() =>
        window.updateAgent({
          ...window.agent,
          workerDefaults: {
            model: "gpt-6-luna",
            effort: "high",
            fastMode: false,
          },
        }),
      );
      await expect(
        page.getByRole("option", { name: "Astra 6 recommended", exact: true }),
      ).toBeVisible();
      await page
        .getByRole("option", { name: "Astra 6 recommended", exact: true })
        .click();
      await page.waitForFunction(() =>
        window.calls.some(
          (call) => call.review_defaults?.model === "gpt-6-astra",
        ),
      );
      await capture("review-codex");
      await page
        .getByRole("button", { name: "Orchestrator", exact: true })
        .click();
      await chooseSetupValue(
        page.getByLabel("Main agent account", { exact: true }),
        "account-5",
      );
      // A default alias reads as the model it resolves to, listed once.
      await expect(
        page.getByRole("option", {
          name: "Opus 5.5 recommended",
          exact: true,
        }),
      ).toBeVisible();
      await expect(
        page.getByRole("option", { name: /^Opus 5\.5/ }),
      ).toHaveCount(1);
      await expect(page.locator(".execution-menu").first()).toContainText(
        "Opus 5.5",
      );
      await expect(page.locator(".execution-menu").first()).not.toContainText(
        "default",
      );
      await capture("orchestrator-claude");
      await page.evaluate(() => {
        window.reset();
        window.scene("rows");
      });
      const mainRow = page.getByRole("button", {
        name: "Main agent settings",
        exact: true,
      });
      const workersRow = page.getByRole("button", {
        name: "Subagent defaults",
        exact: true,
      });
      const reviewRow = page.getByRole("button", {
        name: "Review settings",
        exact: true,
      });
      await expect(mainRow).toContainText("Astra 6 · Medium · Work 1");
      await expect(workersRow).toContainText("Luna 6 · High · Work 1");
      await expect(reviewRow).toContainText("Astra 6 · Medium");
      await expect(reviewRow).not.toContainText("Work 1");
      await expect(reviewRow.locator(".execution-selected")).toHaveAttribute(
        "data-inherited",
        "true",
      );
      for (const [row, role] of [
        [mainRow, "Orchestrator"],
        [workersRow, "Worker"],
        [reviewRow, "Review"],
      ]) {
        await row.click();
        await expect(
          page.getByRole("button", { name: role, exact: true }),
        ).toHaveAttribute("aria-pressed", "true");
        await page.keyboard.press("Escape");
        await expect(page.locator(".execution-dropdown:visible")).toHaveCount(
          0,
        );
      }
      await reviewRow.click();
      await page
        .getByLabel("Default review model", { exact: true })
        .locator('[data-value="gpt-6-luna"]')
        .click();
      await chooseSetupValue(
        page.getByLabel("Default review reasoning", { exact: true }),
        "high",
      );
      await page.keyboard.press("Escape");
      await expect(reviewRow).toContainText("Luna 6 · High");
      await expect(
        reviewRow.locator(".execution-selected"),
      ).not.toHaveAttribute("data-inherited");
      assert.equal(
        await page.evaluate(
          () =>
            window.calls.filter((call) => call.review_defaults).at(-1)
              .expected_account_key,
        ),
        "account-0",
      );
      await capture("chat-settings-model-rows");
      for (const scene of ["chat-account", "project", "shared"]) {
        await page.evaluate((scene) => window.scene(scene), scene);
        if (scene === "shared")
          await page
            .getByRole("button", { name: "Settings for agent 1" })
            .click();
        const group = page.getByLabel(
          scene === "chat-account"
            ? "Account"
            : scene === "project"
              ? "Accounts shown first for this project"
              : "Account for agent 1",
          { exact: true },
        );
        await expect(group).toBeVisible();
        await capture(scene + "-codex");
        await group.locator("label").filter({ hasText: "Claude" }).click();
        await capture(scene + "-claude");
        if (scene === "project") {
          await group.locator('[data-account-key="account-4"]').click();
          await page
            .getByRole("button", { name: "Save accounts", exact: true })
            .click();
          await page.waitForFunction(() =>
            window.calls.some(
              (call) =>
                call.endpoint === "/api/projects" &&
                !call.account_keys.includes("account-4"),
            ),
          );
        } else if (scene === "shared") {
          await group.locator('[data-account-key="account-5"]').click();
          await expect(
            page.getByLabel("Model for agent 1", { exact: true }),
          ).toHaveAttribute("data-value", "default");
          await chooseSetupValue(
            page.getByLabel("Reasoning for agent 1", { exact: true }),
            "high",
          );
          await page.keyboard.press("Escape");
          await page
            .getByRole("button", { name: "Create", exact: true })
            .click();
          await page.waitForFunction(() =>
            window.calls.some((call) => call.endpoint === "/api/peer-teams"),
          );
          assert.deepEqual(
            await page.evaluate(() => {
              const participant = window.calls.find(
                (call) => call.endpoint === "/api/peer-teams",
              ).participants[0];
              return [
                participant.account_key,
                participant.model,
                participant.effort,
              ];
            }),
            ["account-5", "default", "high"],
          );
        }
      }
      assert.deepEqual(errors, []);
    }
    await writeFile(
      join(output, "clipping.json"),
      JSON.stringify(results, null, 2),
    );
    console.log(`Screenshots: ${output}`);
    assert.deepEqual(
      results.filter((row) => !row.intentional),
      [],
    );
  } finally {
    await Promise.all(contexts.map((context) => context.close()));
    await server.close();
    await rm(cacheDir, { recursive: true, force: true });
  }
});
