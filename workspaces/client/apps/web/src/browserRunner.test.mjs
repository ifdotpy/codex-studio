import assert from "node:assert/strict";
import { rmSync } from "node:fs";
import { dirname, join } from "node:path";
import { afterEach, beforeEach, test, vi } from "vitest";

const childProcess = vi.hoisted(() => ({ spawnSync: vi.fn() }));
vi.mock("node:child_process", () => childProcess);

const callerHome = "/fixture/caller-home";
const callerCache = "/fixture/prepared-cache";
const bootstrapPython = "/fixture/bootstrap-python";
const preparedPython = "/fixture/prepared-python";
const runRoots = new Set();
let originalArgv;
let originalExitCode;

beforeEach(() => {
  originalArgv = process.argv;
  originalExitCode = process.exitCode;
  process.argv = [process.execPath, "run-browser-tests.mjs", "--", "--list"];
  vi.stubEnv("HOME", callerHome);
  vi.stubEnv("XDG_CACHE_HOME", callerCache);
  vi.stubEnv("CODEX_HOME", join(callerHome, ".codex"));
  vi.stubEnv("CLAUDE_CONFIG_DIR", join(callerHome, ".claude"));
  vi.stubEnv("PYTHON_BIN", bootstrapPython);
  vi.stubEnv("CODEX_AGENTS_PYTHON", "");
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
    environment.CLAUDE_CONFIG_DIR,
    join(environment.HOME, ".claude"),
  );
  assert.equal(
    dirname(environment.XDG_CACHE_HOME),
    dirname(environment.PLAYWRIGHT_OUTPUT_DIR),
  );
}

test("discovers prepared Python before isolation and probes and runs in isolation", async () => {
  const calls = [];
  childProcess.spawnSync.mockImplementation((command, args, options) => {
    calls.push(observeCommand(command, args, options));
    if (command === bootstrapPython) {
      assert.equal(options.env.HOME, callerHome);
      assert.equal(options.env.XDG_CACHE_HOME, callerCache);
      assert.equal(args.length, 1);
      assert.match(args[0], /codex_python\.py$/);
      return { status: 0, stdout: preparedPython + "\n" };
    }
    assertIsolated(options.env);
    return { status: 0 };
  });

  await import("../scripts/run-browser-tests.mjs");
  assert.deepEqual(
    calls.map((call) => call.command),
    [bootstrapPython, preparedPython, "pnpm"],
  );
  assert.match(calls[1].args[1], /codex_operations_adapter/);
  assert.deepEqual(calls[2].args, ["exec", "playwright", "test", "--list"]);
  assert.equal(calls[2].environment.CODEX_AGENTS_PYTHON, preparedPython);
  assert.equal(process.exitCode, 0);
});

test("explicit equipped Python bypasses discovery and retains the isolated probe", async () => {
  vi.stubEnv("CODEX_AGENTS_PYTHON", preparedPython);
  const calls = [];
  childProcess.spawnSync.mockImplementation((command, args, options) => {
    calls.push(observeCommand(command, args, options));
    assertIsolated(options.env);
    return { status: 0 };
  });

  await import("../scripts/run-browser-tests.mjs");
  assert.deepEqual(
    calls.map((call) => call.command),
    [preparedPython, "pnpm"],
  );
  assert.match(calls[0].args[1], /codex_operations_adapter/);
  assert.equal(process.exitCode, 0);
});

test("a mismatched native operations probe prevents the browser command", async () => {
  vi.stubEnv("CODEX_AGENTS_PYTHON", preparedPython);
  childProcess.spawnSync.mockImplementation((command, args, options) => {
    observeCommand(command, args, options);
    assert.equal(command, preparedPython);
    assertIsolated(options.env);
    assert.match(args[1], /codex_operations_adapter/);
    return { status: 1, stderr: "native operations protocol mismatch" };
  });

  await import("../scripts/run-browser-tests.mjs");
  assert.equal(childProcess.spawnSync.mock.calls.length, 1);
  assert.equal(process.exitCode, 1);
  assert.match(console.error.mock.calls[0][0], /protocol mismatch/);
});
