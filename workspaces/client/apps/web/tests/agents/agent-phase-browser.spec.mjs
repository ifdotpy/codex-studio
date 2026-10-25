// Component check with an isolated Vite server. No backend or model calls.
import assert from "node:assert/strict";
import { createRequire } from "node:module";
import { mkdtemp } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test, expect, createFixtureViteServer } from "../playwright.mjs";

test("chat reconnect follows transport across hidden conversations", async ({
  page,
}) => {
  test.setTimeout(60_000);
  const root = join(import.meta.dirname, "../../");
  const entry = join(root, "chat-connection-fixture.tsx");
  const server = await createFixtureViteServer({
    configFile: false,
    root,
    server: { host: "127.0.0.1", port: 0 },
    plugins: [
      {
        name: "chat-connection-fixture",
        configureServer(server) {
          server.middlewares.use("/check", (_request, response) => {
            response.setHeader("Content-Type", "text/html");
            response.end(
              '<div id="root"></div><script type="module" src="/chat-connection-fixture.tsx"></script>',
            );
          });
        },
        resolveId(id) {
          if (id === "/chat-connection-fixture.tsx") return entry;
        },
        load(id) {
          if (id !== entry) return;
          return `
import React, {useState} from 'react';
import {createRoot} from 'react-dom/client';
import {useMessages} from '/src/hooks.ts';
import AgentPhase from '/src/components/agents/AgentPhase.tsx';
import {ServerServicesProvider} from '/src/servers/ServerServicesContext.tsx';
import {createSubscriptions} from '/src/sync/subscriptions/module.ts';
import {createResourceEvents} from '/src/sync/resourceEvents.ts';
import {API_SCHEMA_HASH} from '/src/generated/apiSchema.ts';
import {SYNC_PROTOCOL_VERSION} from '/src/generated/api.ts';
import {SYNC_TRANSCRIPT_INITIAL_TAIL_ITEMS} from '/src/generated/sharedValues.ts';
const refs = Object.fromEntries(['first','second'].map(agentId => [agentId, {kind:'transcript',agentId,tail:SYNC_TRANSCRIPT_INITIAL_TAIL_ITEMS}]));
const payloads = Object.fromEntries(['first','second'].map(id => [id, {items:[],agent:{id,status:'running',activity:{phase:'writing'}}}]));
window.reads = [];
window.renders = {first:0,second:0};
function scope(ownerId, hold = false) {
  const connections = new Set(), sources = [];
  let revision = 0, retryReady;
  class Source {
    listeners = new Map(); closed = false;
    constructor(url) { this.resources = JSON.parse(new URL(url,location.origin).searchParams.get('resources')); }
    addEventListener(name,accept) { this.listeners.set(name,accept); }
    emit(name,value) { this.listeners.get(name)?.({data:JSON.stringify(value)}); }
    close() { this.closed = true; }
    baseline() { this.emit('resources',{protocol:SYNC_PROTOCOL_VERSION,workspaceId:ownerId,epoch:'fixture-epoch',revision:++revision,reason:'initial',resources:this.resources,resourceVersions:this.resources.map(resource=>({resource,revision,cursor:{workspaceId:ownerId,seq:revision},replyKind:'delta'}))}); }
  }
  const transport = createResourceEvents({
    api: {
      get: (...args) => { window.reads.push(args); throw new Error('Unexpected transport read'); },
      post: async (_path,body) => {
        const source = sources.at(-1);
        source.resources = [...source.resources.filter(ref => !body.remove.some(removed => JSON.stringify(removed)===JSON.stringify(ref))), ...body.add.map(item=>item.resource)];
        queueMicrotask(()=>{source.emit('subscriptions',{gen:body.gen}); if(!hold) source.baseline();});
        return {gen:body.gen};
      },
      isApiSchemaMismatch:()=>false,onApiSchemaMismatch:()=>()=>{},matchingApiSchemaResponseGeneration:()=>0,clearSchemaUpdateAttemptAfterMatch:()=>{},markApiSchemaMismatch:()=>{throw new Error('Unexpected schema mismatch');},
    },
    sync:{syncDatabase:async()=>({workspaceId:ownerId}),persistResourceEntityChanges:async()=>{},readSubscriptionCursor:async()=>undefined},
    tokenRates:{configureTokenRateStream:()=>{},receiveResourceTokenRates:()=>true,getWorkspaceTokenRates:()=>({records:[]})},
    storageName:name=>ownerId+':'+name,
    openEvents: url => {
      const source = new Source(url); sources.push(source);
      queueMicrotask(()=>{source.onopen?.(); source.emit('api-schema',{hash:API_SCHEMA_HASH}); source.emit('session',{sessionId:ownerId+sources.length}); if(!hold)source.baseline(); retryReady?.(); retryReady=undefined;});
      return source;
    },
  });
  const resources = createSubscriptions({...transport,watchResourceConnection:accept=>{connections.add(accept);const stop=transport.watchResourceConnection(accept);return()=>{connections.delete(accept);stop();};}}, (ref,accept)=>{accept(payloads[ref.agentId]);return()=>{};});
  return {
    resources,
    connections,
    services:{owner:{id:ownerId,origin:location.origin},resources,api:{syncGet:(...args)=>{window.reads.push(args);throw new Error('Unexpected read');}},transcripts:{peekTranscript:()=>undefined}},
    setConnection(value) {
      if(value==='offline'){hold=true;sources.at(-1).onerror?.();}
      else if(value==='connecting'){if(sources.at(-1).closed)return new Promise(resolve=>{retryReady=resolve;});}
      else if(value==='live'){hold=false;sources.at(-1).baseline();}
    },
  };
}
const firstScope = scope('fixture'), otherScope = scope('other-server',true);
const services = firstScope.services, otherServices = otherScope.services;
window.setConnection = value => firstScope.setConnection(value);
window.connectionListeners = () => firstScope.connections.size;
window.repeatLive = () => {for(const accept of firstScope.connections) accept('live','open');};
window.phase = (id,phase) => {payloads[id]={...payloads[id],agent:{...payloads[id].agent,activity:{phase}}}; firstScope.resources.publish(refs[id],payloads[id]);};
function Chat({id,visible}) {
  window.renders[id]++;
  const messages = useMessages(id,'agent',true,'fixture','fixture',visible);
  const agent = {id,source:'managed',status:'running',nativeStatus:{phase:'retrying',message:'Provider is reconnecting'},...messages.liveAgent};
  return <section hidden={!visible} data-chat={id} data-connection={messages.connection} data-loaded={messages.loaded}><AgentPhase agent={agent} connection={messages.connection}/></section>;
}
function Harness() { const [selected,select] = useState('first'); const [other,selectServer] = useState(false); window.selectChat=select; window.selectServer=selectServer; return <ServerServicesProvider services={other ? otherServices : services}>{['first','second'].map(id=><Chat key={id} id={id} visible={id===selected}/>)}</ServerServicesProvider>; }
createRoot(document.getElementById('root')).render(<Harness/>);`;
        },
      },
    ],
  });
  const requests = [],
    errors = [];
  page.on("pageerror", (error) => errors.push(error.message));
  await page.route("**/api/**", (route) => {
    requests.push(route.request().url());
    return route.abort();
  });
  try {
    await server.listen();
    await page.goto(
      `http://127.0.0.1:${server.httpServer.address().port}/check`,
    );
    const first = page.locator('[data-chat="first"]');
    const second = page.locator('[data-chat="second"]');
    await expect(first).toHaveAttribute("data-loaded", "true");
    await expect(first).toHaveAttribute("data-connection", "live");
    await expect(first).toContainText("Writing");
    await expect(second).toHaveAttribute("data-connection", "");
    await page.evaluate(() => window.setConnection("offline"));
    await expect(first).toContainText("Connection lost. Reconnecting…");
    await page.evaluate(() => window.setConnection("connecting"));
    await expect(first).toContainText("Connection lost. Reconnecting…");
    await page.evaluate(() => window.selectChat("second"));
    await expect(second).toContainText("Connection lost. Reconnecting…");
    await expect(first).toHaveAttribute("data-connection", "");
    await page.evaluate(() => window.selectChat("first"));
    await expect(first).toContainText("Connection lost. Reconnecting…");
    await page.evaluate(() => window.selectChat("second"));
    await page.evaluate(() => window.setConnection("connecting"));
    await expect(second).toContainText("Connection lost. Reconnecting…");
    await page.evaluate(() => window.selectServer(true));
    await expect(second).toHaveAttribute("data-connection", "");
    await expect(second).not.toContainText("Connection lost.");
    await page.evaluate(() => window.selectServer(false));
    await expect(second).toContainText("Connection lost. Reconnecting…");
    await page.evaluate(() => window.setConnection("live"));
    await expect(second).toHaveAttribute("data-connection", "live");
    await expect(second).toContainText("Writing");
    await page.evaluate(() => window.selectChat("first"));
    await expect(first).toHaveAttribute("data-connection", "live");
    await expect(first).toContainText("Writing");
    await page.evaluate(() => window.phase("first", "retrying"));
    await expect(first).toContainText("Provider is reconnecting");
    await expect(first).not.toContainText("Connection lost.");
    await page.evaluate(() => window.phase("first", "writing"));
    await expect(first).toContainText("Writing");
    await expect(first).not.toContainText("Provider is reconnecting");
    expect(await page.evaluate(() => window.connectionListeners())).toBe(2);
    const beforeRepeatedStatus = await page.evaluate(() => window.renders);
    await page.evaluate(async () => {
      for (let repeated = 0; repeated < 20; repeated++) window.repeatLive();
      await new Promise((resolve) =>
        requestAnimationFrame(() => requestAnimationFrame(resolve)),
      );
    });
    expect(await page.evaluate(() => window.renders)).toEqual(
      beforeRepeatedStatus,
    );
    expect(await page.evaluate(() => window.connectionListeners())).toBe(2);
    expect(await page.evaluate(() => window.reads)).toEqual([]);
    expect(requests).toEqual([]);
    expect(errors).toEqual([]);
  } finally {
    await server.close();
  }
});

