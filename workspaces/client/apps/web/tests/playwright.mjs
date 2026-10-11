// @ts-check

import { spawn } from "node:child_process";
import { randomUUID } from "node:crypto";
import { createRequire } from "node:module";
import { delimiter, dirname, join, resolve, sep } from "node:path";
import { homedir } from "node:os";
import { fileURLToPath } from "node:url";
import {
  appendFileSync,
  existsSync,
  mkdirSync,
  mkdtempSync,
  readdirSync,
  readFileSync,
  rmSync,
  statSync,
  writeFileSync,
} from "node:fs";
import { choosePlaywrightScratchRoot } from "./scratch-root.mjs";
import { browserExecutablePath } from "./browser-executable.mjs";
import { waitForFixturePort } from "./fixture-readiness.mjs";
import {
  registerFixtureProcessGroup,
  signalFixtureProcessGroup,
  stopFixtureProcessGroup,
  unregisterFixtureProcessGroup,
} from "./fixture-process-group.mjs";
import {
  persistWorkerMemorySample,
  sampleProcessTreeMemory,
} from "../playwright-workers.mjs";

const playwrightScratch = choosePlaywrightScratchRoot();
process.env.TMPDIR = playwrightScratch.root;
process.env.TMP = playwrightScratch.root;
process.env.TEMP = playwrightScratch.root;
process.env.CODEX_PLAYWRIGHT_SCRATCH_PARENT = playwrightScratch.root;
if (playwrightScratch.ownedRoot)
  process.once("exit", () => {
    rmSync(playwrightScratch.root, { recursive: true, force: true });
  });
if (!playwrightScratch.inheritedProbe)
  console.log(
    `Playwright scratch ${JSON.stringify({
      root: playwrightScratch.root,
      memoryBacked: playwrightScratch.memoryBacked,
      mount: playwrightScratch.mount,
      availableBytes: playwrightScratch.availableBytes,
      probeBytes: playwrightScratch.probeBytes,
      writeFsyncMs: playwrightScratch.writeFsyncMs,
      tested: playwrightScratch.tested,
    })}`,
  );

const require = createRequire(
  fileURLToPath(new URL("../package.json", import.meta.url)),
);
const { test: baseTest, expect } = require("@playwright/test");
export const playwrightScratchRoot = playwrightScratch.root;
const fixtureViteCaches = new Set();
const fixtureProcessGroupRegistry = join(
  playwrightScratch.root,
  "fixture-process-groups",
);
const repositoryScriptsDirectory = resolve(
  dirname(fileURLToPath(import.meta.url)),
  "../../../../../workspaces/runtime/apps/server/src",
);
process.once("exit", () => {
  for (const cacheDir of fixtureViteCaches)
    rmSync(cacheDir, { recursive: true, force: true });
});
// Playwright global setup resolves this once through the server source tree
// before any test runs and places the selected interpreter in the environment.
export function getFixturePythonCommand() {
  const command =
    process.env.CODEX_AGENTS_PYTHON ||
    process.env.PYTHON_BIN ||
    process.env.PYTHON;
  if (!command)
    throw new Error(
      "Playwright fixture Python was not resolved; check the configured global setup.",
    );
  return command;
}

function isPythonCommand(command) {
  return (
    typeof command === "string" &&
    /^(?:.*[/\\])?python(?:\d+(?:\.\d+)*)?$/.test(command)
  );
}

const generatedApiSchema = readFileSync(
  new URL("../src/generated/apiSchema.ts", import.meta.url),
  "utf8",
);

export const FIXTURE_OPTIMIZE_DEPS = {
  noDiscovery: true,
  include: [
    "dexie",
    "react",
    "react/jsx-runtime",
    "react/jsx-dev-runtime",
    "react-dom/client",
    "react-dom",
    "@mantine/core",
    "@mantine/hooks",
    "lucide-react",
  ],
};

/**
 * Start a Vite fixture server with a private dependency cache. Parallel
 * Playwright workers otherwise share web/node_modules/.vite, where one
 * server's optimizer can invalidate modules another server is serving.
 * @param {{ optimizeDeps?: { include?: string[]; [key: string]: unknown }; [key: string]: unknown }} config
 */
export async function createFixtureViteServer(config) {
  const cacheDir = mkdtempSync(join(playwrightScratch.root, "vite-cache-"));
  fixtureViteCaches.add(cacheDir);
  try {
    const { createServer } = await import(require.resolve("vite"));
    const configuredOptimizeDeps = config.optimizeDeps ?? {};
    const include = [
      ...new Set([
        ...FIXTURE_OPTIMIZE_DEPS.include,
        ...(configuredOptimizeDeps.include ?? []),
      ]),
    ];
    const server = await createServer({
      ...config,
      cacheDir,
      optimizeDeps: { ...configuredOptimizeDeps, include },
    });
    const dependencyRoot = resolve(cacheDir, "deps");
    server.middlewares.stack.unshift({
      route: "",
      handle(request, response, next) {
        let pathname;
        try {
          pathname = new URL(request.url, "http://fixture.local").pathname;
        } catch {
          return next();
        }
        const prefix = "/node_modules/.vite/deps/";
        if (!pathname.startsWith(prefix)) return next();
        const dependencyPath = resolve(
          dependencyRoot,
          decodeURIComponent(pathname.slice(prefix.length)),
        );
        if (!dependencyPath.startsWith(`${dependencyRoot}${sep}`)) {
          response.statusCode = 400;
          response.end("Invalid optimized dependency path");
          return;
        }
        if (!existsSync(dependencyPath)) return next();
        const metadata = server.environments.client?.depsOptimizer?.metadata;
        const dependency =
          metadata &&
          Object.values({
            ...metadata.optimized,
            ...metadata.chunks,
            ...metadata.discovered,
          }).find((entry) => resolve(entry.file) === dependencyPath);
        if (!dependency?.browserHash) return next();
        // Keep legacy browser-side imports on Vite's canonical optimized URL.
        // Serving these bytes at the legacy path creates a second React module
        // instance beside the /@fs URL imported by transformed application code.
        response.writeHead(302, {
          Location: `/@fs${dependencyPath}?v=${dependency.browserHash}`,
          "Cache-Control": "no-cache",
        });
        response.end();
      },
    });
    const close = server.close.bind(server);
    let closePromise;
    server.close = () => {
      closePromise ??= Promise.resolve()
        .then(() => close())
        .finally(() => {
          rmSync(cacheDir, { recursive: true, force: true });
          fixtureViteCaches.delete(cacheDir);
        });
      return closePromise;
    };
    return server;
  } catch (error) {
    rmSync(cacheDir, { recursive: true, force: true });
    fixtureViteCaches.delete(cacheDir);
    throw error;
  }
}

export const API_SCHEMA_HASH_HEADER = readGeneratedString(
  "API_SCHEMA_HASH_HEADER",
);
export const API_SCHEMA_HASH_PARAM = readGeneratedString(
  "API_SCHEMA_HASH_PARAM",
);
export const legacySnapshotRoute = /\/api\/state(?:\?.*)?$/;

function readGeneratedString(name) {
  const match = generatedApiSchema.match(
    new RegExp(`${name}\\s*=\\s*"([^"]+)"`),
  );
  if (!match) throw new Error(`Generated ${name} is missing.`);
  return match[1];
}

export function readApiSchemaHash() {
  const match = generatedApiSchema.match(
    /API_SCHEMA_HASH\s*=\s*"([0-9a-f]{64})"/,
  );
  if (!match) throw new Error("Generated API schema hash is missing.");
  return match[1];
}

export function apiSchemaHeaders(headers = {}) {
  return { [API_SCHEMA_HASH_HEADER]: readApiSchemaHash(), ...headers };
}

/**
 * Assert the schema update gate is not visible. Mantine may keep its closed
 * alertdialog portal in the DOM, so checking for absence would misread it.
 * @param {import("playwright").Page} page
 */
export async function expectNoSchemaGate(page) {
  await expect(
    page.getByRole("alertdialog", { name: "Studio update required" }),
  ).toBeHidden();
}

const apiSchemaFixtureTargets = new WeakSet();
const apiSchemaFixtureBrowsers = new WeakSet();
const apiSchemaFixtureHeaderState = { enabled: true };

function routeMatchersEqual(left, right) {
  if (Object.is(left, right)) return true;
  return (
    left instanceof RegExp &&
    right instanceof RegExp &&
    left.source === right.source &&
    left.flags === right.flags
  );
}

