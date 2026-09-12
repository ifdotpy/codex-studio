import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { createRequire } from "node:module";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
const root = join(import.meta.dirname, "../web");
const require = createRequire(join(root, "package.json"));
const { chromium, webkit } = require("playwright-core");
const { createServer } = await import(require.resolve("vite"));
const browserType = process.env.BROWSER === "webkit" ? webkit : chromium;
const cacheDir = await mkdtemp(join(tmpdir(), "studio-settings-canonical-"));
const entry = join(root, "settings-canonical-fixture.tsx");
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
      name: "settings-canonical-fixture",
      configureServer(server) {
        server.middlewares.use("/check", (_req, res) => {
          res.setHeader("Content-Type", "text/html");
          res.end(
            '<div id="root"></div><script type="module" src="/settings-canonical-fixture.tsx"></script>',
          );
        });
      },
      resolveId(id) {
        if (id === "/settings-canonical-fixture.tsx") return entry;
      },
      load(id) {
        if (
          process.env.BASELINE === "1" &&
          id === join(root, "src/components/ExecutionSettings.tsx")
        )
          return execFileSync(
            "git",
            ["show", "5217bb9:web/src/components/ExecutionSettings.tsx"],
            { cwd: root, encoding: "utf8" },
          );
        if (id !== entry) return;
        return `import React,{useState} from 'react';import {createRoot} from 'react-dom/client';import {flushSync} from 'react-dom';import {MantineProvider} from '@mantine/core';import '@mantine/core/styles.css';import {ExecutionSettings} from '/src/components/ExecutionSettings.tsx';
const initial={id:'first',accountKey:'default',name:'First',source:'managed',model:'gpt-6-astra',effort:'low',fastMode:false,isLead:true,status:'idle',created:1,yoloMode:false,nextTurnSettingsSupported:true};
const models=['gpt-6-astra','gpt-5.6-sol','gpt-5.6-luna'].map(model=>({model,supportedReasoningEfforts:['low','medium','high','max'].map(reasoningEffort=>({reasoningEffort})),serviceTiers:[{id:'priority'}]}));
window.calls=[];window.reply=null;window.fail=null;window.hold=false;window.refreshFailure=false;window.refreshCount=0;const nativeFetch=window.fetch;window.fetch=async(url,options)=>{if(url!='/api/conversation')return nativeFetch(url,options);window.calls.push(JSON.parse(options.body));if(window.serverAccount && window.calls.at(-1).expected_account_key!==window.serverAccount)return new Response(JSON.stringify({error:'The account changed. Select the model again'}),{status:409});if(window.hold)await new Promise(resolve=>window.release=resolve);if(window.fail==='network')throw new TypeError('Connection lost');return new Response(JSON.stringify(window.fail?{error:'Rejected'}:window.reply),{status:window.fail?409:200,headers:{'Content-Type':'application/json'}})};
function Fixture(){const[agent,setAgent]=useState(initial),[mount,setMount]=useState(0),[defaults,setDefaults]=useState(false);window.setAgent=value=>flushSync(()=>setAgent(value));window.agent=agent;window.reset=options=>flushSync(()=>{setAgent({...initial,...options?.agent});setDefaults(!!options?.defaults);setMount(value=>value+1);window.calls=[];window.reply=null;window.fail=null;window.hold=false;window.refreshFailure=false;window.refreshCount=0;window.serverAccount=null;});return <MantineProvider><ExecutionSettings key={mount} agent={agent} teamDefaults={defaults} catalog={{models,loading:false,error:'',retry:()=>{}}} refresh={async()=>{window.refreshCount++;if(window.refreshFailure)throw new Error('Snapshot unavailable')}}/></MantineProvider>}createRoot(document.getElementById('root')).render(<Fixture/>);`;
      },
    },
  ],
});
let browser;
const failures = [];
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
  const page = await browser.newPage({ viewport: { width: 390, height: 700 } });
  const errors = [];
  page.on("pageerror", (error) => {
    errors.push(error.message);
    console.error(error.message);
  });
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
  const effort = (defaults = false) =>
    page.getByLabel(
      defaults ? "Default subagent reasoning" : "Main agent reasoning",
      { exact: true },
    );
  const value = async (expected, defaults = false) => {
    await page.waitForFunction(
      ({ expected, defaults }) => {
        const label = [...document.querySelectorAll("label")].find(
          (node) =>
            node.textContent ===
            (defaults ? "Default subagent reasoning" : "Main agent reasoning"),
        );
        return (
          label && document.getElementById(label.htmlFor)?.value === expected
        );
      },
      { expected, defaults },
      { timeout: 2500 },
    );
  };
  const check = async (name, run) => {
    if (process.env.CASE && process.env.CASE !== name) return;
    try {
      await run();
      console.log(`PASS ${name}`);
    } catch (error) {
      failures.push(`${name}: ${error.message}`);
      console.error(`FAIL ${name}: ${error.message}`);
    }
  };
  await check("canonical", async () => {
    await reset();
    await page.evaluate(
      () => (window.reply = { ...window.agent, effort: "medium" }),
    );
    await effort().selectOption("high");
    await value("medium");
    await page.evaluate(() =>
      window.setAgent({ ...window.agent, tail: "Unrelated snapshot" }),
    );
    await value("medium");
    await page.evaluate(() =>
      window.setAgent({ ...window.agent, effort: "medium" }),
    );
    await page.waitForTimeout(50);
    await page.evaluate(() =>
      window.setAgent({ ...window.agent, effort: "max" }),
    );
    await value("max");
  });
  await check("replay", async () => {
    await reset({ agent: { status: "running" } });
    await page.evaluate(() => (window.fail = "network"));
    await effort().selectOption("high");
    await page.getByRole("button", { name: "Check settings save" }).waitFor();
    const request = await page.evaluate(() => window.calls[0]);
    await page.reload();
    await page.waitForFunction(() => !!window.reset);
    await page
      .getByRole("button", { name: "Main agent settings", exact: true })
      .click();
    await page.getByRole("button", { name: "Check settings save" }).waitFor();
    await page.evaluate(
      () =>
        (window.reply = {
          ...window.agent,
          pendingSettings: {
            model: "gpt-6-astra",
            effort: "max",
            fastMode: true,
          },
        }),
    );
    await page.getByRole("button", { name: "Check settings save" }).click();
    await value("max");
    assert.deepEqual(await page.evaluate(() => window.calls[0]), request);
    assert.equal(
      await page.getByRole("button", { name: "Check settings save" }).count(),
      0,
    );
    await page.reload();
    await page
      .getByRole("button", { name: "Main agent settings", exact: true })
      .click();
    assert.equal(
      await page.getByRole("button", { name: "Check settings save" }).count(),
      0,
    );
  });
  await check("account-scope", async () => {
    await reset({ agent: { status: "running" } });
    await page.evaluate(() => (window.fail = "network"));
    await effort().selectOption("high");
    await page.getByRole("button", { name: "Check settings save" }).waitFor();
    await page.evaluate(() =>
      window.setAgent({
        ...window.agent,
        accountKey: "second",
        effort: "medium",
      }),
    );
    const button = page.getByRole("button", {
      name: "Main agent settings",
      exact: true,
    });
    if ((await button.getAttribute("aria-expanded")) === "false")
      await button.click();
    await value("medium");
    assert.equal(
      await page.getByRole("button", { name: "Check settings save" }).count(),
      0,
    );
    await page.evaluate(() =>
      window.setAgent({
        ...window.agent,
        accountKey: "default",
        effort: "low",
      }),
    );
    if ((await button.getAttribute("aria-expanded")) === "false")
      await button.click();
    await page.getByRole("button", { name: "Check settings save" }).waitFor();
    await value("high");
  });
  await check("late-scope-response", async () => {
    await reset();
    await page.evaluate(() => {
      window.hold = true;
      window.reply = { ...window.agent, effort: "high" };
    });
    await effort().selectOption("high");
    await page.waitForFunction(() => !!window.release);
    await page.evaluate(() =>
      window.setAgent({ ...window.agent, id: "second", effort: "medium" }),
    );
    await page.evaluate(() => window.release());
    const button = page.getByRole("button", {
      name: "Main agent settings",
      exact: true,
    });
    if ((await button.getAttribute("aria-expanded")) === "false")
      await button.click();
    await value("medium");
    await page.waitForTimeout(50);
    assert.equal(await page.evaluate(() => window.refreshCount), 0);
  });
  await check("defaults-and-refresh-failure", async () => {
    await reset({ defaults: true });
    await page.evaluate(() => {
      window.reply = {
        ...window.agent,
        workerDefaults: {
          model: "gpt-5.6-luna",
          effort: "medium",
          fastMode: false,
        },
      };
      window.refreshFailure = true;
    });
    await effort(true).selectOption("high");
    await value("medium", true);
    assert.equal(
      await page.evaluate(() => window.calls[0].expected_account_key),
      "default",
    );
    await page
      .getByRole("alert")
      .filter({ hasText: "Settings saved." })
      .waitFor();
    await page.evaluate(() =>
      window.setAgent({
        ...window.agent,
        workerDefaults: {
          model: "gpt-5.6-luna",
          effort: "medium",
          fastMode: false,
        },
      }),
    );
    await page.waitForTimeout(50);
    await page.evaluate(() =>
      window.setAgent({
        ...window.agent,
        workerDefaults: {
          model: "gpt-5.6-luna",
          effort: "low",
          fastMode: false,
        },
      }),
    );
    await value("low", true);
  });
  await check("rejected-does-not-pin-old-snapshot", async () => {
    await reset();
    await page.evaluate(() => {
      window.hold = true;
      window.fail = "rejected";
    });
    await effort().selectOption("high");
    await page.waitForFunction(() => !!window.release);
    await page.evaluate(() => {
      window.setAgent({ ...window.agent, effort: "medium" });
      window.release();
    });
    await page.getByRole("alert").waitFor();
    await value("medium");
  });
  await check("rejected-after-confirmed-before-snapshot", async () => {
    await reset();
    await page.evaluate(
      () => (window.reply = { ...window.agent, effort: "medium" }),
    );
    await effort().selectOption("high");
    await value("medium");
    await page.evaluate(() => (window.fail = "rejected"));
    await effort().selectOption("max");
    await page.getByRole("alert").waitFor();
    await value("medium");
    await page.evaluate(() =>
      window.setAgent({ ...window.agent, effort: "max" }),
    );
    await value("max");
  });
  await check("newer-snapshot-handoff", async () => {
    await reset();
    await page.evaluate(
      () => (window.reply = { ...window.agent, effort: "medium" }),
    );
    await effort().selectOption("high");
    await value("medium");
    await page.evaluate(() =>
      window.setAgent({ ...window.agent, effort: "max" }),
    );
    await value("max");
  });
  await check("stale-account-submission", async () => {
    await reset({ agent: { status: "running" } });
    await page.evaluate(() => (window.serverAccount = "second"));
    await effort().selectOption("high");
    await page
      .getByRole("alert")
      .filter({ hasText: "The account changed." })
      .waitFor();
    await value("low");
    assert.equal(
      await page.evaluate(() => window.calls[0].expected_account_key),
      "default",
    );
    assert.equal(
      await page.getByRole("button", { name: "Check settings save" }).count(),
      0,
    );
  });
  await check("legacy-receipt-not-replayed", async () => {
    await reset();
    const legacy = {
      id: "first",
      next_turn: true,
      request_id: "legacy-save",
      model: "gpt-6-astra",
      effort: "high",
      fast_mode: false,
    };
    await page.evaluate((legacy) => {
      localStorage.setItem("next-turn-settings:first", JSON.stringify(legacy));
      window.reset({ agent: { accountKey: "second", effort: "medium" } });
    }, legacy);
    await page
      .getByRole("button", { name: "Main agent settings", exact: true })
      .click();
    await value("medium");
    await page
      .getByText(
        "Previous settings request has no account identity. Check the current settings.",
      )
      .waitFor();
    assert.equal(
      await page.getByRole("button", { name: "Check settings save" }).count(),
      0,
    );
    assert.deepEqual(await page.evaluate(() => window.calls), []);
    assert.deepEqual(
      await page.evaluate(() =>
        JSON.parse(localStorage.getItem("next-turn-settings:first")),
      ),
      legacy,
    );
    await page
      .getByRole("button", { name: "Discard previous settings request" })
      .click();
    assert.equal(
      await page.evaluate(() =>
        JSON.parse(localStorage.getItem("next-turn-settings:first")),
      ),
      null,
    );
  });
  await check("permission-canonical-handoff", async () => {
    await reset();
    await page.getByText("Permissions", { exact: true }).click();
    await page.evaluate(() => {
      window.reply = { ...window.agent, yoloMode: true };
      window.refreshFailure = true;
    });
    const permission = page.getByRole("switch", {
      name: "Full access without approval",
      exact: true,
    });
    await permission.check();
    await page
      .getByRole("alert")
      .filter({ hasText: "Settings saved." })
      .waitFor();
    assert.equal(await permission.isChecked(), true);
    assert.equal(
      await page.evaluate(() => window.calls[0].expected_account_key),
      "default",
    );
    await page.evaluate(() =>
      window.setAgent({ ...window.agent, tail: "Unrelated snapshot" }),
    );
    assert.equal(await permission.isChecked(), true);
    await page.evaluate(() =>
      window.setAgent({ ...window.agent, yoloMode: true }),
    );
    await page.waitForTimeout(50);
    await page.evaluate(() =>
      window.setAgent({ ...window.agent, yoloMode: false }),
    );
    assert.equal(await permission.isChecked(), false);
    await page.evaluate(() => {
      window.reply = { ...window.agent, yoloMode: false };
      window.refreshFailure = false;
    });
    await permission.click();
    await page.waitForFunction(() => window.refreshCount === 2);
    assert.equal(await page.evaluate(() => window.calls[1].yolo_mode), true);
    assert.equal(await permission.isChecked(), false);
  });
  assert.deepEqual(errors, []);
  assert.deepEqual(failures, []);
  console.log(
    `PASS execution settings canonical state (${browserType.name()})`,
  );
} finally {
  await browser?.close();
  await server.close();
  await rm(cacheDir, { recursive: true, force: true });
}
