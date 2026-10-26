#!/usr/bin/env node
// Explicitly selected live acceptance run: a large team driven through the UI
// of a real Studio backend. Never run from the ordinary test tiers. Headless
// Chromium only; no window is activated and no operating-system input is sent.
import assert from "node:assert/strict";
import { runScaleVerifier } from "./scaleVerifierTeardown.mjs";
import { spawnSync } from "node:child_process";
import { randomUUID } from "node:crypto";
import { readdirSync, readFileSync } from "node:fs";
import { mkdir, realpath, rename, writeFile } from "node:fs/promises";
import { chromium } from "playwright";
import type { components } from "../src/generated/api.ts";
import type { Audit, AuditItem, Counters } from "./uiTeamScaleAudit.ts";
import type {
  ObservedAgent,
  SpawnTimeline,
  TeamOptions,
  CriteriaMetrics,
  TeamSummaryMetrics,
} from "./uiTeamScaleHelpers.ts";
import { homedir } from "node:os";
import { dirname, isAbsolute, join, relative, resolve, sep } from "node:path";
import { performance } from "node:perf_hooks";
import { setTimeout as sleep } from "node:timers/promises";
import { fileURLToPath } from "node:url";
import {
  readProcessTable,
  requireScaleModel,
  sampleProviderProcesses,
  scaleAccountKeys,
  scaleProfile,
  parseWorkerWorkspace,
  showsUpstreamLimit,
} from "./scaleVerifierHelpers.mjs";
import {
  CLEANUP_DEADLINE_MS,
  DIAGNOSTICS_INTERVAL_MS,
  ECHO_PROBE_INTERVAL_MS,
  FRAME_GAP_RECORD_MS,
  SAMPLE_INTERVAL_MS,
  USAGE,
  buildLeadPrompt,
  configureNewChat,
  cpuDeltaMs,
  cpuPercent,
  createTeamTracker,
  createEntityReader,
  openEntityStream,
  cleanupVerified,
  readTeamUiCounts,
  approvedModelUiValues,
  assertCreatedTeamProfiles,
  OWNED_CLEANUP_VERIFIED_STATE,
  evaluateCriteria,
  firstReachedMs,
  parseArgs,
  percentile,
  sqliteLockErrors,
  spawnBatchTimings,
  sqliteWriteWaits,
  stageDeadlineMs,
  summaryText,
} from "./uiTeamScaleHelpers.ts";
import {
  AUDIT_FETCH_CONCURRENCY,
  JOURNAL_GREP,
  TRANSCRIPT_PAGE_LIMIT,
  auditTeamDiagnostics,
  auditJournalResult,
  auditLead,
  auditLeadReport,
  auditToolRequests,
  auditWorker,
  auditWorkerEntity,
  diagnosticsCounters,
  mapLimited,
  mergeAudits,
  startOrderInversions,
  toolCalls,
} from "./uiTeamScaleAudit.ts";

type Schemas = components["schemas"];
type HttpRow = {
  at: string;
  method: string;
  path: string | undefined;
  status: number | null;
  elapsedMs: number | null;
};
type Probe = {
  armed: boolean;
  longTasks: { startMs: number; durationMs: number }[];
  frameGaps: { atMs: number; gapMs: number }[];
  echoMs: number[];
};
declare global {
  interface Window {
    __uiProbe: Probe;
  }
}
type TeamTracker = ReturnType<typeof createTeamTracker>;
type EntityReader = ReturnType<typeof createEntityReader>;
type ProcessTable = ReturnType<typeof readProcessTable>;
type Sample = {
  atMs: number;
  ui: ReturnType<typeof readTeamUiCounts> & { rafRoundtripMs: number };
  backendAgents: number;
  started: number;
  replied: number;
  backendCpuPercent: number | null;
  backendRssMiB: number | null;
  providerProcesses: number;
  providerCpuPercent: number | null;
  providerRssMiB: number;
  error?: never;
};
type RunJournal = {
  runTag: string;
  runId: string;
  options: TeamOptions;
  startedAt: string;
  http: HttpRow[];
  samples: (Sample | { atMs: number; error: string })[];
  diagnostics: {
    series: (ReturnType<typeof compactDiagnostics> & { atMs: number })[];
    before?: ReturnType<typeof compactDiagnostics>;
    after?: ReturnType<typeof compactDiagnostics>;
    countersBefore?: Counters;
    countersAfter?: Counters;
  };
  leadId: string | null;
  result: ReturnType<typeof evaluateCriteria> | null;
  cleanup:
    | { state: string; leadId?: string | null }
    | Awaited<ReturnType<typeof cleanup>>;
  backend?: {
    pid: number;
    build: Schemas["DesktopResponse"]["backendBuild"];
    stateDir: string;
  };
  roles?: {
    lead: Awaited<ReturnType<typeof resolveRole>>;
    worker: Awaited<ReturnType<typeof resolveRole>>;
  };
  baselineProviderProcesses?: number;
  audit?: Audit;
  metrics?: CriteriaMetrics & TeamSummaryMetrics & Record<string, unknown>;
  workerTimeline?: SpawnTimeline[];
  teamVisibleScreenshot?: { path: string; atMs: number; teamCount: number };
  chatOpenMs?: number;
  prompt?: string;
  promptSentAtMs?: number;
  sendClickMs?: number;
  deadlineMs?: number;
  deadlineReached?: boolean;
  pageErrors?: string[];
  error?: string;
  uiProbe?: Probe | null;
};
const REPO_ROOT = resolve(
  dirname(fileURLToPath(import.meta.url)),
  "../../../../..",
);
// Browser viewport; wide enough that the team panel is a side panel.
const VIEWPORT = { width: 1440, height: 900 };
// Deadline of one HTTP request and of one UI step, milliseconds.
const REQUEST_DEADLINE_MS = 30_000;
const UI_STEP_TIMEOUT_MS = 30_000;
// A hover-revealed click is retried this often, each attempt this long.
const UI_CLICK_ATTEMPTS = 3;
const UI_CLICK_TIMEOUT_MS = 10_000;
// Poll period of waits that have no event to await, milliseconds.
const POLL_MS = 250;
const BYTES_PER_MIB = 1024 * 1024;
const DIAGNOSTICS_PATH = "/api/diagnostics";
const PROVIDER_PROCESS_NAMES = ["codex", "claude"];
// Provider processes owned by one agent. `codex app-server` is shared by the
// threads of an account and outlives them by design, so it is reported but
// not a leak; loaded threads are checked through the diagnostics counter.
const AGENT_OWNED_PROCESS_NAMES = ["claude"];
// Largest backend journal read for one run window, bytes.
const JOURNAL_MAX_BYTES = 512 * 1024 * 1024;