/** @param {import("playwright").Page | import("playwright").BrowserContext} target */
export function installApiSchemaFixtureHeaders(target) {
  if (apiSchemaFixtureTargets.has(target)) return;
  apiSchemaFixtureTargets.add(target);
  const registerRoute = target.route.bind(target);
  const unregisterRoute = target.unroute.bind(target);
  const wrappedHandlers = [];
  target.route = (url, handler, options) => {
    const wrappedHandler = (route, request) => {
      if (
        !apiSchemaFixtureHeaderState.enabled ||
        !new URL(request.url()).pathname.startsWith("/api/")
      )
        return handler(route, request);
      let fulfillmentPromise;
      const decoratedRoute = new Proxy(route, {
        get(target, property) {
          if (property === "fulfill")
            return (response = {}) => {
              // A Route can only be fulfilled once. Keep duplicate calls from
              // racing through the decorator to Playwright's one-shot API.
              if (fulfillmentPromise) return fulfillmentPromise;
              const originalHeaders =
                typeof response.response?.headers === "function"
                  ? response.response.headers()
                  : {};
              const headers = new Headers(originalHeaders);
              new Headers(response.headers).forEach((value, key) =>
                headers.set(key, value),
              );
              if (!headers.has(API_SCHEMA_HASH_HEADER))
                headers.set(API_SCHEMA_HASH_HEADER, readApiSchemaHash());
              const normalizedHeaders = {};
              headers.forEach((value, key) => {
                normalizedHeaders[key] = value;
              });
              // The fixture handler owns fulfillment. This proxy only decorates
              // its arguments and delegates a single call to the real Route.
              fulfillmentPromise = target.fulfill({
                ...response,
                headers: normalizedHeaders,
              });
              return fulfillmentPromise;
            };
          const value = Reflect.get(target, property, target);
          return typeof value === "function" ? value.bind(target) : value;
        },
      });
      // Keep this route proxy available if a fixture intentionally holds the
      // Route and fulfills it after a later UI event.
      return handler(decoratedRoute, request);
    };
    wrappedHandlers.push({ url, handler, wrappedHandler });
    return registerRoute(url, wrappedHandler, options);
  };
  target.unroute = async (url, handler) => {
    const matching = wrappedHandlers.filter(
      (entry) =>
        routeMatchersEqual(entry.url, url) &&
        (handler === undefined || entry.handler === handler),
    );
    if (handler === undefined) {
      const result = await unregisterRoute(url);
      for (let index = wrappedHandlers.length - 1; index >= 0; index--) {
        if (routeMatchersEqual(wrappedHandlers[index].url, url))
          wrappedHandlers.splice(index, 1);
      }
      return result;
    }
    if (!matching.length) return unregisterRoute(url, handler);
    await Promise.all(
      matching.map((entry) => unregisterRoute(url, entry.wrappedHandler)),
    );
    for (const entry of matching) {
      const index = wrappedHandlers.indexOf(entry);
      if (index >= 0) wrappedHandlers.splice(index, 1);
    }
  };
}

/** @param {import("playwright").Browser} browser */
function installApiSchemaFixtureBrowser(browser) {
  if (apiSchemaFixtureBrowsers.has(browser)) return;
  apiSchemaFixtureBrowsers.add(browser);
  const originalNewContext = browser.newContext;
  const originalNewPage = browser.newPage;
  browser.newContext = async function (...args) {
    const context = await originalNewContext.apply(this, args);
    if (apiSchemaFixtureHeaderState.enabled)
      installApiSchemaFixtureContext(context);
    return context;
  };
  browser.newPage = async function (...args) {
    const page = await originalNewPage.apply(this, args);
    if (apiSchemaFixtureHeaderState.enabled)
      installApiSchemaFixtureHeaders(page);
    return page;
  };
}

/** @param {import("playwright").BrowserContext} context */
function installApiSchemaFixtureContext(context) {
  installApiSchemaFixtureHeaders(context);
  const originalNewPage = context.newPage;
  context.newPage = async function (...args) {
    const page = await originalNewPage.apply(this, args);
    installApiSchemaFixtureHeaders(page);
    return page;
  };
}

/** @param {import("playwright").Page} page */
export async function stubSessionCost(page) {
  await page.route("**/api/session-cost?*", (route) => {
    const agent = new URL(route.request().url()).searchParams.get("agent");
    return route.fulfill({
      headers: { [API_SCHEMA_HASH_HEADER]: readApiSchemaHash() },
      json: {
        rootId: agent || "fixture-root",
        generation: 1,
        totalUSD: 0,
        pricedSamples: 0,
        breakdown: { providers: {}, models: {} },
        unknownModels: [],
        estimated: false,
        pricingState: "ready",
        cacheAgeSeconds: 0,
        refreshing: false,
      },
    });
  });
}

export function apiSchemaHandshakeSse(body = "", overrides = {}) {
  return `event: api-schema\ndata: ${JSON.stringify(apiSchemaHandshakeEvent(overrides))}\n\n${body}`;
}

export function apiSchemaHandshakeEvent(overrides = {}) {
  return { hash: readApiSchemaHash(), ...overrides };
}

export function protocol3SseEvent(name, value) {
  return `event: ${name}\ndata: ${JSON.stringify(value)}\n\n`;
}

const fixtureScopes = new WeakMap();
const childTerminationWaitMs = 1_000;
const fixtureCleanupTimeoutMs = 5_000;

function recordStage(stage, values = {}) {
  const path = process.env.PLAYWRIGHT_STAGE_LOG;
  if (!path) return;
  appendFileSync(
    path,
    `${JSON.stringify({ stage, at: Date.now(), ...values })}\n`,
  );
}

function directoryBytes(path) {
  let bytes = 0;
  for (const entry of readdirSync(path, { withFileTypes: true })) {
    const entryPath = join(path, entry.name);
    try {
      if (entry.isDirectory()) bytes += directoryBytes(entryPath);
      else if (entry.isFile()) bytes += statSync(entryPath).size;
    } catch {
      // A concurrent SQLite write may replace or remove a file mid-sample.
    }
  }
  return bytes;
}

async function waitForChildExit(child) {
  if (child.exitCode !== null || child.signalCode !== null) return;
  await new Promise((resolve) => {
    const timer = setTimeout(() => {
      child.off("close", onClose);
      resolve();
    }, childTerminationWaitMs);
    timer.unref?.();
    const onClose = () => {
      clearTimeout(timer);
      resolve();
    };
    child.once("close", onClose);
  });
}

async function waitForChildStart(child) {
  if (
    child.pid !== undefined ||
    child.exitCode !== null ||
    child.signalCode !== null
  )
    return;
  await new Promise((resolve) => {
    const finish = () => {
      clearTimeout(timer);
      child.off("spawn", finish);
      child.off("error", finish);
      child.off("close", finish);
      resolve();
    };
    const timer = setTimeout(finish, childTerminationWaitMs);
    timer.unref?.();
    child.once("spawn", finish);
    child.once("error", finish);
    child.once("close", finish);
  });
}

async function stopFixtureChild(child) {
  await waitForChildStart(child);
  if (child.pid === undefined) return;
  await stopFixtureProcessGroup(child);
  await waitForChildExit(child);
  if (child.exitCode === null && child.signalCode === null)
    throw new Error(`Fixture process ${child.pid} did not exit after SIGKILL`);
}

/** @typedef {import("@playwright/test").PlaywrightTestArgs & import("@playwright/test").PlaywrightTestOptions & {
 * apiSchemaFixtureHeaders: boolean;
 * apiSchemaFixtureHeaderSetup: void;
 * fixtureChildCleanup: void;
 * }} ClientTestFixtures
 */
/** @typedef {import("@playwright/test").PlaywrightWorkerArgs & import("@playwright/test").PlaywrightWorkerOptions & {
 * apiSchemaFixtureBrowserSetup: void;
 * browserStageTiming: void;
 * }} ClientWorkerFixtures
 */
/** @type {import("@playwright/test").Fixtures<
 * ClientTestFixtures,
 * ClientWorkerFixtures,
 * import("@playwright/test").PlaywrightTestArgs & import("@playwright/test").PlaywrightTestOptions,
 * import("@playwright/test").PlaywrightWorkerArgs & import("@playwright/test").PlaywrightWorkerOptions
 * >}
 */
