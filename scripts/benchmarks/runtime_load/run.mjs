#!/usr/bin/env node
// Eight real App pages, one isolated production Runtime, and synthetic turns.
import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { spawn } from "node:child_process";
import { readFileSync, readdirSync } from "node:fs";
import { mkdtemp, mkdir, readFile, readdir, writeFile } from "node:fs/promises";
import { availableParallelism, homedir } from "node:os";
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
const diagnoseOriginPool = args.includes("--diagnose-origin-pool");
const teamsAt = args.indexOf("--teams");
const teams = teamsAt >= 0 ? Number(args[teamsAt + 1]) : 8;
const workersAt = args.indexOf("--workers-per-team");
const workersPerTeam = workersAt >= 0 ? Number(args[workersAt + 1]) : 32;
const steadySecondsAt = args.indexOf("--steady-seconds");
const steadySeconds = check
  ? 0
  : steadySecondsAt >= 0
    ? Number(args[steadySecondsAt + 1])
    : 30;
const evidenceRoot = join(
  process.env.XDG_STATE_HOME || join(homedir(), ".local", "state"),
  "evidence",
  "latency-components",
);
await mkdir(evidenceRoot, { recursive: true });
const roundsAt = args.indexOf("--rounds");
const rounds = roundsAt >= 0 ? Number(args[roundsAt + 1]) : 1;
const rateAt = args.indexOf("--offered-rate");
const offeredRate = rateAt >= 0 ? Number(args[rateAt + 1]) : 160;
const transportsAt = args.indexOf("--transports");
const transportCount = transportsAt >= 0 ? Number(args[transportsAt + 1]) : 2;
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
const frontendAt = args.indexOf("--frontend-dist");
const frontendDist =
  frontendAt >= 0
    ? resolve(args[frontendAt + 1])
    : join(sourceRoot, "web", "dist");
const frontendRevisionAt = args.indexOf("--frontend-source-revision");
const frontendSourceRevision =
  frontendRevisionAt >= 0
    ? args[frontendRevisionAt + 1]
    : execFileSync("git", ["-C", sourceRoot, "rev-parse", "HEAD"], {
        encoding: "utf8",
      }).trim();
const hashTree = async (root) => {
  const hash = createHash("sha256");
  const visit = async (directory, prefix = "") => {
    for (const entry of (
      await readdir(directory, { withFileTypes: true })
    ).sort((a, b) => a.name.localeCompare(b.name))) {
      const relative = prefix ? `${prefix}/${entry.name}` : entry.name;
      if (entry.isDirectory())
        await visit(join(directory, entry.name), relative);
      else {
        hash.update(relative);
        hash.update(await readFile(join(directory, entry.name)));
      }
    }
  };
  await visit(root);
  return hash.digest("hex");
};
const frontendArtifactSha256 = await hashTree(frontendDist);
const sourceChanges = execFileSync(
  "git",
  ["-C", sourceRoot, "status", "--porcelain", "--untracked-files=no"],
  { encoding: "utf8" },
)
  .trim()
  .split("\n")
  .filter(Boolean);
const productionSourceDirty = sourceChanges.some(
  (line) =>
    /^scripts\/codex_[^/]+\.py$/.test(line.slice(3)) ||
    /^web\/(src|public)\//.test(line.slice(3)),
);
const backendSourceSha256 = await (async () => {
  const scripts = join(sourceRoot, "scripts");
  const files = (await readdir(scripts, { withFileTypes: true }))
    .filter((entry) => entry.isFile() && entry.name.endsWith(".py"))
    .map((entry) => entry.name)
    .sort();
  const hash = createHash("sha256");
  for (const file of files) {
    hash.update(file);
    hash.update(await readFile(join(scripts, file)));
  }
  return hash.digest("hex");
})();
const deadlineMs = check ? 90_000 : 240_000;
assert(
  Number.isInteger(rounds) && rounds >= 1 && rounds <= 8,
  "--rounds must be 1..8",
);
assert(
  Number.isInteger(offeredRate) && offeredRate >= 1 && offeredRate <= 2000,
  "--offered-rate must be 1..2000 worker turns per second",
);
assert(
  Number.isInteger(transportCount) &&
    transportCount >= 1 &&
    transportCount <= 8,
  "--transports must be 1..8",
);
assert(
  Number.isInteger(teams) && teams >= 1 && teams <= 8,
  "--teams must be 1..8",
);
assert(
  Number.isInteger(workersPerTeam) &&
    workersPerTeam >= 2 &&
    workersPerTeam <= 64,
  "--workers-per-team must be 2..64",
);
assert(
  Number.isFinite(steadySeconds) && steadySeconds >= 0 && steadySeconds <= 60,
  "--steady-seconds must be 0..60",
);
if (outputPath && outputPath.startsWith(repo + "/"))
  throw new Error("--output must be outside the checkout");
