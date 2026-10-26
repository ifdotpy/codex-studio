#!/usr/bin/env node
import assert from "node:assert/strict";
import {
  chromium,
  type BrowserContext,
  type Page,
  type Request,
} from "playwright";
import { mkdirSync, readFileSync, renameSync, writeFileSync } from "node:fs";
import { join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { spawnSync } from "node:child_process";

type Options = Partial<
  Record<
    "url" | "pid" | "evidence-dir" | "chromium" | "duration-ms" | "settle-ms",
    string
  >
> & { help?: boolean };
type Observation = {
  api: {
    requestCount: number;
    counts: Record<string, number>;
    omittedEndpointCounts: number;
    samples: { method: string; path: string; resourceType: string }[];
    omittedSamples: number;
  };
  errors: {
    httpCount: number;
    http: { method: string; path: string; status: number }[];
    omittedHttp: number;
    failedRequestCount: number;
    failedRequests: { method: string; path: string; error: string }[];
    omittedFailedRequests: number;
    pageErrorCount: number;
    pageErrors: string[];
    omittedPageErrors: number;
  };
};
type Evidence = Observation & {
  version: number;
  status: string;
  startedAt: string;
  target: { origin: string; pid: number } | null;
  observationConfig: {
    settleMs: number;
    durationMs: number;
    clockTicksPerSecond: number;
  } | null;
  process: Record<
    string,
    string | number | ReturnType<typeof numericCounters>
  > | null;
  failure: { message: string; outerDeadlineExceeded?: boolean } | null;
  observedAt?: { start: string; end: string };
  page?: { origin: string; sidebarVisible: boolean };
  finishedAt?: string;
};
const DEFAULT_DURATION_MS = 75_000;
const DEFAULT_SETTLE_MS = 15_000;
const MIN_DURATION_MS = 1_000;
const MAX_DURATION_MS = 240_000;
const MAX_SETTLE_MS = 120_000;
const NAVIGATION_TIMEOUT_MS = 45_000;
const BROWSER_LAUNCH_TIMEOUT_MS = 20_000;
const OUTER_DEADLINE_MS = 8 * 60_000;
const CLEANUP_TIMEOUT_MS = 10_000;
const MAX_API_ENDPOINTS = 200;
const MAX_API_SAMPLES = 30;
const MAX_ERROR_SAMPLES = 100;
const MAX_ERROR_TEXT = 300;

function parseArgs(argv: string[]) {
  const options: Options = {};
  for (let index = 0; index < argv.length; index += 1) {
    const value = argv[index];
    if (value === "--help" || value === "-h") {
      options.help = true;
      continue;
    }
    if (!value?.startsWith("--"))
      throw new Error(`Unexpected argument: ${value}`);
    const key = value.slice(2);
    if (
      !new Set([
        "url",
        "pid",
        "evidence-dir",
        "chromium",
        "duration-ms",
        "settle-ms",
      ]).has(key)
    )
      throw new Error(`Unknown option: ${value}`);
    const next = argv[index + 1];
    if (!next || next.startsWith("--"))
      throw new Error(`Missing value for ${value}`);
    if (
      key === "url" ||
      key === "pid" ||
      key === "evidence-dir" ||
      key === "chromium" ||
      key === "duration-ms" ||
      key === "settle-ms"
    )
      options[key] = next;
    index += 1;
  }
  return options;
}

function usage() {
  return [
    "Usage:",
    "  node scripts/verify-rust-idle.ts --url HTTPS_URL --pid PID --evidence-dir DIR [--chromium PATH] [--duration-ms MS] [--settle-ms MS]",
    "",
    "Opens the supplied URL in headless Chromium, waits for #sidebar, then records bounded API activity and Linux process counters.",
    "A completed run is measurement-only; it does not prove that the server or its CPU was idle. The verifier is read-only and exits nonzero for observed errors or measurement failures.",
  ].join("\n");
}

function numericOption(
  value: string | undefined,
  fallback: number,
  name: string,
  minimum: number,
  maximum: number,
) {
  if (value === undefined) return fallback;
  assert.match(value, /^\d+$/, `--${name} must be an integer in milliseconds.`);
  const number = Number(value);
  assert.ok(Number.isSafeInteger(number), `--${name} is too large.`);
  assert.ok(number >= minimum, `--${name} must be at least ${minimum}.`);
  assert.ok(number <= maximum, `--${name} must be at most ${maximum}.`);
  return number;
}

function parseBareHttpsUrl(value: string) {
  const url = new URL(value);
  assert.equal(url.protocol, "https:", "--url must use HTTPS.");
  assert.equal(url.username, "", "--url must not include credentials.");
  assert.equal(url.password, "", "--url must not include credentials.");
  assert.equal(url.pathname, "/", "--url must be a bare server URL.");
  assert.equal(url.search, "", "--url must not include a query.");
  assert.equal(url.hash, "", "--url must not include a fragment.");
  return url.origin;
}

function processClockTicks(pid: number) {
  const output = spawnSync("getconf", ["CLK_TCK"], {
    encoding: "utf8",
    timeout: 2_000,
  });
  assert.equal(
    output.status,
    0,
    "Could not read process clock ticks from getconf.",
  );
  const value = Number(output.stdout.trim());
  assert.ok(
    Number.isInteger(value) && value > 0,
    "getconf returned an invalid CLK_TCK value.",
  );
  assert.equal(
    process.platform,
    "linux",
    "Process counters are available only on Linux.",
  );
  assert.ok(
    Number.isSafeInteger(pid) && pid > 1,
    "--pid must be a positive Linux process ID.",
  );
  return value;
}

function processSnapshot(pid: number) {
  const stat = readFileSync(`/proc/${pid}/stat`, "utf8");
  const closingParen = stat.lastIndexOf(")");
  assert.ok(closingParen >= 0, "Linux process stat record is malformed.");
  const fields = stat
    .slice(closingParen + 1)
    .trim()
    .split(/\s+/);
  assert.ok(fields.length > 19, "Linux process stat record is incomplete.");
  const io = new Map(
    readFileSync(`/proc/${pid}/io`, "utf8")
      .trim()
      .split("\n")
      .map((line) => {
        const [key, value] = line.split(":");
        assert.ok(key && value);
        return [key, BigInt(value.trim())];
      }),
  );
  for (const key of ["rchar", "read_bytes", "syscr"])
    assert.ok(io.has(key), `Linux process IO record is missing ${key}.`);
  const rchar = io.get("rchar");
  const physicalReadBytes = io.get("read_bytes");
  const readSyscalls = io.get("syscr");
  assert.ok(
    rchar !== undefined &&
      physicalReadBytes !== undefined &&
      readSyscalls !== undefined,
  );
  return {
    capturedAt: new Date().toISOString(),
    monotonicMs: performance.now(),
    userTicks: BigInt(fields[11] ?? ""),
    systemTicks: BigInt(fields[12] ?? ""),
    startTicks: BigInt(fields[19] ?? ""),
    rchar,
    physicalReadBytes,
    readSyscalls,
  };
}

function waitFor(ms: number, signal: AbortSignal) {
  return new Promise<void>((resolvePromise, reject) => {
    if (signal.aborted) {
      reject(signal.reason || new Error("Observation deadline reached."));
      return;
    }
    const finish = () => {
      signal.removeEventListener("abort", abort);
      resolvePromise();
    };
    const abort = () => {
      clearTimeout(timer);
      signal.removeEventListener("abort", abort);
      reject(signal.reason || new Error("Observation deadline reached."));
    };
    const timer = setTimeout(finish, ms);
    signal.addEventListener("abort", abort, { once: true });
  });
}

function numericCounters(snapshot: ReturnType<typeof processSnapshot>) {
  return {
    userTicks: snapshot.userTicks.toString(),
    systemTicks: snapshot.systemTicks.toString(),
    startTicks: snapshot.startTicks.toString(),
    rchar: snapshot.rchar.toString(),
    physicalReadBytes: snapshot.physicalReadBytes.toString(),
    readSyscalls: snapshot.readSyscalls.toString(),
  };
}

function safePath(url: string) {
  try {
    return new URL(url).pathname
      .replace(
        /[0-9a-f]{8}-[0-9a-f]{4}-[1-8][0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}/gi,
        ":id",
      )
      .slice(0, 500);
  } catch {
    return "<invalid-url>";
  }
}

function emptyObservation(): Observation {
  return {
    api: {
      requestCount: 0,
      counts: {},
      omittedEndpointCounts: 0,
      samples: [],
      omittedSamples: 0,
    },
    errors: {
      httpCount: 0,
      http: [],
      omittedHttp: 0,
      failedRequestCount: 0,
      failedRequests: [],
      omittedFailedRequests: 0,
      pageErrorCount: 0,
      pageErrors: [],
      omittedPageErrors: 0,
    },
  };
}

function boundedPush<T, K extends string>(
  items: T[],
  value: T,
  limit: number,
  omitted: K,
  target: Record<K, number>,
) {
  if (items.length < limit) items.push(value);
  else target[omitted] += 1;
}

function recordApiRequest(observation: Observation, request: Request) {
  const path = safePath(request.url());
  const key = `${request.method()} ${path}`;
  const counts = observation.api.counts;
  if (Object.hasOwn(counts, key)) counts[key] = (counts[key] ?? 0) + 1;
  else if (Object.keys(counts).length < MAX_API_ENDPOINTS) counts[key] = 1;
  else observation.api.omittedEndpointCounts += 1;
  observation.api.requestCount += 1;
  boundedPush(
    observation.api.samples,
    { method: request.method(), path, resourceType: request.resourceType() },
    MAX_API_SAMPLES,
    "omittedSamples",
    observation.api,
  );
}

function addListeners(
  page: Page,
  observation: Observation,
  recording: () => boolean,
) {
  page.on("request", (request) => {
    if (!recording()) return;
    if (/^\/api(?:\/|$)/.test(safePath(request.url())))
      recordApiRequest(observation, request);
  });
  page.on("response", (response) => {
    if (!recording() || response.status() < 400) return;
    const request = response.request();
    if (!/^\/api(?:\/|$)/.test(safePath(request.url()))) return;
    observation.errors.httpCount += 1;
    boundedPush(
      observation.errors.http,
      {
        method: request.method(),
        path: safePath(response.url()),
        status: response.status(),
      },
      MAX_ERROR_SAMPLES,
      "omittedHttp",
      observation.errors,
    );
  });
  page.on("requestfailed", (request) => {
    if (!recording()) return;
    observation.errors.failedRequestCount += 1;
    boundedPush(
      observation.errors.failedRequests,
      {
        method: request.method(),
        path: safePath(request.url()),
        error: (request.failure()?.errorText || "Request failed").slice(
          0,
          MAX_ERROR_TEXT,
        ),
      },
      MAX_ERROR_SAMPLES,
      "omittedFailedRequests",
      observation.errors,
    );
  });
  page.on("pageerror", (error) => {
    if (!recording()) return;
    observation.errors.pageErrorCount += 1;
    boundedPush(
      observation.errors.pageErrors,
      String(error instanceof Error ? error.message : error).slice(
        0,
        MAX_ERROR_TEXT,
      ),
      MAX_ERROR_SAMPLES,
      "omittedPageErrors",
      observation.errors,
    );
  });
}

function writeArtifact(path: string, result: Evidence) {
  const temporary = `${path}.next`;
  writeFileSync(temporary, `${JSON.stringify(result, null, 2)}\n`, {
    encoding: "utf8",
    mode: 0o600,
  });
  renameSync(temporary, path);
}

async function main() {
  let options: Options = {};
  let artifactPath;
  let result: Evidence = {
    version: 1,
    status: "running",
    startedAt: new Date().toISOString(),
    target: null,
    observationConfig: null,
    process: null,
    ...emptyObservation(),
    failure: null,
  };
  let context: BrowserContext | undefined;
  let recording = false;
  let deadlineTimer: ReturnType<typeof setTimeout> | undefined;
  let deadlineExceeded = false;
  const deadlineController = new AbortController();
  try {
    options = parseArgs(process.argv.slice(2));
    if (options.help) {
      process.stdout.write(`${usage()}\n`);
      return;
    }
    assert.ok(
      options["evidence-dir"],
      "--evidence-dir is required to record an artifact.",
    );
    const evidenceDir = resolve(options["evidence-dir"]);
    mkdirSync(evidenceDir, { recursive: true, mode: 0o700 });
    const timestamp = new Date().toISOString().replaceAll(/[:.]/g, "-");
    const safePid = /^\d+$/.test(options.pid || "") ? options.pid : "unknown";
    artifactPath = join(
      evidenceDir,
      `rust-idle-${timestamp}-pid${safePid}.json`,
    );

    assert.ok(options.url, "--url is required.");
    assert.ok(options.pid, "--pid is required.");
    const origin = parseBareHttpsUrl(options.url);
    assert.match(options.pid, /^\d+$/, "--pid must be a Linux process ID.");
    const pid = Number(options.pid);
    const clockTicksPerSecond = processClockTicks(pid);
    const initialProcess = processSnapshot(pid);
    const durationMs = numericOption(
      options["duration-ms"],
      DEFAULT_DURATION_MS,
      "duration-ms",
      MIN_DURATION_MS,
      MAX_DURATION_MS,
    );
    const settleMs = numericOption(
      options["settle-ms"],
      DEFAULT_SETTLE_MS,
      "settle-ms",
      0,
      MAX_SETTLE_MS,
    );
    result.target = { origin, pid };
    result.observationConfig = { settleMs, durationMs, clockTicksPerSecond };
    result.process = {
      initialStartTicks: initialProcess.startTicks.toString(),
      initialCounters: numericCounters(initialProcess),
    };

    const profileDir = resolve(evidenceDir, "profile");
    const deadlineMs = Math.min(
      OUTER_DEADLINE_MS,
      BROWSER_LAUNCH_TIMEOUT_MS +
        NAVIGATION_TIMEOUT_MS +
        settleMs +
        durationMs +
        CLEANUP_TIMEOUT_MS,
    );

    const executablePath =
      options.chromium || process.env.PLAYWRIGHT_CHROMIUM_EXECUTABLE;
    const run = async () => {
      context = await chromium.launchPersistentContext(profileDir, {
        headless: true,
        ...(executablePath ? { executablePath } : {}),
        viewport: { width: 1440, height: 1000 },
        acceptDownloads: false,
        timeout: BROWSER_LAUNCH_TIMEOUT_MS,
      });
      const page = context.pages()[0] || (await context.newPage());
      page.setDefaultNavigationTimeout(NAVIGATION_TIMEOUT_MS);
      page.setDefaultTimeout(NAVIGATION_TIMEOUT_MS);
      addListeners(page, result, () => recording);
      await page.goto(origin, { waitUntil: "domcontentloaded" });
      assert.equal(
        new URL(page.url()).origin,
        origin,
        "The page redirected to another origin.",
      );
      const sidebar = page.locator("#sidebar");
      await sidebar.waitFor({ state: "visible" });
      assert.equal(
        await sidebar.count(),
        1,
        "Expected one top-level #sidebar element.",
      );
      await waitFor(settleMs, deadlineController.signal);

      const before = processSnapshot(pid);
      assert.equal(
        before.startTicks,
        initialProcess.startTicks,
        "PID start time changed while the browser was starting or settling.",
      );
      recording = true;
      await waitFor(durationMs, deadlineController.signal);
      recording = false;
      assert.equal(
        new URL(page.url()).origin,
        origin,
        "The page navigated away from the target origin during observation.",
      );
      assert.equal(
        await sidebar.isVisible(),
        true,
        "The top-level #sidebar stopped being visible during observation.",
      );
      const after = processSnapshot(pid);
      assert.equal(
        after.startTicks,
        before.startTicks,
        "PID start time changed during observation; the process may have exited or been reused.",
      );
      const elapsedSeconds = (after.monotonicMs - before.monotonicMs) / 1000;
      assert.ok(elapsedSeconds > 0, "Observation interval was not positive.");
      const userTicks = after.userTicks - before.userTicks;
      const systemTicks = after.systemTicks - before.systemTicks;
      const rchar = after.rchar - before.rchar;
      const physicalReadBytes =
        after.physicalReadBytes - before.physicalReadBytes;
      const readSyscalls = after.readSyscalls - before.readSyscalls;
      for (const [name, value] of Object.entries({
        userTicks,
        systemTicks,
        rchar,
        physicalReadBytes,
        readSyscalls,
      }))
        assert.ok(value >= 0n, `Process counter ${name} moved backwards.`);
      result.process = {
        initialStartTicks: initialProcess.startTicks.toString(),
        initialCapturedAt: initialProcess.capturedAt,
        initialCounters: numericCounters(initialProcess),
        startTicks: before.startTicks.toString(),
        startTicksAtEnd: after.startTicks.toString(),
        before: numericCounters(before),
        after: numericCounters(after),
        elapsedSeconds,
        cpuPercent:
          (Number(userTicks + systemTicks) /
            clockTicksPerSecond /
            elapsedSeconds) *
          100,
        rcharPerSecond: Number(rchar) / elapsedSeconds,
        physicalReadBytes: physicalReadBytes.toString(),
        readSyscallsPerSecond: Number(readSyscalls) / elapsedSeconds,
      };
      result.observedAt = { start: before.capturedAt, end: after.capturedAt };
      result.page = {
        origin: new URL(page.url()).origin,
        sidebarVisible: true,
      };
    };
    const work = run();
    const deadline = new Promise((_, reject) => {
      deadlineTimer = setTimeout(() => {
        deadlineExceeded = true;
        recording = false;
        deadlineController.abort(
          new Error(`Outer deadline exceeded after ${deadlineMs} ms.`),
        );
        if (context) void context.close().catch(() => {});
        reject(new Error(`Outer deadline exceeded after ${deadlineMs} ms.`));
      }, deadlineMs);
    });
    await Promise.race([work, deadline]);
    result.status = "completed";
  } catch (error) {
    recording = false;
    result.status = "failed";
    result.failure = {
      message: String(error instanceof Error ? error.message : error).slice(
        0,
        MAX_ERROR_TEXT,
      ),
      outerDeadlineExceeded: deadlineExceeded,
    };
  } finally {
    recording = false;
    clearTimeout(deadlineTimer);
    if (!deadlineController.signal.aborted)
      deadlineController.abort(new Error("Observer finished."));
    if (context) {
      try {
        let cleanupTimer: ReturnType<typeof setTimeout> | undefined;
        await Promise.race([
          context.close(),
          new Promise((resolvePromise) => {
            cleanupTimer = setTimeout(resolvePromise, CLEANUP_TIMEOUT_MS);
            cleanupTimer.unref?.();
          }),
        ]);
        clearTimeout(cleanupTimer);
      } catch {}
    }
    result.finishedAt = new Date().toISOString();
    if (
      result.errors.httpCount > 0 ||
      result.errors.failedRequestCount > 0 ||
      result.errors.pageErrorCount > 0
    ) {
      result.status = "failed";
      result.failure ||= {
        message: "Observed HTTP errors, failed requests, or page errors.",
      };
    }
    if (artifactPath) {
      try {
        writeArtifact(artifactPath, result);
        process.stdout.write(
          `${JSON.stringify({ ...result, artifact: artifactPath }, null, 2)}\n`,
        );
      } catch (error) {
        process.stderr.write(
          `Could not write evidence artifact: ${error instanceof Error ? error.message : error}\n`,
        );
        result.status = "failed";
      }
    } else if (result.failure) {
      process.stderr.write(`${result.failure.message}\n`);
    }
  }
  if (result.status !== "completed") process.exitCode = 1;
}

if (
  process.argv[1] &&
  resolve(process.argv[1]) === fileURLToPath(import.meta.url)
) {
  main().catch((error) => {
    process.stderr.write(`${error.stack || error}\n`);
    process.exitCode = 1;
  });
}