const sharedFixtures = {
  apiSchemaFixtureHeaders: [true, { option: true }],
  apiSchemaFixtureBrowserSetup: [
    async ({ playwright, browserName }, use) => {
      const browserType = playwright[browserName];
      const originalLaunch = browserType.launch;
      const wrappedLaunch = async function (...args) {
        const browser = await originalLaunch.apply(this, args);
        installApiSchemaFixtureBrowser(browser);
        return browser;
      };
      browserType.launch = wrappedLaunch;
      try {
        await use();
      } finally {
        if (browserType.launch === wrappedLaunch)
          browserType.launch = originalLaunch;
      }
    },
    { scope: "worker", auto: true },
  ],
  apiSchemaFixtureHeaderSetup: [
    async ({ apiSchemaFixtureHeaders }, use) => {
      const previousEnabled = apiSchemaFixtureHeaderState.enabled;
      apiSchemaFixtureHeaderState.enabled = apiSchemaFixtureHeaders;
      try {
        await use();
      } finally {
        apiSchemaFixtureHeaderState.enabled = previousEnabled;
      }
    },
    { auto: true },
  ],
  browserStageTiming: [
    async ({ playwright, browserName }, use, workerInfo) => {
      const browserType = playwright[browserName];
      const launch = browserType.launch;
      const memoryPeak = {
        rssBytes: 0,
        pssBytes: 0,
        processTreeRssBytes: 0,
        processTreePssBytes: 0,
        processTreeProcessCount: 0,
        processTreeBreakdown: [],
        samples: 0,
      };
      const sampleMemory = () => {
        try {
          const tree = sampleProcessTreeMemory(process.pid);
          if (!tree.processCount || !tree.pssBytes) return;
          if (tree.pssBytes > memoryPeak.processTreePssBytes)
            memoryPeak.processTreeBreakdown = tree.processes;
          memoryPeak.processTreeRssBytes = Math.max(
            memoryPeak.processTreeRssBytes,
            tree.rssBytes,
          );
          memoryPeak.processTreePssBytes = Math.max(
            memoryPeak.processTreePssBytes,
            tree.pssBytes,
          );
          memoryPeak.processTreeProcessCount = Math.max(
            memoryPeak.processTreeProcessCount,
            tree.processCount,
          );
          const rollup = readFileSync("/proc/self/smaps_rollup", "utf8");
          const rssKb = Number(rollup.match(/^Rss:\s+(\d+) kB$/m)?.[1]);
          const pssKb = Number(rollup.match(/^Pss:\s+(\d+) kB$/m)?.[1]);
          if (Number.isFinite(rssKb) && Number.isFinite(pssKb)) {
            memoryPeak.rssBytes = Math.max(memoryPeak.rssBytes, rssKb * 1024);
            memoryPeak.pssBytes = Math.max(memoryPeak.pssBytes, pssKb * 1024);
          }
          memoryPeak.samples += 1;
        } catch {
          // Keep tests portable to systems without procfs.
        }
      };
      sampleMemory();
      const memorySampler = setInterval(sampleMemory, 1_000);
      memorySampler?.unref();
      if (process.env.PLAYWRIGHT_STAGE_LOG) {
        browserType.launch = async function (...args) {
          const startedAt = Date.now();
          try {
            return await launch.apply(this, args);
          } finally {
            recordStage("browser_launch", {
              browserName,
              workerIndex: workerInfo.workerIndex,
              durationMs: Date.now() - startedAt,
            });
          }
        };
      }
      try {
        await use();
      } finally {
        clearInterval(memorySampler);
        sampleMemory();
        let workerMemorySelection = {};
        try {
          workerMemorySelection = JSON.parse(
            process.env.CODEX_PLAYWRIGHT_WORKER_SELECTION || "{}",
          );
        } catch {
          // A malformed inherited selector should not fail fixture cleanup.
        }
        persistWorkerMemorySample({
          runId: workerMemorySelection.workerMemoryRunId,
          workerPid: process.pid,
          processTreePssBytes: memoryPeak.processTreePssBytes,
          processTreeBreakdown: memoryPeak.processTreeBreakdown,
        });
        if (process.env.PLAYWRIGHT_STAGE_LOG) {
          recordStage("worker_memory_peak", {
            workerPid: process.pid,
            workerIndex: workerInfo.workerIndex,
            browserName,
            rssBytes: memoryPeak.rssBytes,
            pssBytes: memoryPeak.pssBytes,
            processTreeRssBytes: memoryPeak.processTreeRssBytes,
            processTreePssBytes: memoryPeak.processTreePssBytes,
            processTreeProcessCount: memoryPeak.processTreeProcessCount,
            processTreeBreakdown: memoryPeak.processTreeBreakdown,
            samples: memoryPeak.samples,
          });
        }
        browserType.launch = launch;
      }
    },
    { scope: "worker", auto: true },
  ],
  fixtureChildCleanup: [
    // oxlint-disable-next-line no-empty-pattern
    async ({}, use, testInfo) => {
      const scope = {
        children: new Set(),
        scratchDirs: new Map(),
        errors: [],
        closing: false,
      };
      fixtureScopes.set(testInfo, scope);
      const testBodyStartedAt = Date.now();
      recordStage("test_body_start", {
        title: testInfo.titlePath.join(" › "),
        file: testInfo.file,
        workerIndex: testInfo.workerIndex,
      });
      let testFailed = false;
      let testError;
      try {
        await use();
      } catch (error) {
        testFailed = true;
        testError = error;
      }
      recordStage("test_body_end", {
        title: testInfo.titlePath.join(" › "),
        file: testInfo.file,
        workerIndex: testInfo.workerIndex,
        durationMs: Date.now() - testBodyStartedAt,
        status: testInfo.status,
        expectedStatus: testInfo.expectedStatus,
      });
      scope.closing = true;
      recordStage("fixture_cleanup_started", {
        title: testInfo.titlePath.join(" › "),
        file: testInfo.file,
        workerIndex: testInfo.workerIndex,
      });
      const results = await Promise.allSettled(
        [...scope.children].map(stopFixtureChild),
      );
      try {
        const tree = sampleProcessTreeMemory(process.pid);
        const rollup = readFileSync("/proc/self/smaps_rollup", "utf8");
        recordStage("worker_memory_after_test", {
          workerPid: process.pid,
          workerIndex: testInfo.workerIndex,
          title: testInfo.titlePath.join(" › "),
          file: testInfo.file,
          rssBytes: Number(rollup.match(/^Rss:\s+(\d+) kB$/m)?.[1]) * 1024,
          pssBytes: Number(rollup.match(/^Pss:\s+(\d+) kB$/m)?.[1]) * 1024,
          processTreeRssBytes: tree.rssBytes,
          processTreePssBytes: tree.pssBytes,
          processTreeProcessCount: tree.processCount,
          processTreeBreakdown: tree.processes,
          nodeMemoryUsage: process.memoryUsage(),
        });
      } catch {
        // Keep per-test telemetry optional on platforms without procfs.
      }
      fixtureScopes.delete(testInfo);
      const cleanupFailures = [
        ...scope.errors,
        ...results
          .filter((result) => result.status === "rejected")
          .map((result) => result.reason),
      ];
      if (cleanupFailures.length)
        throw new AggregateError(
          testFailed ? [testError, ...cleanupFailures] : cleanupFailures,
          "Fixture child process failed or cleanup did not complete",
        );
      if (testFailed) throw testError;
    },
    { auto: true, timeout: fixtureCleanupTimeoutMs },
  ],
};

const test = baseTest.extend(sharedFixtures);

export { test, expect };

export function spawnFixture(command, args, options) {
  let testInfo;
  try {
    testInfo = baseTest.info();
  } catch {
    throw new Error(
      "spawnFixture must be called during a shared Playwright test",
    );
  }
  const scope = fixtureScopes.get(testInfo);
  if (!scope || scope.closing)
    throw new Error(
      "spawnFixture is only available before test cleanup begins",
    );

  const fixtureScratch = mkdtempSync(join(playwrightScratch.root, "fixture-"));
  const home = join(fixtureScratch, "home");
  const inheritedCodexHome = options?.env?.CODEX_HOME;
  const inheritedClaudeConfigDir = options?.env?.CLAUDE_CONFIG_DIR;
  const parentCodexHome =
    process.env.CODEX_HOME || join(process.env.HOME || homedir(), ".codex");
  const parentClaudeConfigDir =
    process.env.CLAUDE_CONFIG_DIR ||
    join(process.env.HOME || homedir(), ".claude");
  // Callers that explicitly pass a separate CODEX_HOME own its contents. A
  // value copied from the parent environment is not an explicit fixture
  // profile: never expose the host's installed plugins or account config to a
  // spawned test backend.
  const ownsCodexHome = !(
    typeof inheritedCodexHome === "string" &&
    resolve(inheritedCodexHome) !== resolve(parentCodexHome)
  );
  const codexHome = ownsCodexHome
    ? join(fixtureScratch, "codex-home")
    : inheritedCodexHome;
  const ownsClaudeConfigDir = !(
    typeof inheritedClaudeConfigDir === "string" &&
    resolve(inheritedClaudeConfigDir) !== resolve(parentClaudeConfigDir)
  );
  const claudeConfigDir = ownsClaudeConfigDir
    ? join(fixtureScratch, "claude-config")
    : inheritedClaudeConfigDir;
  const state = join(fixtureScratch, "state");
  const cache = join(fixtureScratch, "cache");
  for (const directory of [home, codexHome, claudeConfigDir, state, cache])
    mkdirSync(directory, { recursive: true });
  if (ownsCodexHome)
    writeFileSync(
      join(codexHome, "config.toml"),
      '[plugins."chrome@openai-bundled"]\nenabled = false\n',
    );
  // Use synthetic local status metadata so renderer fixtures are independent
  // of host credentials. Signed-out fixtures opt out with
  // CODEX_PLAYWRIGHT_SIGNED_OUT=1; this token is never sent to a provider.
  if (ownsCodexHome && options?.env?.CODEX_PLAYWRIGHT_SIGNED_OUT !== "1")
    writeFileSync(
      join(codexHome, "auth.json"),
      JSON.stringify({
        tokens: {
          account_id: "playwright-fixture-account",
          access_token: "playwright-fixture-only-token",
        },
      }),
    );
  const child = spawn(
    isPythonCommand(command) ? getFixturePythonCommand() : command,
    args,
    {
      ...options,
      detached: process.platform !== "win32",
      env: {
        ...process.env,
        ...options?.env,
        TMPDIR: playwrightScratch.root,
        TMP: playwrightScratch.root,
        TEMP: playwrightScratch.root,
        HOME: home,
        CODEX_HOME: codexHome,
        CLAUDE_CONFIG_DIR: claudeConfigDir,
        XDG_STATE_HOME: state,
        XDG_CACHE_HOME: cache,
        PYTHONPATH: [
          repositoryScriptsDirectory,
          options?.env?.PYTHONPATH,
          process.env.PYTHONPATH,
        ]
          .filter(Boolean)
          .join(delimiter),
      },
    },
  );
  const childKill = child.kill.bind(child);
  child.kill = (signal) =>
    signalFixtureProcessGroup(child, signal) || childKill(signal);
  child.once("spawn", () => {
    try {
      registerFixtureProcessGroup(fixtureProcessGroupRegistry, child.pid);
    } catch (error) {
      scope.errors.push(error);
    }
  });
  let scratchFailure = "";
  child.stderr?.on("data", (chunk) => {
    const text = String(chunk);
    if (
      /ENOSPC|EDQUOT|No space left on device|disk quota exceeded|database or disk is full/i.test(
        text,
      )
    )
      scratchFailure = (scratchFailure + text).slice(-2048);
  });
  const fixtureStartedAt = Date.now();
  const fixtureStateDirectory = args.find((argument) => {
    if (
      typeof argument !== "string" ||
      !argument.startsWith(`${playwrightScratch.root}/`) ||
      !existsSync(argument)
    )
      return false;
    try {
      return statSync(argument).isDirectory();
    } catch {
      return false;
    }
  });
  const sampleScratch =
    process.env.PLAYWRIGHT_SCRATCH_MEASURE === "1" &&
    fixtureStateDirectory !== undefined;
  let scratchInterval;
  const recordScratchSize = () =>
    recordStage("fixture_scratch_size", {
      title: testInfo.titlePath.join(" › "),
      file: testInfo.file,
      workerIndex: testInfo.workerIndex,
      path: fixtureStateDirectory,
      bytes: directoryBytes(fixtureStateDirectory),
    });
  if (sampleScratch) {
    recordScratchSize();
    scratchInterval = setInterval(recordScratchSize, 1000);
    scratchInterval.unref?.();
  }
  recordStage("fixture_spawn", {
    title: testInfo.titlePath.join(" › "),
    file: testInfo.file,
    workerIndex: testInfo.workerIndex,
    pid: child.pid,
    fixtureStateDirectory,
  });
  child.once("spawn", () => {
    recordStage("fixture_process_started", {
      title: testInfo.titlePath.join(" › "),
      file: testInfo.file,
      workerIndex: testInfo.workerIndex,
      pid: child.pid,
      durationMs: Date.now() - fixtureStartedAt,
    });
  });
  child.stdout?.once("data", () => {
    recordStage("fixture_ready_output", {
      title: testInfo.titlePath.join(" › "),
      file: testInfo.file,
      workerIndex: testInfo.workerIndex,
      pid: child.pid,
      durationMs: Date.now() - fixtureStartedAt,
    });
  });
  scope.children.add(child);
  scope.scratchDirs.set(child, fixtureScratch);
  child.on("error", (error) => {
    if (/** @type {NodeJS.ErrnoException} */ (error).code !== "ESRCH")
      scope.errors.push(error);
  });
  child.once("close", async (code, signal) => {
    if (child.pid !== undefined) {
      try {
        // A descendant may outlive the direct fixture after it closes its
        // pipes. Keep the group registered until every process has exited.
        await stopFixtureProcessGroup(child);
        unregisterFixtureProcessGroup(fixtureProcessGroupRegistry, child.pid);
      } catch (error) {
        scope.errors.push(error);
      }
    }
    scope.children.delete(child);
    if (scratchFailure)
      scope.errors.push(
        new Error(
          `Playwright fixture scratch exhausted (exit ${code ?? signal}): ${scratchFailure.slice(-2048)}`,
        ),
      );
    if (scratchInterval) clearInterval(scratchInterval);
    if (sampleScratch) recordScratchSize();
    const scratch = scope.scratchDirs.get(child);
    if (scratch) {
      try {
        rmSync(scratch, { recursive: true, force: true });
      } catch (error) {
        scope.errors.push(error);
      }
      scope.scratchDirs.delete(child);
    }
    recordStage("fixture_exit", {
      title: testInfo.titlePath.join(" › "),
      file: testInfo.file,
      workerIndex: testInfo.workerIndex,
      pid: child.pid,
      durationMs: Date.now() - fixtureStartedAt,
      code,
      signal,
    });
  });
  return child;
}