test("agent phase browser", async ({ page: runnerPage }) => {
  const root = join(import.meta.dirname, "../../");
  const require = createRequire(join(root, "package.json"));
  const { createServer } = await import(require.resolve("vite"));
  const evidence = await mkdtemp(join(tmpdir(), "studio-agent-phase-"));
  const entry = join(root, "agent-phase-fixture.tsx");
  const server = await createServer({
    configFile: false,
    root,
    cacheDir: join(evidence, "cache"),
    server: { host: "127.0.0.1", port: 0 },
    optimizeDeps: {
      noDiscovery: true,
      include: [
        "react",
        "react/jsx-runtime",
        "react/jsx-dev-runtime",
        "react-dom/client",
        "react-dom",
      ],
    },
    plugins: [
      {
        name: "agent-phase-fixture",
        configureServer(server) {
          server.middlewares.use("/check", (_req, res) => {
            res.setHeader("Content-Type", "text/html");
            res.end(
              '<div id="root"></div><script type="module" src="/agent-phase-fixture.tsx"></script>',
            );
          });
        },
        resolveId(id) {
          if (id === "/agent-phase-fixture.tsx") return entry;
        },
        load(id) {
          if (id !== entry) return;
          return `import React,{useState}from'react';import{createRoot}from'react-dom/client';import{flushSync}from'react-dom';import AgentPhase from'/src/components/agents/AgentPhase.tsx';import ChatStatus from'/src/components/agents/ChatStatus.tsx';import{chatWaitState,chatIndicators}from'/src/components/chat-status/chatStatusModel.ts';import'/src/style.css';import'/src/workspace-layout.css';
  const lead={id:'lead',name:'Main',rootId:'lead',source:'managed',status:'waiting',inFlight:false,epoch:2};
  window.base=lead;window.worker=i=>({...lead,id:'child-'+i,name:'Worker '+i,parentId:'lead',status:'running',inFlight:true});
  window.command=i=>({id:'cmd-'+i,agent:'lead',kind:'command',command:'npm run check '+i,status:'running',epoch:2});
  window.monitor=i=>({id:'monitor-'+i,agent:'lead',command:'watch build '+i,status:'running',epoch:2});
  const initial={threads:[lead,window.worker(1),window.worker(2)],runtime:{requests:[],tasks:[],monitors:[]}};
  function Fixture(){const[data,setData]=useState(initial);window.setData=value=>flushSync(()=>setData(value));const agent=data.threads[0];const wait=chatWaitState(data,agent);const indicator=chatIndicators(data).get(agent.id);return <main style={{width:'100%',padding:16,marginTop:260}}><div id="indicator"><ChatStatus status={indicator}/></div><AgentPhase agent={agent} connection="connected" wait={wait}/><div id="anchor">Next content</div></main>};createRoot(document.getElementById('root')).render(<Fixture/>);`;
        },
      },
    ],
  });
  try {
    await server.listen();
    const page = runnerPage;
    await page.setViewportSize({ width: 1280, height: 900 });
    const errors = [];
    page.on("pageerror", (error) => errors.push(error.message));
    await page.goto(
      `http://127.0.0.1:${server.httpServer.address().port}/check`,
    );
    const phase = page.locator(".agent-phase");
    const summary = phase.locator("summary");
    await summary.getByText("Waiting for 2 agents", { exact: true }).waitFor();
    assert.equal(await phase.getAttribute("data-wait-state"), "live");
    const animation = () =>
      phase
        .locator(".phase-dots i")
        .evaluateAll((nodes) =>
          nodes.map((node) => getComputedStyle(node).animationName),
        );
    assert.deepEqual(await animation(), [
      "phase-pulse",
      "phase-pulse",
      "phase-pulse",
    ]);
    assert.equal(
      await page.locator("#indicator .chat-status").getAttribute("aria-label"),
      "Waiting for 2 agents",
    );
    await summary.click();
    await phase.getByText("Agent: Worker 1", { exact: true }).waitFor();
    await summary.click();
    const change = async (kind, count = 1) => {
      await page.evaluate(
        ({ kind, count }) => {
          const runtime = { tasks: [], monitors: [], requests: [] };
          const lead = { ...window.base };
          let threads = [lead];
          if (kind === "agent")
            threads.push(
              ...Array.from({ length: count }, (_, i) => window.worker(i)),
            );
          if (kind === "command")
            runtime.tasks = Array.from({ length: count }, (_, i) =>
              window.command(i),
            );
          if (kind === "monitor")
            runtime.monitors = Array.from({ length: count }, (_, i) =>
              window.monitor(i),
            );
          if (kind === "event")
            Object.assign(lead, {
              status: "parked",
              parkedEvent: "build-ready",
              autoWake: false,
            });
          if (kind === "input")
            runtime.requests = Array.from({ length: count }, (_, i) => ({
              id: "input-" + i,
              agent: "lead",
              status: "pending",
              epoch: 2,
            }));
          if (kind === "mixed") {
            threads.push(window.worker(1), window.worker(2));
            runtime.tasks = [window.command(1)];
            runtime.monitors = [window.monitor(1)];
            lead.parkedEvent = "very-long-event-name-".repeat(10);
            runtime.requests = [
              { id: "input", agent: "lead", status: "pending", epoch: 2 },
            ];
          }
          if (kind === "completed") lead.status = "completed";
          if (kind === "completed-monitor") {
            lead.status = "completed";
            runtime.monitors = [window.monitor(1)];
          }
          window.setData({ threads, runtime });
        },
        { kind, count },
      );
    };
    for (const kind of ["agent", "command", "monitor", "input"]) {
      await change(kind);
      assert.equal(await summary.textContent(), `Waiting for 1 ${kind}`);
      await change(kind, 2);
      assert.equal(await summary.textContent(), `Waiting for 2 ${kind}s`);
    }
    await change("event");
    assert.equal(await summary.textContent(), "Waiting for event build-ready");
    assert.equal(
      await page
        .locator("#indicator .chat-status")
        .getAttribute("data-chat-status"),
      "working",
    );
    await change("completed-monitor");
    assert.equal(await summary.textContent(), "Waiting for 1 monitor");
    for (const width of [1280, 390, 320]) {
      await page.setViewportSize({ width, height: 900 });
      await change("agent", 2);
      const box = await phase.boundingBox();
      const anchor = await page.locator("#anchor").boundingBox();
      await change("mixed");
      assert.equal((await phase.boundingBox()).height, box.height);
      assert.equal((await page.locator("#anchor").boundingBox()).y, anchor.y);
      assert(
        await page.evaluate(
          () => document.documentElement.scrollWidth <= innerWidth,
        ),
      );
      await summary.click();
      await phase
        .getByText("Command: npm run check 1", { exact: true })
        .waitFor();
      await phase
        .getByText("Monitor: watch build 1", { exact: true })
        .waitFor();
      assert(
        await phase
          .locator(".agent-phase-wait-details")
          .evaluate((node) => node.scrollWidth <= node.clientWidth + 1),
      );
      assert.equal(
        (await page.locator("#anchor").boundingBox()).y,
        anchor.y,
        "Open details cannot move the transcript",
      );
      await page.screenshot({ path: join(evidence, `waiting-${width}.png`) });
      await summary.click();
      await change("agent", 99);
      assert.equal((await phase.boundingBox()).height, box.height);
      assert.equal((await page.locator("#anchor").boundingBox()).y, anchor.y);
    }
    await page.emulateMedia({ reducedMotion: "reduce" });
    assert.deepEqual(await animation(), ["none", "none", "none"]);
    assert.equal(
      await page
        .locator("#indicator .chat-status-working svg")
        .evaluate((node) => getComputedStyle(node).animationName),
      "none",
    );
    await page.emulateMedia({ reducedMotion: "no-preference" });
    await change("none");
    assert.equal(await phase.getAttribute("data-wait-state"), "ended");
    assert.equal(
      await phase.innerText(),
      "Turn ended. Send a message to continue.",
    );
    assert.equal(await phase.locator(".phase-dots").count(), 0);
    assert.equal(await phase.getAttribute("class"), "agent-phase ");
    assert.equal(await page.locator("#indicator .chat-status").count(), 0);
    assert.deepEqual(
      await phase.evaluate((node) =>
        node
          .getAnimations({ subtree: true })
          .map((animation) => animation.animationName),
      ),
      [],
    );
    await page.screenshot({ path: join(evidence, "turn-ended-320.png") });
    await change("completed");
    assert.equal(await phase.count(), 0);
    expect(errors).toEqual([]);
    console.log(
      "PASS AgentPhase: exact wake counts and kinds, names by tap, live animation, reduced motion, static end, completed monitor, stable 1280/390/320px layouts",
    );
    console.log("Evidence:", evidence);
  } finally {
    await server.close();
  }
});
