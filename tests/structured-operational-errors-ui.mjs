// Structured diagnostic fixtures through real React components. No live API writes.
import assert from "node:assert/strict";
import { createRequire } from "node:module";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, dirname } from "node:path";
import { fileURLToPath } from "node:url";
const root = dirname(dirname(fileURLToPath(import.meta.url)));
const require = createRequire(join(root, "web/package.json"));
const { chromium, webkit } = require("playwright-core");
const { createServer } = await import(require.resolve("vite"));
const browserType = process.env.BROWSER === "webkit" ? webkit : chromium;
const temporary = await mkdtemp(join(tmpdir(), "studio-operational-errors-"));
const harness = `
import React from 'react';
import {createRoot} from 'react-dom/client';
import {MantineProvider} from '@mantine/core';
import '@mantine/core/styles.css';
import Requests from '/src/components/Requests.tsx';
import UserTasks from '/src/components/UserTasks.tsx';
import BackgroundTasks from '/src/components/BackgroundTasks.tsx';
import AgentPanel from '/src/components/AgentPanel.tsx';
import CapacityRetry from '/src/components/CapacityRetry.tsx';
import SafetyBuffering from '/src/components/SafetyBuffering.tsx';
import {Dictation} from '/src/components/Dictation.tsx';
import {saveRecording,listRecordings} from '/src/dictation/storage.ts';
window.readRecordings=()=>listRecordings('lead');
const root=createRoot(document.getElementById('root'));
const agent={id:'lead',rootId:'lead',name:'Lead',epoch:0,accountKey:'default',turnId:'turn',inFlight:true};
const snapshot={stateDir:'fixture',token:'fixture',threads:[agent],chats:[],runtime:{agents:[agent],tasks:[],monitors:[],requests:[]}};
const noop=()=>{};
const refresh=async()=>{};
let renderId=0;
class Boundary extends React.Component {
 state={error:''};
 static getDerivedStateFromError(error){return {error:error.message};}
 componentDidCatch(error){window.crashes.push(error.message);}
 render(){return this.state.error?React.createElement('p',{'data-crash':true},this.state.error):this.props.children;}
}
window.renderCase=async(which,value)=>{
 let component;
 if(which==='request') {
  const request={id:'approval',agent:'lead',method:'item/commandExecution/requestApproval',params:{reason:value}};
  component=React.createElement(Requests,{requests:[request],allRequests:[request],scope:'fixture',agents:[agent],refresh,notify:noop});
 }
 if(which==='task') component=React.createElement(UserTasks,{data:{...snapshot,runtime:{...snapshot.runtime,userTasks:[{id:'user-task',rootId:'lead',agent:'lead',title:'User check',description:'Check the result',criteria:'Pass',reason:value,status:'open',version:1,history:[]}]}},refresh,notify:noop});
 if(which==='background') component=React.createElement(BackgroundTasks,{opened:true,close:noop,data:{...snapshot,runtime:{...snapshot.runtime,tasks:[{id:'tool',agent:'lead',kind:'tool',name:'dynamicToolCall',status:'running',created:1,error:value,stdinError:value}]}},leadId:'lead',openAgent:noop,refresh,notify:noop});
 if(which==='capacity') component=React.createElement(CapacityRetry,{agentId:'lead',retry:{id:'retry',status:'scheduled',dueAt:Date.now()/1000+100,reason:value}});
 if(which==='safety') component=React.createElement(SafetyBuffering,{agent:{...agent,nativeSafetyRetry:{epoch:0,accountKey:'default',turnId:'turn',stage:'failed',error:value}}});
 if(which==='dictation') {
  window.codexDesktop={};
  await saveRecording({id:'recording',chatId:'lead',state:'ready',sampleRate:16000,samples:4,created:Date.now(),error:value});
  component=React.createElement(Dictation,{chatId:'lead',onInsert:noop});
 }
 if(which==='panel') component=React.createElement(AgentPanel,{agentId:'panel-'+(++renderId),version:1,token:'fixture'});
 root.render(React.createElement(MantineProvider,null,React.createElement(Boundary,{key:which==='panel'?'panel':++renderId},component)));
};
window.crashes=[];
`;
const server = await createServer({
  configFile: false,
  root: join(root, "web"),
  cacheDir: join(temporary, "vite"),
  plugins: [
    {
      name: "operational-errors-fixture",
      resolveId(id) {
        if (id === "virtual:operational-errors") return "\0" + id;
      },
      load(id) {
        if (id === "\0virtual:operational-errors") return harness;
      },
    },
  ],
  server: { host: "127.0.0.1", port: 0, hmr: false },
});
await server.listen();
let browser;
try {
  browser = await browserType.launch({
    headless: true,
    executablePath:
      browserType === chromium
        ? process.env.CHROME_BIN ||
          "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome"
        : undefined,
  });
  const page = await browser.newPage({ viewport: { width: 390, height: 844 } });
  // Chromium requires loopback permission for the sandboxed panel fixture.
  if (browserType === chromium)
    await page.context().grantPermissions(["local-network-access"]);
  const writes = [],
    errors = [],
    consoleErrors = [];
  page.on("console", (m) => {
    if (m.type() === "error") consoleErrors.push(m.text());
  });
  page.on("pageerror", (error) => errors.push(error.message));
  await page.route("**/check", (route) =>
    route.fulfill({
      contentType: "text/html",
      body: '<!doctype html><div id="root"></div><script type="module">import "/@id/__x00__virtual:operational-errors";</script>',
    }),
  );
  let panelError = null,
    callbackBody,
    getFailure;
  await page.route("**/api/**", async (route) => {
    if (route.request().method() === "POST") {
      writes.push(route.request().postDataJSON());
      return route.fulfill({
        status: 409,
        json:
          callbackBody === undefined
            ? {
                error: panelError,
                requestId: "callback-envelope",
                details: { transport: "native", attempt: 9 },
              }
            : callbackBody,
      });
    }
    const url = new URL(route.request().url());
    if (url.pathname === "/api/panel" && getFailure !== undefined)
      return route.fulfill({ status: 503, json: getFailure });
    if (url.pathname === "/api/panel")
      return route.fulfill({
        json: {
          agent: url.searchParams.get("agent"),
          version: 1,
          html: '<button data-callback="check">Check action</button>',
          css: "",
          callbacks: [{ id: "check", label: "Check action" }],
        },
      });
    return route.fulfill({
      json: { tasks: [], monitors: [], requests: [], id: "tool" },
    });
  });
  await page.goto(server.resolvedUrls.local[0] + "check");
  await page.waitForFunction(() => !!window.renderCase);
  const value = {
    message: { message: "Structured operation failed" },
    code: "E_FIXTURE",
    data: { retryable: false, attempt: 7 },
  };
  for (const which of [
    "request",
    "task",
    "background",
    "capacity",
    "safety",
    "dictation",
  ]) {
    await page.evaluate(({ which, value }) => window.renderCase(which, value), {
      which,
      value,
    });
    if (which === "dictation")
      await page
        .getByRole("button", { name: "Dictation", exact: true })
        .click();
    if (which === "background")
      await page.locator('[data-task="tool"]').click();
    try {
      await page
        .getByText("Structured operation failed", { exact: true })
        .first()
        .waitFor({ timeout: 5000 });
    } catch (error) {
      console.error(
        which,
        await page.locator("body").innerText(),
        await page.evaluate(() => window.crashes),
      );
      throw error;
    }
    await page
      .getByRole("button", { name: "Error details", exact: true })
      .first()
      .click();
    assert.match(await page.locator("body").innerText(), /E_FIXTURE/);
    assert.match(await page.locator("body").innerText(), /"attempt": 7/);
    assert.deepEqual(
      await page.evaluate(() => window.crashes),
      [],
      which + " retains the component",
    );
    assert.equal(
      writes.length,
      0,
      which + " does not trigger an action to recover",
    );
    assert.equal(
      await page
        .locator("body")
        .innerText()
        .then((text) => text.includes("[object Object]")),
      false,
    );
  }
  await page.evaluate((value) => {
    window.transcriptionCalls = 0;
    window.codexDesktop.prepareTranscription = async () => {
      throw value;
    };
    window.codexDesktop.transcribeAudio = async () => {
      window.transcriptionCalls++;
      return { text: "Must not run" };
    };
  }, value);
  await page
    .getByRole("button", { name: "Retry transcription", exact: true })
    .click();
  const deadline = Date.now() + 5000;
  let persisted;
  while (Date.now() < deadline) {
    persisted = await page.evaluate(() => window.readRecordings());
    if (
      typeof persisted[0]?.error === "string" &&
      persisted[0].error.includes("E_FIXTURE")
    )
      break;
    await new Promise((resolve) => setTimeout(resolve, 20));
  }
  assert.deepEqual(JSON.parse(persisted[0].error), value);
  assert.equal(persisted[0].id, "recording");
  assert.equal(persisted[0].samples, 4);
  assert.equal(await page.evaluate(() => window.transcriptionCalls), 0);
  await page
    .getByText("Structured operation failed", { exact: true })
    .first()
    .waitFor();
  for (const legacy of [
    "Legacy dictation failure",
    "{unparsed diagnostic",
    '\"quoted diagnostic\"',
  ]) {
    await page.evaluate(
      (value) => window.renderCase("dictation", value),
      legacy,
    );
    await page.getByRole("button", { name: "Dictation", exact: true }).click();
    await page.getByText(legacy, { exact: true }).first().waitFor();
  }
  panelError = value;
  await page.evaluate((value) => window.renderCase("panel", value), value);
  try {
    await page
      .locator('.agent-panel[data-ready="true"]')
      .waitFor({ timeout: 8000 });
  } catch (error) {
    console.error(
      "Panel readiness",
      await page.locator("body").innerText(),
      consoleErrors,
      errors,
    );
    throw error;
  }
  const frame = page.frameLocator('iframe[title="Agent panel content"]');
  await frame.getByRole("button", { name: "Check action" }).click();
  await page
    .getByText("Structured operation failed", { exact: true })
    .first()
    .waitFor();
  await page
    .getByRole("button", { name: "Show panel error details", exact: true })
    .click();
  await page.getByRole("dialog").waitFor();
  assert.match(await page.getByRole("dialog").innerText(), /E_FIXTURE/);
  assert.match(await page.getByRole("dialog").innerText(), /callback-envelope/);
  assert.match(
    await page.getByRole("dialog").innerText(),
    /"transport": "native"/,
  );
  assert.match(await page.getByRole("dialog").innerText(), /"attempt": 7/);
  assert.equal(writes.length, 1);
  assert.equal(writes[0].callback, "check");
  assert.equal(
    await page.getByRole("button", { name: "Retry", exact: true }).count(),
    0,
    "Rejected panel callback remains blocked",
  );
  assert.deepEqual(await page.evaluate(() => window.crashes), []);
  panelError = { ...value, code: "E_NEW_SCOPE" };
  await page.evaluate((value) => window.renderCase("panel", value), panelError);
  await page.locator('.agent-panel[data-ready="true"]').waitFor();
  assert.equal(
    await page.getByRole("dialog").count(),
    0,
    "Switch closes earlier panel details",
  );
  await page
    .frameLocator('iframe[title="Agent panel content"]')
    .getByRole("button", { name: "Check action" })
    .click();
  await page
    .getByText("Structured operation failed", { exact: true })
    .first()
    .waitFor();
  assert.equal(
    await page.getByRole("dialog").count(),
    0,
    "A new chat error cannot reopen earlier details",
  );
  assert.equal(writes.length, 2);
  assert.notEqual(writes[0].agent, writes[1].agent);
  assert.notEqual(writes[0].id, writes[1].id);
  for (const body of [
    {
      message: "Message-only callback failure",
      requestId: "message-callback",
      details: { attempt: 10 },
    },
    null,
  ]) {
    callbackBody = body;
    await page.evaluate((value) => window.renderCase("panel", value), body);
    await page.locator('.agent-panel[data-ready="true"]').waitFor();
    await page
      .frameLocator('iframe[title="Agent panel content"]')
      .getByRole("button", { name: "Check action" })
      .click();
    await page
      .getByText(body?.message || "This panel action is no longer available.", {
        exact: true,
      })
      .waitFor();
    assert.equal(
      await page.getByRole("button", { name: "Retry", exact: true }).count(),
      0,
      "HTTP 409 stays rejected for any response shape",
    );
    if (body) {
      await page
        .getByRole("button", { name: "Show panel error details", exact: true })
        .click();
      assert.deepEqual(
        JSON.parse(await page.getByRole("dialog").locator("pre").innerText()),
        body,
      );
      await page
        .getByRole("dialog")
        .getByRole("button", { name: "Close panel error details", exact: true })
        .click();
    }
  }
  const beforeGetFailures = writes.length;
  for (const body of [
    {
      error: { message: "Structured GET failure", code: "GET_CODE" },
      requestId: "get-envelope",
      details: { attempt: 11 },
    },
    {
      message: "Message-only GET failure",
      requestId: "get-message",
      details: { attempt: 12 },
    },
    null,
  ]) {
    getFailure = body;
    await page.evaluate((value) => window.renderCase("panel", value), body);
    await page
      .getByText(
        "Cannot load the agent panel: " +
          (body?.error?.message || body?.message || "Request failed (503)"),
        { exact: false },
      )
      .waitFor();
    assert.equal(
      writes.length,
      beforeGetFailures,
      "Read errors cannot send callback requests",
    );
    if (body) {
      await page
        .getByRole("button", { name: "Show panel error details", exact: true })
        .click();
      assert.deepEqual(
        JSON.parse(await page.getByRole("dialog").locator("pre").innerText()),
        body,
      );
      await page
        .getByRole("dialog")
        .getByRole("button", { name: "Close panel error details", exact: true })
        .click();
    }
  }
  assert.deepEqual(await page.evaluate(() => window.crashes), []);
  assert.deepEqual(errors, []);
  console.log(
    "PASS: structured request/task/retry/safety/dictation diagnostics and rejected panel callback preserve render and manual actions",
  );
} finally {
  await browser?.close();
  await server.close();
  await rm(temporary, { recursive: true, force: true });
}