let aborted = false;
const checkAborted = () => assert.ok(!aborted, "Interrupted.");

function mainCheckout() {
  const common = spawnSync(
    "git",
    [
      "-C",
      REPO_ROOT,
      "rev-parse",
      "--path-format=absolute",
      "--git-common-dir",
    ],
    { encoding: "utf8" },
  );
  return common.status === 0 ? dirname(common.stdout.trim()) : REPO_ROOT;
}

async function writeJson(path: string, value: unknown) {
  await writeFile(`${path}.next`, `${JSON.stringify(value, null, 2)}\n`, {
    mode: 0o600,
  });
  await rename(`${path}.next`, path);
}

async function assertOutsideCheckout(directory: string) {
  const [repo, actual] = await Promise.all([
    realpath(REPO_ROOT),
    realpath(directory),
  ]);
  const fromRepo = relative(repo, actual);
  assert.ok(
    fromRepo === ".." ||
      fromRepo.startsWith(`..${sep}`) ||
      isAbsolute(fromRepo),
    "The evidence directory must be outside the repository checkout.",
  );
}

// Studio HTTP responses use the Rust-generated endpoint contracts; existing
// run-specific assertions remain at their consumers.
function createApi(
  origin: string,
  token: string,
  journal: { http: HttpRow[] },
) {
  const headers = {
    "content-type": "application/json",
    origin,
    "x-canvas-token": token,
  };
  return async function api<T>(
    method: string,
    path: string,
    body?: unknown,
  ): Promise<T> {
    const startedAt = performance.now();
    const record: HttpRow = {
      at: new Date().toISOString(),
      method,
      path: path.split("?")[0],
      status: null,
      elapsedMs: null,
    };
    journal.http.push(record);
    try {
      const response = await fetch(new URL(path, origin), {
        method,
        headers,
        ...(body === undefined ? {} : { body: JSON.stringify(body) }),
        signal: AbortSignal.timeout(REQUEST_DEADLINE_MS),
      });
      record.status = response.status;
      const text = await response.text();
      assert.ok(
        response.ok,
        `${method} ${path} returned HTTP ${response.status}: ${text.slice(0, 300)}`,
      );
      return JSON.parse(text || "null");
    } finally {
      record.elapsedMs = Math.round(performance.now() - startedAt);
    }
  };
}

function compactDiagnostics(value: Schemas["DiagnosticsResponse"]) {
  const routes = value?.httpRequestMetrics?.routes ?? [];
  const worst = routes.reduce<(typeof routes)[number] | null>(
    (best, route) =>
      route.latencyMs.p95 > (best?.latencyMs.p95 ?? -1) ? route : best,
    null,
  );
  return {
    at: new Date().toISOString(),
    lockErrors: sqliteLockErrors(value),
    loadedThreads: Number.isSafeInteger(value?.studioLoadedThreads)
      ? value.studioLoadedThreads
      : null,
    ...(() => {
      const waits = sqliteWriteWaits(value);
      return { writeWaits: waits.count, writeWaitMs: waits.totalMs };
    })(),
    worstRouteP95: worst
      ? {
          route: `${worst.method} ${worst.route}`,
          p95Ms: worst.latencyMs.p95,
          count: worst.count,
        }
      : null,
  };
}

// Full command lines are searched so that a leaked `sleep` of the run is found.
function processesMentioning(needle: string) {
  const found = [];
  for (const entry of readdirSync("/proc")) {
    if (!/^[1-9]\d*$/.test(entry)) continue;
    try {
      const commandLine = readFileSync(`/proc/${entry}/cmdline`, "utf8");
      if (commandLine.includes(needle))
        found.push({
          pid: Number(entry),
          commandLine: commandLine.replaceAll("\0", " ").slice(0, 200),
        });
    } catch {
      /* the process ended */
    }
  }
  return found;
}

function providerPids(
  table: ProcessTable,
  backendPid: number,
  names: string[],
) {
  return new Set(
    names.flatMap((name) =>
      sampleProviderProcesses(table, backendPid, name).perProcess.map(
        (item) => item.pid,
      ),
    ),
  );
}

const PROBE_SOURCE = ({ frameGapRecordMs }: { frameGapRecordMs: number }) => {
  const probe: Probe = {
    armed: false,
    longTasks: [],
    frameGaps: [],
    echoMs: [],
  };
  window.__uiProbe = probe;
  new PerformanceObserver((list) => {
    for (const entry of list.getEntries())
      probe.longTasks.push({
        startMs: Math.round(entry.startTime),
        durationMs: Math.round(entry.duration),
      });
  }).observe({ type: "longtask", buffered: true });
  let last = performance.now();
  const frame = (now: number) => {
    if (probe.armed && now - last >= frameGapRecordMs)
      probe.frameGaps.push({
        atMs: Math.round(now),
        gapMs: Math.round(now - last),
      });
    last = now;
    requestAnimationFrame(frame);
  };
  requestAnimationFrame(frame);
  let keydownAt: number | null = null;
  document.addEventListener(
    "keydown",
    (event) => {
      if (probe.armed && event.key === "x") keydownAt = event.timeStamp;
    },
    true,
  );
  document.addEventListener(
    "input",
    () => {
      if (keydownAt === null) return;
      const started = keydownAt;
      keydownAt = null;
      requestAnimationFrame(() =>
        setTimeout(
          () => probe.echoMs.push(Math.round(performance.now() - started)),
          0,
        ),
      );
    },
    true,
  );
};

