import assert from "node:assert/strict";
import { createRequire } from "node:module";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
const root = join(import.meta.dirname, "../web");
const require = createRequire(join(root, "package.json"));
const { chromium, webkit } = require("playwright-core");
const { createServer } = await import(require.resolve("vite"));
const browserType = process.env.BROWSER === "webkit" ? webkit : chromium;
const cacheDir = await mkdtemp(join(tmpdir(), "studio-chat-read-"));
const entry = join(root, "chat-read-fixture.tsx");
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
    ],
  },
  plugins: [
    {
      name: "chat-read-fixture",
      configureServer(server) {
        server.middlewares.use("/check", (_req, res) => {
          res.setHeader("Content-Type", "text/html");
          res.end(
            '<div id="root"></div><script type="module" src="/chat-read-fixture.tsx"></script>',
          );
        });
      },
      resolveId(id) {
        if (id === "/chat-read-fixture.tsx") return entry;
      },
      load(id) {
        if (id !== entry) return;
        return `import React,{useState,useRef} from 'react';import{createRoot}from'react-dom/client';import{flushSync}from'react-dom';import{useChatReadState,useVisibleChatResult}from'/src/components/useChatReadState.ts';
window.foreground=false;window.visible=true;Object.defineProperty(document,'visibilityState',{configurable:true,get:()=>window.visible?'visible':'hidden'});document.hasFocus=()=>window.foreground;
const initial={id:'one',threadId:'thread',lastCompletedTurn:'turn',lastCompletedTurnStatus:'completed',source:'managed',status:'completed',name:'One',model:'sol',created:1,readStateSupported:true};
window.calls=[];window.notifications=[];window.refreshes=0;window.hold=false;window.fail=false;window.serverState=JSON.parse(localStorage.getItem('server-read-state')||'null');
const nativeFetch=window.fetch;window.fetch=async(url,options)=>{if(url==='/api/state?view=chat')return new Response(JSON.stringify({stateDir:'/state',threads:[{...window.agent,readState:window.serverState}]}),{headers:{'Content-Type':'application/json'}});if(url!='/api/organization')return nativeFetch(url,options);const body=JSON.parse(options.body);window.calls.push({...body,workspace:options.headers['X-Canvas-Workspace']});if(window.hold)await new Promise(resolve=>window.release=resolve);if(window.fail)return new Response(JSON.stringify({error:'Read state changed'}),{status:409});const value=body.read_state;window.serverState={threadId:value.thread_id,turnId:value.turn_id,read:value.read,revision:value.expected_revision+1};localStorage.setItem('server-read-state',JSON.stringify(window.serverState));return new Response(JSON.stringify({...initial,readState:window.serverState}),{headers:{'Content-Type':'application/json'}})};
function Fixture(){const[agent,setAgent]=useState({...initial,readState:window.serverState}),[opened,setOpened]=useState('one'),[workspace,setWorkspace]=useState('first'),[loaded,setLoaded]=useState(true),[message,setMessage]=useState('turn'),[offset,setOffset]=useState(900),[streaming,setStreaming]=useState(false),[phase,setPhase]=useState("final_answer"),[nodeVersion,setNodeVersion]=useState(0),[turnStatus,setTurnStatus]=useState(undefined),[latestPage,setLatestPage]=useState(true),[resultText,setResultText]=useState("Completed result");const scroll=useRef(null);const data={stateDir:'/state',threads:[agent],runtime:{agents:[agent]}};const controls=useChatReadState(data,opened,text=>window.notifications.push(text),async()=>{window.refreshes++},workspace);window.controls=controls;window.agent=agent;window.state=controls.readStateFor(agent);window.marking=[...controls.marking];window.markUnread=()=>controls.markUnread(agent);window.setAgent=value=>flushSync(()=>setAgent(value));window.setOpened=value=>flushSync(()=>setOpened(value));window.setWorkspace=value=>flushSync(()=>setWorkspace(value));window.setLoaded=value=>flushSync(()=>setLoaded(value));window.setMessage=value=>flushSync(()=>setMessage(value));window.setOffset=value=>flushSync(()=>setOffset(value));window.setStreaming=value=>flushSync(()=>setStreaming(value));window.setPhase=value=>flushSync(()=>setPhase(value));window.replaceResult=()=>flushSync(()=>setNodeVersion(v=>v+1));window.setTurnStatus=value=>flushSync(()=>setTurnStatus(value));window.setLatestPage=value=>flushSync(()=>setLatestPage(value));window.setResultText=value=>flushSync(()=>setResultText(value));window.observe=proof=>controls.observeRead(proof);useVisibleChatResult(scroll,agent,[{id:'result',role:'assistant',turnId:message,phase,streaming,turnStatus,text:resultText}],loaded,controls.observeRead,workspace,latestPage);return <><div id="messages" ref={scroll} style={{height:180,overflow:'auto'}}><div style={{height:offset}}/><section key={nodeVersion} data-turn={message} data-outcome={turnStatus}><article data-message="result" style={{height:80}}>Completed result</article></section></div><button onClick={()=>void controls.markUnread(agent)}>Unread</button></>};const appRoot=createRoot(document.getElementById('root'));window.unmount=()=>appRoot.unmount();appRoot.render(<Fixture/>);`;
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
          executablePath:
            process.env.CHROME_BIN ||
            "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
        }
      : {}),
  });
  const page = await browser.newPage({ viewport: { width: 390, height: 700 } });
  const errors = [];
  page.on("pageerror", (error) => errors.push(error.message));
  const ready = async () => page.waitForFunction(() => !!window.setAgent);
  const settle = () => page.waitForTimeout(120);
  await page.goto(`http://127.0.0.1:${server.httpServer.address().port}/check`);
  await ready();
  await settle();
  assert.equal(await page.evaluate(() => window.calls.length), 0);
  await page.evaluate(() => {
    window.foreground = true;
    window.dispatchEvent(new Event("focus"));
  });
  await settle();
  assert.equal(
    await page.evaluate(() => window.calls.length),
    0,
    "Offscreen result is unread",
  );
  await page.evaluate(() => {
    window.visible = false;
    window.setOffset(0);
    document.dispatchEvent(new Event("visibilitychange"));
  });
  await settle();
  assert.equal(
    await page.evaluate(() => window.calls.length),
    0,
    "Hidden page is unread",
  );
  await page.evaluate(() => {
    window.visible = true;
    window.foreground = false;
    document.dispatchEvent(new Event("visibilitychange"));
  });
  await settle();
  assert.equal(
    await page.evaluate(() => window.calls.length),
    0,
    "Unfocused page is unread",
  );
  await page.evaluate(() => {
    window.foreground = true;
    window.setLoaded(false);
    window.dispatchEvent(new Event("focus"));
  });
  await settle();
  assert.equal(
    await page.evaluate(() => window.calls.length),
    0,
    "Unloaded transcript is unread",
  );
  console.log("PASS offscreen, hidden, unfocused, unloaded");
  await page.evaluate(() => {
    window.setStreaming(true);
    window.setLoaded(true);
  });
  await settle();
  assert.equal(
    await page.evaluate(() => window.calls.length),
    0,
    "Streaming final is not yet a complete result",
  );
  await page.evaluate(() => {
    window.setPhase("commentary");
    window.setStreaming(false);
  });
  await settle();
  assert.equal(
    await page.evaluate(() => window.calls.length),
    0,
    "Visible commentary cannot acknowledge an absent final answer",
  );
  await page.evaluate(() => window.setPhase("final_answer"));
  await page.waitForFunction(() => window.state?.read === true);
  await settle();
  assert.equal(await page.evaluate(() => window.calls.length), 1);
  assert.equal(await page.evaluate(() => window.calls[0].workspace), "first");
  await page.evaluate(() => {
    window.previousControls = window.controls;
    window.setAgent({ ...window.agent, tail: "Old replica update" });
  });
  await settle();
  assert.equal(await page.evaluate(() => window.state.read), true);
  assert.equal(await page.evaluate(() => window.calls.length), 1);
  assert.equal(
    await page.evaluate(
      () =>
        window.previousControls.observeRead === window.controls.observeRead &&
        window.previousControls.markUnread === window.controls.markUnread &&
        window.previousControls.readStateFor === window.controls.readStateFor,
    ),
    true,
    "Unrelated updates preserve callback identities",
  );
  console.log(
    "PASS visible result acknowledged once; canonical handoff and callback identities survive old snapshot",
  );
  await page.evaluate(() => window.markUnread());
  await page.waitForFunction(
    () => window.state?.read === false && window.marking.length === 0,
  );
  await settle();
  assert.equal(await page.evaluate(() => window.calls.length), 2);
  await page.evaluate(() => window.dispatchEvent(new Event("focus")));
  await settle();
  assert.equal(await page.evaluate(() => window.calls.length), 2);
  console.log("PASS explicit unread remains unread while open");
  await page.reload();
  await ready();
  assert.equal(await page.evaluate(() => window.state.read), false);
  assert.equal(await page.evaluate(() => window.calls.length), 0);
  await page.evaluate(() => {
    window.foreground = true;
    window.setOffset(0);
    window.dispatchEvent(new Event("focus"));
  });
  await page.waitForFunction(() => window.state?.read === true);
  console.log("PASS persisted unread reload and reopen");
  await page.evaluate(() =>
    window.setAgent({
      ...window.agent,
      readState: {
        ...window.state,
        read: false,
        revision: window.state.revision + 1,
      },
    }),
  );
  await settle();
  assert.equal(await page.evaluate(() => window.state.read), false);
  assert.equal(
    await page.evaluate(() => window.calls.length),
    1,
    "Another device's Unread does not trigger auto-read on the unchanged visible result",
  );
  await page.evaluate(() =>
    window.setAgent({
      ...window.agent,
      readState: {
        ...window.state,
        read: true,
        revision: window.state.revision + 1,
      },
    }),
  );
  console.log("PASS another device's Unread survives unchanged visible result");
  await page.evaluate(() => {
    window.setAgent({ ...window.agent, lastCompletedTurn: "new-turn" });
    window.observe({ id: "one", threadId: "thread", turnId: "turn" });
  });
  await settle();
  assert.equal(
    await page.evaluate(() => window.calls.length),
    1,
    "Old visible result cannot acknowledge newer result",
  );
  await page.evaluate(() => {
    window.setMessage("new-turn");
    window.hold = true;
    window.setOffset(0);
  });
  await page.waitForFunction(() => window.calls.length === 2);
  await page.evaluate(() => {
    window.foreground = false;
    window.setWorkspace("second");
  });
  await settle();
  const refreshes = await page.evaluate(() => window.refreshes);
  await page.evaluate(() => {
    window.release();
    window.hold = false;
  });
  await settle();
  assert.equal(
    await page.evaluate(() => window.refreshes),
    refreshes,
    "Old workspace response cannot refresh current scope",
  );
  assert.equal(
    await page.evaluate(() => window.state?.turnId),
    "turn",
    "Old workspace response cannot set canonical state",
  );
  console.log("PASS unseen newer tuple and stale workspace response");
  await page.evaluate(() => {
    window.foreground = false;
    window.setAgent({ ...window.agent, readStateSupported: false });
    window.setWorkspace("third");
  });
  await settle();
  const count = await page.evaluate(() => window.calls.length);
  await page.evaluate(() => window.markUnread());
  await settle();
  assert.equal(await page.evaluate(() => window.calls.length), count);
  console.log("PASS unsupported server has no local write fallback");
  await page.evaluate(() => {
    window.setAgent({ ...window.agent, readStateSupported: true });
    window.setWorkspace("fourth");
    window.fail = true;
    window.foreground = true;
    window.dispatchEvent(new Event("focus"));
  });
  await page.waitForFunction(() => window.notifications.length > 0);
  await settle();
  const failed = await page.evaluate(() => window.calls.length);
  await settle();
  assert.equal(
    await page.evaluate(() => window.calls.length),
    failed,
    "A rejected acknowledgement cannot loop",
  );
  console.log("PASS rejected acknowledgement is visible and does not loop");
  await page.evaluate(() => {
    window.foreground = false;
    window.fail = false;
    window.hold = true;
    window.setWorkspace("last");
    void window.markUnread();
  });
  await page.waitForFunction(() => window.marking.length > 0);
  const beforeUnmount = await page.evaluate(() => window.refreshes);
  await page.evaluate(() => {
    window.unmount();
    window.release();
  });
  await settle();
  assert.equal(await page.evaluate(() => window.refreshes), beforeUnmount);
  console.log("PASS unmount ignores in-flight response");
  await page.evaluate(() => localStorage.clear());
  await page.reload();
  await ready();
  await page.evaluate(() => {
    window.hold = true;
    window.foreground = true;
    window.setOffset(0);
    window.dispatchEvent(new Event("focus"));
  });
  await page.waitForFunction(() => window.calls.length === 1);
  await page.evaluate(() => {
    void window.markUnread();
  });
  await settle();
  assert.equal(await page.evaluate(() => window.calls.length), 1);
  await page.evaluate(() => {
    window.hold = false;
    window.release();
  });
  await page.waitForFunction(
    () => window.state?.read === false && window.marking.length === 0,
  );
  await settle();
  assert.equal(await page.evaluate(() => window.calls.length), 2);
  assert.deepEqual(
    await page.evaluate(() => window.calls.map((call) => call.read_state.read)),
    [true, false],
  );
  console.log("PASS explicit unread follows in-flight auto-read exactly once");
  // Earlier history can replace a keyed turn without changing the final message ID.
  await page.evaluate(() => localStorage.clear());
  await page.reload();
  await ready();
  await settle();
  await page.evaluate(() => {
    window.foreground = true;
    window.replaceResult();
  });
  await settle();
  await page.evaluate(() => window.setOffset(0));
  await page.waitForFunction(() => window.state?.read === true, {
    timeout: 2000,
  });
  console.log("PASS replaced turn still acknowledges its visible result");

  await page.evaluate(() => localStorage.clear());
  await page.reload();
  await ready();
  await page.evaluate(() => {
    window.foreground = true;
    window.setPhase("commentary");
    window.setTurnStatus("completed");
    window.setLatestPage(false);
    window.setOffset(0);
  });
  await settle();
  assert.equal(
    await page.evaluate(() => window.calls.length),
    0,
    "Partial history cannot acknowledge an absent final answer",
  );
  await page.evaluate(() => window.setLatestPage(true));
  await page.waitForFunction(() => window.state?.read === true);
  console.log(
    "PASS completed turn without a final answer, only on latest page",
  );

  await page.evaluate(() => localStorage.clear());
  await page.reload();
  await ready();
  await page.evaluate(() => {
    const fetch = window.fetch;
    window.transientFailure = true;
    window.fetch = (url, options) => {
      if (url === "/api/organization" && window.transientFailure) {
        window.transientFailure = false;
        return Promise.reject(new TypeError("Network disconnected"));
      }
      return fetch(url, options);
    };
    window.foreground = true;
    window.setOffset(0);
  });
  await page.waitForFunction(() => window.state?.read === true);
  assert.equal(await page.evaluate(() => window.calls.length), 1);
  console.log("PASS transient read failure recovers without changing chats");

  await page.evaluate(() => localStorage.clear());
  await page.reload();
  await ready();
  await page.evaluate(() => {
    window.foreground = true;
    window.setResultText("");
    window.setTurnStatus("completed");
    window.setOffset(0);
  });
  await page.waitForFunction(() => window.state?.read === true);
  console.log("PASS empty final uses the visible completed turn");

  await page.evaluate(() => localStorage.clear());
  await page.reload();
  await ready();
  await page.evaluate(() => {
    window.foreground = true;
    window.setPhase("commentary");
    window.setTurnStatus("completed");
    document.querySelector("[data-turn]").dataset.outcome = "active";
    window.setOffset(0);
  });
  await settle();
  assert.equal(await page.evaluate(() => window.calls.length), 0);
  await page.evaluate(() => {
    document.querySelector("[data-turn]").dataset.outcome = "completed";
  });
  await page.waitForFunction(() => window.state?.read === true);
  console.log("PASS delayed terminal attribute attaches the observer");

  await page.evaluate(() => localStorage.clear());
  await page.reload();
  await ready();
  await page.evaluate(() => {
    const fetch = window.fetch;
    window.lostWrites = [];
    window.releaseLost = null;
    window.fetch = async (url, options) => {
      if (url !== "/api/organization") return fetch(url, options);
      const body = JSON.parse(options.body);
      if (!body.read_state.read) return fetch(url, options);
      window.lostWrites.push(body);
      // The first request commits. All three replies are lost.
      if (window.lostWrites.length === 1) await fetch(url, options);
      if (window.lostWrites.length === 3)
        await new Promise((resolve) => (window.releaseLost = resolve));
      throw new TypeError("Response lost");
    };
    window.foreground = true;
    window.setOffset(0);
  });
  await page.waitForFunction(() => !!window.releaseLost);
  await page.evaluate(() => {
    void window.markUnread();
    window.releaseLost();
  });
  await page.waitForFunction(
    () => window.marking.length === 0 && window.state?.read === false,
  );
  assert.equal(await page.evaluate(() => window.serverState.read), false);
  assert.deepEqual(
    await page.evaluate(() =>
      window.lostWrites.map((v) => v.read_state.expected_revision),
    ),
    [0, 0, 0],
  );
  assert.deepEqual(
    await page.evaluate(() => window.calls.map((v) => v.read_state.read)),
    [true, false],
  );
  console.log(
    "PASS lost success reconciles before explicit Unread; retries preserve revision",
  );
  assert.deepEqual(errors, []);
} finally {
  await browser?.close();
  await server.close();
  await rm(cacheDir, { recursive: true, force: true });
}