export { browserExecutablePath, waitForFixturePort };
/** @typedef {import("../src/generated/api").components["schemas"]["JsonValue"]} JsonValue */
/** @typedef {import("../src/generated/api").components["schemas"]["SessionResponse"]} SessionResponse */
/** @typedef {import("../src/generated/api").components["schemas"]["SyncPullResponse"]} SyncPullResponse */
/** @typedef {import("../src/generated/api").components["schemas"]["SyncPullResetResponse"]} SyncPullResetResponse */
/** @typedef {import("../src/generated/api").components["schemas"]["SyncEntity"]} SyncEntity */
/** @typedef {import("../src/generated/api").components["schemas"]["TranscriptPageResponse"]} TranscriptPageResponse */
/** @typedef {import("../src/generated/api").components["schemas"]["SyncIdentityResponse"]} SyncIdentityResponse */
/** @typedef {import("../src/generated/api").components["schemas"]["SyncProtocolResponse"]} SyncProtocolResponse */
/** @typedef {import("../src/generated/api").components["schemas"]["ResourceChangeEvent"]} ResourceChangeEvent */
/** @typedef {import("../src/generated/api").components["schemas"]["ResourceHeartbeatEvent"]} ResourceHeartbeatEvent */
/** @typedef {import("../src/generated/api").components["schemas"]["ResourceTokenRatesEvent"]} ResourceTokenRatesEvent */
/** @typedef {import("../src/generated/api").components["schemas"]["ResourceRef"]} ResourceRef */
/** @typedef {import("../src/generated/api").components["schemas"]["ResourceNotifyRequest"]} ResourceNotifyRequest */
/** @typedef {import("../src/generated/api").components["schemas"]["ResourceNotifyAck"]} ResourceNotifyAck */
/** @typedef {import("../src/generated/api").components["schemas"]["AgentEntityDto"]} AgentEntityDto */
/** @typedef {import("../src/generated/api").components["schemas"]["ChatEntityDto"]} ChatEntityDto */
/** @typedef {import("../src/generated/api").components["schemas"]["ComplaintEntityDto"]} ComplaintEntityDto */
/** @typedef {import("../src/generated/api").components["schemas"]["EdgeEntityDto"]} EdgeEntityDto */
/** @typedef {import("../src/generated/api").components["schemas"]["ProjectEntityDto"]} ProjectEntityDto */
/** @typedef {import("../src/generated/api").components["schemas"]["RoomEntityDto"]} RoomEntityDto */
/** @typedef {import("../src/generated/api").components["schemas"]["RuleEntityDto"]} RuleEntityDto */
/** @typedef {import("../src/generated/api").components["schemas"]["TaskEntityDto"]} TaskEntityDto */
/** @typedef {import("../src/generated/api").components["schemas"]["WorkspaceEntityDto"]} WorkspaceEntityDto */
/** @typedef {import("../src/generated/api").components["schemas"]["WorkEntityDto"]} WorkEntityDto */
/** @typedef {import("../src/generated/api").components["schemas"]["EventEntityDto"]} EventEntityDto */
/** @typedef {import("../src/generated/api").components["schemas"]["MonitorEntityDto"]} MonitorEntityDto */
/** @typedef {import("../src/generated/api").components["schemas"]["PeerTeamEntityDto"]} PeerTeamEntityDto */
/** @typedef {import("../src/generated/api").components["schemas"]["RequestEntityDto"]} RequestEntityDto */

/**
 * Keep this view limited to fields in AgentEntityDto.
 * @typedef {Pick<AgentEntityDto,
 *   "id" | "name" | "manualName" | "status" | "source" | "kind" |
 *   "parentId" | "rootId" | "threadId" | "orchestratorId" | "orchestratorName" |
 *   "isLead" | "role" | "sharedRoomId" | "model" | "provider" | "effort" |
 *   "fastMode" | "concurrency" | "accountKey" | "cwd" | "worktree" |
 *   "worktreePreparation" | "imageWorkspace" | "imageWorkspaceReady" |
 *   "imageWorkspacePhase" | "imageWorkspaceError" | "imageWorkspaceRepo" |
 *   "imageWorkspaceBaseRepo" | "imageWorkspaceSubpath" | "imageWorkspaceBaseRef" |
 *   "created" | "updated" | "turnId" | "turnStatus" | "inFlight" | "compactions" |
 *   "tokensUsed" | "contextUsage" | "error" | "tail" | "canSend" | "launcherAlive" |
 *   "empty" | "yoloMode" | "agentMode" | "agentModeRevision" | "agentModeSupported" |
 *   "subagentConcurrencyVersion" | "workerDefaults" | "reviewDefaults" | "parkedEvent" |
 *   "pendingSettings" | "pendingSettingsAccountKey" | "queuedSettings" | "quickCreate" |
 *   "nativeThreadBlock" | "daybreakEnabled" | "accountTransfer" | "convertedFromLead" |
 *   "nativeRelease" | "activity" | "nativeStatus" | "nativeSafetyBuffering" |
 *   "nativeSafetyRetry" | "nativeTurnError" | "connectionCheck" | "readState" |
 *   "nativeLimitErrorAt" | "startAttempt" | "panelVersion" | "panelDataVersion" |
 *   "unreadCount" | "lastReadAt" | "deletedAt" | "autoWake" | "voiceState" |
 *   "nativeError" | "retryAt" | "hasUnread" | "hasQuestion" | "hasApproval" |
 *   "statusDetail" | "lastAnswer" | "lastCompletedTurn" | "nextTurnSettingsSupported" |
 *   "readStateSupported" | "pinned" | "archived" | "projectFolder" |
 *   "projectFolderRevision" | "project"
 * >} TestAgent */