async function resolveRole(
  api: ReturnType<typeof createApi>,
  accounts: Schemas["AccountsResponse"]["accounts"],
  profile: ReturnType<typeof scaleProfile>,
) {
  const accountKeys: string[] = scaleAccountKeys(accounts, profile);
  const accountKey = accountKeys[0];
  assert.ok(accountKey);
  const catalog = await api<Schemas["ModelCatalogResponse"]>(
    "GET",
    `/api/models?account_key=${encodeURIComponent(accountKey)}`,
  );
  const candidate: Schemas["ProviderModel"] = requireScaleModel(
    catalog,
    profile,
  );
  const model = candidate.resolvedModel ?? candidate.model;
  assert.ok(typeof model === "string");
  return {
    profile,
    accountKey,
    uiModel: candidate.model,
    uiModelValues: approvedModelUiValues(catalog, candidate),
    model,
    effort: profile.effort,
  };
}

async function main({
  setCleanup,
  setRemaining,
}: {
  setCleanup(callback: () => Promise<void>): void;
  setRemaining(callback: () => unknown): void;
}) {
  const defaults = { project: mainCheckout() };
  const options = parseArgs(process.argv.slice(2), defaults);
  if (options.help) {
    process.stdout.write(`${USAGE}\n`);
    return;
  }
  assert.equal(
    process.platform,
    "linux",
    "This verifier requires Linux (/proc).",
  );
  const workerWorkspace = parseWorkerWorkspace(options.workerWorkspace);
  const runId = randomUUID();
  const runTag = `UITEAM${runId.slice(0, 8).toUpperCase()}`;
  const evidenceRoot = resolve(
    options.evidenceDir ?? join(homedir(), ".cache", "studio-ui-team-scale"),
  );
  const runDir = join(evidenceRoot, runTag);
  await mkdir(runDir, { recursive: true, mode: 0o700 });
  await assertOutsideCheckout(runDir);
  const journalPath = join(runDir, "evidence.json");
  const journal: RunJournal = {
    runTag,
    runId,
    options: { ...options, workerWorkspace },
    startedAt: new Date().toISOString(),
    http: [],
    samples: [],
    diagnostics: { series: [] },
    leadId: null,
    result: null,
    cleanup: { state: "pending" },
  };
  const save = () =>
    writeJson(journalPath, { ...journal, updatedAt: new Date().toISOString() });
  await save();
  process.stdout.write(`Run ${runTag}; evidence ${journalPath}\n`);

  const session: Schemas["SessionResponse"] = await (
    await fetch(new URL("/api/session", options.origin))
  ).json();
  assert.ok(session.token, "Backend did not provide its session token.");
  const api = createApi(options.origin, session.token, journal);
  const desktop = await api<Schemas["DesktopResponse"]>("GET", "/api/desktop");
  const backendPid = desktop.pid;
  assert.ok(typeof backendPid === "number");
  assert.ok(
    Number.isSafeInteger(backendPid),
    "Backend did not report its pid.",
  );
  journal.backend = {
    pid: backendPid,
    build: desktop.backendBuild,
    stateDir: desktop.stateDir,
  };
  const projects = (
    await api<Schemas["ProjectReadResponse"]>("GET", "/api/projects")
  ).items;
  const project = projects.find((item) => item.path === options.project);
  assert.ok(project, `${options.project} is not a registered project.`);
  const accounts = (
    await api<Schemas["AccountsResponse"]>("GET", "/api/accounts")
  ).accounts;
  const lead = await resolveRole(api, accounts, scaleProfile(options.lead));
  const workerRole = await resolveRole(
    api,
    accounts,
    scaleProfile(options.worker),
  );
  journal.roles = { lead, worker: workerRole };
  const baselineProviderPids = providerPids(
    readProcessTable(),
    backendPid,
    PROVIDER_PROCESS_NAMES,
  );
  journal.baselineProviderProcesses = baselineProviderPids.size;
  const rawDiagnosticsBefore = await api<Schemas["DiagnosticsResponse"]>(
    "GET",
    "/api/diagnostics",
  );
  const diagnosticsBefore = compactDiagnostics(rawDiagnosticsBefore);
  journal.diagnostics.before = diagnosticsBefore;
  journal.diagnostics.countersBefore =
    diagnosticsCounters(rawDiagnosticsBefore);

  const runStart = performance.now();
  const runMs = () => Math.round(performance.now() - runStart);
  const agents = new Map<string, ObservedAgent>();

  let leadId: string | null = null;
  const teamTracker: { current: TeamTracker | null } = { current: null };
  const pending: [ObservedAgent, number][] = [];
  const onAgent = (value: ObservedAgent) => {
    agents.set(value.id, value);
    if (value.deleted) return;
    if (teamTracker.current) teamTracker.current.observe(value, runMs());
    else pending.push([value, runMs()]);
  };
  const protocol = await api<Schemas["SyncProtocolResponse"]>(
    "GET",
    "/api/sync/protocol",
  );
  const pull = createEntityReader(
    (resource, receive, failed) =>
      openEntityStream({
        origin: options.origin,
        token: session.token,
        protocol,
        resource,
        timeoutMs: REQUEST_DEADLINE_MS,
        receive,
        failed,
      }),
    onAgent,
  );

  const browser = await chromium.launch({ headless: true });
  let samplerRunning = false;
  let sampler: Promise<void> | null = null;
  let promptSentAtMs: number | null = null;
  const uiApiLatenciesMs: { path: string; elapsedMs: number }[] = [];
  let runError: unknown = null;
  let leadResponseId: string | null = null;
  let messageAgentId: string | null = null;
  setRemaining(() => ({
    evidence: journalPath,
    leadId: leadId ?? leadResponseId ?? messageAgentId,
    cleanup: journal.cleanup,
    agents: [...agents.values()]
      .filter(
        (value) =>
          !value.deleted &&
          (value.id === (leadId ?? leadResponseId ?? messageAgentId) ||
            value.rootId === (leadId ?? leadResponseId ?? messageAgentId)),
      )
      .map((value) => value.id),
  }));
  setCleanup(async () => {
    // Recover ownership even when measurement or evidence persistence throws.
    leadId ??= leadResponseId ?? messageAgentId;
    journal.leadId = leadId;
    // Audit even when a measurement or evidence write failed. Deletion must
    // still run when the audit itself fails.
    journal.result ??= { passed: false, failures: ["runFailed"], checks: {} };
    try {
      if (leadId) {
        await pull();
        pull.assertComplete();
        const identity = await api<Schemas["SyncIdentityResponse"]>(
          "GET",
          "/api/sync/identity",
        );
        journal.audit = await auditRun({
          stateDir: desktop.stateDir,
          workspaceId: identity.workspaceId,
          origin: options.origin,
          token: session.token,
          backendPid,
          journalUnit: options.journalUnit,
          since: journal.startedAt,
          leadId,
          runTag,
          workerIds: (teamTracker.current?.workers() ?? []).map(
            (row) => row.id,
          ),
          entities: agents,
          timeline: journal.workerTimeline ?? [],
          rel: (ms: number | null) =>
            ms === null || promptSentAtMs === null ? null : ms - promptSentAtMs,
          tracker: teamTracker.current,
          throttling: teamTracker.current?.throttling(runMs()) ?? { events: 0 },
          diagnostics: {
            before: journal.diagnostics.countersBefore,
            after: journal.diagnostics.countersAfter,
          },
        });
        pull.assertComplete();
        const kinds = [
          ...new Set(journal.audit.failures.map((entry) => entry.kind)),
        ];
        journal.result.failures.push(...kinds);
        journal.result.passed &&= kinds.length === 0;
      }
    } catch (error) {
      journal.audit = {
        failures: [
          {
            kind: "auditFailed",
            error: String(error instanceof Error ? error.stack : error),
          },
        ],
        anomalies: [],
      };
      journal.result.passed = false;
      journal.result.failures.push("auditFailed");
    }
    try {
      // Cleanup runs on success, failure and interrupt.
      if (options.keep) {
        journal.cleanup = { state: "skipped (--keep)", leadId };
        process.stdout.write(
          `--keep: chat ${leadId} and its agents remain; inspect them in Studio.\n`,
        );
      } else if (leadId) {
        journal.cleanup = await cleanup({
          api,
          pull,
          agents,
          leadId,
          runTag,
          backendPid,
          baselineProviderPids,
          baselineLoadedThreads: diagnosticsBefore.loadedThreads,
        });
      } else {
        journal.cleanup = { state: "nothing created" };
      }
      await save();
      const summaryPath = join(runDir, "summary.txt");
      if (journal.metrics) {
        assert.ok(journal.result);
        await writeFile(
          summaryPath,
          summaryText({
            ...journal,
            result: journal.result,
            metrics: journal.metrics,
          }),
        );
        process.stdout.write(
          `\n${summaryText({ ...journal, result: journal.result, metrics: journal.metrics })}`,
        );
      }
      const cleanupOk =
        journal.cleanup.state === OWNED_CLEANUP_VERIFIED_STATE ||
        journal.cleanup.state.startsWith("skipped") ||
        journal.cleanup.state === "nothing created";
      process.stdout.write(`evidence: ${journalPath}\n`);
      if (!journal.result?.passed || !cleanupOk) process.exitCode = 1;
    } finally {
      await pull.close();
      await browser.close().catch(() => {});
    }
  });
  try {
    await pull();
    const context = await browser.newContext({ viewport: VIEWPORT });
    const page = await context.newPage();
    page.setDefaultTimeout(UI_STEP_TIMEOUT_MS);
    const pageErrors: string[] = [];
    page.on("pageerror", (error) => pageErrors.push(error.message));
    page.on("requestfinished", async (request) => {
      if (
        !request.url().includes("/api/") ||
        request.url().includes("/api/sync/stream")
      )
        return;
      const timing = request.timing();
      if (timing.responseEnd >= 0)
        uiApiLatenciesMs.push({
          path: new URL(request.url()).pathname,
          elapsedMs: Math.round(timing.responseEnd),
        });
    });
    page.on("response", async (response) => {
      const request = response.request();
      if (request.method() !== "POST") return;
      if (response.url().endsWith("/api/leads") && response.ok()) {
        leadResponseId ??=
          (await response.json().catch(() => null))?.id ?? null;
        if (leadResponseId) pull.watch(leadResponseId);
      }
      if (response.url().endsWith("/api/messages"))
        messageAgentId ??= request.postDataJSON()?.agent ?? null;
    });
    await page.addInitScript(PROBE_SOURCE, {
      frameGapRecordMs: FRAME_GAP_RECORD_MS,
    });
    await page.goto(options.origin);
    await page.locator("#message").waitFor();
    await page.evaluate(() => {
      window.__uiProbe.armed = true;
    });

    // Observation loop: one sample per period until stopped.
    const baselineTable = readProcessTable();
    let previousCpu = {
      at: performance.now(),
      backend: baselineTable.get(backendPid)?.cpuMs ?? null,
      providers: new Map(
        PROVIDER_PROCESS_NAMES.flatMap(
          (name) =>
            sampleProviderProcesses(baselineTable, backendPid, name).perProcess,
        ).map((item) => [item.pid, item.cpuMs]),
      ),
    };
    let nextDiagnosticsAt = 0;
    let nextEchoAt = 0;
    samplerRunning = true;
    sampler = (async () => {
      while (samplerRunning) {
        const tickStart = performance.now();
        try {
          await pull();
          const uiStart = performance.now();
          const ui = await page.evaluate(readTeamUiCounts, undefined);
          const uiRoundtripMs = Math.round(performance.now() - uiStart);
          if (ui.teamPanelClosed)
            await page
              .locator("#team-toggle")
              .click({ timeout: POLL_MS * 4 })
              .catch(() => {});
          const table = readProcessTable();
          const backend = table.get(backendPid);
          const providers = PROVIDER_PROCESS_NAMES.flatMap(
            (name) =>
              sampleProviderProcesses(table, backendPid, name).perProcess,
          );
          const wallMs = performance.now() - previousCpu.at;
          const workersSeen = teamTracker.current
            ? teamTracker.current.summary()
            : null;
          if (
            promptSentAtMs !== null &&
            ui.teamCount > 0 &&
            !journal.teamVisibleScreenshot
          ) {
            const path = join(runDir, "workers-visible.png");
            await page.screenshot({ path });
            journal.teamVisibleScreenshot = {
              path,
              atMs: runMs(),
              teamCount: ui.teamCount,
            };
          }
          journal.samples.push({
            atMs: runMs(),
            ui: { ...ui, rafRoundtripMs: uiRoundtripMs },
            backendAgents: workersSeen?.visible ?? 0,
            started: workersSeen?.started ?? 0,
            replied: workersSeen?.replied ?? 0,
            backendCpuPercent:
              backend && previousCpu.backend !== null
                ? cpuPercent(
                    Math.max(0, backend.cpuMs - previousCpu.backend),
                    wallMs,
                  )
                : null,
            backendRssMiB: backend
              ? Math.round(backend.rssBytes / BYTES_PER_MIB)
              : null,
            providerProcesses: providers.length,
            providerCpuPercent: cpuPercent(
              cpuDeltaMs(previousCpu.providers, providers),
              wallMs,
            ),
            providerRssMiB: Math.round(
              providers.reduce((sum, item) => sum + item.rssBytes, 0) /
                BYTES_PER_MIB,
            ),
          });
          previousCpu = {
            at: performance.now(),
            backend: backend?.cpuMs ?? null,
            providers: new Map(providers.map((item) => [item.pid, item.cpuMs])),
          };
          if (performance.now() >= nextDiagnosticsAt) {
            nextDiagnosticsAt = performance.now() + DIAGNOSTICS_INTERVAL_MS;
            journal.diagnostics.series.push({
              atMs: runMs(),
              ...compactDiagnostics(
                await api<Schemas["DiagnosticsResponse"]>(
                  "GET",
                  "/api/diagnostics",
                ),
              ),
            });
          }
          if (promptSentAtMs !== null && performance.now() >= nextEchoAt) {
            nextEchoAt = performance.now() + ECHO_PROBE_INTERVAL_MS;
            await page.locator("#message").press("x");
            await page.locator("#message").press("Backspace");
          }
        } catch (error) {
          journal.samples.push({
            atMs: runMs(),
            error: String(error instanceof Error ? error.message : error).slice(
              0,
              300,
            ),
          });
        }
        await sleep(
          Math.max(0, SAMPLE_INTERVAL_MS - (performance.now() - tickStart)),
        );
      }
    })();

    // 1. New lead chat through the dialog.
    const heading = page
      .locator(".project-tree-heading")
      .filter({ has: page.getByText(project.name, { exact: true }) })
      .first();
    const newChatButton = page.getByRole("button", {
      name: `New chat in ${project.name}`,
      exact: true,
    });
    // The row action appears on hover; a sidebar still re-rendering under load
    // can cover it, so hover and click are retried a bounded number of times.
    for (let attempt = 1; ; attempt += 1) {
      try {
        await heading.hover();
        await newChatButton.click({ timeout: UI_CLICK_TIMEOUT_MS });
        break;
      } catch (error) {
        if (attempt >= UI_CLICK_ATTEMPTS) throw error;
      }
    }
    const dialog = page.getByRole("dialog", { name: "New chat", exact: true });
    await dialog.waitFor();
    // The dialog loads accounts and the catalogue before its settings can be saved.
    await dialog
      .getByRole("radiogroup", { name: "Agent role", exact: true })
      .waitFor();
    await page.waitForFunction(
      () => !document.querySelector('[role="dialog"] [role="alert"]'),
    );
    await configureNewChat(dialog, lead, workerRole);
    checkAborted();
    const startClickAt = performance.now();
    await dialog
      .getByRole("button", { name: "Start chat", exact: true })
      .click();
    await page.waitForFunction(
      () =>
        document.querySelector("#message") &&
        !document.querySelector('[role="dialog"][aria-label="New chat"]'),
    );
    while (!leadResponseId) {
      checkAborted();
      await sleep(POLL_MS);
    }
    leadId = leadResponseId;
    pull.watch(leadId);
    journal.leadId = leadId;
    await save();
    await page
      .locator(`[data-chat="${leadId}"][aria-current="true"]`)
      .waitFor();
    journal.chatOpenMs = Math.round(performance.now() - startClickAt);
    teamTracker.current = createTeamTracker(leadId, runTag, options.workers);
    for (const [value, at] of pending.splice(0))
      teamTracker.current.observe(value, at);
    process.stdout.write(
      `Lead chat ${leadId} opened in ${journal.chatOpenMs} ms\n`,
    );

    // Stored profile of the lead and of its worker defaults, from the API.
    let leadEntity = agents.get(leadId);
    for (
      let waited = 0;
      (!leadEntity ||
        leadEntity.fastMode === undefined ||
        !leadEntity.workerDefaults) &&
      waited < REQUEST_DEADLINE_MS;
      waited += POLL_MS
    ) {
      await sleep(POLL_MS);
      leadEntity = agents.get(leadId);
    }
    assert.ok(leadEntity);
    assertCreatedTeamProfiles(leadEntity, lead, workerRole);

    // The team limit is a user setting; raise it in the UI when it is too low.
    if (!((leadEntity.concurrency ?? NaN) >= options.workers)) {
      await page
        .getByRole("button", { name: "Chat settings", exact: true })
        .click();
      const field = page.getByLabel("Subagent parallelism", { exact: true });
      await field.fill(String(options.workers));
      await field.blur();
      for (
        let waited = 0;
        !((agents.get(leadId)?.concurrency ?? NaN) >= options.workers);
        waited += POLL_MS
      ) {
        checkAborted();
        assert.ok(
          waited < REQUEST_DEADLINE_MS,
          "The subagent parallelism change was not confirmed.",
        );
        await sleep(POLL_MS);
      }
      await page.keyboard.press("Escape");
    }

    // 2. The prompt.
    const prompt = buildLeadPrompt({
      workers: options.workers,
      runTag,
      workspace: workerWorkspace,
    });
    journal.prompt = prompt;
    await page.locator("#message").fill(prompt);
    const promptSendStart = performance.now();
    await page.locator("#send").click();
    promptSentAtMs = runMs();
    journal.promptSentAtMs = promptSentAtMs;
    journal.sendClickMs = Math.round(performance.now() - promptSendStart);
    assert.equal(
      messageAgentId === null || messageAgentId === leadId,
      true,
      "The prompt went to another agent.",
    );

    // 3. Wait for the team.
    const deadlineMs = stageDeadlineMs(
      options.workers,
      scaleAccountKeys(accounts, scaleProfile(options.worker)).length,
    );
    journal.deadlineMs = deadlineMs;
    let lastLine = "";
    for (;;) {
      checkAborted();
      const summary = teamTracker.current.summary();
      const line = `visible ${summary.visible}/${options.workers} started ${summary.started} replied ${summary.replied} lead ${summary.leadReportedAtMs === null ? "waiting" : "reported"}`;
      if (line !== lastLine)
        process.stdout.write(
          `[${((runMs() - promptSentAtMs) / 1000).toFixed(0)}s] ${line}\n`,
        );
      lastLine = line;
      if (
        summary.replied === options.workers &&
        summary.leadReportedAtMs !== null
      )
        break;
      if (runMs() - promptSentAtMs > deadlineMs) {
        journal.deadlineReached = true;
        break;
      }
      await sleep(POLL_MS);
    }
    await sleep(SAMPLE_INTERVAL_MS * 2);
    // Stop the probe typing before the screenshot and leave the composer empty.
    samplerRunning = false;
    await sampler;
    await page.locator("#message").fill("");
    await page.screenshot({ path: join(runDir, "final.png") });
    journal.pageErrors = pageErrors;
  } catch (error) {
    runError = error;
    journal.error = String(error instanceof Error ? error.stack : error).slice(
      0,
      2000,
    );
    process.stderr.write(`Run failed: ${journal.error}\n`);
  } finally {
    samplerRunning = false;
    await sampler?.catch(() => {});
    // A chat created whose id was not yet adopted is still owned by this run.
    leadId ??= leadResponseId ?? messageAgentId;
    journal.leadId = leadId;
  }

  // Measurements.
  try {
    const probePage = browser.contexts()[0]?.pages()[0];
    const probe = probePage
      ? await probePage.evaluate(() => window.__uiProbe)
      : null;
    const rawDiagnosticsAfter = await api<Schemas["DiagnosticsResponse"]>(
      "GET",
      "/api/diagnostics",
    );
    const diagnosticsAfter = compactDiagnostics(rawDiagnosticsAfter);
    journal.diagnostics.after = diagnosticsAfter;
    journal.diagnostics.countersAfter =
      diagnosticsCounters(rawDiagnosticsAfter);
    const summary = teamTracker.current?.summary() ?? {
      started: 0,
      replied: 0,
      leadReportedAtMs: null,
      missingTokens: [],
      accountDistribution: {},
    };
    const throttling = teamTracker.current?.throttling(runMs()) ?? {
      events: 0,
      agents: 0,
      waitMs: 0,
    };
    const good = journal.samples.filter(
      (sample): sample is Sample => !sample.error,
    );
    const rel = (ms: number | null) =>
      ms === null || promptSentAtMs === null ? null : ms - promptSentAtMs;
    const peak = (
      key:
        | "backendCpuPercent"
        | "backendRssMiB"
        | "providerCpuPercent"
        | "providerRssMiB"
        | "providerProcesses",
    ) => good.reduce((best, sample) => Math.max(best, sample[key] ?? 0), 0);
    // The diagnostics endpoint is the instrument, not a user path: reported
    // on its own and excluded from the API p95 pass limit.
    const apiRows = [
      ...journal.http.map((row) => ({
        path: row.path,
        elapsedMs: row.elapsedMs,
      })),
      ...uiApiLatenciesMs,
    ].filter(
      (row): row is { path: string | undefined; elapsedMs: number } =>
        row.elapsedMs !== null,
    );
    const allApiMs = apiRows
      .filter((row) => row.path !== DIAGNOSTICS_PATH)
      .map((row) => row.elapsedMs);
    const diagnosticsApiMs = apiRows
      .filter((row) => row.path === DIAGNOSTICS_PATH)
      .map((row) => row.elapsedMs);
    const workerRows = teamTracker.current?.workers() ?? [];
    const workerTimeline = workerRows.map((row) => ({
      ...row,
      startedAfterPromptMs: rel(row.startedAtMs),
      completedAfterPromptMs: rel(row.completedAtMs),
    }));
    const waitsBefore = diagnosticsBefore.writeWaits;
    // Before the prompt the page may still show the previously open chat's
    // team, so visibility counts only samples taken after the prompt.
    const afterPrompt = good.filter(
      (sample) => promptSentAtMs !== null && sample.atMs >= promptSentAtMs,
    );
    const metrics = {
      uiFirstVisibleMs: rel(
        firstReachedMs(afterPrompt, (s) => s.ui.teamCount, 1),
      ),
      uiAllVisibleMs: rel(
        firstReachedMs(afterPrompt, (s) => s.ui.teamCount, options.workers),
      ),
      uiAllCardsMs: rel(
        firstReachedMs(afterPrompt, (s) => s.ui.workerCards, options.workers),
      ),
      backendFirstVisibleMs: rel(
        firstReachedMs(afterPrompt, (s) => s.backendAgents, 1),
      ),
      backendAllVisibleMs: rel(
        firstReachedMs(afterPrompt, (s) => s.backendAgents, options.workers),
      ),
      started: summary.started,
      replied: summary.replied,
      accountDistribution: summary.accountDistribution ?? {},
      spawnBatches: spawnBatchTimings(
        workerTimeline.map((row) => ({
          ...row,
          firstSeenAtMs: rel(row.firstSeenAtMs),
          startedAtMs: rel(row.startedAtMs),
          completedAtMs: rel(row.completedAtMs),
        })),
        options.workers,
      ),
      throttleEvents: throttling.events,
      throttledAgents: throttling.agents,
      throttleWaitMs: throttling.waitMs,
      missingTokens: summary.missingTokens,
      leadReportedAtMs: rel(summary.leadReportedAtMs),
      maxFrameGapMs: probe
        ? probe.frameGaps.reduce((best, gap) => Math.max(best, gap.gapMs), 0)
        : null,
      maxEchoMs: probe?.echoMs.length ? Math.max(...probe.echoMs) : null,
      echoSamples: probe?.echoMs.length ?? 0,
      longTaskCount: probe?.longTasks.length ?? 0,
      longestLongTaskMs: probe
        ? probe.longTasks.reduce(
            (best, task) => Math.max(best, task.durationMs),
            0,
          )
        : null,
      maxRafRoundtripMs: good.reduce(
        (best, sample) => Math.max(best, sample.ui.rafRoundtripMs),
        0,
      ),
      sidebarChatsPeak: good.reduce(
        (best, sample) => Math.max(best, sample.ui.sidebarChats),
        0,
      ),
      apiP95Ms: percentile(allApiMs, 0.95),
      apiMaxMs: allApiMs.length ? Math.max(...allApiMs) : null,
      apiRequestCount: allApiMs.length,
      // Request count per path, most frequent first, so a request-volume
      // regression can be attributed to the endpoint that causes it.
      apiRequestsByPath: Object.fromEntries(
        [
          ...apiRows
            .reduce(
              (counts, row) =>
                counts.set(row.path, (counts.get(row.path) ?? 0) + 1),
              new Map(),
            )
            .entries(),
        ].sort((left, right) => right[1] - left[1]),
      ),
      diagnosticsP95Ms: percentile(diagnosticsApiMs, 0.95),
      sqliteLockErrorDelta:
        diagnosticsAfter.lockErrors !== null &&
        diagnosticsBefore.lockErrors !== null
          ? diagnosticsAfter.lockErrors - diagnosticsBefore.lockErrors
          : null,
      sqliteWriteWaitDelta:
        waitsBefore !== null && diagnosticsAfter.writeWaits !== null
          ? diagnosticsAfter.writeWaits - waitsBefore
          : null,
      sqliteWriteWaitMsDelta:
        diagnosticsBefore.writeWaitMs !== null &&
        diagnosticsAfter.writeWaitMs !== null
          ? Math.round(
              diagnosticsAfter.writeWaitMs - diagnosticsBefore.writeWaitMs,
            )
          : null,
      backendCpuPercentPeak: peak("backendCpuPercent"),
      backendRssMiBPeak: peak("backendRssMiB"),
      providerCpuPercentPeak: peak("providerCpuPercent"),
      providerRssMiBPeak: peak("providerRssMiB"),
      providerProcessesPeak: peak("providerProcesses"),
      workerErrors: workerRows
        .filter((row) => row.error)
        .map((row) => ({
          id: row.id,
          error: String(row.error).slice(0, 200),
          upstreamLimit: showsUpstreamLimit(row.error),
        })),
    };
    journal.metrics = metrics;
    journal.uiProbe = probe;
    journal.workerTimeline = workerTimeline;
    journal.result = runError
      ? { passed: false, failures: ["runFailed"], checks: {} }
      : evaluateCriteria(metrics, options.workers);
    if (journal.deadlineReached) journal.result.failures.push("stageDeadline");
    journal.result.passed &&= !journal.deadlineReached;
  } catch (error) {
    journal.result = {
      passed: false,
      failures: [
        `measurementFailed: ${error instanceof Error ? error.message : error}`,
      ],
      checks: {},
    };
    runError ??= error;
  }
  await save();
}

