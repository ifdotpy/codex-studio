import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { createRequire } from "node:module";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { modelValue } from "../../model-picker.mjs";
import { test, expect } from "../playwright.mjs";

test("execution settings canonical browser", async ({ page }) => {
  const root = join(import.meta.dirname, "../../../web");
  const require = createRequire(join(root, "package.json"));
  const { createServer } = await import(require.resolve("vite"));
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
            id === join(root, "src/components/agents/ExecutionSettings.tsx")
          )
            return execFileSync(
              "git",
              ["show", "5217bb9:web/src/components/ExecutionSettings.tsx"],
              { cwd: root, encoding: "utf8" },
            );
          if (id !== entry) return;
          return `import React,{useState} from 'react';import {createRoot} from 'react-dom/client';import {flushSync} from 'react-dom';import {MantineProvider} from '@mantine/core';import '@mantine/core/styles.css';import {ExecutionSettings} from '/src/components/agents/ExecutionSettings.tsx';
  const initial={id:'first',accountKey:'default',name:'First',source:'managed',model:'gpt-6-astra',effort:'low',fastMode:false,isLead:true,status:'idle',created:1,yoloMode:false,nextTurnSettingsSupported:true};
  const models=['gpt-6-astra','gpt-5.6-sol','gpt-5.6-luna','gpt-6-luna'].map(model=>({model,supportedReasoningEfforts:['low','medium','high','max'].map(reasoningEffort=>({reasoningEffort})),serviceTiers:[{id:'priority'}]}));
  window.catalogRequests=[];window.calls=[];window.transfers=[];window.transferFail=false;window.reply=null;window.fail=null;window.hold=false;window.refreshFailure=false;window.refreshCount=0;const nativeFetch=window.fetch;window.fetch=async(url,options)=>{if(String(url).startsWith('/api/models?')){window.catalogRequests.push(url);if(window.catalogHold)await new Promise(resolve=>window.catalogRelease=resolve);return new Response(JSON.stringify({data:String(url).includes('account_key=claude')?[{model:'claude-opus-4-6',provider:'claude',displayName:'Claude Opus 4.6',isDefault:true,supportedReasoningEfforts:[{reasoningEffort:'high'}]}]:models}),{headers:{'Content-Type':'application/json'}})}if(url=='/api/agents/account-transfer'){const body=JSON.parse(options.body);window.transfers.push(body);if(window.transferFail)throw new TypeError('Connection lost');return new Response(JSON.stringify({id:body.request_id,status:'completed',targetAccountKey:body.account_key,completed:2,moved:2,total:3,leftOnSource:[{id:'w-claude',name:'Claude worker',provider:'claude',reason:'Uses claude'}]}),{headers:{'Content-Type':'application/json'}})}if(url!='/api/conversation')return nativeFetch(url,options);window.calls.push(JSON.parse(options.body));if(window.serverAccount && window.calls.at(-1).expected_account_key!==window.serverAccount)return new Response(JSON.stringify({error:'The account changed. Select the model again'}),{status:409});if(window.hold)await new Promise(resolve=>window.release=resolve);if(window.fail==='network')throw new TypeError('Connection lost');return new Response(JSON.stringify(window.fail?{error:'Rejected'}:window.reply),{status:window.fail?409:200,headers:{'Content-Type':'application/json'}})};
  function Fixture(){const[agent,setAgent]=useState(initial),[mount,setMount]=useState(0),[defaults,setDefaults]=useState(false);window.setAgent=value=>flushSync(()=>setAgent(value));window.agent=agent;window.reset=options=>flushSync(()=>{setAgent({...initial,...options?.agent});setDefaults(!!options?.defaults);setMount(value=>value+1);window.catalogRequests=[];window.calls=[];window.transfers=[];window.transferFail=false;window.catalogHold=false;window.reply=null;window.fail=null;window.hold=false;window.refreshFailure=false;window.refreshCount=0;window.serverAccount=null;});return <MantineProvider><ExecutionSettings key={mount} agent={agent} teamDefaults={defaults} accounts={[{id:'default',email:'first@example.com',status:'ready'},{id:'second',email:'second@example.com',status:'ready'},{id:'claude',label:'Claude',provider:'claude',status:'ready'},{id:'offline',label:'Offline',status:'error'}]} team={[{id:'w1',name:'Codex worker',rootId:'first',provider:'codex',accountKey:'default'},{id:'w2',name:'Second codex worker',rootId:'first',provider:'codex',accountKey:'default'},{id:'w3',name:'Claude worker',rootId:'first',provider:'claude',accountKey:'claude'}]} catalog={{models,loading:false,error:'',retry:()=>{}}} refresh={async()=>{window.refreshCount++;if(window.refreshFailure)throw new Error('Snapshot unavailable')}}/></MantineProvider>}createRoot(document.getElementById('root')).render(<Fixture/>);`;
        },
      },
    ],
  });
  const failures = [];
  try {
    await server.listen();
    await page.setViewportSize({ width: 390, height: 700 });
    const errors = [];
    page.on("pageerror", (error) => {
      errors.push(error.message);
      console.error(error.message);
    });
    await page.goto(
      `http://127.0.0.1:${server.httpServer.address().port}/check`,
    );
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
              (defaults
                ? "Default subagent reasoning"
                : "Main agent reasoning"),
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
        console.error(
          JSON.stringify(
            await page.evaluate(() => ({
              calls: window.calls,
              transfers: window.transfers,
              refreshCount: window.refreshCount,
              catalogRequests: window.catalogRequests,
              text: document.querySelector('[role="dialog"]')?.innerText,
            })),
          ),
        );
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
    await check("subagent-account-instant-transfer", async () => {
      await reset({
        defaults: true,
        agent: {
          workerDefaults: {
            model: "gpt-5.6-luna",
            effort: "high",
            fastMode: false,
          },
        },
      });
      const account = page.getByLabel("Subagent account", { exact: true });
      const model = page.getByLabel("Default subagent model", { exact: true });
      const dialog = page.getByRole("dialog", { name: "Subagent defaults" });
      assert.equal(await account.inputValue(), "");
      assert.equal(await account.locator('option[value="offline"]').count(), 0);
      assert.equal(
        await dialog.getByRole("button", { name: /Save/ }).count(),
        0,
      );
      // The catalog of the target account is slow. The choice still applies at once.
      await page.evaluate(() => {
        window.catalogHold = true;
        window.reply = {
          ...window.agent,
          workerDefaults: {
            accountKey: "claude",
            model: "claude-opus-4-6",
            effort: null,
            fastMode: false,
          },
        };
      });
      await account.selectOption("claude");
      assert.equal(await account.inputValue(), "claude", "optimistic account");
      await dialog
        .getByRole("status")
        .filter({
          hasText:
            "Moving 1 subagent to Claude. 2 subagents stay on codex: Codex worker, Second codex worker.",
        })
        .waitFor();
      await dialog
        .getByRole("status")
        .filter({ hasText: /^Saved/ })
        .waitFor();
      assert.deepEqual(
        await page.evaluate(() =>
          window.transfers.map((row) => [row.account_key, row.scope]),
        ),
        [["claude", "subagents"]],
      );
      assert.equal(
        await page.evaluate(() => window.calls.length),
        0,
        "no settings save before the catalog",
      );
      await page.evaluate(() =>
        window.setAgent({
          ...window.agent,
          workerDefaults: {
            accountKey: "claude",
            model: "gpt-5.6-luna",
            effort: "high",
            fastMode: false,
          },
          accountTransfer: {
            id: window.transfers[0].request_id,
            scope: "subagents",
            status: "completed",
            targetAccountKey: "claude",
            completed: 2,
            moved: 2,
            total: 3,
            leftOnSource: [
              {
                id: "w-claude",
                name: "Claude worker",
                provider: "claude",
                reason: "Uses claude",
              },
            ],
          },
        }),
      );
      await dialog
        .getByText("Claude worker left on source (claude): Uses claude")
        .waitFor();
      assert.equal(
        await modelValue(model),
        "gpt-5.6-luna",
        "current value shows while the catalog loads",
      );
      assert.equal(
        await model.isDisabled(),
        false,
        "the catalog never blocks the control",
      );
      await page.waitForFunction(() => !!window.catalogRelease);
      await page.evaluate(() => window.catalogRelease());
      // The stored model is not in the target catalog: the account default replaces it, in one line.
      await dialog
        .getByRole("status")
        .filter({
          hasText:
            "Saved · Luna is not available on Claude. Using Claude Opus 4.6, the account default.",
        })
        .waitFor();
      assert.deepEqual(
        await page.evaluate(() => window.calls[0].worker_defaults),
        {
          account_key: "claude",
          model: "claude-opus-4-6",
          effort: null,
          fast_mode: false,
          daybreak_enabled: false,
        },
      );
      // One background refresh after the transfer, one after the model save.
      await page.waitForFunction(() => window.refreshCount === 2);
      await page.evaluate(() =>
        window.setAgent({
          ...window.agent,
          workerDefaults: {
            accountKey: "claude",
            model: "claude-opus-4-6",
            effort: null,
            fastMode: false,
          },
        }),
      );
      assert.equal(await modelValue(model), "claude-opus-4-6");
      // A lost transfer response reverts the account; Retry reuses the request id.
      await page.evaluate(() => (window.transferFail = true));
      await account.selectOption("second");
      await page
        .getByRole("alert")
        .filter({ hasText: "Connection lost" })
        .waitFor();
      assert.equal(await account.inputValue(), "claude", "revert on failure");
      await page.evaluate(() => (window.transferFail = false));
      await dialog.getByRole("button", { name: "Retry", exact: true }).click();
      await dialog
        .getByRole("status")
        .filter({ hasText: /^Saved/ })
        .waitFor();
      const ids = await page.evaluate(() =>
        window.transfers.map((row) => row.request_id),
      );
      assert.equal(ids.length, 3);
      assert.equal(ids[1], ids[2], "retry keeps the request id");
      assert.notEqual(ids[0], ids[1]);
      assert.equal(await account.inputValue(), "second");
      // Automatic saves the defaults without a transfer.
      await page.evaluate(() => {
        window.reply = {
          ...window.agent,
          workerDefaults: {
            accountKey: null,
            model: "gpt-5.6-luna",
            effort: "high",
            fastMode: false,
          },
        };
      });
      await account.selectOption("");
      await page.waitForFunction(() => window.calls.length === 2);
      assert.equal(
        await page.evaluate(() => window.calls[1].worker_defaults.account_key),
        null,
      );
      assert.equal(await page.evaluate(() => window.transfers.length), 3);
      await reset();
      assert.equal(await account.count(), 0);
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
    await check("review-defaults", async () => {
      await reset({ defaults: true });
      const reviewModel = page.locator("#review-model");
      await page.evaluate(() => {
        window.reply = {
          ...window.agent,
          reviewDefaults: { model: "gpt-5.6-luna", effort: null },
        };
      });
      await reviewModel.click();
      await page
        .locator('[role="option"][data-value="gpt-5.6-luna"]:visible')
        .click();
      await page.waitForFunction(() => window.calls.length === 1);
      assert.deepEqual(
        await page.evaluate(() => window.calls[0].review_defaults),
        {
          model: "gpt-5.6-luna",
          effort: null,
        },
      );
      assert.equal(await modelValue(reviewModel), "gpt-5.6-luna");
      await page.evaluate(() => {
        window.setAgent({
          ...window.agent,
          reviewDefaults: { model: "gpt-5.6-luna", effort: null },
        });
        window.reply = {
          ...window.agent,
          reviewDefaults: { model: "gpt-5.6-luna", effort: "high" },
        };
      });
      await page
        .getByLabel("Default review reasoning", { exact: true })
        .selectOption("high");
      await page.waitForFunction(() => window.calls.length === 2);
      assert.deepEqual(
        await page.evaluate(() => window.calls[1].review_defaults),
        {
          model: "gpt-5.6-luna",
          effort: "high",
        },
      );
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
        localStorage.setItem(
          "next-turn-settings:first",
          JSON.stringify(legacy),
        );
        window.reset({ agent: { accountKey: "second", effort: "medium" } });
      }, legacy);
      await page
        .getByRole("button", { name: "Main agent settings", exact: true })
        .click();
      await value("medium");
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
      assert.equal(
        await page
          .getByRole("button", { name: "Discard previous settings request" })
          .count(),
        0,
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
    expect(errors).toEqual([]);
    assert.deepEqual(failures, []);
    console.log(
      `PASS execution settings canonical state (${process.env.BROWSER ?? "chromium"})`,
    );
  } finally {
    await server.close();
    await rm(cacheDir, { recursive: true, force: true });
  }
});
