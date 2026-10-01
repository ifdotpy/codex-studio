#!/usr/bin/env node
// Eight real App pages, one isolated production Runtime, and synthetic turns.
import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { spawn } from "node:child_process";
import { mkdtemp, mkdir, readFile, writeFile } from "node:fs/promises";
import { homedir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { createRequire } from "node:module";
import { execFileSync } from "node:child_process";

const harness = dirname(fileURLToPath(import.meta.url));
const repo = resolve(harness, "../../..");
const harnessFiles = [
  "README.md",
  "run.mjs",
  "server.py",
  "test_runtime_load.py",
];
const harnessSourceSha256 = createHash("sha256")
  .update(
    await Promise.all(
      harnessFiles.map((name) => readFile(join(harness, name))),
    ).then((values) => Buffer.concat(values)),
  )
  .digest("hex");
const args = process.argv.slice(2);
const check = args.includes("--check");
const evidenceRoot = join(
  process.env.XDG_STATE_HOME || join(homedir(), ".local", "state"),
  "evidence",
  "latency-components",
);
await mkdir(evidenceRoot, { recursive: true });
const roundsAt = args.indexOf("--rounds");
const rounds = roundsAt >= 0 ? Number(args[roundsAt + 1]) : 1;
const outputAt = args.indexOf("--output");
const outputPath =
  outputAt >= 0
    ? resolve(args[outputAt + 1])
    : join(
        evidenceRoot,
        check ? "runtime-load-check.json" : "runtime-load-256.json",
      );
const sourceAt = args.indexOf("--source-root");
const sourceRoot = sourceAt >= 0 ? resolve(args[sourceAt + 1]) : repo;
const productionSourceDirty = Boolean(
  execFileSync(
    "git",
    ["-C", sourceRoot, "status", "--porcelain", "--untracked-files=no"],
    { encoding: "utf8" },
  ).trim(),
);
const deadlineMs = check ? 90_000 : 180_000;
assert(
  Number.isInteger(rounds) && rounds >= 1 && rounds <= 8,
  "--rounds must be 1..8",
);
if (outputPath && outputPath.startsWith(repo + "/"))
  throw new Error("--output must be outside the checkout");
const packageRequire = createRequire(join(sourceRoot, "web/package.json"));
const { chromium } = packageRequire("playwright-core");
const state = await mkdtemp(join(evidenceRoot, ".runtime-load-case-"));
const fixture = spawn(
  "python3",
  ["-B", join(harness, "server.py"), state, sourceRoot],
  {
    cwd: sourceRoot,
    env: {
      ...process.env,
      ...(check ? { BENCH_QUICK_CHECK: "1" } : {}),
      BENCH_SOURCE_REVISION: execFileSync(
        "git",
        ["-C", sourceRoot, "rev-parse", "HEAD"],
        { encoding: "utf8" },
      ).trim(),
    },
    stdio: ["pipe", "pipe", "pipe"],
  },
);
let childOut = "",
  childErr = "",
  browser,
  browserServer,
  browserResourceTimer,
  browserResourcePeak = { processCount: 0, rssKiB: 0, cpuPercent: 0 },
  contexts = [],
  pages = [];
let phase = "fixture-startup";
let pageErrors = [];
let ready;
fixture.stderr.on("data", (chunk) => {
  childErr += chunk;
});
fixture.stdout.on("data", (chunk) => {
  childOut += chunk;
});
const start = Date.now();
let deadlineExpired = false;
const hardDeadline = setTimeout(() => {
  deadlineExpired = true;
  fixture.kill("SIGTERM");
  void browser?.close().catch(() => {});
}, deadlineMs);
const waitLine = async (predicate, label, timeout = 20_000) => {
  const until = Date.now() + timeout;
  while (Date.now() < until) {
    const index = childOut.indexOf("\n");
    const line = index >= 0 ? childOut.slice(0, index) : childOut;
    if (index >= 0) childOut = childOut.slice(index + 1);
    if (line) {
      let value;
      try {
        value = JSON.parse(line);
      } catch {
        throw new Error(`Fixture emitted invalid JSON: ${line}`);
      }
      if (predicate(value)) return value;
    }
    if (fixture.exitCode !== null)
      throw new Error(`Fixture exited ${fixture.exitCode}: ${childErr}`);
    await new Promise((resolve) => setTimeout(resolve, 20));
  }
  throw new Error(`${label} timed out: ${childErr}`);
};
let result;
try {
  ready = await waitLine(
    (line) => line.kind === "ready",
    "Fixture startup",
    check ? 45_000 : 90_000,
  );
  const identityResponse = await fetch(`${ready.origin}/api/state?view=chat`);
  assert.equal(identityResponse.status, 200);
  const stateData = await identityResponse.json();
  const expectedLeads = check ? 1 : 8;
  const expectedWorkers = check ? 2 : 256;
  assert.equal(
    stateData.threads.length,
    expectedLeads + expectedWorkers,
    "fixture must expose the requested lead and worker identities",
  );
  const leads = stateData.threads.filter((agent) => agent.isLead);
  assert.equal(
    leads.length,
    expectedLeads,
    "fixture must expose the requested lead teams",
  );

  const browserOptions = {
    headless: true,
    ...(process.env.CHROME_BIN
      ? { executablePath: process.env.CHROME_BIN }
      : {}),
    args: [
      "--no-sandbox",
      "--disable-dev-shm-usage",
      "--renderer-process-limit=1",
      "--process-per-site",
      "--disable-site-isolation-trials",
      "--disable-extensions",
      "--disable-background-networking",
      "--no-proxy-server",
    ],
  };
  phase = "browser-startup";
  browserServer = await chromium.launchServer(browserOptions);
  browser = await chromium.connect(browserServer.wsEndpoint());
  const browserPid = browserServer.process().pid;
  const sampleBrowserResources = () => {
    if (!browserPid) return;
    try {
      const rows = execFileSync("ps", ["-e", "-o", "pid=,ppid=,rss=,pcpu="], {
        encoding: "utf8",
      })
        .trim()
        .split("\n")
        .map((line) => line.trim().split(/\s+/).map(Number))
        .filter((row) => row.length === 4 && row.every(Number.isFinite));
      const parent = new Map(rows.map(([pid, ppid]) => [pid, ppid]));
      const pids = new Set([browserPid]);
      let changed = true;
      while (changed) {
        changed = false;
        for (const [pid, ppid] of parent)
          if (pids.has(ppid) && !pids.has(pid)) {
            pids.add(pid);
            changed = true;
          }
      }
      const selected = rows.filter(([pid]) => pids.has(pid));
      browserResourcePeak = {
        processCount: Math.max(
          browserResourcePeak.processCount,
          selected.length,
        ),
        rssKiB: Math.max(
          browserResourcePeak.rssKiB,
          selected.reduce((sum, row) => sum + row[2], 0),
        ),
        cpuPercent: Math.max(
          browserResourcePeak.cpuPercent,
          selected.reduce((sum, row) => sum + row[3], 0),
        ),
      };
    } catch {
      // Resource accounting is supplemental; the browser workflow remains authoritative.
    }
  };
  browserResourceTimer = setInterval(sampleBrowserResources, 1000);
  const context = await browser.newContext({
    viewport: { width: 1440, height: 960 },
    serviceWorkers: "block",
  });
  contexts.push(context);
  phase = "production-app-tabs";
  await context.addInitScript(
    ({ stateDir }) => {
      window.__bench = {
        startedAt: performance.now(),
        resources: [],
        mutations: 0,
        longTasks: [],
        apiFailures: [],
        staleIntervals: [],
      };
      try {
        localStorage.setItem(`codex-desktop-opened:${stateDir}`, "");
      } catch {}
      try {
        new PerformanceObserver((list) =>
          window.__bench.longTasks.push(
            ...list.getEntries().map((entry) => entry.duration),
          ),
        ).observe({ type: "longtask", buffered: true });
      } catch {}
      new PerformanceObserver((list) => {
        for (const entry of list.getEntries())
          if (entry.name.includes("/api/"))
            window.__bench.resources.push({
              path: new URL(entry.name).pathname,
              durationMs: entry.duration,
              responseStatus: entry.responseStatus || null,
              transferBytes: entry.transferSize || 0,
              encodedBytes: entry.encodedBodySize || 0,
              decodedBytes: entry.decodedBodySize || 0,
            });
      }).observe({ type: "resource", buffered: true });
      new MutationObserver((entries) => {
        window.__bench.mutations += entries.length;
      }).observe(document, {
        subtree: true,
        childList: true,
        characterData: true,
      });
      let expectedTick = performance.now() + 50;
      setInterval(() => {
        const now = performance.now();
        window.__bench.staleIntervals.push(Math.max(0, now - expectedTick));
        expectedTick = now + 50;
      }, 50);
    },
    { stateDir: stateData.stateDir },
  );

  const tabCount = check ? 1 : 8;
  for (let index = 0; index < tabCount; index++) {
    const page = await context.newPage();
    page.setDefaultTimeout(25_000);
    page.on("pageerror", (error) => pageErrors.push(error.message));
    page.on("crash", () => pageErrors.push(`renderer ${index + 1} crashed`));
    page.on("console", (message) => {
      if (message.type() === "error") pageErrors.push(message.text());
    });
    page.on("requestfailed", (request) =>
      pageErrors.push(
        `request failed ${request.url()}: ${request.failure()?.errorText}`,
      ),
    );
    pages.push(page);
    const lead = leads[index];
    await page.addInitScript(
      ({ stateDir, id }) =>
        localStorage.setItem(`codex-desktop-opened:${stateDir}`, id),
      { stateDir: stateData.stateDir, id: lead.id },
    );
    await page.goto(ready.origin, {
      waitUntil: "domcontentloaded",
      timeout: 25_000,
    });
    try {
      await page.waitForFunction(
        () =>
          document.querySelector("#messages") ||
          document.querySelector(".startup"),
        null,
        { timeout: 25_000 },
      );
    } catch (error) {
      console.error(
        `tab ${index + 1} DOM: ${await page
          .locator("body")
          .innerText()
          .catch(() => "<unavailable>")}`,
      );
      console.error(
        `tab ${index + 1} browser diagnostics: ${JSON.stringify(pageErrors)}`,
      );
      throw error;
    }
    await page.waitForSelector("#messages", { timeout: 25_000 });
  }
  const preflight = await Promise.all(
    pages.map((page) =>
      page.evaluate(() => ({
        sync: performance
          .getEntriesByType("resource")
          .filter((entry) => entry.name.includes("/api/sync/")).length,
        title:
          document.querySelector("#messages")?.getAttribute("aria-label") ||
          document.title,
        mutations: window.__bench.mutations,
      })),
    ),
  );
  assert(
    preflight.every((entry) => entry.sync > 0),
    "each production App page must initialize RxDB sync",
  );

  phase = "offered-load";
  const browserStarted = Date.now();
  fixture.stdin.write(
    JSON.stringify({ action: "run", rounds: check ? 1 : rounds }) + "\n",
  );
  const envelope = await waitLine(
    (line) => line.kind === "result",
    "Load completion",
    deadlineMs,
  );
  result = envelope.report;
  await Promise.all(pages.map((page) => page.waitForTimeout(1200)));
  const ui = await Promise.all(
    pages.map(async (page) =>
      page.evaluate(() => {
        const text = document.body.innerText;
        const api = window.__bench.resources;
        const byPath = {};
        for (const entry of api) (byPath[entry.path] ||= []).push(entry);
        const summarize = (values) => {
          if (!values.length)
            return { samples: 0, p50: null, p95: null, p99: null, max: null };
          const sorted = [...values].sort((a, b) => a - b);
          const at = (p) =>
            sorted[Math.max(0, Math.ceil(p * sorted.length) - 1)];
          return {
            samples: values.length,
            p50: at(0.5),
            p95: at(0.95),
            p99: at(0.99),
            max: sorted.at(-1),
          };
        };
        return {
          syncConnections: performance
            .getEntriesByType("resource")
            .filter((e) => e.name.includes("/api/sync/")).length,
          apiLatencyMs: Object.fromEntries(
            Object.entries(byPath).map(([path, values]) => [
              path,
              {
                ...summarize(values.map((item) => item.durationMs)),
                responseBytes: values.reduce(
                  (n, item) => n + item.decodedBytes,
                  0,
                ),
                responseCountByStatus: Object.fromEntries(
                  [...new Set(values.map((item) => item.responseStatus))].map(
                    (status) => [
                      status ?? "unknown",
                      values.filter((item) => item.responseStatus === status)
                        .length,
                    ],
                  ),
                ),
              },
            ]),
          ),
          pageErrors: [],
          longTasks: window.__bench.longTasks,
          mutationRecords: window.__bench.mutations,
          staleIntervalsMs: window.__bench.staleIntervals,
          visibleTeams: (text.match(/Load \d\d\/\d\d/g) || []).length,
          displayedAssistantItems: (text.match(/synthetic answer/g) || [])
            .length,
        };
      }),
    ),
  );
  const feedReads = await Promise.all(
    leads.map((lead, index) =>
      pages[index].evaluate(async (id) => {
        const started = performance.now();
        const response = await fetch(`/api/agent-chat?room=feed%3A${id}`);
        if (!response.ok)
          throw new Error(
            `lead feed read failed for ${id}: ${response.status}`,
          );
        const data = await response.json();
        return {
          messages: data.messages.length,
          latencyMs: performance.now() - started,
          bytes: new TextEncoder().encode(JSON.stringify(data)).byteLength,
          status: response.status,
        };
      }, lead.id),
    ),
  );
  feedReads.forEach((feed, index) => {
    ui[index].apiLatencyMs["/api/agent-chat"] = {
      samples: 1,
      p50: feed.latencyMs,
      p95: feed.latencyMs,
      p99: feed.latencyMs,
      max: feed.latencyMs,
      responseBytes: feed.bytes,
      responseCountByStatus: { [feed.status]: 1 },
    };
  });
  phase = "browser-render-and-report";
  sampleBrowserResources();
  assert.equal(result.queue.drained, true, "all harness work must drain");
  assert.equal(
    result.exactlyOnce.completedDispatches,
    result.exactlyOnce.offeredEvents,
    "every offered production event/message must complete once",
  );
  assert.equal(
    result.exactlyOnce.uniqueDispatchedIdentities,
    result.exactlyOnce.offeredEvents,
    "every accepted event/message identity must be unique",
  );
  assert.equal(
    result.exactlyOnce.durableChatMessages,
    expectedWorkers * 8 * (check ? 1 : rounds),
    "every worker must persist peer and lead messages in every phase",
  );
  assert.equal(
    result.exactlyOnce.queuedRuntimeEventsAdded,
    expectedWorkers * 12 * (check ? 1 : rounds),
    "durable chat messages and completed workers must queue production runtime events",
  );
  assert.equal(
    result.exactlyOnce.queuedRuntimeEventsByKind.agent_message,
    expectedWorkers * 8 * (check ? 1 : rounds),
    "each worker-to-worker and worker-to-lead chat must queue one production agent message",
  );
  assert.equal(
    result.exactlyOnce.queuedRuntimeEventsByKind.child_result,
    expectedWorkers * 4 * (check ? 1 : rounds),
    "each synthetic completed turn must queue one production child result",
  );
  assert.equal(
    result.transcriptItemsByRole.assistant,
    expectedWorkers * 4 * (check ? 1 : rounds),
    "all four phase finals must persist for each worker/round",
  );
  assert.equal(
    pageErrors.length,
    0,
    `browser page errors: ${pageErrors.join(" | ")}`,
  );
  const browserReport = {
    tabs: pages.length,
    mode: "Chromium headless; real production App/RxDB and HTTP API",
    processResources: {
      pid: browserServer.process().pid,
      unit: "KiB, percent",
      sampledPeak: browserResourcePeak,
    },
    elapsedMs: Date.now() - browserStarted,
    pageErrors,
    syncAndPullLatencyByTab: ui.map((tab) => tab.apiLatencyMs),
    longTasksMs: ui.flatMap((tab) => tab.longTasks),
    tabStalenessMs: ui.map((tab) => ({
      p95: tab.staleIntervalsMs.length
        ? [...tab.staleIntervalsMs].sort((a, b) => a - b)[
            Math.ceil(0.95 * tab.staleIntervalsMs.length) - 1
          ]
        : null,
      max: tab.staleIntervalsMs.length
        ? Math.max(...tab.staleIntervalsMs)
        : null,
    })),
    mutationRecords: ui.reduce((sum, tab) => sum + tab.mutationRecords, 0),
    visibleTeamRows: ui.map((tab) => tab.visibleTeams),
    pulledMessagesByTeam: feedReads,
    renderedByCategory: {
      assistantFinalTextMatches: ui.map((tab) => tab.displayedAssistantItems),
      visibleWorkerRows: ui.map((tab) => tab.visibleTeams),
      toolAndLifecycleViaUi:
        "not directly rendered by selected lead conversation",
    },
  };
  const report = {
    schemaVersion: 1,
    benchmark: "runtime_load",
    sourceRevision: ready.sourceRevision,
    productionSourceDirty,
    harnessSourceSha256,
    sourceRoot,
    evidenceTier: "local-verified",
    syntheticTurnsOnly: true,
    omitted: [
      "native AppServer callbacks/callback queue (native factories forbidden)",
      "real provider/model execution",
      "native tool execution; tool lifecycle/output are protocol-shaped synthetic Runtime notifications",
    ],
    runtime: result,
    browser: browserReport,
    resourceLimits: {
      deadlineMs,
      queueCapacity: result.queue.capacity,
      hardTimeoutMs: deadlineMs,
      stateProfile: "unique temporary directories",
      nativeSessions: 0,
    },
    elapsedMs: Date.now() - start,
  };
  if (outputPath) {
    await mkdir(dirname(outputPath), { recursive: true });
    await writeFile(outputPath, JSON.stringify(report, null, 2) + "\n");
  }
  console.log(JSON.stringify(report, null, 2));
} catch (error) {
  console.error(
    `${deadlineExpired ? `Benchmark exceeded its ${deadlineMs}ms hard deadline.\n` : ""}${error.stack || error}\nFixture stderr:\n${childErr}`,
  );
  try {
    await mkdir(dirname(outputPath), { recursive: true });
    await writeFile(
      outputPath,
      JSON.stringify(
        {
          schemaVersion: 1,
          benchmark: "runtime_load",
          status: "failed",
          phase,
          sourceRevision: ready?.sourceRevision || null,
          productionSourceDirty,
          harnessSourceSha256,
          sourceRoot,
          error: String(error?.message || error),
          pageErrors,
          browserProcessResources: browserResourcePeak,
          elapsedMs: Date.now() - start,
          runtimePlatform: {
            node: process.version,
            os: process.platform,
            arch: process.arch,
          },
          cleanup:
            "browser pages closed; isolated fixture stopped; temporary state removed",
        },
        null,
        2,
      ) + "\n",
    );
    console.error(`Failure evidence saved to ${outputPath}`);
  } catch (writeError) {
    console.error(`Could not save failure evidence: ${writeError}`);
  }
  process.exitCode = 1;
} finally {
  clearTimeout(hardDeadline);
  clearInterval(browserResourceTimer);
  for (const page of pages) await page.close().catch(() => {});
  for (const context of contexts) await context.close().catch(() => {});
  await browser?.close().catch(() => {});
  await browserServer?.close().catch(() => {});
  if (fixture.exitCode === null) {
    fixture.stdin.end();
    fixture.kill("SIGTERM");
    await new Promise((resolve) => {
      const timer = setTimeout(() => {
        fixture.kill("SIGKILL");
        resolve();
      }, 5000);
      fixture.once("exit", () => {
        clearTimeout(timer);
        resolve();
      });
    });
  }
  // The temporary Runtime/SQLite/profile is removed only after all child and browser work ends.
  const { rm } = await import("node:fs/promises");
  await rm(state, { recursive: true, force: true });
}
