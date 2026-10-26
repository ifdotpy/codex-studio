import assert from "node:assert/strict";
import { rmSync } from "node:fs";
import { dirname, join } from "node:path";
import { afterEach, beforeEach, test, vi } from "vitest";

const childProcess = vi.hoisted(() => ({ spawnSync: vi.fn() }));
vi.mock("node:child_process", () => childProcess);

const callerHome = "/fixture/caller-home";
const callerCache = "/fixture/prepared-cache";
const runRoots = new Set();
let originalArgv;
let originalExitCode;

beforeEach(() => {
  originalArgv = process.argv;
  originalExitCode = process.exitCode;
  process.argv = [process.execPath, "run-browser-tests.ts", "--", "--list"];
  vi.stubEnv("HOME", callerHome);
  vi.stubEnv("XDG_CACHE_HOME", callerCache);
  vi.stubEnv("CODEX_HOME", join(callerHome, ".codex"));
  vi.stubEnv("CLAUDE_CONFIG_DIR", join(callerHome, ".claude"));
  vi.stubEnv("PLAYWRIGHT_OUTPUT_DIR", "");
  vi.spyOn(console, "log").mockImplementation(() => {});
  vi.spyOn(console, "error").mockImplementation(() => {});
  childProcess.spawnSync.mockReset();
  vi.resetModules();
});

afterEach(() => {
  process.argv = originalArgv;
  process.exitCode = originalExitCode;
  vi.unstubAllEnvs();
  vi.restoreAllMocks();
  for (const root of runRoots) rmSync(root, { recursive: true, force: true });
  runRoots.clear();
});

function observeCommand(command, args, options) {
  if (options.env.PLAYWRIGHT_OUTPUT_DIR)
    runRoots.add(dirname(options.env.PLAYWRIGHT_OUTPUT_DIR));
  return { command, args, environment: options.env };
}

function assertIsolated(environment) {
  assert.notEqual(environment.HOME, callerHome);
  assert.notEqual(environment.XDG_CACHE_HOME, callerCache);
  assert.equal(environment.CODEX_HOME, join(environment.HOME, ".codex"));
  assert.equal(
    environment.COREPACK_HOME,
    join(callerCache, "node", "corepack"),
  );
  assert.equal(
    environment.CLAUDE_CONFIG_DIR,
    join(environment.HOME, ".claude"),
  );
  assert.equal(
    dirname(environment.XDG_CACHE_HOME),
    dirname(environment.PLAYWRIGHT_OUTPUT_DIR),
  );
}

test("runs Playwright in an isolated home with no interpreter probe", async () => {
  const calls = [];
  childProcess.spawnSync.mockImplementation((command, args, options) => {
    calls.push(observeCommand(command, args, options));
    assertIsolated(options.env);
    return { status: 0 };
  });

  await import("../scripts/run-browser-tests.ts");
  assert.deepEqual(
    calls.map((call) => call.command),
    ["pnpm"],
  );
  assert.deepEqual(calls[0].args, ["exec", "playwright", "test", "--list"]);
  assert.equal(process.exitCode, 0);
});

test("a failing Playwright run sets the exit code", async () => {
  childProcess.spawnSync.mockImplementation((command, args, options) => {
    observeCommand(command, args, options);
    return { status: 3 };
  });

  await import("../scripts/run-browser-tests.ts");
  assert.equal(process.exitCode, 3);
});