/** @typedef {Pick<RoomEntityDto, "id" | "name" | "kind" | "members" | "rootId" | "updated" | "userHidden" | "projectPath" | "radio" | "peerTeamId" | "peerTeamName" | "lastMessage" | "peerLabel" | "localMembers">} TestRoom */
/** @typedef {Pick<TaskEntityDto, "id" | "turnId" | "agent" | "kind" | "status" | "created" | "finished" | "name" | "command" | "query" | "cwd" | "processId" | "durationMs" | "timeout_ms" | "interactive" | "stdinClosed" | "stdinCloseRequested" | "stdinError" | "cancelRequested" | "exitCode" | "bytes" | "log" | "outputTruncated">} TestTask */
/** @typedef {Pick<ProjectEntityDto, "homeServerId" | "locations" | "locationsRevision" | "projectAliases" | "id" | "path" | "name" | "created" | "updated" | "accountKey" | "accountRevision" | "accountKeys" | "organizationRevision" | "peerTeamsRevision" | "folders" | "peerTeams" | "workerBaseRef" | "workerBaseRevision" | "workerEnvironment" | "workerEnvironmentRevision">} TestProject */
/** @typedef {Pick<ComplaintEntityDto, "id" | "leadId" | "author" | "authorName" | "leadName" | "title" | "status" | "needsUserResponse" | "created" | "readAt" | "recipient" | "version">} TestComplaint */
/** @typedef {{ agents: TestAgent[], rooms: TestRoom[], tasks: TestTask[], monitors: MonitorEntityDto[], complaints: TestComplaint[], projects: TestProject[], events: EventEntityDto[], peerTeams: PeerTeamEntityDto[], requests: RequestEntityDto[], rules: RuleEntityDto[], work: WorkEntityDto[], connected?: WorkspaceEntityDto["connected"], peerTeamsVersion?: WorkspaceEntityDto["peerTeamsVersion"], projectOrganizationVersion?: WorkspaceEntityDto["projectOrganizationVersion"], tasksHistoryLimit?: WorkspaceEntityDto["tasksHistoryLimit"], nativeNotices: WorkspaceEntityDto["nativeNotices"], sidebarOrder: WorkspaceEntityDto["sidebarOrder"], rateLimits: WorkspaceEntityDto["rateLimits"], rateLimitsByAccount: WorkspaceEntityDto["rateLimitsByAccount"], stateDir?: string }} TestRuntimeView */
/** @typedef {{ token: string, stateDir?: string, chats: ChatEntityDto[], edges: EdgeEntityDto[], runtime: TestRuntimeView, threads: TestAgent[] }} TestStateView */
/** @typedef {Omit<TestStateView, "token" | "threads">} EntityFixtureState */

/** @typedef {Record<string, JsonValue>} JsonObject */
/** @typedef {{ collection: string, id: string, value: JsonValue }} EntityEnvelope */

/** @typedef {{ scope?: string, after?: number, limit?: number, fresh?: boolean, initialHigh?: number, reset?: boolean, floor?: number, maxSeq?: number, generation?: number, priorityId?: string, tombstones?: SyncEntity[], transcript?: TranscriptPageResponse, transcriptSeq?: number, drafts?: SyncEntity[], documents?: SyncEntity[] }} EntityPullFixtureOptions */

/** @type {number} */
const ENTITY_PAGE_SIZE = 500;
/** @type {ResourceChangeEvent["protocol"]} */
const RESOURCE_STREAM_PROTOCOL = 3;

async function readJson(response, path) {
  if (!response.ok)
    throw new Error(`GET ${path} failed with HTTP ${response.status}`);
  return response.json();
}

/** @param {JsonValue} value @returns {value is JsonObject} */
function isJsonObject(value) {
  return value !== null && typeof value === "object" && !Array.isArray(value);
}

/**
 * Read the session token and current entity rows used by browser specs.
 * The returned API is deliberately collection based; it is not a snapshot copy.
 * @param {string} origin
 * @returns {Promise<TestStateView>}
 */
export async function readTestState(origin) {
  const sessionUrl = new URL("/api/session", origin);
  const firstPullUrl = new URL("/api/sync/pull", origin);
  firstPullUrl.searchParams.set("scope", "state:entities:v1");
  firstPullUrl.searchParams.set("after", "0");
  firstPullUrl.searchParams.set("limit", String(ENTITY_PAGE_SIZE));
  firstPullUrl.searchParams.set("fresh", "1");
  const [sessionResponse, firstPullResponse] = await Promise.all([
    fetch(sessionUrl),
    fetch(firstPullUrl),
  ]);
  /** @type {SessionResponse} */
  const session = await readJson(sessionResponse, "/api/session");
  /** @type {Map<string, Map<string, JsonValue>>} */
  const entities = new Map();
  let after = 0;
  let fresh = false;
  let nextResponse = firstPullResponse;

  for (;;) {
    /** @type {SyncPullResponse | SyncPullResetResponse} */
    const page = await readJson(nextResponse, "/api/sync/pull");
    if (page.reset === true) {
      after = 0;
      fresh = true;
    } else {
      fresh = false;
      for (const document of page.documents) {
        if (document._deleted) continue;
        /** @type {JsonValue} */
        const decoded = JSON.parse(document.payload);
        if (
          !isJsonObject(decoded) ||
          typeof decoded.collection !== "string" ||
          typeof decoded.id !== "string" ||
          !isJsonObject(decoded.value)
        )
          throw new Error(`Invalid entity envelope in ${document.id}`);
        /** @type {EntityEnvelope} */
        const entity = {
          collection: decoded.collection,
          id: decoded.id,
          value: decoded.value,
        };
        let collection = entities.get(entity.collection);
        if (!collection) {
          collection = new Map();
          entities.set(entity.collection, collection);
        }
        collection.set(entity.id, entity.value);
      }
      const next = page.checkpoint.seq;
      if (next <= after || next >= (page.maxSeq ?? next)) break;
      after = next;
    }
    const url = new URL("/api/sync/pull", origin);
    url.searchParams.set("scope", "state:entities:v1");
    url.searchParams.set("after", String(after));
    url.searchParams.set("limit", String(ENTITY_PAGE_SIZE));
    if (fresh) url.searchParams.set("fresh", "1");
    nextResponse = await fetch(url);
  }

  /** @param {string} collection @returns {JsonObject[]} */
  const list = (collection) =>
    [...(entities.get(collection)?.values() ?? [])].filter(isJsonObject);
  /** @param {string} collection @param {string} id @returns {JsonObject | undefined} */
  const get = (collection, id) => {
    const value = entities.get(collection)?.get(id);
    return isJsonObject(value) ? value : undefined;
  };

  // Materialize only the collections used by browser specs. Keeping these as
  // ordinary arrays also lets fixture specs adjust the data they serve.
  const agents = /** @type {TestAgent[]} */ (list("agent"));
  const rooms = /** @type {TestRoom[]} */ (list("room"));
  const tasks = /** @type {TestTask[]} */ (list("task"));
  const monitors = /** @type {MonitorEntityDto[]} */ (list("monitor"));
  const complaints = /** @type {TestComplaint[]} */ (list("complaint"));
  const projects = /** @type {TestProject[]} */ (list("project"));
  const events = /** @type {EventEntityDto[]} */ (list("event"));
  const peerTeams = /** @type {PeerTeamEntityDto[]} */ (list("peerTeam"));
  const requests = /** @type {RequestEntityDto[]} */ (list("request"));
  const rules = /** @type {RuleEntityDto[]} */ (list("rule"));
  const work = /** @type {WorkEntityDto[]} */ (list("work"));
  const chats = /** @type {ChatEntityDto[]} */ (list("chat"));
  const edges = /** @type {EdgeEntityDto[]} */ (list("edge"));
  /** @type {TestRuntimeView} */
  const runtime = {
    agents,
    rooms,
    tasks,
    monitors,
    complaints,
    projects,
    events,
    peerTeams,
    requests,
    rules,
    work,
    connected: /** @type {WorkspaceEntityDto["connected"]} */ (
      get("workspace", "current")?.connected
    ),
    peerTeamsVersion: /** @type {WorkspaceEntityDto["peerTeamsVersion"]} */ (
      get("workspace", "current")?.peerTeamsVersion
    ),
    projectOrganizationVersion:
      /** @type {WorkspaceEntityDto["projectOrganizationVersion"]} */ (
        get("workspace", "current")?.projectOrganizationVersion
      ),
    tasksHistoryLimit: /** @type {WorkspaceEntityDto["tasksHistoryLimit"]} */ (
      get("workspace", "current")?.tasksHistoryLimit
    ),
    get nativeNotices() {
      return /** @type {WorkspaceEntityDto["nativeNotices"]} */ (
        get("workspace", "current")?.nativeNotices
      );
    },
    get sidebarOrder() {
      return /** @type {WorkspaceEntityDto["sidebarOrder"]} */ (
        get("workspace", "current")?.sidebarOrder
      );
    },
    get rateLimits() {
      return /** @type {WorkspaceEntityDto["rateLimits"]} */ (
        get("workspace", "current")?.rateLimits
      );
    },
    get rateLimitsByAccount() {
      return /** @type {WorkspaceEntityDto["rateLimitsByAccount"]} */ (
        get("workspace", "current")?.rateLimitsByAccount
      );
    },
    get stateDir() {
      return /** @type {string | undefined} */ (
        get("workspace", "current")?.stateDir
      );
    },
  };

  return /** @type {TestStateView} */ ({
    token: session.token,
    get stateDir() {
      return get("workspace", "current")?.stateDir;
    },
    chats,
    edges,
    runtime,
    threads: agents,
  });
}

/** @type {(keyof WorkspaceEntityDto)[]} */
const entityWorkspaceKeys = [
  "connected",
  "nativeNotices",
  "peerTeamsVersion",
  "projectOrganizationVersion",
  "rateLimits",
  "rateLimitsByAccount",
  "sidebarOrder",
  "stateDir",
  "tasksHistoryLimit",
];

