// Headless production renderer with a large synthetic entity feed.
import { spawn } from "node:child_process";
import { mkdtemp, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { createRequire } from "node:module";
import { gzipSync } from "node:zlib";

const root = dirname(dirname(fileURLToPath(import.meta.url)));
const { chromium } = createRequire(join(root, "web/package.json"))("playwright-core");
const directory = await mkdtemp(join(tmpdir(), "studio-renderer-memory-"));
const fixture = spawn("python3", ["-B", join(root, "tests/simple-ui-fixture.py"), join(directory, "state")],
  { stdio: ["ignore", "pipe", "pipe"] });
let log = "";
fixture.stderr.on("data", (data) => { log += data; });
let browser;
try {
  const port = await new Promise((resolve, reject) => {
    fixture.stdout.once("data", (data) => resolve(Number(String(data).trim())));
    fixture.once("exit", () => reject(new Error(log)));
  });
  const origin = `http://127.0.0.1:${port}`;
  const state = await (await fetch(origin + "/api/state?view=chat")).json();
  const lead = state.threads.find((agent) => agent.name === "Release lead");
  const workspaceId = (await (await fetch(origin + "/api/sync/identity")).json()).workspaceId;
  const base = [];
  let cursor = 0;
  for (;;) {
    const page = await (await fetch(`${origin}/api/sync/pull?scope=state%3Aentities%3Av1&after=${cursor}&limit=500&fresh=1&initialHigh=0`)).json();
    base.push(...page.documents.filter((doc) => !doc._deleted));
    cursor = page.checkpoint.seq;
    if (cursor >= page.maxSeq) break;
  }
  const make = (collection, id, value) => ({
    id: `entity:${collection}:${id}`,
    payload: JSON.stringify({ collection, id, value }),
    _deleted: false,
  });
  const agents = Array.from({ length: 356 }, (_, index) => {
    const id = index === 0 ? lead.id : `perf-worker-${index}`;
    return { id, name: index ? `Perf Worker ${index}` : lead.name,
      rootId: lead.id, isLead: index === 0, source: "managed", kind: "agent",
      status: "completed", created: index, archived: false, projectFolder: null };
  });
  const fullAgents = agents.map((agent) => make("agent", agent.id, {
    ...agent, overview: { task: "x".repeat(1100), result: "y".repeat(1100) },
    lastAnswer: "z".repeat(650), error: "", nativeStatus: { error: "" },
  }));
  const lightAgents = agents.map((agent) => make("agent", agent.id, agent));
  const core = base.filter((doc) => !/^entity:(agent|event|monitor):/.test(doc.id));
  const events = Array.from({ length: 12200 }, (_, index) =>
    make("event", `perf-event-${index}`, { id: `perf-event-${index}`,
      agent: lead.id, kind: "message", status: "delivered", created: index,
      error: "x".repeat(280) }));
  const monitors = Array.from({ length: 3420 }, (_, index) =>
    make("monitor", `perf-monitor-${index}`, { id: `perf-monitor-${index}`,
      agent: lead.id, status: "completed", created: index,
      command: "x".repeat(900) }));
  const cases = [
    { name: "oldVolume", docs: [...core, ...fullAgents, ...events, ...monitors] },
    { name: "bounded", docs: [...core, ...fullAgents, ...events.slice(-200), ...monitors.slice(-100)] },
    { name: "lightAgentsOnly", docs: [...core.filter((doc) => doc.id === "entity:workspace:current"), ...lightAgents] },
  ];
  browser = await chromium.launch({ headless: true,
    executablePath: process.env.CHROME_BIN || "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome" });
  const measurements = { fixture: { eventRows: events.length, monitorRows: monitors.length,
    agentRows: agents.length, coreRows: core.length }, cases: {} };
  for (const testCase of cases) {
    const docs = testCase.docs.map((doc, index) => ({ ...doc, seq: index + 1 }));
    const wire = JSON.stringify(docs);
    const context = await browser.newContext({ viewport: { width: 1280, height: 800 } });
    const page = await context.newPage();
    const errors = [];
    page.on("pageerror", (error) => errors.push(error.message));
    let pages = 0;
    let complete = false;
    await page.route("**/api/sync/pull?*", (route) => {
      const url = new URL(route.request().url());
      if (url.searchParams.get("scope") !== "state:entities:v1") return route.continue();
      const after = Number(url.searchParams.get("after") || 0);
      const limit = Number(url.searchParams.get("limit") || 500);
      const batch = docs.slice(after, after + limit);
      const checkpoint = batch.at(-1)?.seq || docs.length;
      pages++;
      if (checkpoint >= docs.length) complete = true;
      return route.fulfill({ json: { workspaceId, documents: batch,
        checkpoint: { seq: checkpoint }, maxSeq: docs.length,
        initialHigh: docs.length } });
    });
    const cdp = await context.newCDPSession(page);
    await cdp.send("Performance.enable");
    const metric = async (name) => (await cdp.send("Performance.getMetrics")).metrics
      .find((entry) => entry.name === name)?.value;
    const cpuBefore = await metric("TaskDuration");
    const started = Date.now();
    await page.goto(origin, { waitUntil: "domcontentloaded" });
    const deadline = started + 180000;
    while (!complete && Date.now() < deadline)
      await new Promise((resolve) => setTimeout(resolve, 250));
    if (complete) {
      await page.locator("#message").waitFor({ timeout: 15000 });
      await page.waitForTimeout(2000);
    }
    await cdp.send("HeapProfiler.collectGarbage");
    const heap = await cdp.send("Runtime.getHeapUsage");
    const cpuAfter = await metric("TaskDuration");
    const storage = await cdp.send("Storage.getUsageAndQuota", { origin });
    measurements.cases[testCase.name] = {
      documents: docs.length, payloadBytes: Buffer.byteLength(wire),
      payloadGzipBytes: gzipSync(wire, { level: 3 }).length,
      pages, complete, firstLoadMs: Date.now() - started,
      taskCpuSeconds: Number((cpuAfter - cpuBefore).toFixed(3)),
      usedHeapBytes: heap.usedSize,
      indexedDbBytes: storage.usageBreakdown.find((entry) => entry.storageType === "indexeddb")?.usage || 0,
      errors: errors.slice(0, 3),
    };
    await context.close();
  }
  await writeFile(join(directory, "result.json"), JSON.stringify(measurements, null, 2));
  console.log(JSON.stringify({ ...measurements, evidence: join(directory, "result.json") }));
} finally {
  await browser?.close();
  fixture.kill("SIGTERM");
  if (fixture.exitCode === null)
    await new Promise((resolve) => fixture.once("exit", resolve));
}