// Strict audit before the cleanup deletes the team: transcripts, tool
// requests, backend journal and diagnostics of the run window. Its requests
// are kept out of the run's API latency journal.
async function auditRun({
  stateDir,
  workspaceId,
  origin,
  token,
  backendPid,
  journalUnit,
  since,
  leadId,
  runTag,
  workerIds,
  entities,
  timeline,
  rel,
  tracker,
  throttling,
  diagnostics,
}: {
  stateDir: string;
  workspaceId: string;
  origin: string;
  token: string;
  backendPid: number;
  journalUnit: string;
  since: string;
  leadId: string;
  runTag: string;
  workerIds: string[];
  entities: Map<string, ObservedAgent>;
  timeline: SpawnTimeline[];
  rel: (ms: number | null) => number | null;
  tracker: TeamTracker | null;
  throttling: { events: number };
  diagnostics: { before: Counters | undefined; after: Counters | undefined };
}) {
  const api = createApi(origin, token, { http: [] });
  const transcript = async (id: string) => {
    const items: AuditItem[] = [];
    let before: string | null | undefined = null;
    do {
      const page: Schemas["TranscriptPageResponse"] = await api<
        Schemas["TranscriptPageResponse"]
      >(
        "GET",
        `/api/transcript/page?id=${encodeURIComponent(id)}&limit=${TRANSCRIPT_PAGE_LIMIT}${before ? `&before=${encodeURIComponent(before)}` : ""}`,
      );
      items.unshift(...page.items);
      before = page.nextCursor;
    } while (before);
    return items;
  };
  const parts: Partial<Audit>[] = [];
  const desktop = await api<Schemas["DesktopResponse"]>("GET", "/api/desktop");
  if (desktop.pid !== backendPid)
    parts.push({
      failures: [
        { kind: "backendRestarted", from: backendPid, to: desktop.pid },
      ],
    });
  const leadItems = await transcript(leadId);
  const lead = auditLead(leadItems, workerIds, `${runTag}_ALL_REPLIED`);
  parts.push(lead);
  const requests = (
    await api<Schemas["ToolRequestList"]>(
      "GET",
      `/api/tool-requests?agent=${encodeURIComponent(leadId)}`,
    )
  ).requests;
  parts.push(auditToolRequests(requests, toolCalls(leadItems)));
  const workers = await mapLimited(
    workerIds,
    AUDIT_FETCH_CONCURRENCY,
    async (id) => auditWorker(id, await transcript(id), leadId),
  );
  parts.push(...workers);
  parts.push({
    failures: workerIds.flatMap((id) =>
      entities.get(id) ? auditWorkerEntity(entities.get(id) ?? { id }) : [],
    ),
  });
  const summary = tracker?.summary();
  parts.push({
    failures: auditLeadReport(
      (tracker?.leadReports() ?? []).map(rel).filter((ms) => ms !== null),
      rel(summary?.lastReplyMs ?? null),
    ),
  });
  parts.push({
    anomalies: startOrderInversions(
      timeline.map((row) => ({
        accountKey: row.accountKey,
        firstSeenAtMs: row.firstSeenAtMs,
        startedAtMs: row.startedAtMs,
      })),
    ),
  });
  if (throttling.events)
    parts.push({ anomalies: [{ kind: "providerRetries", ...throttling }] });
  const diagnosticsAudit = auditTeamDiagnostics({
    before: diagnostics.before,
    after: diagnostics.after,
    stateDir,
    workspaceId,
    agentIds: [leadId, ...workerIds],
    deadlineAt: performance.now() + REQUEST_DEADLINE_MS,
  });
  parts.push(diagnosticsAudit);
  const journalRead = spawnSync(
    "journalctl",
    [
      "--user",
      "--unit",
      journalUnit,
      "--since",
      `@${Math.floor(Date.parse(since) / 1000)}`,
      "--output",
      "short-iso-precise",
      "--no-pager",
      "--quiet",
      "--case-sensitive=false",
      "--grep",
      JOURNAL_GREP,
    ],
    { encoding: "utf8", maxBuffer: JOURNAL_MAX_BYTES },
  );
  parts.push(auditJournalResult(journalRead, journalUnit));
  return {
    ...mergeAudits(parts),
    teamInputAudit: diagnosticsAudit.teamInputAudit,
    queueGauges: diagnosticsAudit.queueGauges,
    leadTurns: lead.turns,
    leadChildResults: lead.childResults,
    leadMaxInputWaitMs: lead.maxInputWaitMs,
    workerTurns: workers.reduce((sum, worker) => sum + worker.turns, 0),
    leadToolRequests: requests.length,
  };
}