/** @type {Set<string>} */
const snapshotOnlyAgentKeys = new Set([
  "activeTools",
  "compactionsObservedOnly",
  "complaintsPresented",
  "events",
  "executionSettingsAccountKey",
  "maxAgents",
  "maxAgentsExplicit",
  "nativeEffort",
  "nativeNameFailure",
  "nativeToolCatalog",
  "needsTitle",
  "prepareAttempt",
  "preparedContext",
  "profileId",
  "profileInstructions",
  "tokenBudget",
  "turnEpoch",
  "usageResumeEnabled",
  "wave",
  "workerBaseBehindMain",
  "workerBaseCommit",
  "workerBaseMainRef",
  "workerBaseRef",
  "worktreeReady",
  "worktreeWarning",
]);

/** @template {object} T @param {T} value @param {Iterable<keyof T>} keys @returns {JsonObject} */
function pickJsonKeys(value, keys) {
  const source = /** @type {Record<string, JsonValue>} */ (value);
  /** @type {JsonObject} */
  const picked = {};
  for (const key of keys) {
    const jsonKey = String(key);
    if (Object.hasOwn(source, jsonKey)) picked[jsonKey] = source[jsonKey];
  }
  return picked;
}

/** @param {SyncEntity} document @returns {SyncEntity} */
function generatedSyncEntity(document) {
  return document;
}

/** @param {SyncPullResponse | SyncPullResetResponse} response @returns {SyncPullResponse | SyncPullResetResponse} */
function generatedSyncPullResponse(response) {
  return response;
}

/**
 * Build entity values from a fixture, omitting fields that the entity contract
 * does not expose.
 * @param {EntityFixtureState} snapshot
 * @returns {{ collection: string, id: string, value: JsonValue }[]}
 */
function entityValuesFromSnapshot(snapshot) {
  const runtime = snapshot.runtime;
  /** @type {{ collection: string, id: string, value: JsonValue }[]} */
  const values = [];
  /** @param {string} collection @param {Array<{id?: string}> | null | undefined} rows @param {(row: object) => JsonValue} [project] */
  const append = (
    collection,
    rows,
    project = (row) => /** @type {JsonValue} */ (row),
  ) => {
    for (const row of rows ?? []) {
      if (typeof row.id !== "string") continue;
      values.push({ collection, id: row.id, value: project(row) });
    }
  };
  append("project", runtime.projects, (row) =>
    pickJsonKeys(row, [
      "homeServerId",
      "locations",
      "locationsRevision",
      "projectAliases",
      "id",
      "path",
      "name",
      "created",
      "updated",
      "accountKey",
      "accountRevision",
      "accountKeys",
      "organizationRevision",
      "peerTeamsRevision",
      "folders",
      "peerTeams",
      "workerBaseRef",
      "workerBaseRevision",
      "workerEnvironment",
      "workerEnvironmentRevision",
      "layrReadiness",
      "layrSync",
      "vault",
    ]),
  );
  append("request", runtime.requests);
  append("event", runtime.events, (row) =>
    pickJsonKeys(row, ["id", "agent", "kind", "status", "created", "error"]),
  );
  append("room", runtime.rooms, (row) =>
    pickJsonKeys(row, [
      "id",
      "name",
      "kind",
      "members",
      "rootId",
      "updated",
      "userHidden",
      "projectPath",
      "radio",
      "peerTeamId",
      "peerTeamName",
      "lastMessage",
      "peerLabel",
      "localMembers",
    ]),
  );
  append("agent", runtime.agents, (row) => {
    const entity = /** @type {Record<string, JsonValue>} */ ({ ...row });
    for (const key of snapshotOnlyAgentKeys) delete entity[key];
    const agentNestedAllowLists = {
      activity: ["phase", "at", "tools"],
      nativeStatus: ["phase", "error", "message", "turnId", "at"],
      nativeSafetyBuffering: [
        "turnId",
        "threadId",
        "accountKey",
        "connectionId",
        "at",
        "dismissed",
        "responseStarted",
        "showBufferingUi",
        "fasterModel",
      ],
      nativeSafetyRetry: [
        "id",
        "stage",
        "model",
        "turnId",
        "created",
        "updated",
        "epoch",
        "accountKey",
        "error",
        "newThreadId",
        "acceptedTurnId",
        "requestId",
        "rpcMethod",
      ],
      nativeTurnError: ["turnId", "error"],
      nativeThreadBlock: ["threadId", "error"],
      connectionCheck: [
        "epoch",
        "accountKey",
        "threadId",
        "turnId",
        "at",
        "previousError",
        "nativeState",
        "restartTurnStatus",
        "readError",
      ],
      readState: ["threadId", "turnId", "read", "revision"],
      startAttempt: ["prepareError", "responseError", "retiredEvents"],
    };
    for (const [key, allowed] of Object.entries(agentNestedAllowLists)) {
      if (isJsonObject(entity[key]))
        entity[key] = pickJsonKeys(entity[key], allowed);
    }
    if (isJsonObject(entity.nativeRelease))
      entity.nativeRelease = pickJsonKeys(entity.nativeRelease, [
        "phase",
        "resetPending",
      ]);
    return entity;
  });
  append("task", runtime.tasks);
  append("monitor", runtime.monitors);
  append("complaint", runtime.complaints, (row) => {
    const entity = /** @type {Record<string, JsonValue>} */ ({ ...row });
    delete entity.needsResponse;
    return entity;
  });
  append("peerTeam", runtime.peerTeams);
  append("rule", runtime.rules);
  append("work", runtime.work);
  append("chat", snapshot.chats);
  append("edge", snapshot.edges);
  /** @type {Record<string, JsonValue>} */
  const workspaceSource = { ...runtime, stateDir: snapshot.stateDir };
  values.push({
    collection: "workspace",
    id: "current",
    value: pickJsonKeys(workspaceSource, entityWorkspaceKeys),
  });
  return values;
}

/** @param {string} [workspaceId] @returns {SyncIdentityResponse} */
export function syncIdentityFixture(
  workspaceId = randomUUID().replaceAll("-", ""),
) {
  if (!/^[a-f0-9]{32}$/.test(workspaceId))
    throw new Error(
      "Sync fixture workspaceId must match the server's 32-hex identity",
    );
  return { workspaceId, syncProtocol: 2 };
}

/** @param {{ unixSocket?: boolean }} [options] @returns {SyncProtocolResponse} */
export function syncProtocolFixture({ unixSocket = false } = {}) {
  return {
    protocolVersion: 3,
    supportedVersions: [3],
    capabilities: [
      "pull",
      "stream",
      "streamChanges",
      "entityReset",
      "typedResources",
      "tokenRates",
      ...(unixSocket ? ["unixSocket"] : []),
    ],
    scopes: ["state:entities:v1", "transcript:<agent-id>", "drafts"],
    pullEndpoint: "/api/sync/pull",
    streamEndpoint: "/api/sync/stream",
    maxEntityPage: 500,
    maxOtherPage: 100,
    maxStreamDocuments: 100,
    maxStreamBytes: 1_048_576,
  };
}

/** @param {{ skip: (condition: boolean, description?: string) => void }} test @param {unknown} error */
export function skipIfWebKitCannotLaunch(test, error) {
  const missingLibrary = String(error).match(
    /error while loading shared libraries: ([^:]+): cannot open shared object file/i,
  )?.[1];
  if (missingLibrary) {
    const reason = `WebKit cannot launch here: missing ${missingLibrary}, ...`;
    console.log(`SKIP: ${reason}`);
    test.skip(true, reason);
  }
}

/** @param {string} origin @returns {Promise<{ identity: SyncIdentityResponse, protocol: SyncProtocolResponse }>} */
export async function readFixtureSyncContract(origin) {
  const [identityResponse, protocolResponse] = await Promise.all([
    fetch(new URL("/api/sync/identity", origin)),
    fetch(new URL("/api/sync/protocol", origin)),
  ]);
  const identity = /** @type {SyncIdentityResponse} */ (
    await readJson(identityResponse, "/api/sync/identity")
  );
  const protocol = /** @type {SyncProtocolResponse} */ (
    await readJson(protocolResponse, "/api/sync/protocol")
  );
  if (
    !/^[a-f0-9]{32}$/.test(identity.workspaceId) ||
    identity.syncProtocol !== 2 ||
    protocol.protocolVersion !== 3
  )
    throw new Error("Fixture server returned an invalid sync contract");
  return { identity, protocol };
}

/** @typedef {{ documents: Map<string, SyncEntity>, maxSeq: number }} MutableEntityPullState */

/** @type {WeakMap<EntityFixtureState, MutableEntityPullState>} */
const nodeEntityPullStates = new WeakMap();

/** @returns {MutableEntityPullState} */
function createMutableEntityPullState() {
  return { documents: new Map(), maxSeq: 0 };
}

/**
 * Version changed entities and tombstones so clients that have advanced their
 * cursor can observe fixture state changes on a later pull.
 * @param {MutableEntityPullState} pullState
 * @param {EntityFixtureState} snapshot
 */
