import { spawn } from "node:child_process";
import { SYNC_PROTOCOL_VERSION } from "../src/generated/api.ts";
import { SUBSCRIPTION_ENTITY_SCOPES } from "../src/generated/subscriptionEntities.ts";
import {
  SYNC_ENTITY_STREAM_MAX_DOCUMENTS,
  SYNC_STREAM_EVENT_MAX_BYTES,
} from "../src/generated/sharedValues.ts";
import { randomUUID } from "node:crypto";
import { test as baseTest, expect } from "@playwright/test";
import type {
  BrowserType,
  Page,
  Route,
  Request,
  TestInfo,
} from "@playwright/test";
import type { ChildProcess, SpawnOptions } from "node:child_process";
import type { IncomingMessage, ServerResponse } from "node:http";
import type { Connect } from "vite";
import type { InlineConfig } from "vite";
import { join, resolve, sep } from "node:path";
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
import { DatabaseSync } from "node:sqlite";
import { fixtureStateDirectory as rustFixtureStateDirectory } from "./fixtures/rust-ui-fixture.ts";
import { readResourceBaselines } from "./fixtures/resource-baselines.ts";
import { choosePlaywrightScratchRoot } from "./scratch-root.ts";
import { browserExecutablePath } from "./browser-executable.ts";
import { waitForFixturePort } from "./fixture-readiness.ts";
import {
  registerFixtureProcessGroup,
  signalFixtureProcessGroup,
  stopFixtureProcessGroup,
  unregisterFixtureProcessGroup,
} from "./fixture-process-group.ts";
import {
  persistWorkerMemorySample,
  sampleProcessTreeMemory,
} from "../playwright-workers.ts";

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

export const playwrightScratchRoot = playwrightScratch.root;
const fixtureViteCaches = new Set<string>();
const fixtureProcessGroupRegistry = join(
  playwrightScratch.root,
  "fixture-process-groups",
);
process.once("exit", () => {
  for (const cacheDir of fixtureViteCaches)
    rmSync(cacheDir, { recursive: true, force: true });
});
const fixtureEntitySchemas = JSON.parse(
  readFileSync(
    new URL(
      "../../../../runtime/packages/runtime-http/openapi.json",
      import.meta.url,
    ),
    "utf8",
  ),
).components.schemas;
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
 */
