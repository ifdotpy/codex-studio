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
import { createRequire } from "node:module";
import { homedir } from "node:os";
import { dirname, isAbsolute, join, relative, resolve, sep } from "node:path";
import { performance } from "node:perf_hooks";
import { setTimeout as sleep } from "node:timers/promises";
import { fileURLToPath } from "node:url";
import {
  assertStoredScaleProfile,
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
  cpuDeltaMs,
  cpuPercent,
  createTeamTracker,
  evaluateCriteria,
  firstReachedMs,
  parseArgs,
  percentile,
  sqliteLockErrors,
  spawnBatchTimings,
  sqliteWriteWaits,
  stageDeadlineMs,
  summaryText,
} from "./uiTeamScaleHelpers.mjs";
import {
  AUDIT_FETCH_CONCURRENCY,
  JOURNAL_GREP,
  TRANSCRIPT_PAGE_LIMIT,
  auditDiagnostics,
  auditJournal,
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
} from "./uiTeamScaleAudit.mjs";

const REPO_ROOT = resolve(
  dirname(fileURLToPath(import.meta.url)),
  "../../../../..",
);
const WEB_ROOT = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const { chromium } = createRequire(join(WEB_ROOT, "package.json"))(
  "playwright",
);

// Browser viewport; wide enough that the team panel is a side panel.
const VIEWPORT = { width: 1440, height: 900 };
// Deadline of one HTTP request and of one UI step, milliseconds.
const REQUEST_DEADLINE_MS = 30_000;
const UI_STEP_TIMEOUT_MS = 30_000;
// A hover-revealed click is retried this often, each attempt this long.
const UI_CLICK_ATTEMPTS = 3;
const UI_CLICK_TIMEOUT_MS = 10_000;
// Entity documents per sync pull page.
const PULL_PAGE_LIMIT = 500;
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

async function writeJson(path, value) {
  await writeFile(`${path}.next`, `${JSON.stringify(value, null, 2)}\n`, {
    mode: 0o600,
  });
  await rename(`${path}.next`, path);
}

