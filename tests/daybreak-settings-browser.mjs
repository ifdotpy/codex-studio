// Exercise the real settings component without a backend or model request.
import assert from "node:assert/strict";
import { createRequire } from "node:module";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";

const root = join(import.meta.dirname, "../web");
const require = createRequire(join(root, "package.json"));
const { chromium } = require("playwright-core");
const { createServer } = await import(require.resolve("vite"));
const cacheDir = await mkdtemp(join(tmpdir(), "studio-daybreak-settings-"));
const entry = join(root, "daybreak-settings-fixture.tsx");
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
      "lucide-react",
    ],
  },
  plugins: [
    {
      name: "daybreak-settings-fixture",
      configureServer(server) {
        server.middlewares.use("/check", (_req, res) => {
          res.setHeader("Content-Type", "text/html");
          res.end(
            '<div id="root"></div><script type="module" src="/daybreak-settings-fixture.tsx"></script>',
          );
        });
      },
      resolveId(id) {
        if (id === "/daybreak-settings-fixture.tsx") return entry;
      },
      load(id) {
        if (id !== entry) return;
        return `
import React,{useState} from 'react';
import {createRoot} from 'react-dom/client';
import {flushSync} from 'react-dom';
import {MantineProvider} from '@mantine/core';
import '@mantine/core/styles.css';
import {ExecutionSettings} from '/src/components/ExecutionSettings.tsx';
const initial={id:'first',accountKey:'default',name:'First',source:'managed',model:'gpt-6-astra',effort:'high',fastMode:true,daybreakEnabled:false,isLead:true,status:'idle',created:1,yoloMode:false,nextTurnSettingsSupported:true};
const row=(model,cyber,extra={})=>({model,displayName:model,supportedReasoningEfforts:['low','high','max'].map(reasoningEffort=>({reasoningEffort})),serviceTiers:[{id:'priority'}],...(cyber===undefined?{}:{availableAccessPrograms:{cyber}}),...extra});
const initialModels=[row('gpt-6-astra',['standard']),row('gpt-5.6-sol',['standard','daybreakBlue'],{isDefault:true}),row('gpt-5.6-terra',['standard','daybreakRed']),row('gpt-5.6-luna',undefined),row('daybreak-only',['daybreakBlue'],{serviceTiers:[],supportedReasoningEfforts:[{reasoningEffort:'low'}]}),row('gpt-daybreak-blue-latest',['daybreakBlue']),row('gpt-daybreak-red-latest',['daybreakRed'])];
window.calls=[];window.fail=null;window.reply=null;window.refreshCount=0;window.retries=0;
const nativeFetch=window.fetch;
window.fetch=async(url,options)=>{
  if(url!='/api/conversation')return nativeFetch(url,options);
  const request=JSON.parse(options.body);window.calls.push(request);
  if(window.hold)await new Promise(resolve=>window.release=resolve);
  if(window.fail==='network')throw new TypeError('Connection lost');
  if(window.fail==='rejected')return new Response(JSON.stringify({error:'Mode not supported'}),{status:422});
  const values=request.worker_defaults||request;
  const settings={model:values.model,effort:values.effort,fastMode:values.fast_mode,daybreakEnabled:values.daybreak_enabled};
  const canonical=window.reply||{...window.agent,...(request.worker_defaults?{workerDefaults:settings}:request.next_turn?{pendingSettings:settings,pendingSettingsAccountKey:window.agent.accountKey}:settings)};
  return new Response(JSON.stringify(canonical),{status:200,headers:{'Content-Type':'application/json'}});
};
function Fixture(){
  const[agent,setAgent]=useState(initial),[models,setModels]=useState(initialModels),[mount,setMount]=useState(0),[defaults,setDefaults]=useState(false);
  window.agent=agent;window.setAgent=value=>flushSync(()=>setAgent(value));
  window.setModels=value=>flushSync(()=>setModels(value));
  window.reset=options=>flushSync(()=>{setAgent({...initial,...options?.agent});setModels(options?.models||initialModels);setDefaults(!!options?.defaults);setMount(value=>value+1);window.calls=[];window.fail=null;window.reply=null;window.hold=false;window.refreshCount=0;window.retries=0;});
  return <MantineProvider><ExecutionSettings key={mount} agent={agent} teamDefaults={defaults} catalog={{models,loading:false,error:'',retry:()=>window.retries++}} refresh={async()=>{window.refreshCount++}}/></MantineProvider>;
}
createRoot(document.getElementById('root')).render(<Fixture/>);`;
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
  const page = await browser.newPage({ viewport: { width: 390, height: 700 } });
  const errors = [];
  page.on("pageerror", (error) => errors.push(error.message));
  await page.goto(`http://127.0.0.1:${server.httpServer.address().port}/check`);
  await page.waitForFunction(() => !!window.reset);
  const reset = async (options) => {
    await page.evaluate((options) => {
      localStorage.clear();
      window.reset(options);
    }, options);
    await page
      .getByRole("button", {
        name: options?.defaults ? "Subagent defaults" : "Main agent settings",
        exact: true,
      })
      .click();
  };
  const mode = () =>
    page.getByRole("switch", { name: "Daybreak", exact: true });
  const model = (defaults = false) =>
    page.getByLabel(defaults ? "Default subagent model" : "Main agent model", {
      exact: true,
    });
  const settled = async () =>
    page.waitForFunction(
      () =>
        window.refreshCount === window.calls.length && window.calls.length > 0,
    );

  await reset();
  assert.equal(
    await model().locator('option[value="gpt-daybreak-blue-latest"]').count(),
    0,
  );
  assert.equal(
    await model().locator('option[value="gpt-daybreak-red-latest"]').count(),
    0,
  );
  assert.equal(
    await model().locator('option[value="daybreak-only"]').isDisabled(),
    true,
  );
  await mode().check();
  await settled();
  assert.deepEqual(await page.evaluate(() => window.calls[0]), {
    id: "first",
    expected_account_key: "default",
    model: "gpt-5.6-sol",
    effort: "high",
    fast_mode: true,
    daybreak_enabled: true,
  });
  assert.equal(await model().inputValue(), "gpt-5.6-sol");
  assert.equal(await mode().isChecked(), true);
  assert.equal(
    await model().locator('option[value="gpt-6-astra"]').isDisabled(),
    true,
  );
  assert.equal(
    await model().locator('option[value="gpt-5.6-luna"]').isDisabled(),
    true,
  );
  assert.equal(
    await model().locator('option[value="gpt-5.6-terra"]').isDisabled(),
    false,
  );
  assert.match(
    await page
      .getByRole("button", { name: "Main agent settings", exact: true })
      .innerText(),
    /Sol · Daybreak/,
  );
  if (process.env.SCREENSHOT)
    await page.screenshot({ path: process.env.SCREENSHOT });
  // A stale replication snapshot must not erase the acknowledged mode.
  await page.evaluate(() =>
    window.setAgent({ ...window.agent, tail: "Unrelated update" }),
  );
  assert.equal(await mode().isChecked(), true);
  await page.evaluate(() =>
    window.setAgent({
      ...window.agent,
      model: "gpt-5.6-sol",
      daybreakEnabled: true,
    }),
  );
  await page.waitForTimeout(50);
  await page.evaluate(() =>
    window.setAgent({ ...window.agent, daybreakEnabled: false }),
  );
  assert.equal(await mode().isChecked(), false);
  console.log(
    "PASS separate mode, alias exclusion, capability gating, fallback model, effort preservation, canonical replication",
  );

  // Explicit daybreak-only rows cannot be selected when standard mode is active.
  await reset({
    agent: {
      model: "daybreak-only",
      daybreakEnabled: true,
      effort: "low",
      fastMode: false,
    },
  });
  await mode().uncheck();
  await settled();
  assert.equal(await model().inputValue(), "gpt-5.6-sol");
  assert.equal(
    await page.evaluate(() => window.calls[0].daybreak_enabled),
    false,
  );
  await reset({ agent: { model: "gpt-5.6-terra" } });
  await mode().check();
  await settled();
  assert.equal(
    await model().inputValue(),
    "gpt-5.6-terra",
    "Red-only support keeps the selected model",
  );
  await model().selectOption("daybreak-only");
  await settled();
  assert.equal(
    await page.getByLabel("Main agent reasoning", { exact: true }).inputValue(),
    "__model_default__",
  );
  assert.equal(
    await page
      .getByRole("switch", { name: "Fast mode", exact: true })
      .isChecked(),
    false,
  );
  console.log(
    "PASS standard fallback, Red support, model-dependent effort and Fast reset",
  );

  await reset({
    models: [
      {
        model: "gpt-6-astra",
        supportedReasoningEfforts: [{ reasoningEffort: "high" }],
      },
    ],
  });
  assert.equal(await mode().isDisabled(), true);
  assert.equal(
    await model().locator('option[value="gpt-6-astra"]').isDisabled(),
    false,
  );
  await page
    .getByRole("button", { name: "Refresh model list", exact: true })
    .click();
  assert.equal(await page.evaluate(() => window.retries), 1);
  assert.deepEqual(await page.evaluate(() => window.calls), []);
  await reset({ agent: { model: "gpt-daybreak-blue-latest" } });
  assert.equal(
    await model()
      .locator('option[value="gpt-daybreak-blue-latest"]')
      .isDisabled(),
    true,
  );
  assert.equal(
    await page.getByLabel("Main agent reasoning", { exact: true }).isDisabled(),
    true,
  );
  await mode().check();
  await settled();
  assert.equal(await model().inputValue(), "gpt-5.6-sol");
  console.log(
    "PASS missing metadata fails closed, refresh, legacy alias remains read-only without invented mapping",
  );

  await reset({ agent: { status: "running" } });
  await page.evaluate(() => (window.fail = "network"));
  await mode().check();
  await page
    .getByRole("button", { name: "Check settings save", exact: true })
    .waitFor();
  const request = await page.evaluate(() => window.calls[0]);
  assert.equal(request.next_turn, true);
  assert.equal(request.daybreak_enabled, true);
  assert.equal(await page.evaluate(() => window.agent.daybreakEnabled), false);
  assert.match(
    await page
      .getByRole("button", { name: "Main agent settings", exact: true })
      .innerText(),
    /Daybreak next turn/,
  );
  await page.reload();
  await page.waitForFunction(() => !!window.reset);
  await page
    .getByRole("button", { name: "Main agent settings", exact: true })
    .click();
  await page
    .getByRole("button", { name: "Check settings save", exact: true })
    .click();
  await settled();
  assert.deepEqual(
    await page.evaluate(() => window.calls[0]),
    request,
    "Retry retains the exact mode and request identity",
  );
  assert.equal(await mode().isChecked(), true);
  console.log(
    "PASS queued Daybreak, current turn unchanged, lost response reload and exact receipt replay",
  );

  await reset({ agent: { status: "running" } });
  await page.evaluate(() => (window.fail = "network"));
  await mode().check();
  await page
    .getByRole("button", { name: "Check settings save", exact: true })
    .waitFor();
  await page.evaluate(() =>
    window.setAgent({
      ...window.agent,
      accountKey: "second",
      model: "gpt-5.6-terra",
      daybreakEnabled: false,
    }),
  );
  await page
    .getByRole("button", { name: "Main agent settings", exact: true })
    .click();
  assert.equal(await mode().isChecked(), false);
  assert.equal(
    await page
      .getByRole("button", { name: "Check settings save", exact: true })
      .count(),
    0,
  );
  await reset({
    agent: {
      pendingSettingsAccountKey: "old-account",
      pendingSettings: { model: "gpt-5.6-sol", daybreakEnabled: true },
    },
  });
  assert.equal(await mode().isChecked(), false);
  await page.evaluate(() => (window.fail = "rejected"));
  await mode().check();
  await page
    .getByRole("alert")
    .filter({ hasText: "Mode not supported" })
    .waitFor();
  assert.equal(await mode().isChecked(), false);
  assert.equal(await model().inputValue(), "gpt-6-astra");
  console.log(
    "PASS account-scoped mode receipts, stale account settings ignored, rejected mode rollback",
  );

  await reset({
    defaults: true,
    agent: {
      workerDefaults: {
        model: null,
        effort: "high",
        fastMode: false,
        daybreakEnabled: false,
      },
    },
  });
  await mode().check();
  await settled();
  assert.deepEqual(await page.evaluate(() => window.calls[0]), {
    id: "first",
    expected_account_key: "default",
    worker_defaults: {
      model: "gpt-5.6-sol",
      effort: "high",
      fast_mode: false,
      daybreak_enabled: true,
    },
  });
  assert.equal(
    await model(true).locator('option[value="__model_default__"]').isDisabled(),
    true,
  );
  assert.equal(
    await page.evaluate(
      () => document.documentElement.scrollWidth <= innerWidth + 1,
    ),
    true,
  );
  await reset({ agent: { provider: "claude" } });
  assert.equal(await mode().count(), 0);
  console.log(
    "PASS subagent defaults, inherited model capability gate, narrow layout, Claude mode hidden",
  );
  assert.deepEqual(errors, []);
} finally {
  await browser?.close();
  await server.close();
  await rm(cacheDir, { recursive: true, force: true });
}