function reconcileMutableEntityPullState(pullState, snapshot) {
  const nextById = new Map(
    entityValuesFromSnapshot(snapshot).map((entity) => [
      `entity:${entity.collection}:${entity.id}`,
      entity,
    ]),
  );
  for (const [id, previous] of pullState.documents) {
    if (previous._deleted) continue;
    const next = nextById.get(id);
    if (!next) {
      const previousEntity = JSON.parse(previous.payload);
      pullState.documents.set(
        id,
        generatedSyncEntity({
          ...previous,
          seq: ++pullState.maxSeq,
          payload: JSON.stringify({
            collection: previousEntity.collection,
            id: previousEntity.id,
            value: {},
          }),
          _deleted: true,
        }),
      );
      continue;
    }
    const payload = JSON.stringify(next);
    if (payload !== previous.payload)
      pullState.documents.set(
        id,
        generatedSyncEntity({
          ...previous,
          seq: ++pullState.maxSeq,
          payload,
          _deleted: false,
        }),
      );
  }
  for (const [id, entity] of nextById) {
    const previous = pullState.documents.get(id);
    if (!previous || previous._deleted)
      pullState.documents.set(
        id,
        generatedSyncEntity({
          id,
          seq: ++pullState.maxSeq,
          payload: JSON.stringify(entity),
          _deleted: false,
        }),
      );
  }
}

/**
 * Commit a fixture state change and return the entity documents its mutation
 * response should carry. This mirrors the server's `_syncEntities` envelope.
 * @param {EntityFixtureState} snapshot
 * @returns {SyncEntity[]}
 */
export function updateEntitySyncFixture(snapshot) {
  let pullState = nodeEntityPullStates.get(snapshot);
  if (!pullState) {
    pullState = createMutableEntityPullState();
    nodeEntityPullStates.set(snapshot, pullState);
  }
  const previousMaxSeq = pullState.maxSeq;
  reconcileMutableEntityPullState(pullState, snapshot);
  return [...pullState.documents.values()].filter(
    (document) => document.seq > previousMaxSeq,
  );
}

/**
 * Build a mutation response that both carries the changed entities to the
 * current client and leaves the entity-pull fixture at the same state.
 * Mutate the snapshot before calling this helper.
 * @param {EntityFixtureState} snapshot
 * @param {Record<string, unknown>} response
 * @returns {Record<string, unknown>}
 */
export function entityMutationFixtureResponse(snapshot, response) {
  return {
    ...response,
    _syncEntities: updateEntitySyncFixture(snapshot),
  };
}

/** @param {import("@playwright/test").Page | import("@playwright/test").Locator} page @param {string} label */
export function accountTileGroup(page, label) {
  return page.getByRole("group", { name: label, exact: true });
}

/** @param {import("@playwright/test").Page | import("@playwright/test").Locator} page @param {string} label @param {string} accountName */
export function accountTileButton(page, label, accountName) {
  return accountTileGroup(page, label).getByRole("button", {
    name: `Select account ${accountName}`,
    exact: true,
  });
}

/**
 * Serve the same sync routes from a small Node HTTP fixture backend.
 * @param {import("node:http").IncomingMessage} request
 * @param {import("node:http").ServerResponse} response
 * @param {{ snapshot: EntityFixtureState, workspaceId: string, unixSocket?: boolean, versionedResources?: boolean, epoch?: string, onPull?: () => void, onStreamReady?: (notify: (resources: ResourceRef[], reason?: "change" | "reconnect") => void, resources: ResourceRef[]) => (() => void) | void }} fixture
 * @returns {boolean} whether this was an /api/sync route
 */
export function handleEntitySyncFixtureRequest(request, response, fixture) {
  const url = new URL(request.url || "/", "http://fixture.invalid");
  if (!url.pathname.startsWith("/api/sync/")) return false;
  const identity = syncIdentityFixture(fixture.workspaceId);
  const json = (value) => {
    response.setHeader("Content-Type", "application/json");
    response.setHeader(API_SCHEMA_HASH_HEADER, readApiSchemaHash());
    response.end(JSON.stringify(value));
  };
  if (url.pathname === "/api/sync/identity") {
    json(identity);
    return true;
  }
  if (url.pathname === "/api/sync/protocol") {
    json(syncProtocolFixture({ unixSocket: fixture.unixSocket }));
    return true;
  }
  if (url.pathname === "/api/sync/pull") {
    fixture.onPull?.();
    const validation = validateSyncPullRequest(url);
    if (validation.ok === false) {
      response.statusCode = validation.status;
      json(validation.body);
      return true;
    }
    /** @type {EntityPullFixtureOptions} */
    let pullOptions = validation.params;
    if (validation.params.scope === "state:entities:v1") {
      let pullState = nodeEntityPullStates.get(fixture.snapshot);
      if (!pullState) {
        pullState = createMutableEntityPullState();
        nodeEntityPullStates.set(fixture.snapshot, pullState);
      }
      reconcileMutableEntityPullState(pullState, fixture.snapshot);
      pullOptions = {
        ...validation.params,
        documents: [...pullState.documents.values()],
        maxSeq: pullState.maxSeq,
      };
    }
    const pullResponse = generatedSyncPullResponse({
      workspaceId: identity.workspaceId,
      ...entityPullFixture(fixture.snapshot, pullOptions),
    });
    json(pullResponse);
    return true;
  }
  if (url.pathname === "/api/sync/stream") {
    const epoch = fixture.epoch || randomUUID().replaceAll("-", "");
    const requestedResources = JSON.parse(
      url.searchParams.get("resources") || "[]",
    );
    let revision = 0;
    let eventId = 0;
    const resourceRevisions = new Map();
    const withResourceVersions = (resources, advance = false) =>
      resources.map((resource) => {
        const key = JSON.stringify(resource);
        const next = (resourceRevisions.get(key) || 0) + (advance ? 1 : 0);
        resourceRevisions.set(key, next || 1);
        return { resource, revision: next || 1 };
      });
    response.writeHead(200, {
      "Cache-Control": "no-cache, no-transform",
      Connection: "keep-alive",
      "Content-Type": "text/event-stream; charset=utf-8",
      [API_SCHEMA_HASH_HEADER]: readApiSchemaHash(),
      "X-Accel-Buffering": "no",
    });
    response.write(apiSchemaHandshakeSse());
    /** @param {string} name @param {ResourceChangeEvent | ResourceHeartbeatEvent | ResourceTokenRatesEvent} value */
    const writeFrame = (name, value) => {
      eventId += 1;
      response.write(
        `event: ${name}\ndata: ${JSON.stringify(value)}\nid: ${eventId}\n\n`,
      );
    };
    /** @type {ResourceChangeEvent} */
    const initial = {
      epoch,
      protocol: RESOURCE_STREAM_PROTOCOL,
      reason: "initial",
      resources: requestedResources,
      // No per-resource versions: the page must always pull after a fixture frame.
      resourceVersions: fixture.versionedResources
        ? withResourceVersions(requestedResources)
        : [],
      revision: ++revision,
      workspaceId: identity.workspaceId,
    };
    writeFrame("resources", initial);
    /** @type {ResourceTokenRatesEvent} */
    const rates = {
      epoch,
      protocol: RESOURCE_STREAM_PROTOCOL,
      rates: {},
      revision,
      teams: {},
      workspaceId: identity.workspaceId,
    };
    writeFrame("token-rates", rates);
    const heartbeat = () => {
      if (response.destroyed) return;
      /** @type {ResourceHeartbeatEvent} */
      const event = {
        epoch,
        protocol: RESOURCE_STREAM_PROTOCOL,
        revision: ++revision,
        workspaceId: identity.workspaceId,
      };
      writeFrame("heartbeat", event);
    };
    const heartbeatTimer = setInterval(heartbeat, 5000);
    heartbeatTimer.unref?.();
    response.once("close", () => clearInterval(heartbeatTimer));
    const cleanup = fixture.onStreamReady?.((resources, reason = "change") => {
      if (response.destroyed) return;
      /** @type {ResourceChangeEvent} */
      const changed = {
        epoch,
        protocol: RESOURCE_STREAM_PROTOCOL,
        reason,
        resources,
        resourceVersions: fixture.versionedResources
          ? withResourceVersions(resources, true)
          : [],
        revision: ++revision,
        workspaceId: identity.workspaceId,
      };
      writeFrame("resources", changed);
    }, requestedResources);
    if (cleanup !== undefined) {
      if (typeof cleanup !== "function") {
        throw new TypeError(
          "Entity sync fixture onStreamReady must return a cleanup function or undefined",
        );
      }
      response.once("close", cleanup);
    }
    return true;
  }
  response.statusCode = 404;
  json({ error: "Not found" });
  return true;
}

/**
 * Build a typed entity-pull fixture with the protocol's page, scope, tombstone,
 * reset, priority and checkpoint semantics.
 * @param {EntityFixtureState} snapshot
 * @param {number | EntityPullFixtureOptions} [optionsOrAfter]
 * @returns {Omit<SyncPullResponse, "workspaceId"> | Omit<SyncPullResetResponse, "workspaceId">}
 */