async function assertOutsideCheckout(directory) {
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

function createApi(origin, token, journal) {
  const headers = {
    "content-type": "application/json",
    origin,
    "x-canvas-token": token,
    "X-Codex-Sync-Protocol": "3",
  };
  return async function api(method, path, body) {
    const startedAt = performance.now();
    const record = {
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
      return text ? JSON.parse(text) : null;
    } finally {
      record.elapsedMs = Math.round(performance.now() - startedAt);
    }
  };
}

// Incremental reader of the agent entities that change after `cursor`.
function createEntityReader(api, onAgent) {
  let cursor = null;
  return async function pull() {
    if (cursor === null) {
      const first = await api(
        "GET",
        "/api/sync/pull?scope=state:entities:v1&after=0&fresh=1&reset=1&limit=1",
      );
      cursor = first.maxSeq;
      return;
    }
    for (;;) {
      const page = await api(
        "GET",
        `/api/sync/pull?scope=state:entities:v1&after=${cursor}&limit=${PULL_PAGE_LIMIT}`,
      );
      for (const document of page.documents) {
        const id = /^entity:agent:(.+)$/.exec(document.id)?.[1];
        if (!id) continue;
        if (document._deleted) onAgent({ id, deletedAt: true, deleted: true });
        else {
          const entity = JSON.parse(document.payload);
          if (entity.collection === "agent")
            onAgent({
              ...entity.value,
              deleted: Boolean(entity.value.deletedAt),
            });
        }
      }
      const next = page.checkpoint.seq;
      if (!page.documents.length || next === cursor) return;
      cursor = next;
    }
  };
}

function compactDiagnostics(value) {
  const routes = value?.httpRequestMetrics?.routes ?? [];
  const worst = routes.reduce(
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
function processesMentioning(needle) {
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

function providerPids(table, backendPid, names) {
  return new Set(
    names.flatMap((name) =>
      sampleProviderProcesses(table, backendPid, name).perProcess.map(
        (item) => item.pid,
      ),
    ),
  );
}

const PROBE_SOURCE = ({ frameGapRecordMs }) => {
  const probe = { armed: false, longTasks: [], frameGaps: [], echoMs: [] };
  window.__uiProbe = probe;
  new PerformanceObserver((list) => {
    for (const entry of list.getEntries())
      probe.longTasks.push({
        startMs: Math.round(entry.startTime),
        durationMs: Math.round(entry.duration),
      });
  }).observe({ type: "longtask", buffered: true });
  let last = performance.now();
  const frame = (now) => {
    if (probe.armed && now - last >= frameGapRecordMs)
      probe.frameGaps.push({
        atMs: Math.round(now),
        gapMs: Math.round(now - last),
      });
    last = now;
    requestAnimationFrame(frame);
  };
  requestAnimationFrame(frame);
  let keydownAt = null;
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

const UI_COUNTS_SOURCE = () => ({
  teamCount: [...document.querySelectorAll("[data-team-count] dd")].reduce(
    (sum, node) => sum + Number(node.textContent),
    0,
  ),
  // The collapsed Finished group renders no cards, so its overview count stands
  // in for them: this still reaches the team size once every card is reachable.
  workerCards:
    document.querySelectorAll("[data-worker]").length +
    Number(
      document.querySelector('[data-team-count="completed"] dd')?.textContent ||
        0,
    ),
  sidebarChats: document.querySelectorAll("#sidebar [data-chat]").length,
  teamPanelClosed:
    document.querySelector("#team-toggle")?.getAttribute("aria-expanded") ===
    "false",
});

// Operate a visible segmented control or the account tiles with real clicks.
async function chooseSetupValue(locator, value) {
  const segment = locator.locator(`input[value="${value}"]`);
  if (
    (await locator.getAttribute("role")) === "radiogroup" &&
    (await segment.count())
  ) {
    const id = await segment.getAttribute("id");
    await locator.locator(`label[for="${id}"]`).click();
    return;
  }
  const tile = locator.locator(`[data-account-key="${value}"]`);
  // A tile of another provider shows after that provider is selected.
  for (const provider of await locator.locator('input[type="radio"]').all()) {
    if (await tile.isVisible()) break;
    const id = await provider.getAttribute("id");
    await locator.locator(`label[for="${id}"]`).click();
  }
  await tile.click();
}

async function configureNewChat(page, dialog, lead, worker) {
  const main = await dialog.getByRole("button", {
    name: "Main agent settings",
    exact: true,
  });
  await main.click();
  const mainPopover = page.getByRole("dialog", {
    name: "Main agent settings",
    exact: true,
  });
  if (lead.accountKey)
    await chooseSetupValue(
      mainPopover.getByLabel("Main agent account", { exact: true }),
      lead.accountKey,
    );
  const mainModel = mainPopover.getByLabel("Main agent model", { exact: true });
  await mainModel.locator(`[data-value="${lead.uiModel}"]`).click();
  await page.waitForFunction(
    ([label, value]) =>
      document
        .querySelector(`[aria-label="${label}"]`)
        ?.getAttribute("data-value") === value,
    ["Main agent model", lead.uiModel],
  );
  await chooseSetupValue(
    mainPopover.getByLabel("Main agent reasoning", { exact: true }),
    lead.effort,
  );
  await page.keyboard.press("Escape");
  await mainPopover.waitFor({ state: "hidden" });
  await dialog
    .getByRole("button", { name: "Subagent defaults", exact: true })
    .click();
  const workerPopover = page.getByRole("dialog", {
    name: "Subagent defaults",
    exact: true,
  });
  await workerPopover
    .getByLabel("Default subagent model", { exact: true })
    .locator(`[data-value="${worker.uiModel}"]`)
    .click();
  await chooseSetupValue(
    workerPopover.getByLabel("Default subagent reasoning", { exact: true }),
    worker.effort,
  );
  await page.keyboard.press("Escape");
  await workerPopover.waitFor({ state: "hidden" });
}

async function resolveRole(api, accounts, profile) {
  const accountKeys = scaleAccountKeys(accounts, profile);
  const catalog = await api(
    "GET",
    `/api/models?account_key=${encodeURIComponent(accountKeys[0])}`,
  );
  const candidate = requireScaleModel(catalog, profile);
  return {
    profile,
    accountKey: accountKeys[0],
    uiModel: candidate.model,
    model: candidate.resolvedModel ?? candidate.model,
    effort: profile.effort,
  };
}

async function main({ setCleanup, setRemaining }) {
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
  const journal = {
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

  const session = await (
    await fetch(new URL("/api/session", options.origin))
  ).json();
  assert.ok(session.token, "Backend did not provide its session token.");
  const api = createApi(options.origin, session.token, journal);
  const desktop = await api("GET", "/api/desktop");
  const backendPid = desktop.pid;
  assert.ok(
    Number.isSafeInteger(backendPid),
    "Backend did not report its pid.",
  );
  journal.backend = {
    pid: backendPid,
    build: desktop.backendBuild,
    stateDir: desktop.stateDir,
  };
  const projects = (await api("GET", "/api/projects")).items;
  const project = projects.find((item) => item.path === options.project);
  assert.ok(project, `${options.project} is not a registered project.`);
  const accounts = (await api("GET", "/api/accounts")).accounts;
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
  const rawDiagnosticsBefore = await api("GET", "/api/diagnostics");
  const diagnosticsBefore = compactDiagnostics(rawDiagnosticsBefore);
  journal.diagnostics.before = diagnosticsBefore;
  journal.diagnostics.countersBefore =
    diagnosticsCounters(rawDiagnosticsBefore);

  const runStart = performance.now();
  const runMs = () => Math.round(performance.now() - runStart);
  const agents = new Map();
  let leadId = null;
  let tracker = null;
  const pending = [];
  const onAgent = (value) => {
    agents.set(value.id, value);
    if (value.deleted) return;
    if (tracker) tracker.observe(value, runMs());
    else pending.push([value, runMs()]);
  };
  const pull = createEntityReader(api, onAgent);
  await pull();

  const browser = await chromium.launch({ headless: true });
  let samplerRunning = false;
  let sampler = null;
  let promptSentAtMs = null;
  const uiApiLatenciesMs = [];
  let runError = null;
  let leadResponseId = null;
  let messageAgentId = null;
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
        journal.audit = await auditRun({
          origin: options.origin,
          token: session.token,
          backendPid,
          journalUnit: options.journalUnit,
          since: journal.startedAt,
          leadId,
          runTag,
          workerIds: (tracker?.workers() ?? []).map((row) => row.id),
          entities: agents,
          timeline: journal.workerTimeline ?? [],
          rel: (ms) =>
            ms === null || promptSentAtMs === null ? null : ms - promptSentAtMs,
          tracker,
          throttling: tracker?.throttling(runMs()) ?? { events: 0 },
          diagnostics: {
            before: journal.diagnostics.countersBefore,
            after: journal.diagnostics.countersAfter,
          },
        });
        const kinds = [
          ...new Set(journal.audit.failures.map((entry) => entry.kind)),
        ];
        journal.result.failures.push(...kinds);
        journal.result.passed &&= kinds.length === 0;
      }
    } catch (error) {
      journal.audit = {
        failures: [
          { kind: "auditFailed", error: String(error?.stack ?? error) },
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
        await writeFile(summaryPath, summaryText(journal));
        process.stdout.write(`\n${summaryText(journal)}`);
      }
      const cleanupOk =
        journal.cleanup.state === "verified" ||
        journal.cleanup.state.startsWith("skipped") ||
        journal.cleanup.state === "nothing created";
      process.stdout.write(`evidence: ${journalPath}\n`);
      if (!journal.result?.passed || !cleanupOk) process.exitCode = 1;
    } finally {
      await browser.close().catch(() => {});
    }
  });
  try {
    const context = await browser.newContext({ viewport: VIEWPORT });
    const page = await context.newPage();
    page.setDefaultTimeout(UI_STEP_TIMEOUT_MS);
    const pageErrors = [];
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
      if (response.url().endsWith("/api/leads") && response.ok())
        leadResponseId ??=
          (await response.json().catch(() => null))?.id ?? null;
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
          const ui = await page.evaluate(UI_COUNTS_SOURCE);
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
          const workersSeen = tracker ? tracker.summary() : null;
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
              ...compactDiagnostics(await api("GET", "/api/diagnostics")),
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
            error: String(error?.message ?? error).slice(0, 300),
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
      .getByRole("button", { name: "Main agent settings", exact: true })
      .waitFor();
    await page.waitForFunction(
      () => !document.querySelector('[role="dialog"] [role="alert"]'),
    );
    await configureNewChat(page, dialog, lead, workerRole);
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
    journal.leadId = leadId;
    await save();
    await page
      .locator(`[data-chat="${leadId}"][aria-current="true"]`)
      .waitFor();
    journal.chatOpenMs = Math.round(performance.now() - startClickAt);
    tracker = createTeamTracker(leadId, runTag, options.workers);
    for (const [value, at] of pending.splice(0)) tracker.observe(value, at);
    process.stdout.write(
      `Lead chat ${leadId} opened in ${journal.chatOpenMs} ms\n`,
    );

    // Stored profile of the lead and of its worker defaults, from the API.
    let leadEntity = agents.get(leadId);
    for (
      let waited = 0;
      !leadEntity && waited < REQUEST_DEADLINE_MS;
      waited += POLL_MS
    ) {
      await sleep(POLL_MS);
      leadEntity = agents.get(leadId);
    }
    assertStoredScaleProfile(leadEntity, {
      id: leadId,
      model: lead.model,
      effort: lead.effort,
      fastMode: false,
    });
    assert.equal(
      leadEntity.accountKey,
      lead.accountKey,
      "The lead is not on the selected account.",
    );
    assert.equal(
      leadEntity.workerDefaults?.model,
      workerRole.model,
      "Worker default model differs from the approved model.",
    );
    assert.equal(
      leadEntity.workerDefaults?.effort,
      workerRole.effort,
      "Worker default effort differs from the approved effort.",
    );

    // The team limit is a user setting; raise it in the UI when it is too low.
    if (!(leadEntity.concurrency >= options.workers)) {
      await page
        .getByRole("button", { name: "Chat settings", exact: true })
        .click();
      const field = page.getByLabel("Subagent parallelism", { exact: true });
      await field.fill(String(options.workers));
      await field.blur();
      for (
        let waited = 0;
        !(agents.get(leadId)?.concurrency >= options.workers);
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
      const summary = tracker.summary();
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
    journal.error = String(error?.stack ?? error).slice(0, 2000);
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
    const probe = browser.contexts()[0]?.pages()[0]
      ? await browser
          .contexts()[0]
          .pages()[0]
          .evaluate(() => window.__uiProbe)
      : null;
    const rawDiagnosticsAfter = await api("GET", "/api/diagnostics");
    const diagnosticsAfter = compactDiagnostics(rawDiagnosticsAfter);
    journal.diagnostics.after = diagnosticsAfter;
    journal.diagnostics.countersAfter =
      diagnosticsCounters(rawDiagnosticsAfter);
    const summary = tracker?.summary() ?? {
      started: 0,
      replied: 0,
      leadReportedAtMs: null,
      missingTokens: [],
    };
    const throttling = tracker?.throttling(runMs()) ?? {
      events: 0,
      agents: 0,
      waitMs: 0,
    };
    const good = journal.samples.filter((sample) => !sample.error);
    const rel = (ms) =>
      ms === null || promptSentAtMs === null ? null : ms - promptSentAtMs;
    const peak = (key) =>
      good.reduce((best, sample) => Math.max(best, sample[key] ?? 0), 0);
    // The diagnostics endpoint is the instrument, not a user path: reported
    // on its own and excluded from the API p95 pass limit.
    const apiRows = [
      ...journal.http.map((row) => ({
        path: row.path,
        elapsedMs: row.elapsedMs,
      })),
      ...uiApiLatenciesMs,
    ].filter((row) => row.elapsedMs !== null);
    const allApiMs = apiRows
      .filter((row) => row.path !== DIAGNOSTICS_PATH)
      .map((row) => row.elapsedMs);
    const diagnosticsApiMs = apiRows
      .filter((row) => row.path === DIAGNOSTICS_PATH)
      .map((row) => row.elapsedMs);
    const workerRows = tracker?.workers() ?? [];
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
    journal.metrics = {
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
    journal.uiProbe = probe;
    journal.workerTimeline = workerTimeline;
    journal.result = runError
      ? { passed: false, failures: ["runFailed"], checks: {} }
      : evaluateCriteria(journal.metrics, options.workers);
    if (journal.deadlineReached) journal.result.failures.push("stageDeadline");
    journal.result.passed &&= !journal.deadlineReached;
  } catch (error) {
    journal.result = {
      passed: false,
      failures: [`measurementFailed: ${error?.message}`],
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
}) {
  const api = createApi(origin, token, { http: [] });
  const transcript = async (id) => {
    const items = [];
    let before = null;
    do {
      const page = await api(
        "GET",
        `/api/transcript/page?id=${encodeURIComponent(id)}&limit=${TRANSCRIPT_PAGE_LIMIT}${before ? `&before=${encodeURIComponent(before)}` : ""}`,
      );
      items.unshift(...page.items);
      before = page.nextCursor;
    } while (before);
    return items;
  };
  const parts = [];
  const desktop = await api("GET", "/api/desktop");
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
    await api("GET", `/api/tool-requests?agent=${encodeURIComponent(leadId)}`)
  ).requests;
  parts.push(auditToolRequests(requests, toolCalls(leadItems)));
  const workers = await mapLimited(
    workerIds,
    AUDIT_FETCH_CONCURRENCY,
    async (id) => auditWorker(id, await transcript(id)),
  );
  parts.push(...workers);
  parts.push({
    failures: workerIds.flatMap((id) =>
      entities.has(id) ? auditWorkerEntity(entities.get(id)) : [],
    ),
  });
  const summary = tracker?.summary();
  parts.push({
    failures: auditLeadReport(
      (tracker?.leadReports() ?? []).map(rel),
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
  parts.push(auditDiagnostics(diagnostics.before, diagnostics.after));
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
  if (journalRead.status === 0)
    parts.push(auditJournal(journalRead.stdout.split("\n")));
  else
    parts.push({
      anomalies: [
        {
          kind: "journalUnavailable",
          unit: journalUnit,
          error: String(journalRead.error?.message ?? journalRead.stderr).slice(
            0,
            300,
          ),
        },
      ],
    });
  return {
    ...mergeAudits(parts),
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
}) {
  const state = {
    state: "started",
    leadId,
    deleteRequest: null,
    remaining: null,
    leakedProviderPids: [],
    sharedAppServersAdded: null,
    loadedThreads: null,
    baselineLoadedThreads,
    leakedProcesses: [],
  };
  const owned = () =>
    [...agents.values()].filter(
      (value) => value.id === leadId || value.rootId === leadId,
    );
  const startedAt = performance.now();
  try {
    await pull().catch((error) => {
      state.observationError = String(error?.message ?? error);
    });
    state.ownedAgents = owned().length;
    // One delete command: the server removes the lead and every descendant. A
    // lost reply is resolved by observation, never by a second command.
    try {
      await api("POST", "/api/conversation/delete", { id: leadId });
      state.deleteRequest = "confirmed";
    } catch (error) {
      state.deleteRequest = `unknown: ${String(error?.message ?? error).slice(0, 200)}`;
    }
    for (;;) {
      await pull();
      const live = owned().filter((value) => !value.deleted);
      state.remaining = live.length;
      const table = readProcessTable();
      const providerLeaks = [
        ...providerPids(table, backendPid, AGENT_OWNED_PROCESS_NAMES),
      ].filter((pid) => !baselineProviderPids.has(pid));
      state.sharedAppServersAdded = [
        ...providerPids(table, backendPid, PROVIDER_PROCESS_NAMES),
      ].filter(
        (pid) => !baselineProviderPids.has(pid) && !providerLeaks.includes(pid),
      ).length;
      state.loadedThreads = compactDiagnostics(
        await api("GET", DIAGNOSTICS_PATH),
      ).loadedThreads;
      const threadLeak =
        state.loadedThreads !== null &&
        baselineLoadedThreads !== null &&
        state.loadedThreads > baselineLoadedThreads;
      const toolLeaks = processesMentioning(runTag);
      state.leakedProviderPids = providerLeaks;
      state.leakedProcesses = toolLeaks;
      if (
        !live.length &&
        !providerLeaks.length &&
        !toolLeaks.length &&
        !threadLeak
      ) {
        state.state = "verified";
        break;
      }
      if (performance.now() - startedAt > CLEANUP_DEADLINE_MS) {
        state.state = "FAILED: agents, threads or processes remain";
        break;
      }
      await sleep(POLL_MS * 4);
    }
  } catch (error) {
    state.state = `FAILED: ${String(error?.message ?? error).slice(0, 300)}`;
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
  process.stderr.write(`${error?.stack ?? error}\n`);
  process.exitCode = 1;
});