export async function createFixtureViteServer(config: InlineConfig) {
  const cacheDir = mkdtempSync(join(playwrightScratch.root, "vite-cache-"));
  fixtureViteCaches.add(cacheDir);
  try {
    const { createServer } = await import("vite");
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
      handle(
        request: IncomingMessage,
        response: ServerResponse,
        next: Connect.NextFunction,
      ) {
        let pathname;
        try {
          pathname = new URL(request.url ?? "", "http://fixture.local")
            .pathname;
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
    let closePromise: Promise<void> | undefined;
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

function readGeneratedString(name: string) {
  const match = generatedApiSchema.match(
    new RegExp(`${name}\\s*=\\s*"([^"]+)"`),
  );
  if (!match?.[1]) throw new Error(`Generated ${name} is missing.`);
  return match[1];
}

export function readApiSchemaHash() {
  const match = generatedApiSchema.match(
    /API_SCHEMA_HASH\s*=\s*"([0-9a-f]{64})"/,
  );
  if (!match?.[1]) throw new Error("Generated API schema hash is missing.");
  return match[1];
}

export function apiSchemaHeaders(headers: Record<string, string> = {}) {
  return { [API_SCHEMA_HASH_HEADER]: readApiSchemaHash(), ...headers };
}

/**
 * Assert the schema update gate is not visible. Mantine may keep its closed
 * alertdialog portal in the DOM, so checking for absence would misread it.
 */
export async function expectNoSchemaGate(page: import("playwright").Page) {
  await expect(
    page.getByRole("alertdialog", { name: "Studio update required" }),
  ).toBeHidden();
}

const apiSchemaFixtureTargets = new WeakSet();
const apiSchemaFixtureBrowsers = new WeakSet();
const apiSchemaFixtureHeaderState = { enabled: true };

function routeMatchersEqual(
  left: Parameters<Page["route"]>[0],
  right: Parameters<Page["route"]>[0],
) {
  if (Object.is(left, right)) return true;
  return (
    left instanceof RegExp &&
    right instanceof RegExp &&
    left.source === right.source &&
    left.flags === right.flags
  );
}

export function installApiSchemaFixtureHeaders(
  target: import("playwright").Page | import("playwright").BrowserContext,
) {
  if (apiSchemaFixtureTargets.has(target)) return;
  apiSchemaFixtureTargets.add(target);
  const registerRoute = target.route.bind(target);
  const unregisterRoute = target.unroute.bind(target);
  const wrappedHandlers: {
    url: Parameters<Page["route"]>[0];
    handler: Parameters<Page["route"]>[1];
    wrappedHandler: Parameters<Page["route"]>[1];
  }[] = [];
  target.route = (url, handler, options) => {
    const wrappedHandler = (route: Route, request: Request) => {
      if (
        !apiSchemaFixtureHeaderState.enabled ||
        !new URL(request.url()).pathname.startsWith("/api/")
      )
        return handler(route, request);
      let fulfillmentPromise: Promise<void> | undefined;
      const decoratedRoute = new Proxy(route, {
        get(target, property) {
          if (property === "fulfill")
            return (response: Parameters<Route["fulfill"]>[0] = {}) => {
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
              const normalizedHeaders: Record<string, string> = {};
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
        if (routeMatchersEqual(wrappedHandlers[index]?.url ?? url, url))
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

function installApiSchemaFixtureBrowser(browser: import("playwright").Browser) {
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

function installApiSchemaFixtureContext(
  context: import("playwright").BrowserContext,
) {
  installApiSchemaFixtureHeaders(context);
  const originalNewPage = context.newPage;
  context.newPage = async function (...args) {
    const page = await originalNewPage.apply(this, args);
    installApiSchemaFixtureHeaders(page);
    return page;
  };
}

export async function stubSessionCost(page: import("playwright").Page) {
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

export function protocol3SseEvent(name: string, value: unknown) {
  return `event: ${name}\ndata: ${JSON.stringify(value)}\n\n`;
}

type FixtureScope = {
  children: Set<ChildProcess>;
  scratchDirs: Map<ChildProcess, string>;
  errors: unknown[];
  closing?: boolean;
};
const fixtureScopes = new WeakMap<TestInfo, FixtureScope>();
const childTerminationWaitMs = 1_000;
const fixtureCleanupTimeoutMs = 5_000;

function recordStage(stage: string, values: Record<string, unknown> = {}) {
  const path = process.env.PLAYWRIGHT_STAGE_LOG;
  if (!path) return;
  appendFileSync(
    path,
    `${JSON.stringify({ stage, at: Date.now(), ...values })}\n`,
  );
}

function directoryBytes(path: string) {
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

async function waitForChildExit(child: ChildProcess) {
  if (child.exitCode !== null || child.signalCode !== null) return;
  await new Promise<void>((resolve) => {
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

async function waitForChildStart(child: ChildProcess) {
  if (
    child.pid !== undefined ||
    child.exitCode !== null ||
    child.signalCode !== null
  )
    return;
  await new Promise<void>((resolve) => {
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

async function stopFixtureChild(child: ChildProcess) {
  await waitForChildStart(child);
  if (child.pid === undefined) return;
  await stopFixtureProcessGroup(child);
  await waitForChildExit(child);
  if (child.exitCode === null && child.signalCode === null)
    throw new Error(`Fixture process ${child.pid} did not exit after SIGKILL`);
}

export type ClientTestFixtures = import("@playwright/test").PlaywrightTestArgs &
  import("@playwright/test").PlaywrightTestOptions & {
    apiSchemaFixtureHeaders: boolean;
    apiSchemaFixtureHeaderSetup: void;
    fixtureChildCleanup: void;
    readOnlyRust: {
      origin: string;
      open: (page: import("playwright").Page) => Promise<void>;
    };
  };
export type ClientWorkerFixtures =
  import("@playwright/test").PlaywrightWorkerArgs &
    import("@playwright/test").PlaywrightWorkerOptions & {
      apiSchemaFixtureBrowserSetup: void;
      browserStageTiming: void;
      readOnlyRustOrigin: string;
    };

const sharedFixtures: import("@playwright/test").Fixtures<
  ClientTestFixtures,
  ClientWorkerFixtures,
  import("@playwright/test").PlaywrightTestArgs &
    import("@playwright/test").PlaywrightTestOptions,
  import("@playwright/test").PlaywrightWorkerArgs &
    import("@playwright/test").PlaywrightWorkerOptions
> = {
  apiSchemaFixtureHeaders: [true, { option: true }],
  apiSchemaFixtureBrowserSetup: [
    async ({ playwright, browserName }, use) => {
      const browserType = playwright[browserName];
      const originalLaunch = browserType.launch;
      const wrappedLaunch = async function (
        this: BrowserType,
        ...args: Parameters<BrowserType["launch"]>
      ) {
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
      const processTreeBreakdown: ReturnType<
        typeof sampleProcessTreeMemory
      >["processes"] = [];
      const memoryPeak = {
        rssBytes: 0,
        pssBytes: 0,
        processTreeRssBytes: 0,
        processTreePssBytes: 0,
        processTreeProcessCount: 0,
        processTreeBreakdown,
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
        let workerMemorySelection: { workerMemoryRunId?: string } = {};
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
  // Opt in only for layout/navigation witnesses. Mutating integration tests keep
  // their own backend, while these tests reuse one minimal seed per worker.
  readOnlyRustOrigin: [
    async ({ browserName: _browserName }, use, workerInfo) => {
      const scope: FixtureScope = {
        children: new Set(),
        scratchDirs: new Map(),
        errors: [],
      };
      const root = mkdtempSync(join(playwrightScratch.root, "readonly-rust-"));
      const child = spawnScopedFixture(
        scope,
        {
          ...workerInfo,
          titlePath: ["read-only Rust fixture"],
          file: "",
        },
        process.execPath,
        [rustUiFixture, root, "--workers", "0"],
        {
          stdio: ["ignore", "pipe", "pipe"],
        },
      );
      let diagnostics = "";
      child.stderr?.on("data", (chunk) => {
        diagnostics += chunk;
      });
      try {
        const port = await waitForFixturePort(child, {
          diagnostics: () => diagnostics,
        });
        await use(`http://127.0.0.1:${port}`);
      } finally {
        await stopFixtureChild(child);
        rmSync(root, { recursive: true, force: true });
        rmSync(rustFixtureStateDirectory(root), {
          recursive: true,
          force: true,
        });
      }
      if (scope.errors.length)
        throw new AggregateError(
          scope.errors,
          "Read-only fixture cleanup failed",
        );
    },
    { scope: "worker", timeout: 60_000 },
  ],
  readOnlyRust: async ({ readOnlyRustOrigin }, use) => {
    const writes: string[] = [];
    await use({
      origin: readOnlyRustOrigin,
      async open(page) {
        // Browser storage and preference replies remain private to each context.
        // Any other persistent mutation means this test needs an isolated server.
        await page.context().route("**/api/**", (route) => {
          const request = route.request();
          const path = new URL(request.url()).pathname;
          if (
            request.method() === "GET" ||
            request.method() === "HEAD" ||
            path === "/api/sync/subscriptions"
          )
            return route.fallback();
          if (path === "/api/sync/preferences")
            return route.fulfill({
              json: { fields: request.postDataJSON().fields },
            });
          if (path === "/api/organization") {
            const body = request.postDataJSON();
            if (
              body.read_state &&
              Object.keys(body).every(
                (key) => key === "id" || key === "read_state",
              )
            ) {
              const { thread_id, turn_id, read, expected_revision } =
                body.read_state;
              return route.fulfill({
                json: {
                  id: body.id,
                  readState: {
                    threadId: thread_id,
                    turnId: turn_id,
                    read,
                    revision: expected_revision + 1,
                  },
                },
              });
            }
          }
          writes.push(`${request.method()} ${path}`);
          return route.fulfill({
            status: 405,
            json: { error: "Read-only fixture" },
          });
        });
        await page.goto(readOnlyRustOrigin);
      },
    });
    expect(
      writes,
      "Read-only witnesses must not mutate the shared backend",
    ).toEqual([]);
  },
  fixtureChildCleanup: [
    async ({ browserName: _browserName }, use, testInfo) => {
      const scope: FixtureScope = {
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

const test = baseTest.extend<ClientTestFixtures, ClientWorkerFixtures>(
  sharedFixtures,
);

export { test, expect };

export function spawnFixture(
  command: string,
  args: readonly string[],
  options?: SpawnOptions,
) {
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

  return spawnScopedFixture(scope, testInfo, command, args, options);
}

function spawnScopedFixture(
  scope: FixtureScope,
  testInfo: Pick<TestInfo, "titlePath" | "file" | "workerIndex">,
  command: string,
  args: readonly string[],
  options?: SpawnOptions,
) {
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
    : (inheritedCodexHome ?? parentCodexHome);
  const ownsClaudeConfigDir = !(
    typeof inheritedClaudeConfigDir === "string" &&
    resolve(inheritedClaudeConfigDir) !== resolve(parentClaudeConfigDir)
  );
  const claudeConfigDir = ownsClaudeConfigDir
    ? join(fixtureScratch, "claude-config")
    : (inheritedClaudeConfigDir ?? parentClaudeConfigDir);
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
  const child = spawn(command, args, {
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
    },
  });
  const childKill = child.kill.bind(child);
  child.kill = (signal) =>
    signalFixtureProcessGroup(child, signal) || childKill(signal);
  child.once("spawn", () => {
    try {
      if (child.pid !== undefined)
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
  let scratchInterval: ReturnType<typeof setInterval> | undefined;
  const recordScratchSize = () =>
    recordStage("fixture_scratch_size", {
      title: testInfo.titlePath.join(" › "),
      file: testInfo.file,
      workerIndex: testInfo.workerIndex,
      path: fixtureStateDirectory,
      bytes:
        fixtureStateDirectory === undefined
          ? 0
          : directoryBytes(fixtureStateDirectory),
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
    if (("code" in error ? error.code : undefined) !== "ESRCH")
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
export type JsonValue =
  import("../src/generated/api.ts").components["schemas"]["JsonValue"];
export type SessionResponse =
  import("../src/generated/api.ts").components["schemas"]["SessionResponse"];
export type SyncPullResponse =
  import("../src/generated/api.ts").components["schemas"]["SyncPullResponse"];
export type SyncPullResetResponse =
  import("../src/generated/api.ts").components["schemas"]["SyncPullResetResponse"];
export type SyncEntity =
  import("../src/generated/api.ts").components["schemas"]["SyncEntity"];
export type AccountRateLimitsDto =
  import("../src/generated/api.ts").components["schemas"]["AccountRateLimitsDto"];
export type TranscriptPageResponse =
  import("../src/generated/api.ts").components["schemas"]["TranscriptPageResponse"];
export type SyncIdentityResponse =
  import("../src/generated/api.ts").components["schemas"]["SyncIdentityResponse"];
export type SyncProtocolResponse =
  import("../src/generated/api.ts").components["schemas"]["SyncProtocolResponse"];
export type ResourceChangeEvent =
  import("../src/generated/api.ts").components["schemas"]["ResourceChangeEvent"];
export type ResourceHeartbeatEvent =
  import("../src/generated/api.ts").components["schemas"]["ResourceHeartbeatEvent"];
export type ResourceTokenRatesEvent =
  import("../src/generated/api.ts").components["schemas"]["ResourceTokenRatesEvent"];
export type ResourceRef =
  import("../src/generated/api.ts").components["schemas"]["ResourceRef"];
export type ResourceNotifyRequest =
  import("../src/generated/api.ts").components["schemas"]["ResourceNotifyRequest"];
export type ResourceNotifyAck =
  import("../src/generated/api.ts").components["schemas"]["ResourceNotifyAck"];
export type AgentEntityDto =
  import("../src/generated/api.ts").components["schemas"]["AgentEntityDto"];
export type ChatEntityDto =
  import("../src/generated/api.ts").components["schemas"]["ChatEntityDto"];
export type ComplaintEntityDto =
  import("../src/generated/api.ts").components["schemas"]["ComplaintEntityDto"];
export type EdgeEntityDto =
  import("../src/generated/api.ts").components["schemas"]["EdgeEntityDto"];
export type ProjectEntityDto =
  import("../src/generated/api.ts").components["schemas"]["ProjectEntityDto"];
export type RoomEntityDto =
  import("../src/generated/api.ts").components["schemas"]["RoomEntityDto"];
export type RuleEntityDto =
  import("../src/generated/api.ts").components["schemas"]["RuleEntityDto"];
export type TaskEntityDto =
  import("../src/generated/api.ts").components["schemas"]["TaskEntityDto"];
export type WorkspaceEntityDto =
  import("../src/generated/api.ts").components["schemas"]["WorkspaceEntityDto"];
export type WorkEntityDto =
  import("../src/generated/api.ts").components["schemas"]["WorkEntityDto"];
export type EventEntityDto =
  import("../src/generated/api.ts").components["schemas"]["EventEntityDto"];
export type MonitorEntityDto =
  import("../src/generated/api.ts").components["schemas"]["MonitorEntityDto"];
export type PeerTeamEntityDto =
  import("../src/generated/api.ts").components["schemas"]["PeerTeamEntityDto"];
export type RequestEntityDto =
  import("../src/generated/api.ts").components["schemas"]["RequestEntityDto"];

export type TestAgent = AgentEntityDto;

export type TestRoom = RoomEntityDto;
export type TestTask = TaskEntityDto;
export type TestProject = ProjectEntityDto;
export type TestComplaint = ComplaintEntityDto;
export type TestRuntimeView = {
  agents: TestAgent[];
  rooms: TestRoom[];
  tasks: TestTask[];
  monitors: MonitorEntityDto[];
  complaints: TestComplaint[];
  projects: TestProject[];
  events: EventEntityDto[];
  peerTeams: PeerTeamEntityDto[];
  requests: RequestEntityDto[];
  rules: RuleEntityDto[];
  work: WorkEntityDto[];
  viewScopedAgents?: boolean;
  workDetails?: import("../src/generated/api.ts").components["schemas"]["WorkItem"][];
  agentDetails?: import("../src/generated/api.ts").components["schemas"]["AgentDetailEntityDto"][];
  connected?: WorkspaceEntityDto["connected"];
  peerTeamsVersion?: WorkspaceEntityDto["peerTeamsVersion"];
  projectOrganizationVersion?: WorkspaceEntityDto["projectOrganizationVersion"];
  tasksHistoryLimit?: WorkspaceEntityDto["tasksHistoryLimit"];
  nativeNotices: WorkspaceEntityDto["nativeNotices"];
  sidebarOrder: WorkspaceEntityDto["sidebarOrder"];
  accountsSummary?: import("../src/generated/api.ts").components["schemas"]["AccountsSummaryDto"];
  desktop?: import("../src/generated/api.ts").components["schemas"]["DesktopEntityDto"];
  accountCostsByAccount?: Record<
    string,
    import("../src/generated/api.ts").components["schemas"]["AccountCostEntityDto"]
  >;
  sessionCostsByAgent?: Record<
    string,
    import("../src/generated/api.ts").components["schemas"]["SessionCostEntityDto"]
  >;
  terminals?: import("../src/generated/api.ts").components["schemas"]["TerminalEntityDto"][];
  studioErrors?: import("../src/generated/api.ts").components["schemas"]["StudioError"][];
  rateLimits: AccountRateLimitsDto | null;
  queuesByAgent?: Record<
    string,
    import("../src/generated/api.ts").components["schemas"]["QueueView"]
  >;
  rateLimitsByAccount: Record<string, AccountRateLimitsDto>;
  stateDir?: WorkspaceEntityDto["stateDir"];
};
export type TestStateView = {
  token: string;
  stateDir?: WorkspaceEntityDto["stateDir"];
  chats: ChatEntityDto[];
  edges: EdgeEntityDto[];
  runtime: TestRuntimeView;
  threads: TestAgent[];
};
export type EntityFixtureState = Omit<TestStateView, "token" | "threads">;

export type JsonObject = Record<string, JsonValue>;
export type EntityEnvelope = {
  collection: string;
  id: string;
  value: JsonValue;
};

export type EntityPullFixtureOptions = {
  scope?: string;
  after?: number;
  limit?: number;
  fresh?: boolean;
  initialHigh?: number;
  reset?: boolean;
  floor?: number;
  maxSeq?: number;
  generation?: number;
  priorityId?: string | undefined;
  tombstones?: SyncEntity[];
  transcript?: TranscriptPageResponse;
  transcriptSeq?: number;
  drafts?: import("../src/generated/api.ts").components["schemas"]["SyncDocument"][];
  documents?: SyncEntity[];
};

const ENTITY_PAGE_SIZE: number = 500;

const RESOURCE_STREAM_PROTOCOL: ResourceChangeEvent["protocol"] =
  SYNC_PROTOCOL_VERSION;

function readJson(
  response: Response,
  path: "/api/session",
): Promise<SessionResponse>;
function readJson(
  response: Response,
  path: "/api/sync/pull",
): Promise<SyncPullResponse | SyncPullResetResponse>;
function readJson(
  response: Response,
  path: "/api/sync/identity",
): Promise<SyncIdentityResponse>;
function readJson(
  response: Response,
  path: "/api/sync/protocol",
): Promise<SyncProtocolResponse>;
function readJson(
  response: Response,
  path: "/api/sync/notify",
): Promise<ResourceNotifyAck>;
async function readJson(response: Response, path: string): Promise<unknown> {
  if (!response.ok)
    throw new Error(
      `GET ${path} failed with HTTP ${response.status}: ${(await response.text()).slice(0, 1000)}`,
    );
  return response.json();
}

function isJsonObject(value: unknown): value is JsonObject {
  return value !== null && typeof value === "object" && !Array.isArray(value);
}

/**
 * Read the session token and current entity rows used by browser specs.
 * The returned API is deliberately collection based; it is not a snapshot copy.
 */
export async function readTestState(
  origin: string,
  options: { agentDetails?: boolean } = {},
): Promise<TestStateView> {
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

  const session: SessionResponse = await readJson(
    sessionResponse,
    "/api/session",
  );

  type Upsert = Extract<SyncEntity, { op: "upsert" }>;
  const entities = new Map<string, Map<string, Upsert>>();
  let after = 0;
  let fresh = false;
  let nextResponse = firstPullResponse;

  for (;;) {
    const page: SyncPullResponse | SyncPullResetResponse = await readJson(
      nextResponse,
      "/api/sync/pull",
    );
    if (page.reset === true) {
      after = 0;
      fresh = true;
    } else {
      fresh = false;
      for (const document of page.documents) {
        if (!("op" in document)) throw new Error("Expected an entity change");
        if (document.op === "delete") continue;
        const decoded = document;
        if (
          !isJsonObject(decoded) ||
          typeof decoded.collection !== "string" ||
          typeof decoded.id !== "string" ||
          !isJsonObject(decoded.value)
        )
          throw new Error(`Invalid entity envelope in ${document.id}`);

        const entity: EntityEnvelope = {
          collection: decoded.collection,
          id: decoded.id,
          value: decoded.value,
        };
        let collection = entities.get(entity.collection);
        if (!collection) {
          collection = new Map();
          entities.set(entity.collection, collection);
        }
        collection.set(entity.id, document);
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

  function list<C extends Upsert["collection"]>(
    collection: C,
  ): Extract<Upsert, { collection: C }>["value"][];
  function list(collection: Upsert["collection"]): Upsert["value"][] {
    return [...(entities.get(collection)?.values() ?? [])].map(
      (document) => document.value,
    );
  }
  function get<C extends Upsert["collection"]>(
    collection: C,
    id: string,
  ): Extract<Upsert, { collection: C }>["value"] | undefined;
  function get(
    collection: Upsert["collection"],
    id: string,
  ): Upsert["value"] | undefined {
    return entities.get(collection)?.get(id)?.value;
  }

  // Materialize only the collections used by browser specs. Keeping these as
  // ordinary arrays also lets fixture specs adjust the data they serve.
  if (options.agentDetails) {
    const baselines = await readResourceBaselines(
      origin,
      list("agentSummary").map((agent) => ({
        kind: "agentDetail",
        agentId: String(agent.id),
      })),
    );
    const details = new Map<string, Upsert>();
    for (const entry of baselines)
      for (const document of entry.entityChanges?.documents || [])
        if (document.op === "upsert") details.set(document.id, document);
    entities.set("agentDetail", details);
  }
  const agents = list("agentSummary").map((summary) => ({
    ...get("agentDetail", summary.id),
    ...summary,
  }));
  const rooms = list("room");
  const tasks = list("task");
  const monitors = list("monitor");
  const complaints = list("complaint");
  const projects = list("project");
  const events = list("event");
  const peerTeams = list("peerTeam");
  const requests = list("request");
  const rules = list("rule");
  const work = list("work");
  const chats = list("chat");
  const edges = list("edge");

  const runtime: TestRuntimeView = {
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
    connected: get("workspace", "current")?.connected,
    peerTeamsVersion: get("workspace", "current")?.peerTeamsVersion,
    projectOrganizationVersion: get("workspace", "current")
      ?.projectOrganizationVersion,
    tasksHistoryLimit: get("workspace", "current")?.tasksHistoryLimit,
    get nativeNotices() {
      return get("workspace", "current")?.nativeNotices;
    },
    get sidebarOrder() {
      return get("workspace", "current")?.sidebarOrder;
    },
    // Rate limits are per-account `rateLimit` entities, as in the renderer projection.
    get rateLimitsByAccount() {
      return Object.fromEntries(
        [...(entities.get("rateLimit") ?? [])]
          .filter(
            (
              entry,
            ): entry is [
              string,
              Extract<SyncEntity, { collection: "rateLimit"; op: "upsert" }>,
            ] => entry[1].collection === "rateLimit",
          )
          .map(([id, document]) => [id, document.value]),
      );
    },
    get rateLimits() {
      return this.rateLimitsByAccount.default ?? null;
    },
    get stateDir() {
      return get("workspace", "current")?.stateDir;
    },
  };

  return {
    token: session.token,
    get stateDir() {
      return get("workspace", "current")?.stateDir;
    },
    chats,
    edges,
    runtime,
    threads: agents,
  };
}

const entityWorkspaceKeys: (keyof WorkspaceEntityDto)[] = [
  "connected",
  "nativeNotices",
  "peerTeamsVersion",
  "projectOrganizationVersion",
  "sidebarOrder",
  "stateDir",
  "tasksHistoryLimit",
];

const snapshotOnlyAgentKeys: Set<string> = new Set([
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

function pickJsonKeys(
  value: Partial<JsonObject>,
  keys: Iterable<string>,
): JsonObject {
  const source = value;

  const picked: JsonObject = {};
  for (const key of keys) {
    const jsonKey = String(key);
    const item = source[jsonKey];
    if (Object.hasOwn(source, jsonKey) && item !== undefined)
      picked[jsonKey] = item;
  }
  return picked;
}

/**
 * The fixture projection selects fields by collection. The real-server parity
 * witness validates these dynamically projected DTOs against the schema owner.
 */
function generatedSyncEntity(document: JsonObject): SyncEntity {
  return document as SyncEntity;
}

function generatedSyncPullResponse(
  response: SyncPullResponse | SyncPullResetResponse,
): SyncPullResponse | SyncPullResetResponse {
  return response;
}

/**
 * Build entity values from a fixture, omitting fields that the entity contract
 * does not expose.
 */
function entityValuesFromSnapshot(
  snapshot: EntityFixtureState,
): { collection: string; id: string; value: JsonValue }[] {
  const runtime = snapshot.runtime;

  const values: { collection: string; id: string; value: JsonValue }[] = [];

  const append = (
    collection: string,
    rows: JsonObject[] | null | undefined,
    project: (row: JsonObject) => JsonValue = (row) => row,
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
  append(
    runtime.viewScopedAgents ? "agentSummary" : "agent",
    runtime.agents,
    (row) => {
      if (runtime.viewScopedAgents) {
        const properties =
          fixtureEntitySchemas.AgentSummaryEntityDto.properties;
        return pickJsonKeys(row, Object.keys(properties));
      }
      const entity = { ...row };
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
    },
  );
  append("task", runtime.tasks);
  append("monitor", runtime.monitors);
  append("complaint", runtime.complaints, (row) => {
    const entity = { ...row };
    delete entity.needsResponse;
    return entity;
  });
  append("peerTeam", runtime.peerTeams);
  append("rule", runtime.rules);
  append("work", runtime.work);
  append("workDetail", runtime.workDetails, (row) => ({
    ...row,
    results: [],
    decisions: [],
    releases: [],
  }));
  for (const task of runtime.workDetails ?? []) {
    for (const [field, collection] of [
      ["results", "workResult"],
      ["decisions", "workDecision"],
      ["releases", "workRelease"],
    ] as const) {
      const history = task[field];
      if (!Array.isArray(history)) continue;
      for (const [index, value] of history.entries()) {
        const id = `${task.id}:${index}`;
        values.push({
          collection,
          id,
          value: { id, taskId: task.id, rootId: task.rootId, index, value },
        });
      }
    }
  }
  append("agentDetail", runtime.agentDetails, (row) =>
    pickJsonKeys(
      row,
      Object.keys(fixtureEntitySchemas.AgentDetailEntityDto.properties),
    ),
  );
  append("studioError", runtime.studioErrors);
  append("chat", snapshot.chats);
  append("edge", snapshot.edges);
  for (const [field, collection] of [
    ["accountsSummary", "accounts"],
    ["desktop", "desktop"],
  ] as const) {
    if (isJsonObject(runtime[field]))
      values.push({ collection, id: "current", value: runtime[field] });
  }
  for (const [field, collection] of [
    ["accountCostsByAccount", "accountCost"],
    ["sessionCostsByAgent", "sessionCost"],
  ] as const) {
    if (isJsonObject(runtime[field]))
      for (const [id, value] of Object.entries(runtime[field]))
        if (isJsonObject(value)) values.push({ collection, id, value });
  }
  append("terminal", runtime.terminals);
  if (isJsonObject(runtime.queuesByAgent)) {
    for (const [id, value] of Object.entries(runtime.queuesByAgent))
      if (isJsonObject(value)) values.push({ collection: "queue", id, value });
  }
  const limits = isJsonObject(runtime.rateLimitsByAccount)
    ? runtime.rateLimitsByAccount
    : isJsonObject(runtime.rateLimits)
      ? { default: runtime.rateLimits }
      : {};
  for (const [account, value] of Object.entries(limits))
    if (isJsonObject(value))
      values.push({ collection: "rateLimit", id: account, value });

  const workspaceSource = {
    ...runtime,
    ...(snapshot.stateDir === undefined ? {} : { stateDir: snapshot.stateDir }),
  };
  values.push({
    collection: "workspace",
    id: "current",
    value: pickJsonKeys(workspaceSource, entityWorkspaceKeys),
  });
  return values;
}

export function syncIdentityFixture(
  workspaceId: string = randomUUID().replaceAll("-", ""),
): SyncIdentityResponse {
  if (!/^[a-f0-9]{32}$/.test(workspaceId))
    throw new Error(
      "Sync fixture workspaceId must match the server's 32-hex identity",
    );
  return { workspaceId, syncProtocol: 2 };
}

export function syncProtocolFixture({
  unixSocket = false,
} = {}): SyncProtocolResponse {
  return {
    protocolVersion: SYNC_PROTOCOL_VERSION,
    supportedVersions: [SYNC_PROTOCOL_VERSION],
    capabilities: [
      "pull",
      "stream",
      "streamChanges",
      "entityReset",
      "typedResources",
      "tokenRates",
      "entitySeedExcluded:canvasAgentThreads",
      "entitySeedExcluded:canvasSpawnEdges",
      ...(unixSocket ? ["unixSocket"] : []),
    ],
    scopes: ["state:entities:v1", "transcript:<agent-id>", "drafts"],
    pullEndpoint: "/api/sync/pull",
    streamEndpoint: "/api/sync/stream",
    maxEntityPage: 500,
    maxOtherPage: 100,
    maxStreamDocuments: SYNC_ENTITY_STREAM_MAX_DOCUMENTS,
    maxStreamBytes: SYNC_STREAM_EVENT_MAX_BYTES,
  };
}

export function skipIfWebKitCannotLaunch(
  test: { skip: (condition: boolean, description?: string) => void },
  error: unknown,
) {
  const missingLibrary = String(error).match(
    /error while loading shared libraries: ([^:]+): cannot open shared object file/i,
  )?.[1];
  if (missingLibrary) {
    const reason = `WebKit cannot launch here: missing ${missingLibrary}, ...`;
    console.log(`SKIP: ${reason}`);
    test.skip(true, reason);
  }
}

export async function readFixtureSyncContract(
  origin: string,
): Promise<{ identity: SyncIdentityResponse; protocol: SyncProtocolResponse }> {
  const [identityResponse, protocolResponse] = await Promise.all([
    fetch(new URL("/api/sync/identity", origin)),
    fetch(new URL("/api/sync/protocol", origin)),
  ]);
  const identity = await readJson(identityResponse, "/api/sync/identity");
  const protocol = await readJson(protocolResponse, "/api/sync/protocol");
  if (
    !/^[a-f0-9]{32}$/.test(identity.workspaceId) ||
    identity.syncProtocol !== 2 ||
    protocol.protocolVersion !== SYNC_PROTOCOL_VERSION
  )
    throw new Error("Fixture server returned an invalid sync contract");
  return { identity, protocol };
}

export type MutableEntityPullState = {
  documents: Map<string, SyncEntity>;
  maxSeq: number;
};

const nodeEntityPullStates: WeakMap<MutableEntityPullState> = new WeakMap();

const nodeSyncSessions: WeakMap<
  Map<
    string,
    (
      update: import("../src/generated/api.ts").components["schemas"]["SyncSubscriptionRequest"],
    ) => void
  >
> = new WeakMap();

function createMutableEntityPullState(): MutableEntityPullState {
  return { documents: new Map(), maxSeq: 0 };
}

/**
 * Version changed entities and tombstones so clients that have advanced their
 * cursor can observe fixture state changes on a later pull.
 */
function reconcileMutableEntityPullState(
  pullState: MutableEntityPullState,
  snapshot: EntityFixtureState,
) {
  const nextById = new Map(
    entityValuesFromSnapshot(snapshot).map((entity) => [
      `entity:${entity.collection}:${entity.id}`,
      structuredClone(entity),
    ]),
  );
  for (const [key, previous] of pullState.documents) {
    if (previous.op === "delete") continue;
    const next = nextById.get(key);
    if (!next) {
      pullState.documents.set(
        key,
        generatedSyncEntity({
          op: "delete",
          collection: previous.collection,
          id: previous.id,
          seq: ++pullState.maxSeq,
        }),
      );
      continue;
    }
    if (JSON.stringify(next.value) !== JSON.stringify(previous.value))
      pullState.documents.set(
        key,
        generatedSyncEntity({
          op: "upsert",
          ...next,
          seq: ++pullState.maxSeq,
        }),
      );
  }
  for (const [key, entity] of nextById) {
    const previous = pullState.documents.get(key);
    if (!previous || previous.op === "delete")
      pullState.documents.set(
        key,
        generatedSyncEntity({
          op: "upsert",
          ...entity,
          seq: ++pullState.maxSeq,
        }),
      );
  }
}

/**
 * Commit a fixture state change and return the entity documents its mutation
 * response should carry. This mirrors the server's `_syncEntities` envelope.
 */
export function updateEntitySyncFixture(
  snapshot: EntityFixtureState,
): SyncEntity[] {
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
 */
export function entityMutationFixtureResponse(
  snapshot: EntityFixtureState,
  response: Record<string, unknown>,
): Record<string, unknown> {
  return {
    ...response,
    _syncEntities: updateEntitySyncFixture(snapshot),
  };
}

export function accountTileGroup(
  page: import("@playwright/test").Page | import("@playwright/test").Locator,
  label: string,
) {
  return page.getByRole("group", { name: label, exact: true });
}

export function accountTileButton(
  page: import("@playwright/test").Page | import("@playwright/test").Locator,
  label: string,
  accountName: string,
) {
  return accountTileGroup(page, label).getByRole("button", {
    name: `Select account ${accountName}`,
    exact: true,
  });
}

/**
 * Serve the same sync routes from a small Node HTTP fixture backend.
 */
export function handleEntitySyncFixtureRequest(
  request: import("node:http").IncomingMessage,
  response: import("node:http").ServerResponse,
  fixture: {
    snapshot: EntityFixtureState;
    workspaceId: string;
    unixSocket?: boolean;
    versionedResources?: boolean;
    epoch?: string;
    onPull?: () => void;
    subscriptionUpdate?: import("../src/generated/api.ts").components["schemas"]["SyncSubscriptionRequest"];
    progress?: (
      resource: ResourceRef,
    ) =>
      | (
          | Pick<
              import("../src/generated/api.ts").components["schemas"]["AuthProgressEvent"],
              "resource" | "value"
            >
          | Pick<
              import("../src/generated/api.ts").components["schemas"]["VaultReadinessEvent"],
              "resource" | "value"
            >
        )
      | undefined;
    onStreamReady?: (
      notify: (
        resources: ResourceRef[],
        reason?: "change" | "reconnect",
      ) => void,
      resources: ResourceRef[],
    ) => (() => void) | void;
  },
): boolean {
  const url = new URL(request.url || "/", "http://fixture.invalid");
  if (!url.pathname.startsWith("/api/sync/")) return false;
  const identity = syncIdentityFixture(fixture.workspaceId);
  const json = (value: unknown) => {
    response.setHeader("Content-Type", "application/json");
    response.setHeader(API_SCHEMA_HASH_HEADER, readApiSchemaHash());
    response.end(JSON.stringify(value));
  };
  if (url.pathname === "/api/sync/subscriptions" && request.method === "POST") {
    const apply = (
      update: import("../src/generated/api.ts").components["schemas"]["SyncSubscriptionRequest"],
    ) => {
      const session = nodeSyncSessions
        .get(fixture.snapshot)
        ?.get(update.sessionId);
      if (!session) {
        response.statusCode = 410;
        json({ error: "Unknown sync session; reopen the stream" });
        return;
      }
      session(update);
      json({ gen: update.gen });
    };
    if (fixture.subscriptionUpdate) apply(fixture.subscriptionUpdate);
    else {
      let body = "";
      request.on("data", (chunk) => {
        body += chunk;
      });
      request.on("end", () => apply(JSON.parse(body)));
    }
    return true;
  }
  if (url.pathname === "/api/sync/identity") {
    json(identity);
    return true;
  }
  if (url.pathname === "/api/sync/protocol") {
    json(syncProtocolFixture({ unixSocket: fixture.unixSocket ?? false }));
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

    let pullOptions: EntityPullFixtureOptions = validation.params;
    if (validation.params.scope === "state:entities:v1") {
      let pullState = nodeEntityPullStates.get(fixture.snapshot);
      if (!pullState) {
        pullState = createMutableEntityPullState();
        nodeEntityPullStates.set(fixture.snapshot, pullState);
      }
      reconcileMutableEntityPullState(pullState, fixture.snapshot);
      pullOptions = {
        ...validation.params,
        documents: [...pullState.documents.values()].filter(
          (document) =>
            ![
              "workDetail",
              "workResult",
              "workDecision",
              "workRelease",
              "agentDetail",
            ].includes(document.collection),
        ),
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
    const requestedResources: ResourceRef[] = JSON.parse(
      url.searchParams.get("resources") || "[]",
    );
    let revision = 0;
    let eventId = 0;
    const resourceRevisions = new Map();
    const withResourceVersions = (
      resources: ResourceRef[],
      advance = false,
    ): ResourceChangeEvent["resourceVersions"] => {
      const changed = updateEntitySyncFixture(fixture.snapshot);
      const current = nodeEntityPullStates.get(fixture.snapshot);
      return resources.map((resource) => {
        const key = JSON.stringify(resource);
        const next = (resourceRevisions.get(key) || 0) + (advance ? 1 : 0);
        resourceRevisions.set(key, next || 1);
        if (
          resource.kind === "state" &&
          fixture.snapshot.runtime.viewScopedAgents &&
          current
        ) {
          return {
            resource,
            revision: current.maxSeq,
            cursor: { workspaceId: identity.workspaceId, seq: current.maxSeq },
            replyKind: "snapshot",
            entityChanges: {
              after: 0,
              through: current.maxSeq,
              documents: [...current.documents.values()].filter(
                (document) =>
                  document.op === "upsert" &&
                  ![
                    "workDetail",
                    "workResult",
                    "workDecision",
                    "workRelease",
                    "agentDetail",
                  ].includes(document.collection),
              ),
            },
          };
        }
        if (resource.kind === "state") {
          const documents = changed.filter(
            (document) =>
              ![
                "workDetail",
                "workResult",
                "workDecision",
                "workRelease",
                "agentDetail",
              ].includes(document.collection),
          );
          if (documents.length)
            return {
              resource,
              revision: documents.at(-1)?.seq ?? 0,
              entities: {
                after: (documents[0]?.seq ?? 0) - 1,
                through: documents.at(-1)?.seq ?? 0,
                documents,
              },
            };
        }
        const scope = (
          resource.kind in SUBSCRIPTION_ENTITY_SCOPES
            ? Reflect.get(SUBSCRIPTION_ENTITY_SCOPES, resource.kind)
            : undefined
        ) as
          | {
              collections: readonly string[];
              idField?: string | null;
              agentField?: string | null;
            }
          | undefined;
        const selector: Record<string, unknown> = resource;
        if (resource.kind !== "state" && scope && current) {
          const documents = [...current.documents.values()].filter(
            (document) =>
              document.op === "upsert" &&
              scope.collections.includes(document.collection) &&
              (!scope.idField || document.id === selector[scope.idField]) &&
              (!scope.agentField ||
                ("rootId" in document.value &&
                  document.value.rootId === selector[scope.agentField]) ||
                ("agent" in document.value &&
                  document.value.agent === selector[scope.agentField])),
          );
          return {
            resource,
            revision: current.maxSeq,
            cursor: { workspaceId: identity.workspaceId, seq: current.maxSeq },
            replyKind: "snapshot",
            entityChanges: { after: 0, through: current.maxSeq, documents },
          };
        }
        return { resource, revision: next || 1 };
      });
    };
    response.writeHead(200, {
      "Cache-Control": "no-cache, no-transform",
      Connection: "keep-alive",
      "Content-Type": "text/event-stream; charset=utf-8",
      [API_SCHEMA_HASH_HEADER]: readApiSchemaHash(),
      "X-Accel-Buffering": "no",
    });
    const sessionId = randomUUID();
    response.write(protocol3SseEvent("session", { sessionId }));
    response.write(apiSchemaHandshakeSse());

    const writeFrame = (
      name: string,
      value:
        | ResourceChangeEvent
        | ResourceHeartbeatEvent
        | ResourceTokenRatesEvent
        | import("../src/generated/api.ts").components["schemas"]["SyncSubscriptionAck"]
        | import("../src/generated/api.ts").components["schemas"]["ResourceProgressEvent"],
    ) => {
      eventId += 1;
      response.write(
        `event: ${name}\ndata: ${JSON.stringify(value)}\nid: ${eventId}\n\n`,
      );
    };
    const writeProgress = (resources: ResourceRef[]) => {
      for (const resource of resources) {
        if (
          !requestedResources.some(
            (requested) =>
              JSON.stringify(requested) === JSON.stringify(resource),
          )
        )
          continue;
        const progress = fixture.progress?.(resource);
        if (!progress) continue;
        writeFrame("resource-progress", {
          protocol: RESOURCE_STREAM_PROTOCOL,
          workspaceId: identity.workspaceId,
          epoch,
          revision,
          ...progress,
        });
      }
    };

    const initial: ResourceChangeEvent = {
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

    const rates: ResourceTokenRatesEvent = {
      epoch,
      protocol: RESOURCE_STREAM_PROTOCOL,
      snapshot: true,
      removed: [],
      records: [],
      revision,
      workspaceId: identity.workspaceId,
    };
    writeFrame("token-rates", rates);
    writeProgress(requestedResources);
    const heartbeat = () => {
      if (response.destroyed) return;

      const event: ResourceHeartbeatEvent = {
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
    let sessions = nodeSyncSessions.get(fixture.snapshot);
    if (!sessions)
      nodeSyncSessions.set(fixture.snapshot, (sessions = new Map()));
    sessions.set(sessionId, (update) => {
      const removed = new Set(
        update.remove.map((resource) => JSON.stringify(resource)),
      );
      const next = requestedResources.filter(
        (resource) => !removed.has(JSON.stringify(resource)),
      );
      const keys = new Set(next.map((resource) => JSON.stringify(resource)));
      const addedResources = update.add.map(({ resource }) => resource);
      for (const resource of addedResources) {
        if (!keys.has(JSON.stringify(resource))) next.push(resource);
        keys.add(JSON.stringify(resource));
      }
      // Preserve the array observed by the caller's subscription assertions.
      requestedResources.splice(0, requestedResources.length, ...next);
      writeFrame("subscriptions", { gen: update.gen });
      if (update.add.length)
        writeFrame("resources", {
          ...initial,
          resources: addedResources,
          resourceVersions: fixture.versionedResources
            ? withResourceVersions(addedResources)
            : [],
          revision: ++revision,
        });
      writeProgress(addedResources);
    });
    response.once("close", () => sessions.delete(sessionId));
    const cleanup = fixture.onStreamReady?.((resources, reason = "change") => {
      if (response.destroyed) return;

      const changed: ResourceChangeEvent = {
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
      writeProgress(resources);
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
 */
export function entityPullFixture(
  snapshot: EntityFixtureState,
  optionsOrAfter: number | EntityPullFixtureOptions = {},
):
  | Omit<SyncPullResponse, "workspaceId">
  | Omit<SyncPullResetResponse, "workspaceId"> {
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
        seq: page.length === limit ? (page.at(-1)?.seq ?? 0) : high,
      },
    };
  }
  if (scope.startsWith("transcript:")) {
    const highWatermark = options.transcriptSeq ?? 1;
    const limit = Math.min(100, Math.max(1, options.limit ?? 100));

    const transcript: TranscriptPageResponse = options.transcript ?? {
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
      {
        id: scope,
        seq: highWatermark,
        payload: JSON.stringify(transcript),
        _deleted: false,
      },
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
            ? (page.at(-1)?.seq ?? 0)
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
      op: "upsert",
      ...entity,
      seq: index + 1,
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
      (document.op !== "delete" || document.seq > floor) &&
      (!fresh || document.op !== "delete" || document.seq > initialHigh),
  );
  if (after === 0 && fresh && options.priorityId) {
    eligible = eligible.sort((left, right) => {
      return (
        Number(
          right.collection === "agent" && right.id === options.priorityId,
        ) -
        Number(left.collection === "agent" && left.id === options.priorityId)
      );
    });
  }
  const page = eligible.slice(0, limit);
  const checkpoint = page.length === limit ? (page.at(-1)?.seq ?? 0) : high;
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
 * The real API rejects negative and malformed cursors and unknown scopes.
 */
export function validateSyncPullRequest(requestUrl: string | URL):
  | {
      ok: true;
      params: Required<
        Pick<
          EntityPullFixtureOptions,
          "scope" | "after" | "limit" | "fresh" | "initialHigh" | "reset"
        >
      > &
        Pick<EntityPullFixtureOptions, "priorityId">;
    }
  | { ok: false; status: number; body: JsonObject } {
  const url = new URL(requestUrl, "http://fixture.invalid");
  const query = url.searchParams;
  const first = (key: string) =>
    query.getAll(key).find((value) => value !== "");
  const scope = first("scope") ?? "state:entities:v1";

  const numbers = { after: 0, limit: 500, initialHigh: 0 };

  const defaults: Array<[keyof typeof numbers, number]> = [
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
    if (!/^-?\d+$/.test(raw))
      return {
        ok: false,
        status: 400,
        body: { error: "Invalid request", location: ["query", key] },
      };
    numbers[key] = Number(raw);
    if (numbers[key] < 0)
      return {
        ok: false,
        status: 400,
        body: { error: "Invalid request", location: ["query", key] },
      };
  }
  const supportedScope =
    scope === "state:entities:v1" ||
    scope === "drafts" ||
    (scope.startsWith("transcript:") && scope.length < 300);
  if (!supportedScope)
    return {
      ok: false,
      status: 400,
      body: { error: "Invalid request", location: ["query", "scope"] },
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
 */
export function entityPullFixtureForRequest(
  snapshot: EntityFixtureState,
  requestUrl: string | URL,
  overrides: EntityPullFixtureOptions = {},
):
  | Omit<SyncPullResponse, "workspaceId">
  | Omit<SyncPullResetResponse, "workspaceId"> {
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
 */
export async function stubEntityState(
  page: Pick<Page, "route">,
  snapshot: EntityFixtureState,
  contract: { identity: SyncIdentityResponse; protocol: SyncProtocolResponse },
): Promise<{
  update: (
    nextSnapshot: EntityFixtureState,
    options?: { origin: string; token: string; resources?: ResourceRef[] },
  ) => Promise<ResourceNotifyAck | undefined>;
}> {
  const { identity: identityResponse, protocol: protocolResponse } = contract;
  if (
    !/^[a-f0-9]{32}$/.test(identityResponse.workspaceId) ||
    identityResponse.syncProtocol !== 2 ||
    protocolResponse.protocolVersion !== SYNC_PROTOCOL_VERSION
  )
    throw new Error(
      "stubEntityState requires the fixture backend's generated sync contract",
    );
  const identity = identityResponse.workspaceId;
  let generation = 1;

  let currentSnapshot: EntityFixtureState = snapshot;
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
   */
  const update = async (
    nextSnapshot: EntityFixtureState,
    publish?: { origin: string; token: string; resources?: ResourceRef[] },
  ): Promise<ResourceNotifyAck | undefined> => {
    currentSnapshot = nextSnapshot;
    reconcileMutableEntityPullState(pullState, currentSnapshot);
    generation++;
    if (!publish) return undefined;
    const body = {
      requestId: randomUUID(),
      resources: publish.resources ?? [{ kind: "state" }],
    } satisfies ResourceNotifyRequest;
    const response = await fetch(new URL("/api/sync/notify", publish.origin), {
      method: "POST",
      headers: {
        "Content-Type": "application/json",
        "X-Canvas-Token": publish.token,
        [API_SCHEMA_HASH_HEADER]: readApiSchemaHash(),
      },
      body: JSON.stringify(body),
    });
    return await readJson(response, "/api/sync/notify");
  };
  return { update };
}

export { rustFixtureStateDirectory };

/**
 * Read-only rows of the fixture server's canvas database, for assertions on
 * persisted state. `root` is the directory passed to the fixture.
 */
export function queryFixtureDatabase(
  root: string,
  sql: string,
  ...parameters: import("node:sqlite").SQLInputValue[]
): Record<string, unknown>[] {
  const database = new DatabaseSync(
    join(rustFixtureStateDirectory(root), "canvas.sqlite3"),
    { readOnly: true },
  );
  try {
    database.exec("PRAGMA busy_timeout=5000");
    return database.prepare(sql).all(...parameters);
  } finally {
    database.close();
  }
}

/** Browser fixture: the real Rust server with a scripted provider; spawn it with `process.execPath`. */
export const rustUiFixture = fileURLToPath(
  new URL("./fixtures/rust-ui-fixture.ts", import.meta.url),
);