async function cleanup({
  api,
  pull,
  agents,
  leadId,
  runTag,
  backendPid,
  baselineProviderPids,
  baselineLoadedThreads,
}: {
  api: ReturnType<typeof createApi>;
  pull: EntityReader;
  agents: Map<string, ObservedAgent>;
  leadId: string;
  runTag: string;
  backendPid: number;
  baselineProviderPids: Set<number>;
  baselineLoadedThreads: number | null;
}) {
  const state: {
    state: string;
    leadId: string;
    deleteRequest: string | null;
    remaining: number | null;
    leakedProviderPids: number[];
    sharedAppServersAdded: number | null;
    loadedThreads: number | null;
    baselineLoadedThreads: number | null;
    loadedThreadAudit: string;
    providerProcessAudit: string;
    leakedProcesses: ReturnType<typeof processesMentioning>;
    observationError?: string;
    ownedAgents?: number;
    elapsedMs?: number;
  } = {
    state: "started",
    leadId,
    deleteRequest: null,
    remaining: null,
    leakedProviderPids: [],
    sharedAppServersAdded: null,
    loadedThreads: null,
    baselineLoadedThreads,
    loadedThreadAudit:
      "unavailable: diagnostics exposes counts only; global count is evidence only",
    providerProcessAudit:
      "owned agent/thread identities matched against provider command lines; processes without an exposed identity cannot be attributed",
    leakedProcesses: [],
  };
  const owned = () =>
    [...agents.values()].filter(
      (value) => value.id === leadId || value.rootId === leadId,
    );
  const startedAt = performance.now();
  const ownedIdentity = new Set([leadId]);
  try {
    await pull().catch((error) => {
      state.observationError = String(
        error instanceof Error ? error.message : error,
      );
    });
    state.ownedAgents = owned().length;
    for (const agent of owned()) {
      ownedIdentity.add(agent.id);
      if (agent.threadId) ownedIdentity.add(agent.threadId);
    }
    // One delete command: the server removes the lead and every descendant. A
    // lost reply is resolved by observation, never by a second command.
    try {
      await api("POST", "/api/conversation/delete", { id: leadId });
      state.deleteRequest = "confirmed";
    } catch (error) {
      state.deleteRequest = `unknown: ${String(error instanceof Error ? error.message : error).slice(0, 200)}`;
    }
    for (;;) {
      await pull();
      const live = owned().filter((value) => !value.deleted);
      state.remaining = live.length;
      const table = readProcessTable();
      const providerLeaks = [
        ...providerPids(table, backendPid, AGENT_OWNED_PROCESS_NAMES),
      ].filter((pid) => {
        try {
          const command = readFileSync(`/proc/${pid}/cmdline`, "utf8");
          return [...ownedIdentity].some((id) => command.includes(id));
        } catch {
          return false;
        }
      });
      state.sharedAppServersAdded = [
        ...providerPids(table, backendPid, PROVIDER_PROCESS_NAMES),
      ].filter(
        (pid) => !baselineProviderPids.has(pid) && !providerLeaks.includes(pid),
      ).length;
      state.loadedThreads = compactDiagnostics(
        await api<Schemas["DiagnosticsResponse"]>("GET", DIAGNOSTICS_PATH),
      ).loadedThreads;
      const toolLeaks = processesMentioning(runTag);
      state.leakedProviderPids = providerLeaks;
      state.leakedProcesses = toolLeaks;
      let complete = true;
      try {
        pull.assertComplete();
      } catch (error) {
        complete = false;
        state.observationError = String(
          error instanceof Error ? error.message : error,
        );
      }
      if (complete && cleanupVerified(state)) {
        state.state = OWNED_CLEANUP_VERIFIED_STATE;
        break;
      }
      if (performance.now() - startedAt > CLEANUP_DEADLINE_MS) {
        state.state = "FAILED: agents, threads or processes remain";
        break;
      }
      await sleep(POLL_MS * 4);
    }
  } catch (error) {
    state.state = `FAILED: ${String(error instanceof Error ? error.message : error).slice(0, 300)}`;
  }
  state.elapsedMs = Math.round(performance.now() - startedAt);
  process.stdout.write(
    `cleanup: ${state.state} (${state.ownedAgents ?? "?"} agents, ${state.elapsedMs} ms)\n`,
  );
  return state;
}

runScaleVerifier(main, {
  onInterrupt() {
    aborted = true;
  },
}).catch((error) => {
  process.stderr.write(`${error instanceof Error ? error.stack : error}\n`);
  process.exitCode = 1;
});