const packageRequire = createRequire(join(sourceRoot, "web/package.json"));
const { chromium } = packageRequire("playwright-core");
const state = await mkdtemp(join(evidenceRoot, ".runtime-load-case-"));
const fixture = spawn(
  "python3",
  ["-B", join(harness, "server.py"), state, sourceRoot, frontendDist],
  {
    cwd: sourceRoot,
    env: {
      ...process.env,
      ...(check ? { BENCH_QUICK_CHECK: "1" } : {}),
      BENCH_OFFERED_TURNS_PER_SECOND: String(offeredRate),
      BENCH_ACCOUNT_COUNT: String(transportCount),
      BENCH_TEAMS: String(teams),
      BENCH_WORKERS_PER_TEAM: String(workersPerTeam),
      BENCH_STEADY_SECONDS: String(steadySeconds),
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
  browserLog = "",
  browserExit = null,
  previousTmpdir,
  browser,
  browserServer,
  browserResourceTimer,
  browserResourcePeak = { processCount: 0, rssKiB: 0, cpuPercent: 0 },
  contexts = [],
  pages = [],
  cdpSessions = [],
  networkTraces = [];
let phase = "fixture-startup";
let diagnosticOnlyResult = false;
let pageErrors = [];
let requestFailures = [];
let consoleErrors = [];
let expectedApiUnavailable = [];
let tabInitializationMs = [];
const tabInitializationDeadlineMs = 15_000;
let steadyWitnessLatencyByTab = [];
let steadyFinalOfferToDOM = null;
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
    const line = index >= 0 ? childOut.slice(0, index) : "";
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
const summarize = (values) => {
  if (!values.length)
    return { samples: 0, p50: null, p95: null, p99: null, max: null };
  const sorted = [...values].sort((a, b) => a - b);
  const at = (p) => sorted[Math.max(0, Math.ceil(p * sorted.length) - 1)];
  return {
    samples: values.length,
    p50: at(0.5),
    p95: at(0.95),
    p99: at(0.99),
    max: sorted.at(-1),
  };
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
  const expectedLeads = check ? 1 : teams;
  const expectedWorkers = check ? 2 : teams * workersPerTeam;
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
  assert(
    ready.witnesses.every((witness) =>
      stateData.threads.some((agent) => agent.id === witness.agentId),
    ),
    `each browser witness must be an App identity: ${JSON.stringify(
      ready.witnesses.map((w) => ({
        id: w.agentId,
        found: stateData.threads.some((agent) => agent.id === w.agentId),
      })),
    )}`,
  );

  const browserOptions = {
    headless: true,
    ...(process.env.CHROME_BIN
      ? { executablePath: process.env.CHROME_BIN }
      : {}),
    args: [
      "--no-sandbox",
      "--disable-dev-shm-usage",
      "--disable-extensions",
      "--disable-background-networking",
      "--no-proxy-server",
      "--enable-logging=stderr",
    ],
  };
  phase = "browser-startup";
  const browserTemp = join(state, "browser-tmp");
  await mkdir(browserTemp, { recursive: true });
  // Playwright's user-data directory follows os.tmpdir(). Keep the isolated
  // browser profile inside this unique state, not under shared /tmp quota.
  previousTmpdir = process.env.TMPDIR;
  process.env.TMPDIR = browserTemp;
  browserServer = await chromium.launchServer({
    ...browserOptions,
    env: { ...process.env, TMPDIR: browserTemp },
  });
  browser = await chromium.connect(browserServer.wsEndpoint());
  const browserProcess = browserServer.process();
  browserProcess?.on("exit", (code, signal) => {
    browserExit = { code, signal };
  });
  browserProcess?.stderr?.on("data", (chunk) => {
    browserLog += chunk.toString();
  });
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
        witnessFirstSeen: {},
      };
      const scanWitnesses = () => {
        const text = document.querySelector("#messages")?.innerText || "";
        for (const marker of text.match(/witness-[\w-]+/g) || []) {
          if (!window.__bench.witnessFirstSeen[marker]) {
            window.__bench.witnessFirstSeen[marker] = {
              epochMs: Date.now(),
              highResolutionMs: performance.now(),
            };
          }
        }
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
        scanWitnesses();
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

  const tabCount = check ? 1 : teams;
  for (let index = 0; index < tabCount; index++) {
    const page = await context.newPage();
    page.setDefaultTimeout(25_000);
    page.on("pageerror", (error) => pageErrors.push(error.message));
    page.on("crash", () => pageErrors.push(`renderer ${index + 1} crashed`));
    page.on("console", (message) => {
      if (message.type() === "error") consoleErrors.push(message.text());
    });
    page.on("requestfailed", (request) => {
      const failure = request.failure()?.errorText || "unknown";
      const record = {
        url: request.url(),
        method: request.method(),
        failure,
        atEpochMs: Date.now(),
      };
      requestFailures.push(record);
      if (!failure.includes("ERR_ABORTED"))
        pageErrors.push(`request failed ${record.url}: ${failure}`);
    });
    page.on("response", async (response) => {
      if (response.status() >= 400) {
        const detail = await response.text().catch(() => "<body unavailable>");
        const parsedUrl = new URL(response.url());
        const path = parsedUrl.pathname;
        const syntheticAccount = parsedUrl.searchParams
          .get("account_key")
          ?.startsWith("bench-");
        const expectedUnavailable =
          path === "/api/models" ||
          path === "/api/costs" ||
          (path === "/api/limits" && syntheticAccount);
        const entry = `HTTP ${response.status()} ${response.url()}: ${detail.slice(0, 1000)}`;
        if (expectedUnavailable) expectedApiUnavailable.push(entry);
        else pageErrors.push(entry);
      }
    });
    pages.push(page);
    if (diagnoseOriginPool) {
      const cdp = await context.newCDPSession(page);
      cdpSessions.push(cdp);
      await cdp.send("Network.enable");
      const trace = { tab: index + 1, active: {}, events: [] };
      networkTraces.push(trace);
      const record = (event) => {
        if (trace.events.length < 5000) trace.events.push(event);
      };
      cdp.on("Network.requestWillBeSent", (event) => {
        trace.active[event.requestId] = {
          url: event.request.url,
          type: event.type,
          startedAtEpochMs: Date.now(),
        };
        record({
          kind: "request",
          requestId: event.requestId,
          url: event.request.url,
          type: event.type,
          atEpochMs: Date.now(),
        });
      });
      cdp.on("Network.responseReceived", (event) => {
        const active = trace.active[event.requestId];
        if (active) {
          active.status = event.response.status;
          active.responseAtEpochMs = Date.now();
        }
        record({
          kind: "response",
          requestId: event.requestId,
          url: event.response.url,
          type: event.type,
          status: event.response.status,
          atEpochMs: Date.now(),
        });
      });
      cdp.on("Network.loadingFinished", (event) => {
        const active = trace.active[event.requestId];
        if (active?.type === "EventSource")
          active.finishedAtEpochMs = Date.now();
        delete trace.active[event.requestId];
      });
      cdp.on("Network.loadingFailed", (event) => {
        record({
          kind: "loadingFailed",
          requestId: event.requestId,
          error: event.errorText,
          canceled: event.canceled,
          atEpochMs: Date.now(),
        });
        delete trace.active[event.requestId];
      });
    }
    const witness = ready.witnesses[index];
    assert(witness, `missing representative worker for tab ${index + 1}`);
    const tabStartedAt = Date.now();
    const remainingStartupMs = () =>
      Math.max(1, tabInitializationDeadlineMs - (Date.now() - tabStartedAt));
    const syncPullResult = page
      .waitForResponse(
        (response) => {
          const url = new URL(response.url());
          return url.pathname === "/api/sync/pull" && response.status() === 200;
        },
        { timeout: tabInitializationDeadlineMs },
      )
      .then(
        (response) => ({ response }),
        (error) => ({ error }),
      );
    await page.addInitScript(
      ({ stateDir, id }) =>
        localStorage.setItem(
          `codex-desktop-opened:${stateDir}`,
          JSON.stringify(id),
        ),
      { stateDir: stateData.stateDir, id: witness.agentId },
    );
    if (diagnoseOriginPool && index === 5) {
      const navigation = page
        .goto(ready.origin, { waitUntil: "domcontentloaded", timeout: 20_000 })
        .then(
          () => ({ completed: true }),
          (error) => ({ completed: false, error: error.message }),
        );
      const firstWait = await Promise.race([
        navigation,
        new Promise((resolve) => setTimeout(() => resolve(null), 3000)),
      ]);
      const readServerDiagnostics = async () =>
        fetch(`${ready.origin}/__bench/diagnostics`)
          .then((response) => response.json())
          .catch((error) => ({ error: error.message }));
      const serverBefore = await readServerDiagnostics();
      const activeSourcesBefore = networkTraces.flatMap((trace) =>
        Object.entries(trace.active)
          .filter(([, request]) => request.type === "EventSource")
          .map(([requestId, request]) => ({
            tab: trace.tab,
            requestId,
            ...request,
          })),
      );
      let releasedByClosingFirstPage = null;
      if (firstWait === null) {
        await pages[0].close();
        releasedByClosingFirstPage = await Promise.race([
          navigation,
          new Promise((resolve) =>
            setTimeout(
              () => resolve({ completed: false, timeout: true }),
              8000,
            ),
          ),
        ]);
      }
      const serverAfter = await readServerDiagnostics();
      const evidence = {
        triggerTab: index + 1,
        initialNavigation: firstWait,
        serverBefore,
        activeSourcesBefore,
        outstandingRequestsByTab: networkTraces.map((trace) => ({
          tab: trace.tab,
          outstanding: Object.values(trace.active),
          events: trace.events,
        })),
        releasedByClosingFirstPage,
        serverAfter,
      };
      const diagnostic = new Error("origin-pool diagnostic completed");
      diagnostic.benchmarkDiagnostic = evidence;
      throw diagnostic;
    }
    try {
      await page.goto(ready.origin, {
        waitUntil: "domcontentloaded",
        timeout: remainingStartupMs(),
      });
      await page.waitForSelector("#messages", {
        timeout: remainingStartupMs(),
      });
      const syncPull = await syncPullResult;
      if (!syncPull.response) throw syncPull.error;
      tabInitializationMs.push({
        tab: index + 1,
        elapsedMs: Date.now() - tabStartedAt,
        syncPullStatus: syncPull.response.status(),
      });
    } catch (error) {
      if (!tabInitializationMs.some((sample) => sample.tab === index + 1))
        tabInitializationMs.push({
          tab: index + 1,
          elapsedMs: Date.now() - tabStartedAt,
          error: error.message,
        });
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
  }
  assert(
    tabInitializationMs.length === tabCount &&
      tabInitializationMs.every(
        (sample) => sample.elapsedMs <= tabInitializationDeadlineMs,
      ),
    "each real App tab must initialize and complete an RxDB sync pull within15s",
  );
  const preflight = await Promise.all(
    pages.map((page) =>
      page.evaluate(() => ({
        sync: performance
          .getEntriesByType("resource")
          .filter((entry) => entry.name.includes("/api/sync/")).length,
        title:
          document.querySelector("#messages")?.getAttribute("aria-label") ||
          document.title,
        visibilityState: document.visibilityState,
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
  const baseTurnCount = expectedWorkers * (check ? 1 : rounds);
  const expectedPhaseTurnCounts = {
    warmup: baseTurnCount,
    steady: Math.max(
      baseTurnCount,
      Math.ceil(offeredRate * (check ? 0 : steadySeconds)),
    ),
    burst: baseTurnCount,
    drain: baseTurnCount,
  };
  assert.deepEqual(
    result.phaseTurnCounts,
    expectedPhaseTurnCounts,
    "fixture must offer the configured sustained steady workload",
  );
  const expectedTotalTurns = Object.values(expectedPhaseTurnCounts).reduce(
    (sum, count) => sum + count,
    0,
  );
  await Promise.all(
    pages.map((page) =>
      page.evaluate((offeredAt) => {
        window.__bench.finalWitnessOfferedAtEpochMs = offeredAt;
      }, result.finalWitnessOfferedAtEpochMs),
    ),
  );
  steadyWitnessLatencyByTab = await Promise.all(
    pages.map(async (page, index) => {
      const witness = ready.witnesses[index];
      const markers = result.steadyWitnessMarkersByAgent[witness.agentId] || [];
      assert(
        markers.length > 0,
        `missing steady final witnesses for tab ${index + 1}`,
      );
      await page
        .waitForFunction(
          (expected) => {
            const text = document.querySelector("#messages")?.innerText || "";
            return expected.every((marker) => text.includes(marker));
          },
          markers,
          { timeout: check ? 5000 : 15_000 },
        )
        .catch(async (error) => {
          const details = await page
            .evaluate(
              ({ stateDir }) => ({
                selected: localStorage.getItem(
                  `codex-desktop-opened:${stateDir}`,
                ),
                messages: document
                  .querySelector("#messages")
                  ?.innerText?.slice(-2000),
              }),
              { stateDir: stateData.stateDir },
            )
            .catch(() => ({}));
          const transcript = await fetch(
            `${ready.origin}/api/transcript?id=${encodeURIComponent(witness.agentId)}`,
          )
            .then((response) => response.json())
            .catch((cause) => ({ error: String(cause) }));
          throw new Error(
            `tab ${index + 1} did not render all ${markers.length} steady final markers; selected=${JSON.stringify(details)}; transcript=${JSON.stringify(transcript.items?.slice(-5) || transcript)}; DOM=${(
              await page
                .locator("body")
                .innerText()
                .catch(() => "<unavailable>")
            ).slice(-2000)}; ${error.message}`,
          );
        });
      const samples = await page.evaluate(
        (expectedMarkers) =>
          expectedMarkers.map((marker) => {
            const firstSeen = window.__bench.witnessFirstSeen[marker] || null;
            const offeredAt =
              window.__bench.finalWitnessOfferedAtEpochMs?.[marker];
            return {
              marker,
              firstSeen,
              renderedAtEpochMs: firstSeen?.epochMs || null,
              finalOfferToFirstDOMAppearanceMs:
                firstSeen && offeredAt != null
                  ? firstSeen.epochMs - offeredAt
                  : null,
            };
          }),
        markers,
      );
      return { tab: index + 1, agentId: witness.agentId, samples };
    }),
  );
  const steadyFinalRenderSamples = steadyWitnessLatencyByTab.flatMap(
    (tab) => tab.samples,
  );
  steadyFinalOfferToDOM = summarize(
    steadyFinalRenderSamples.map(
      (sample) => sample.finalOfferToFirstDOMAppearanceMs,
    ),
  );
  const ui = await Promise.all(
    pages.map(async (page, index) =>
      page.evaluate((expectedMarkers) => {
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
          visibilityState: document.visibilityState,
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
          renderedWitnessMarkers: expectedMarkers.filter((marker) =>
            text.includes(marker),
          ),
          renderedAllSteadyWitnessMarkers: expectedMarkers.every((marker) =>
            text.includes(marker),
          ),
        };
      }, result.steadyWitnessMarkersByAgent[ready.witnesses[index].agentId]),
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
    result.exactlyOnce.offeredCallbackIdentitiesSha256,
    result.exactlyOnce.dispatchedCallbackIdentitiesSha256,
    "the offered and dispatched callback identity sets must match exactly",
  );
  assert.equal(
    result.exactlyOnce.originalRuntimeEventIdsExpected,
    result.exactlyOnce.originalRuntimeEventIdsAcknowledged,
    "every original Runtime event ID must be acknowledged exactly once",
  );
  assert.equal(
    result.exactlyOnce.durableChatMessages,
    expectedTotalTurns * 2,
    "every worker must persist peer and lead messages in every phase",
  );
  assert.equal(
    result.exactlyOnce.queuedRuntimeEventsAdded,
    expectedTotalTurns * 3,
    "durable chat messages and completed workers must queue production runtime events",
  );
  assert.equal(
    result.exactlyOnce.queuedRuntimeEventsByKind.agent_message,
    expectedTotalTurns * 2,
    "each worker-to-worker and worker-to-lead chat must queue one production agent message",
  );
  assert.equal(
    result.exactlyOnce.queuedRuntimeEventsByKind.child_result,
    expectedTotalTurns,
    "each synthetic completed turn must queue one production child result",
  );
  assert.equal(
    result.exactlyOnce.consumedRuntimeEvents,
    result.exactlyOnce.queuedRuntimeEventsAdded,
    "every original chat and child-result runtime-event ID must receive a production callback receipt",
  );
  assert.equal(
    result.exactlyOnce.pendingRuntimeEvents,
    0,
    "no synthetic Runtime inbox event may remain pending after the acknowledgement drain",
  );
  assert.equal(
    result.callbackEventSampleCount,
    result.exactlyOnce.offeredEvents,
    "AppServer callback samples must account for every offered event, including coalesced delta fragments",
  );
  assert.equal(
    result.offered.assistantDelta,
    expectedTotalTurns * 8,
    "each worker turn must offer eight assistant fragments",
  );
  assert.equal(
    result.offered.hookLifecycle,
    expectedTotalTurns * 2,
    "each worker turn must offer hook start and completion lifecycle callbacks",
  );
  assert.equal(
    result.transcriptItemsByRole.assistant,
    expectedTotalTurns,
    "all four phase finals must persist for each worker/round",
  );
  assert.equal(result.httpServerErrors.length, 0, "no HTTP handler may fail");
  const deadlineRequestFailures = requestFailures.filter((sample) =>
    /ERR_(TIMED_OUT|CONNECTION_REFUSED|CONNECTION_RESET|ADDRESS_UNREACHABLE)/i.test(
      sample.failure,
    ),
  );
  assert.deepEqual(
    deadlineRequestFailures,
    [],
    "no local API request may time out or lose its server connection",
  );
  if (!check) {
    assert(
      result.phaseAchievedOfferedTurnsPerSecond.steady >= offeredRate * 0.9,
      `steady actual offered rate must be at least90% of configured ${offeredRate}/s`,
    );
    assert(
      result.burstDrainMs != null && result.burstDrainMs <= 30_000,
      "all production callbacks offered through burst must drain within30s",
    );
  }
  const browserReport = {
    tabs: pages.length,
    tabInitializationDeadlineMs,
    tabInitializationMs,
    mode: "Chromium headless; real production App/RxDB and HTTP API",
    processResources: {
      pid: browserServer.process().pid,
      unit: "KiB, percent",
      sampledPeak: browserResourcePeak,
    },
    elapsedMs: Date.now() - browserStarted,
    pageErrors,
    requestFailures,
    consoleErrors,
    expectedApiUnavailable,
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
    tabVisibilityState: ui.map((tab) => tab.visibilityState),
    visibleTeamRows: ui.map((tab) => tab.visibleTeams),
    pulledMessagesByTeam: feedReads,
    renderedByCategory: {
      assistantFinalTextMatches: ui.map((tab) => tab.displayedAssistantItems),
      steadyFinalOfferToDOM,
      steadyWitnessLatencyByTab,
      markerRenderedOnEveryTab: ui.map(
        (tab) => tab.renderedAllSteadyWitnessMarkers,
      ),
      visibleWorkerRows: ui.map((tab) => tab.visibleTeams),
      toolAndLifecycleViaUi:
        "tool and hook details are measured through Runtime transcript and analytics; assistant marker is rendered in each representative worker transcript",
    },
  };
  assert(
    ui.every((tab) => tab.renderedAllSteadyWitnessMarkers),
    "each team tab must render all steady final markers for its representative worker",
  );
  assert(
    steadyFinalRenderSamples.every(
      (item) => item.firstSeen && item.finalOfferToFirstDOMAppearanceMs >= 0,
    ),
    "every steady final marker must first appear in the DOM after final offer",
  );
  assert(
    steadyFinalOfferToDOM.p95 <= 3000 && steadyFinalOfferToDOM.p99 <= 5000,
    "steady final offer-to-DOM p95/p99 must meet 3s/5s targets",
  );
  // Let open UI requests finish or abort their own way before stopping HTTP.
  // In particular, App session refreshes can remain active after the last pull.
  await Promise.all(pages.map((page) => page.close()));
  await Promise.all(contexts.map((browserContext) => browserContext.close()));
  await new Promise((resolve) => setImmediate(resolve));
  assert.equal(
    pageErrors.length,
    0,
    `browser page errors: ${pageErrors.join(" | ")}`,
  );
  fixture.stdin.write(JSON.stringify({ action: "shutdown" }) + "\n");
  await new Promise((resolve, reject) => {
    const timer = setTimeout(
      () => reject(new Error("fixture shutdown timed out")),
      10000,
    );
    fixture.once("exit", () => {
      clearTimeout(timer);
      resolve();
    });
  });
  const report = {
    schemaVersion: 1,
    benchmark: "runtime_load",
    sourceRevision: ready.sourceRevision,
    productionSourceDirty,
    backendSourceSha256,
    harnessSourceSha256,
    sourceRoot,
    frontend: {
      dist: frontendDist,
      sourceRevision: frontendSourceRevision,
      artifactSha256: frontendArtifactSha256,
    },
    evidenceTier: "local-verified",
    syntheticTurnsOnly: true,
    omitted: [
      "real provider/model execution",
      "native tool execution; tool lifecycle/output are protocol-shaped synthetic Runtime notifications",
      "application hook execution (lifecycle notifications and analytics are synthetic protocol events)",
    ],
    runtime: result,
    browser: browserReport,
    acceptance: {
      tabInitializationDeadlineMs,
      tabInitializationMs,
      steadyConfiguredTurnsPerSecond: offeredRate,
      steadyActualTurnsPerSecond:
        result.phaseAchievedOfferedTurnsPerSecond.steady,
      steadyMinimumActualTurnsPerSecond: check ? null : offeredRate * 0.9,
      steadyFinalOfferToDOM,
      burstDrainMs: result.burstDrainMs,
      burstDrainDeadlineMs: check ? null : 30_000,
      callbackIdentitySetsEqual: true,
      originalRuntimeEventIdsExactlyOnce: true,
      httpServerErrors: result.httpServerErrors.length,
      deadlineRequestErrors: deadlineRequestFailures.length,
    },
    resourceLimits: {
      deadlineMs,
      queueCapacityPerAppServerTransport: result.queue.capacityPerTransport,
      hardTimeoutMs: deadlineMs,
      stateProfile: "unique temporary directories",
      nativeSessions: 0,
      browserLimits: processLimits(),
      hostMemoryAtReport: memoryInfo(),
    },
    elapsedMs: Date.now() - start,
  };
  if (outputPath) {
    await mkdir(dirname(outputPath), { recursive: true });
    await writeFile(outputPath, JSON.stringify(report, null, 2) + "\n");
  }
  console.log(JSON.stringify(report, null, 2));
} catch (error) {
  if (error?.benchmarkDiagnostic) {
    diagnosticOnlyResult = true;
    await mkdir(dirname(outputPath), { recursive: true });
    await writeFile(
      outputPath,
      JSON.stringify(
        {
          schemaVersion: 1,
          benchmark: "runtime_load",
          status: "diagnostic",
          diagnostic: "origin-connection-pool-release-probe",
          sourceRevision: ready?.sourceRevision || null,
          backendSourceSha256,
          harnessSourceSha256,
          frontend: {
            dist: frontendDist,
            sourceRevision: frontendSourceRevision,
            artifactSha256: frontendArtifactSha256,
          },
          evidence: error.benchmarkDiagnostic,
          browserProcessResources: browserResourcePeak,
          browserDiagnostics: processLimits(),
          hostMemory: memoryInfo(),
          elapsedMs: Date.now() - start,
          cleanup: "pending",
        },
        null,
        2,
      ) + "\n",
    );
    console.log(`Origin-pool diagnostic evidence saved to ${outputPath}`);
    process.exitCode = 0;
  } else {
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
            backendSourceSha256,
            harnessSourceSha256,
            sourceRoot,
            frontend: {
              dist: frontendDist,
              sourceRevision: frontendSourceRevision,
              artifactSha256: frontendArtifactSha256,
            },
            error: String(error?.message || error),
            tabInitializationDeadlineMs,
            tabInitializationMs,
            steadyWitnessLatencyByTab,
            steadyFinalOfferToDOM,
            pageErrors,
            requestFailures,
            consoleErrors,
            expectedApiUnavailable,
            runtimeSummary: result
              ? {
                  teams: result.teams,
                  offered: result.offered,
                  dispatched: result.dispatched,
                  transcriptItemsByRole: result.transcriptItemsByRole,
                  notificationCoverage: result.notificationCoverage,
                  workerStates: result.workerStates,
                  notificationSamples: result.notificationSamples,
                  httpServerErrors: result.httpServerErrors,
                  exactlyOnce: result.exactlyOnce,
                  latencyMs: result.latencyMs,
                  phaseTurnCounts: result.phaseTurnCounts,
                  phaseElapsedSeconds: result.phaseElapsedSeconds,
                  phaseAchievedOfferedTurnsPerSecond:
                    result.phaseAchievedOfferedTurnsPerSecond,
                  phaseOfferedTurnLatenessMs: result.phaseOfferedTurnLatenessMs,
                  burstDrainMs: result.burstDrainMs,
                  queue: result.queue,
                }
              : null,
            browserProcessResources: browserResourcePeak,
            browserLogTail: browserLog.slice(-30000),
            browserExit,
            browserDiagnostics: processLimits(),
            hostMemory: memoryInfo(),
            elapsedMs: Date.now() - start,
            runtimePlatform: {
              node: process.version,
              os: process.platform,
              arch: process.arch,
            },
            cleanup: "pending",
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
  }
} finally {
  clearTimeout(hardDeadline);
  clearInterval(browserResourceTimer);
  for (const session of cdpSessions) await session.detach().catch(() => {});
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
  if (previousTmpdir === undefined) delete process.env.TMPDIR;
  else process.env.TMPDIR = previousTmpdir;
  if (process.exitCode || diagnosticOnlyResult) {
    try {
      const failed = JSON.parse(await readFile(outputPath, "utf8"));
      failed.cleanup = {
        fixtureExited: fixture.exitCode !== null || fixture.signalCode !== null,
        fixtureExitCode: fixture.exitCode,
        fixtureSignalCode: fixture.signalCode,
        browserClosed: !browser || browser._isClosed?.() !== false,
        uniqueTemporaryStateRemoved: true,
      };
      failed.browserLogPath = outputPath + ".browser.log";
      await writeFile(outputPath + ".browser.log", browserLog);
      await writeFile(outputPath, JSON.stringify(failed, null, 2) + "\n");
    } catch {}
  }
}

function processLimits() {
  const read = (path) => {
    try {
      return readFileSync(path, "utf8").trim();
    } catch {
      return null;
    }
  };
  const countEntries = (path) => {
    try {
      return readdirSync(path).length;
    } catch {
      return null;
    }
  };
  const cgroupPath = (read("/proc/self/cgroup") || "")
    .split("\n")
    .find((line) => line.startsWith("0::"))
    ?.slice(3);
  const cgroupRoot = cgroupPath ? `/sys/fs/cgroup${cgroupPath}` : null;
  const cgroupRead = (name) =>
    cgroupRoot ? read(`${cgroupRoot}/${name}`) : null;
  return {
    nodeFileDescriptors: countEntries("/proc/self/fd"),
    processLimits: read("/proc/self/limits"),
    systemFileTable: read("/proc/sys/fs/file-nr"),
    cgroupPidsCurrent: cgroupRead("pids.current"),
    cgroupPidsMax: cgroupRead("pids.max"),
    cgroupMemoryCurrent: cgroupRead("memory.current"),
    cgroupMemoryMax: cgroupRead("memory.max"),
    availableParallelism: availableParallelism(),
    memoryUsage: process.memoryUsage(),
  };
}

function memoryInfo() {
  const values = {};
  try {
    for (const line of readFileSync("/proc/meminfo", "utf8").split("\n")) {
      const match = /^(MemAvailable|MemFree|SwapFree):\s+(\d+)\s+kB$/.exec(
        line,
      );
      if (match) values[match[1]] = Number(match[2]);
    }
  } catch {}
  return values;
}
