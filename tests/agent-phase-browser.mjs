// Component check with an isolated Vite server. No backend or model calls.
import assert from "node:assert/strict";
import { createRequire } from "node:module";
import { mkdtemp } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
const root = join(import.meta.dirname, "../web");
const require = createRequire(join(root, "package.json"));
const { chromium } = require("playwright-core");
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
        return `import React,{useState}from'react';import{createRoot}from'react-dom/client';import{flushSync}from'react-dom';import AgentPhase from'/src/components/AgentPhase.tsx';import ChatStatus from'/src/components/ChatStatus.tsx';import{chatWaitState,chatIndicators}from'/src/components/chatStatusModel.ts';import'/src/style.css';import'/src/workspace-layout.css';
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
    viewport: { width: 1280, height: 900 },
  });
  const errors = [];
  page.on("pageerror", (error) => errors.push(error.message));
  await page.goto(`http://127.0.0.1:${server.httpServer.address().port}/check`);
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
    await phase.getByText("Monitor: watch build 1", { exact: true }).waitFor();
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
  assert.deepEqual(errors, []);
  console.log(
    "PASS AgentPhase: exact wake counts and kinds, names by tap, live animation, reduced motion, static end, completed monitor, stable 1280/390/320px layouts",
  );
  console.log("Evidence:", evidence);
} finally {
  await browser?.close();
  await server.close();
}