export function entityPullFixture(snapshot, optionsOrAfter = {}) {
  const options =
    typeof optionsOrAfter === "number"
      ? { after: optionsOrAfter }
      : optionsOrAfter;
  const scope = options.scope ?? "state:entities:v1";
  const after = Math.max(0, options.after ?? 0);
  const generation = options.generation ?? 1;
  if (scope === "drafts") {
    const drafts = options.drafts ?? [];
    const high = drafts.at(-1)?.seq ?? 0;
    const limit = Math.min(100, Math.max(1, options.limit ?? 100));
    const page = drafts
      .filter((document) => document.seq > after)
      .slice(0, limit);
    return {
      generation,
      documents: page,
      checkpoint: {
        seq: page.length === limit ? page.at(-1).seq : high,
      },
    };
  }
  if (scope.startsWith("transcript:")) {
    const highWatermark = options.transcriptSeq ?? 1;
    const limit = Math.min(100, Math.max(1, options.limit ?? 100));
    /** @type {TranscriptPageResponse} */
    const transcript = options.transcript ?? {
      items: [],
      truncated: false,
      agent: null,
      historyVersion: "0",
      nextAfterCursor: null,
      nextCursor: null,
      tail: null,
      unavailable: null,
    };
    const availableDocuments = [
      generatedSyncEntity({
        id: scope,
        seq: highWatermark,
        payload: JSON.stringify(transcript),
        _deleted: false,
      }),
    ];
    const page = availableDocuments
      .filter((document) => document.seq > after)
      .slice(0, limit);
    return {
      generation,
      documents: page,
      checkpoint: {
        seq:
          page.length === limit
            ? page.at(-1).seq
            : Math.max(after, highWatermark),
      },
    };
  }
  if (scope !== "state:entities:v1")
    throw new Error(`Unsupported fixture pull scope: ${scope}`);

  const limit = Math.min(500, Math.max(1, options.limit ?? 500));
  const values = entityValuesFromSnapshot(snapshot);
  const baseDocuments = values.map((entity, index) =>
    generatedSyncEntity({
      id: `entity:${entity.collection}:${entity.id}`,
      seq: index + 1,
      payload: JSON.stringify(entity),
      _deleted: false,
    }),
  );
  const tombstones = options.tombstones ?? [];
  const documents = [
    ...(options.documents ?? baseDocuments),
    ...tombstones,
  ].sort((left, right) => left.seq - right.seq);
  const high = options.maxSeq ?? documents.at(-1)?.seq ?? 0;
  const floor = options.floor ?? 0;
  const fresh = options.fresh === true;
  const initialHigh = fresh
    ? options.initialHigh && options.initialHigh > 0
      ? Math.min(high, options.initialHigh)
      : high
    : 0;
  if (options.reset && !fresh && after > 0 && after < floor)
    return { generation, reset: true, floor, maxSeq: high };

  let eligible = documents.filter(
    (document) =>
      document.seq > after &&
      (!document._deleted || document.seq > floor) &&
      (!fresh || !document._deleted || document.seq > initialHigh),
  );
  if (after === 0 && fresh && options.priorityId) {
    eligible = eligible.sort((left, right) => {
      const priority = `entity:agent:${options.priorityId}`;
      return Number(right.id === priority) - Number(left.id === priority);
    });
  }
  const page = eligible.slice(0, limit);
  const checkpoint = page.length === limit ? page.at(-1).seq : high;
  return {
    generation,
    documents: page,
    checkpoint: { seq: checkpoint },
    maxSeq: high,
    initialHigh,
  };
}

/**
 * Parse /api/sync/pull query semantics shared by the browser and Node fixtures.
 * The real API accepts negative integer cursors (the store clamps them to zero)
 * and returns 400 for malformed integers and unknown scopes.
 * @param {string | URL} requestUrl
 * @returns {{ ok: true, params: Required<Pick<EntityPullFixtureOptions, "scope" | "after" | "limit" | "fresh" | "initialHigh" | "reset">> & Pick<EntityPullFixtureOptions, "priorityId"> } | { ok: false, status: number, body: JsonObject }}
 */
export function validateSyncPullRequest(requestUrl) {
  const url = new URL(requestUrl, "http://fixture.invalid");
  const query = url.searchParams;
  const first = (key) => query.getAll(key).find((value) => value !== "");
  const scope = first("scope") ?? "state:entities:v1";
  /** @type {Record<string, number>} */
  const numbers = {};
  /** @type {Array<[keyof typeof numbers, number]>} */
  const defaults = [
    ["after", 0],
    ["limit", 500],
    ["initialHigh", 0],
  ];
  for (const [key, fallback] of defaults) {
    const raw = first(key);
    if (raw === undefined) {
      numbers[key] = fallback;
      continue;
    }
    if (!/^[+-]?\d+$/.test(raw))
      return {
        ok: false,
        status: 400,
        body: {
          error: "Invalid request",
          details: [
            {
              location: ["query", key],
              message:
                "Input should be a valid integer, unable to parse string as an integer",
            },
          ],
        },
      };
    numbers[key] = Number(raw);
  }
  const supportedScope =
    scope === "state:entities:v1" ||
    scope === "drafts" ||
    (scope.startsWith("transcript:") && scope.length < 300);
  if (!supportedScope)
    return {
      ok: false,
      status: 400,
      body: { error: "Invalid sync scope" },
    };
  return {
    ok: true,
    params: {
      scope,
      after: numbers.after,
      limit: numbers.limit,
      fresh: first("fresh") === "1",
      initialHigh: numbers.initialHigh,
      reset: first("reset") === "1",
      priorityId: first("priorityId"),
    },
  };
}

/**
 * Convert an incoming fixture request URL to the shared pull fixture options.
 * @param {EntityFixtureState} snapshot
 * @param {string | URL} requestUrl
 * @param {EntityPullFixtureOptions} [overrides]
 * @returns {Omit<SyncPullResponse, "workspaceId"> | Omit<SyncPullResetResponse, "workspaceId">}
 */
export function entityPullFixtureForRequest(
  snapshot,
  requestUrl,
  overrides = {},
) {
  const validation = validateSyncPullRequest(requestUrl);
  if (validation.ok === false) {
    const error = new Error(String(validation.body.error));
    Object.assign(error, {
      status: validation.status,
      body: validation.body,
    });
    throw error;
  }
  return entityPullFixture(snapshot, { ...validation.params, ...overrides });
}

/**
 * Route a snapshot-shaped test fixture through the current entity sync paths.
 * @param {{ route: Function }} page
 * @param {EntityFixtureState} snapshot
 * @param {{ identity: SyncIdentityResponse, protocol: SyncProtocolResponse }} contract
 * @returns {Promise<{ update: (nextSnapshot: EntityFixtureState, options?: { origin: string, token: string, resources?: ResourceRef[] }) => Promise<ResourceNotifyAck | undefined> }>}
 */
export async function stubEntityState(page, snapshot, contract) {
  const { identity: identityResponse, protocol: protocolResponse } = contract;
  if (
    !/^[a-f0-9]{32}$/.test(identityResponse.workspaceId) ||
    identityResponse.syncProtocol !== 2 ||
    protocolResponse.protocolVersion !== 3
  )
    throw new Error(
      "stubEntityState requires the fixture backend's generated sync contract",
    );
  const identity = identityResponse.workspaceId;
  let generation = 1;
  /** @type {EntityFixtureState} */
  let currentSnapshot = snapshot;
  const pullState = createMutableEntityPullState();
  reconcileMutableEntityPullState(pullState, currentSnapshot);
  await page.route("**/api/sync/identity", (route) =>
    route.fulfill({
      json: identityResponse,
      headers: { [API_SCHEMA_HASH_HEADER]: readApiSchemaHash() },
    }),
  );
  await page.route("**/api/sync/protocol", (route) =>
    route.fulfill({
      json: protocolResponse,
      headers: { [API_SCHEMA_HASH_HEADER]: readApiSchemaHash() },
    }),
  );
  // Keep a real backend stream open: route.fulfill closes SSE immediately and
  // EventSource then reconnects continuously. The pull route below remains the
  // deterministic entity fixture; the live stream exercises real frame shape.
  await page.route("**/api/sync/stream?*", (route) => route.continue());
  await page.route("**/api/sync/pull**", (route) => {
    const requestUrl = route.request().url();
    const validation = validateSyncPullRequest(requestUrl);
    if (validation.ok === false)
      return route.fulfill({
        status: validation.status,
        json: validation.body,
        headers: { [API_SCHEMA_HASH_HEADER]: readApiSchemaHash() },
      });
    reconcileMutableEntityPullState(pullState, currentSnapshot);
    const pageData = entityPullFixture(currentSnapshot, {
      ...validation.params,
      generation,
      documents: [...pullState.documents.values()],
      maxSeq: pullState.maxSeq,
    });
    return route.fulfill({
      json: generatedSyncPullResponse({
        workspaceId: identity,
        ...pageData,
      }),
    });
  });

  /**
   * Update changed entity documents with monotonic sequences and tombstone
   * removed entities. If origin/token are provided, publish the state
   * invalidation as a stand-in for the missing base server commit notification.
   * @param {EntityFixtureState} nextSnapshot
   * @param {{ origin: string, token: string, resources?: ResourceRef[] }} [publish]
   * @returns {Promise<ResourceNotifyAck | undefined>}
   */
  const update = async (nextSnapshot, publish) => {
    currentSnapshot = nextSnapshot;
    reconcileMutableEntityPullState(pullState, currentSnapshot);
    generation++;
    if (!publish) return undefined;
    const body = /** @type {ResourceNotifyRequest} */ ({
      requestId: randomUUID(),
      resources: publish.resources ?? [{ kind: "state" }],
    });
    const response = await fetch(new URL("/api/sync/notify", publish.origin), {
      method: "POST",
      headers: {
        "Content-Type": "application/json",
        "X-Canvas-Token": publish.token,
        [API_SCHEMA_HASH_HEADER]: readApiSchemaHash(),
      },
      body: JSON.stringify(body),
    });
    return /** @type {ResourceNotifyAck} */ (
      await readJson(response, "/api/sync/notify")
    );
  };
  return { update };
}
