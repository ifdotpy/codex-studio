import test from "node:test";
import assert from "node:assert/strict";
import {
  affectedPlan,
  boundaryFindings,
  commandFor,
  execute,
  executeRecipe,
  evaluatePlaywrightReport,
  nextestError,
  nextestSelectionError,
  packageTarget,
  parseCounts,
  parseSelection,
  pruneLogs,
  serverRunDiagnostics,
  serverShardRunOptions,
} from "../facade.mjs";
import {
  mkdtempSync,
  mkdirSync,
  chmodSync,
  statSync,
  rmSync,
  utimesSync,
  writeFileSync,
  readFileSync,
} from "node:fs";
import os from "node:os";
import path from "node:path";
import { execFileSync } from "node:child_process";

test("pnpm Vitest positional selection is a file path substring; --name is a native name filter", () => {
  const item = {
    name: "codex-agents-web",
    ecosystem: "pnpm",
    manifest: new URL(
      "../../../../client/apps/web/package.json",
      import.meta.url,
    ),
  };
  assert.deepEqual(
    commandFor(process.cwd(), item, "test", "src/chat.test.ts"),
    [
      [
        "pnpm",
        [
          "--filter",
          item.name,
          "exec",
          "vitest",
          "run",
          "--config",
          "vitest.unit.config.ts",
          "src/chat.test.ts",
        ],
      ],
    ],
  );
  assert.deepEqual(
    commandFor(process.cwd(), item, "test", "src/errorPresentation", {
      name: "renders retry button",
    }),
    [
      [
        "pnpm",
        [
          "--filter",
          item.name,
          "exec",
          "vitest",
          "run",
          "--config",
          "vitest.unit.config.ts",
          "src/errorPresentation",
          "-t",
          "renders retry button",
        ],
      ],
    ],
  );
});

test("pnpm Node test selection uses the native test-name filter", () => {
  const item = {
    name: "codex-studio-tooling",
    ecosystem: "pnpm",
    manifest: new URL("../../../../../package.json", import.meta.url),
  };
  assert.deepEqual(
    commandFor(process.cwd(), item, "test", "Cargo zero-match summaries"),
    [
      [
        "pnpm",
        [
          "--filter",
          item.name,
          "exec",
          "node",
          "--test",
          "--test-reporter=tap",
          "--test-name-pattern",
          "Cargo zero-match summaries",
          "workspaces/tooling/apps/repository-checks/tests/facade.test.mjs",
        ],
      ],
    ],
  );
});

test("Cargo uses nextest filters and keeps package and test selectors narrow", () => {
  const item = { name: "studio-diagnostics", ecosystem: "cargo" };
  assert.deepEqual(commandFor("/repo", item, "test", "cli::tests::help"), [
    [
      "cargo",
      [
        "nextest",
        "run",
        "--locked",
        "--no-tests",
        "fail",
        "--message-format",
        "libtest-json-plus",
        "-p",
        item.name,
        "--filterset",
        "test(~cli::tests::help)",
      ],
    ],
  ]);
  assert.deepEqual(
    commandFor("/repo", item, "test", "cli::tests::help", {
      passthrough: ["--exact", "--test", "integration"],
      cargoTargets: ["--test", "compatibility"],
    }),
    [
      [
        "cargo",
        [
          "nextest",
          "run",
          "--locked",
          "--no-tests",
          "fail",
          "--message-format",
          "libtest-json-plus",
          "-p",
          item.name,
          "--test",
          "compatibility",
          "--filterset",
          "test(~cli::tests::help)",
          "--",
          "--exact",
          "--test",
          "integration",
        ],
      ],
    ],
  );
  assert.deepEqual(
    commandFor("/repo", item, "test", "cli::tests::help", {
      name: "help renders",
    })[0][1].slice(-2),
    ["--filterset", "test(~cli::tests::help) and test(~help renders)"],
  );
  assert.deepEqual(
    parseCounts(
      item,
      [
        '{"type":"suite","event":"started","test_count":1}',
        '{"type":"suite","event":"ok","passed":2,"failed":0,"ignored":1}',
        '{"type":"suite","event":"failed","passed":0,"failed":1,"ignored":0}',
      ].join("\n"),
    ),
    {
      passed: 2,
      failed: 1,
      skipped: 1,
      executed: 3,
      summaryFound: true,
    },
  );
  const checkCommands = commandFor(
    "/repo",
    { ...item, docTests: true },
    "check",
  );
  assert.deepEqual(checkCommands.at(-1), [
    "cargo",
    ["test", "--doc", "--locked", "-p", item.name],
  ]);
  assert.equal(
    commandFor("/repo", { ...item, docTests: false }, "check").some(
      ([, args]) => args.includes("--doc"),
    ),
    false,
  );
});

test("nextest empty-selection and missing-runner errors stay actionable", () => {
  assert.equal(
    nextestSelectionError(4),
    "nextest found no tests matching this selection",
  );
  assert.equal(nextestSelectionError(0), null);
  const missing = nextestError("error: no such command: `nextest`");
  assert.match(missing, /cargo-nextest 0\.9\.146 is required/);
  assert.match(
    missing,
    /cargo install cargo-nextest --version 0\.9\.146 --locked/,
  );
  assert.equal(nextestError("error: failed to compile package"), null);
});

test("Cargo facade failures retain selection, test failure, install guidance and logs", async () => {
  const root = mkdtempSync(path.join(os.tmpdir(), "codex-nextest-facade-"));
  const bin = path.join(root, "bin");
  mkdirSync(bin);
  const cargo = path.join(bin, "cargo");
  writeFileSync(
    cargo,
    `#!/bin/sh
printf '%s' "$CODEX_NODE" > "$FIXTURE_NODE_CAPTURE"
case "$FIXTURE_CARGO_MODE" in
  fail) printf '%s\\n' '{"type":"suite","event":"failed","passed":0,"failed":1,"ignored":0}'; exit 100 ;;
  empty) echo 'error: no tests to run'; exit 4 ;;
  missing) echo 'error: no such command: \`nextest\`'; exit 101 ;;
esac
`,
  );
  chmodSync(cargo, 0o755);
  const original = {
    PATH: process.env.PATH,
    CARGO_TARGET_DIR: process.env.CARGO_TARGET_DIR,
    CODEX_STUDIO_CHECKS_CACHE: process.env.CODEX_STUDIO_CHECKS_CACHE,
    CODEX_NODE: process.env.CODEX_NODE,
    FIXTURE_NODE_CAPTURE: process.env.FIXTURE_NODE_CAPTURE,
  };
  process.env.PATH = `${bin}:${original.PATH}`;
  process.env.CARGO_TARGET_DIR = path.join(root, "target");
  process.env.CODEX_STUDIO_CHECKS_CACHE = path.join(root, "logs");
  const nodeCapture = path.join(root, "codex-node.txt");
  process.env.FIXTURE_NODE_CAPTURE = nodeCapture;
  const originalWrite = process.stdout.write;
  process.stdout.write = () => true;
  const item = { name: "fixture", ecosystem: "cargo" };
  try {
    process.env.FIXTURE_CARGO_MODE = "empty";
    const empty = await execute(root, item, "missing_case", true);
    assert.equal(empty.exitCode, 1);
    assert.equal(readFileSync(nodeCapture, "utf8"), process.execPath);
    assert.deepEqual(empty.errors, [
      "nextest found no tests matching this selection",
    ]);
    assert.match(empty.command, /--filterset/);
    assert.ok(readFileSync(empty.logPath, "utf8").includes("no tests to run"));

    process.env.FIXTURE_CARGO_MODE = "fail";
    const failed = await execute(root, item, "fails", true);
    assert.equal(failed.exitCode, 100);
    assert.equal(failed.failed, 1);
    assert.match(failed.command, /cargo.*nextest.*run/);
    assert.ok(failed.logPath);
    assert.ok(
      readFileSync(failed.logPath, "utf8").includes('"event":"failed"'),
    );

    process.env.FIXTURE_CARGO_MODE = "missing";
    const absent = await execute(root, item, "", true);
    assert.equal(absent.exitCode, 1);
    assert.match(
      absent.errors[0],
      /cargo install cargo-nextest --version 0\.9\.146 --locked/,
    );
    assert.ok(absent.logPath);
  } finally {
    process.stdout.write = originalWrite;
    if (original.PATH === undefined) delete process.env.PATH;
    else process.env.PATH = original.PATH;
    if (original.CARGO_TARGET_DIR === undefined)
      delete process.env.CARGO_TARGET_DIR;
    else process.env.CARGO_TARGET_DIR = original.CARGO_TARGET_DIR;
    if (original.CODEX_STUDIO_CHECKS_CACHE === undefined)
      delete process.env.CODEX_STUDIO_CHECKS_CACHE;
    else
      process.env.CODEX_STUDIO_CHECKS_CACHE =
        original.CODEX_STUDIO_CHECKS_CACHE;
    if (original.CODEX_NODE === undefined) delete process.env.CODEX_NODE;
    else process.env.CODEX_NODE = original.CODEX_NODE;
    if (original.FIXTURE_NODE_CAPTURE === undefined)
      delete process.env.FIXTURE_NODE_CAPTURE;
    else process.env.FIXTURE_NODE_CAPTURE = original.FIXTURE_NODE_CAPTURE;
    delete process.env.FIXTURE_CARGO_MODE;
    rmSync(root, { recursive: true, force: true });
  }
});

test("unknown Rust package stays a facade error", async () => {
  assert.throws(
    () =>
      packageTarget(
        [{ name: "studio-operations", ecosystem: "cargo" }],
        "codex-package-that-does-not-exist",
      ),
    /Unknown package/,
  );
});

test("selection parser preserves multiword name filters and Cargo pass-through", () => {
  assert.deepEqual(
    parseSelection("errorPresentation", [
      "--name",
      "renders",
      "readable",
      "error",
      "--json",
    ]),
    {
      caseName: "errorPresentation",
      options: {
        name: "renders readable error",
        passthrough: [],
        cargoTargets: [],
      },
    },
  );
  assert.deepEqual(parseSelection("case", ["--", "--exact", "--test", "x"]), {
    caseName: "case",
    options: {
      name: "",
      passthrough: ["--exact"],
      cargoTargets: ["--test", "x"],
    },
  });
});

test("composed package checks invoke only mapped local scripts, never install or build unrelated packages", () => {
  for (const [item, action] of [
    [
      {
        name: "codex-agents-web",
        ecosystem: "pnpm",
        manifest: new URL(
          "../../../../client/apps/web/package.json",
          import.meta.url,
        ),
      },
      "check",
    ],
    [
      {
        name: "codex-agents-desktop",
        ecosystem: "pnpm",
        manifest: new URL(
          "../../../../client/apps/desktop/package.json",
          import.meta.url,
        ),
      },
      "test",
    ],
  ]) {
    const commands = commandFor(process.cwd(), item, action);
    assert.ok(commands.length > 0);
    for (const [bin, args] of commands) {
      assert.equal(bin, "pnpm");
      assert.ok(args.includes("--filter"));
      assert.ok(!args.some((arg) => /^(install|build)$/.test(arg)));
      assert.ok(
        !args.some(
          (arg) => arg.startsWith("codex-agents-") && arg !== item.name,
        ),
      );
    }
  }
});

test("focused log cache prunes runs older than fourteen days and retains recent runs", () => {
  const root = mkdtempSync(path.join(os.tmpdir(), "codex-facade-cache-test-"));
  try {
    const stale = path.join(root, "run-stale");
    const recent = path.join(root, "run-recent");
    mkdirSync(stale);
    mkdirSync(recent);
    const now = Date.now();
    const old = new Date(now - 15 * 24 * 60 * 60 * 1000);
    const newDate = new Date(now - 1 * 24 * 60 * 60 * 1000);
    utimesSync(stale, old, old);
    utimesSync(recent, newDate, newDate);
    pruneLogs(root, now);
    assert.throws(() => statSync(stale));
    assert.ok(statSync(recent).isDirectory());
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});

test("server summaries count assertion scripts as executed suites and reject no runnable suite output", () => {
  const item = { name: "server", ecosystem: "server" };
  assert.equal(
    parseCounts(
      item,
      "Server suites: 1 passed, 0 failed, 0 skipped (opt-in), 0 skipped (environment), 0.2s; plain assertion scripts are counted per suite; 0 runner errors",
    ).executed,
    1,
  );
  assert.equal(
    parseCounts(item, "No runnable server suites: 1 skipped (opt-in).")
      .executed,
    0,
  );
});

test("server shard facade options are zero-based and diagnostics report plan and ten slowest suites", () => {
  assert.deepEqual(serverShardRunOptions("1", "4").serverArgs, [
    "--shard-index",
    "1",
    "--shard-count",
    "4",
  ]);
  assert.throws(
    () => serverShardRunOptions("4", "4"),
    /Usage: test-server-shard/,
  );
  const output = [
    "Finished nested-fixture.py: passed, 900.00s",
    `Server runner diagnostics: ${JSON.stringify({
      workerPlan: { workers: 4 },
      suiteDurations: Object.fromEntries(
        Array.from({ length: 12 }, (_, index) => [`suite-${index}`, index + 1]),
      ),
    })}`,
    ...Array.from(
      { length: 12 },
      (_, index) => `Finished suite-${index}: passed, ${index + 1}.00s`,
    ),
  ].join("\n");
  const diagnostics = serverRunDiagnostics(output);
  assert.equal(diagnostics.planLine, 'Selected worker plan: {"workers":4}');
  assert.equal(diagnostics.durations.length, 10);
  assert.deepEqual(diagnostics.durations.slice(0, 2), [
    { suite: "suite-11", seconds: 12 },
    { suite: "suite-10", seconds: 11 },
  ]);
});

test("server failure counts do not double count structured failures or passing tracebacks", () => {
  const item = { name: "server", ecosystem: "server" };
  const output = [
    "Exception in thread Thread-1 (serve_forever):",
    "Traceback (most recent call last):",
    "ValueError: Invalid file descriptor: -1",
    "test_mobile_origin (__main__.MobileOriginContract.test_config_is_exact_https_origin) ... ok",
    "Finished workspaces/runtime/apps/server/tests/mobile-origin-contract.py: passed, 0.02s",
    "Server suites: 1 passed, 1 failed, 0 skipped (opt-in), 0 skipped (environment)",
    'Test outcomes: {"error":0,"failed":1,"passed":8}',
    "FAIL workspaces/runtime/apps/server/tests/tool-request-http-contract.py: test failure; failing IDs: ['ToolRequestHTTPContract.test_provider_tool_request']",
  ].join("\n");
  assert.equal(parseCounts(item, output).failed, 1);

  const plainScriptFailure = [
    "Server suites: 0 passed, 1 failed, 0 skipped (opt-in), 0 skipped (environment)",
    "FAIL workspaces/runtime/apps/server/tests/assertion-script.py: exit 1",
  ].join("\n");
  assert.equal(parseCounts(item, plainScriptFailure).failed, 1);
});

test("server failure summaries print failing suite IDs", async () => {
  const originalWrite = process.stdout.write;
  let printed = "";
  process.stdout.write = (chunk) => {
    printed += chunk.toString();
    return true;
  };
  try {
    const output = [
      "Server suites: 0 passed, 1 failed, 0 skipped (opt-in), 0 skipped (environment)",
      'Test outcomes: {"error":1,"failed":0,"passed":3}',
      "FAIL tests/example-contract.py: test failure; failing IDs: ['Example.test_reports_failure']",
    ].join("\n");
    const result = await execute(
      process.cwd(),
      { name: "server", ecosystem: "server" },
      "",
      false,
      {
        commands: [
          [
            "node",
            [
              "-e",
              `process.stdout.write(${JSON.stringify(output)}); process.exit(1)`,
            ],
          ],
        ],
      },
    );
    assert.equal(result.exitCode, 1);
    assert.match(printed, /Failing server suites:/);
    assert.match(printed, /Example\.test_reports_failure/);
  } finally {
    process.stdout.write = originalWrite;
  }
});

test("Vitest summaries count test cases rather than test files", () => {
  const item = { name: "codex-agents-web", ecosystem: "pnpm" };
  assert.deepEqual(
    parseCounts(item, " Test Files  1 passed (1)\n      Tests  3 passed (3)"),
    {
      passed: 3,
      failed: 0,
      skipped: 0,
      executed: 3,
      summaryFound: true,
    },
  );
  assert.deepEqual(parseCounts(item, "Tests  1 failed | 2 passed (3)"), {
    passed: 2,
    failed: 1,
    skipped: 0,
    executed: 3,
    summaryFound: true,
  });
  assert.equal(
    parseCounts(item, "No test files found, exiting with code 1").executed,
    0,
  );
  assert.deepEqual(
    parseCounts(
      item,
      "\u001b[2m Test Files \u001b[22m \u001b[1m\u001b[32m8 passed\u001b[39m\u001b[22m\u001b[90m (8)\u001b[39m\n" +
        "\u001b[2m      Tests \u001b[22m\u001b[1m\u001b[32m58 passed\u001b[39m\u001b[22m\u001b[90m (58)\u001b[39m",
    ),
    {
      passed: 58,
      failed: 0,
      skipped: 0,
      executed: 58,
      summaryFound: true,
    },
  );
});

test("Playwright JSON reports enforce process and test outcome status", () => {
  const evaluate = (stats, exitCode = 0) =>
    evaluatePlaywrightReport(JSON.stringify({ stats }), exitCode);
  const passed = evaluate({ expected: 4, unexpected: 0, flaky: 0, skipped: 1 });
  assert.equal(passed.exitCode, 0);
  assert.deepEqual(passed.counts, {
    passed: 4,
    failed: 0,
    flaky: 0,
    skipped: 1,
    executed: 4,
    summaryFound: true,
  });

  const processFailure = evaluate(
    { expected: 4, unexpected: 0, flaky: 0, skipped: 1 },
    1,
  );
  assert.equal(processFailure.exitCode, 1);
  assert.ok(processFailure.errors.includes("Playwright exited with code 1"));

  const unexpected = evaluate({
    expected: 322,
    unexpected: 1,
    flaky: 0,
    skipped: 11,
  });
  assert.equal(unexpected.exitCode, 1);
  assert.deepEqual(
    [
      unexpected.counts.passed,
      unexpected.counts.failed,
      unexpected.counts.flaky,
      unexpected.counts.skipped,
    ],
    [322, 1, 0, 11],
  );
  assert.ok(
    unexpected.errors.includes("Playwright reported 1 unexpected test(s)"),
  );

  const flaky = evaluate({ expected: 4, unexpected: 0, flaky: 1, skipped: 0 });
  assert.equal(flaky.exitCode, 1);
  assert.ok(flaky.errors.includes("Playwright reported 1 flaky test(s)"));

  const empty = evaluate({ expected: 0, unexpected: 0, flaky: 0, skipped: 0 });
  assert.equal(empty.exitCode, 1);
  assert.ok(empty.errors.includes("Playwright executed zero tests"));

  const allSkipped = evaluate({
    expected: 0,
    unexpected: 0,
    flaky: 0,
    skipped: 3,
  });
  assert.equal(allSkipped.exitCode, 1);
  assert.ok(allSkipped.errors.includes("Playwright executed zero tests"));

  const missing = evaluatePlaywrightReport(null, 0);
  assert.equal(missing.exitCode, 1);
  assert.ok(missing.errors.includes("Playwright JSON report is missing"));

  const malformed = evaluatePlaywrightReport("{bad json", 0);
  assert.equal(malformed.exitCode, 1);
  assert.match(malformed.errors[0], /^Playwright JSON report is invalid:/);

  const invalidStats = evaluatePlaywrightReport(
    JSON.stringify({ stats: { expected: -1 } }),
    1,
  );
  assert.equal(invalidStats.exitCode, 1);
  assert.ok(invalidStats.errors.includes("Playwright exited with code 1"));
  assert.ok(
    invalidStats.errors.includes(
      "Playwright JSON report has invalid or missing stats",
    ),
  );
});

test("browser wrapper trusts JSON stats, not human summary text", async () => {
  const root = mkdtempSync(path.join(os.tmpdir(), "codex-browser-report-"));
  const originalCache = process.env.CODEX_STUDIO_CHECKS_CACHE;
  const originalWrite = process.stdout.write;
  process.env.CODEX_STUDIO_CHECKS_CACHE = path.join(root, "cache");
  process.stdout.write = () => true;
  const report = {
    stats: { expected: 322, unexpected: 1, flaky: 0, skipped: 11 },
  };
  const source = `import { writeFileSync } from 'node:fs';
writeFileSync(process.env.CODEX_STUDIO_PLAYWRIGHT_JSON_REPORT, ${JSON.stringify(JSON.stringify(report))});
process.stdout.write('322 passed (7.1m)\\n');
process.exit(1);`;
  try {
    const result = await execute(
      root,
      { name: "codex-agents-web", ecosystem: "pnpm" },
      "",
      true,
      {
        playwrightJsonReport: true,
        commands: [["node", ["--input-type=module", "-e", source]]],
      },
    );
    assert.equal(result.exitCode, 1);
    assert.equal(result.failed, 1);
    assert.equal(result.flaky, 0);
    assert.equal(result.passed, 322);
    assert.equal(result.skipped, 11);
    assert.ok(
      readFileSync(result.reportPath, "utf8").includes('"unexpected":1'),
    );
    assert.match(result.errors.join(" "), /Playwright exited with code 1/);
  } finally {
    process.stdout.write = originalWrite;
    if (originalCache === undefined)
      delete process.env.CODEX_STUDIO_CHECKS_CACHE;
    else process.env.CODEX_STUDIO_CHECKS_CACHE = originalCache;
    rmSync(root, { recursive: true, force: true });
  }
});

test("browser wrapper rejects a missing JSON report even after process success", async () => {
  const root = mkdtempSync(path.join(os.tmpdir(), "codex-browser-no-report-"));
  const originalCache = process.env.CODEX_STUDIO_CHECKS_CACHE;
  const originalWrite = process.stdout.write;
  process.env.CODEX_STUDIO_CHECKS_CACHE = path.join(root, "cache");
  process.stdout.write = () => true;
  try {
    const result = await execute(
      root,
      { name: "codex-agents-web", ecosystem: "pnpm" },
      "",
      true,
      {
        playwrightJsonReport: true,
        commands: [["node", ["-e", "process.exit(0)"]]],
      },
    );
    assert.equal(result.exitCode, 1);
    assert.ok(result.errors.includes("Playwright JSON report is missing"));
  } finally {
    process.stdout.write = originalWrite;
    if (originalCache === undefined)
      delete process.env.CODEX_STUDIO_CHECKS_CACHE;
    else process.env.CODEX_STUDIO_CHECKS_CACHE = originalCache;
    rmSync(root, { recursive: true, force: true });
  }
});

test("zero-test package command is a failure unless explicitly allowed", async () => {
  const result = await execute(
    process.cwd(),
    { name: "fixture", ecosystem: "pnpm", dir: ".", manifest: null },
    "",
    false,
    {
      commands: [
        ["node", ["-e", "process.stdout.write('No test files found')"]],
      ],
    },
  );
  assert.equal(result.exitCode, 1);
  assert.ok(result.errors.includes("native runner executed zero tests"));
});

test("composed runs name and log a failed constituent step", async () => {
  let printed = "";
  const write = process.stdout.write;
  process.stdout.write = function (chunk, ...args) {
    printed += chunk.toString();
    return write.call(this, chunk, ...args);
  };
  try {
    const result = await executeRecipe(process.cwd(), "test-fast", [
      {
        item: { name: "fixture step", ecosystem: "pnpm" },
        label: "fixture type check",
        options: { allowZeroTests: true },
        commands: [
          [
            "node",
            [
              "-e",
              "process.stdout.write('fixture failure output\\n'); process.exit(1)",
            ],
          ],
        ],
      },
    ]);
    assert.equal(result.exitCode, 1);
    assert.equal(result.failedStep, "fixture type check");
    assert.match(printed, /test-fast: failed at fixture type check/);
    assert.match(
      readFileSync(result.logPath, "utf8"),
      /fixture failure output/,
    );
  } finally {
    process.stdout.write = write;
  }
});

test("Node test runner summaries count executed and skipped cases", () => {
  const item = { name: "codex-studio-tooling", ecosystem: "pnpm" };
  assert.deepEqual(
    parseCounts(
      item,
      "ℹ tests 5\nℹ suites 0\nℹ pass 4\nℹ fail 0\nℹ cancelled 0\nℹ skipped 1",
    ),
    { passed: 4, failed: 0, skipped: 1, executed: 4, summaryFound: true },
  );
  assert.deepEqual(
    parseCounts(
      item,
      "TAP version 13\n1..0\n# tests 1\n# pass 1\n# fail 0\n# cancelled 0\n# skipped 0",
    ),
    { passed: 0, failed: 0, skipped: 0, executed: 0, summaryFound: true },
  );
});

function workspaceFixture() {
  const root = mkdtempSync(path.join(os.tmpdir(), "codex-boundaries-test-"));
  const packages = [];
  function add(name, dir, manifest = {}) {
    const absolute = path.join(root, dir);
    mkdirSync(absolute, { recursive: true });
    const manifestPath = path.join(absolute, "package.json");
    writeFileSync(manifestPath, JSON.stringify({ name, ...manifest }));
    packages.push({ name, dir, ecosystem: "pnpm", manifest: manifestPath });
    return absolute;
  }
  return { root, packages, add };
}

test("boundary negative: undeclared relative workspace import is rejected", () => {
  const fixture = workspaceFixture();
  try {
    const a = fixture.add("app-a", "workspaces/demo/apps/a");
    fixture.add("app-b", "workspaces/demo/apps/b");
    const source = path.join(a, "src", "entry.mjs");
    mkdirSync(path.dirname(source));
    writeFileSync(source, 'import "../../b/private.mjs";');
    assert.ok(
      boundaryFindings(fixture.root, fixture.packages, [source]).some(
        (finding) => finding.rule === "relative-workspace-import",
      ),
    );
  } finally {
    rmSync(fixture.root, { recursive: true, force: true });
  }
});

test("boundary negative: undeclared bare dependency is rejected", () => {
  const fixture = workspaceFixture();
  try {
    const app = fixture.add("app-a", "workspaces/demo/apps/a");
    const source = path.join(app, "entry.mjs");
    writeFileSync(source, 'import { thing } from "not-declared";');
    assert.ok(
      boundaryFindings(fixture.root, fixture.packages, [source]).some(
        (finding) => finding.rule === "undeclared-bare-import",
      ),
    );
  } finally {
    rmSync(fixture.root, { recursive: true, force: true });
  }
});

test("boundary negative: workspace packages cannot import apps by relative or package path", () => {
  const fixture = workspaceFixture();
  try {
    const app = fixture.add("app-a", "workspaces/demo/apps/a");
    const pkg = fixture.add("shared-pkg", "workspaces/demo/packages/shared", {
      dependencies: { "app-a": "workspace:*" },
    });
    const relative = path.join(pkg, "relative.mjs");
    const bare = path.join(pkg, "bare.mjs");
    writeFileSync(relative, 'import "../../apps/a/private.mjs";');
    writeFileSync(bare, 'import "app-a/private.mjs";');
    const findings = boundaryFindings(fixture.root, fixture.packages, [
      relative,
      bare,
    ]);
    assert.equal(
      findings.filter((finding) => finding.rule === "package-imports-app")
        .length,
      2,
    );
    assert.equal(app, path.join(fixture.root, "workspaces/demo/apps/a"));
  } finally {
    rmSync(fixture.root, { recursive: true, force: true });
  }
});

test("check-affected selects a changed package and transitive workspace dependents", () => {
  const fixture = workspaceFixture();
  try {
    const a = fixture.add("web-a", "workspaces/client/apps/a");
    fixture.add("desktop-b", "workspaces/client/apps/b", {
      dependencies: { "web-a": "workspace:*" },
    });
    const aItem = fixture.packages[0];
    aItem.manifest = path.join(a, "package.json");
    assert.deepEqual(
      affectedPlan(fixture.root, "origin/main", fixture.packages, [
        "workspaces/client/apps/a/src/page.ts",
      ]),
      [
        {
          package: "web-a",
          reason:
            "changed file workspaces/client/apps/a/src/page.ts belongs to workspaces/client/apps/a",
        },
        { package: "desktop-b", reason: "reverse dependency of web-a" },
      ],
    );
    const widened = affectedPlan(
      fixture.root,
      "origin/main",
      fixture.packages,
      ["root-unowned.md"],
    );
    assert.equal(widened.length, 2);
    assert.ok(
      widened.every((item) =>
        item.reason.includes("unowned file root-unowned.md"),
      ),
    );
    assert.ok(
      affectedPlan(fixture.root, "origin/main", fixture.packages, [
        "package.json",
      ]).every((item) => item.reason.includes("root policy file package.json")),
    );
    assert.throws(
      () => affectedPlan(fixture.root, "origin/main", fixture.packages, []),
      /No changed files/,
    );
  } finally {
    rmSync(fixture.root, { recursive: true, force: true });
  }
});

test("check-affected adds Cargo workspace dependents from cargo metadata", () => {
  const root = mkdtempSync(path.join(os.tmpdir(), "codex-cargo-affected-"));
  try {
    mkdirSync(path.join(root, "crates/a"), { recursive: true });
    mkdirSync(path.join(root, "crates/b"), { recursive: true });
    mkdirSync(path.join(root, "crates/a/src"));
    mkdirSync(path.join(root, "crates/b/src"));
    writeFileSync(
      path.join(root, "Cargo.toml"),
      '[workspace]\nresolver = "3"\nmembers = ["crates/a", "crates/b"]\n',
    );
    writeFileSync(
      path.join(root, "crates/a/Cargo.toml"),
      '[package]\nname = "crate-a"\nversion = "0.1.0"\nedition = "2024"\n',
    );
    writeFileSync(
      path.join(root, "crates/b/Cargo.toml"),
      '[package]\nname = "crate-b"\nversion = "0.1.0"\nedition = "2024"\n[dependencies]\ncrate-a = { path = "../a" }\n',
    );
    writeFileSync(
      path.join(root, "crates/a/src/lib.rs"),
      "pub fn value() {}\n",
    );
    writeFileSync(
      path.join(root, "crates/b/src/lib.rs"),
      "pub fn value() {}\n",
    );
    const packages = [
      {
        name: "crate-a",
        ecosystem: "cargo",
        dir: "crates/a",
        manifest: path.join(root, "crates/a/Cargo.toml"),
      },
      {
        name: "crate-b",
        ecosystem: "cargo",
        dir: "crates/b",
        manifest: path.join(root, "crates/b/Cargo.toml"),
      },
    ];
    assert.deepEqual(
      affectedPlan(root, "origin/main", packages, ["crates/a/src/lib.rs"]),
      [
        {
          package: "crate-a",
          reason: "changed file crates/a/src/lib.rs belongs to crates/a",
        },
        { package: "crate-b", reason: "reverse dependency of crate-a" },
      ],
    );
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});

test("contracts check mode detects an authoritative schema field change without writing tracked files", () => {
  const root = path.resolve(
    new URL("../../../../../", import.meta.url).pathname,
  );
  const server = path.join(root, "workspaces/runtime/apps/server/src");
  const apiTypes = path.join(
    root,
    "workspaces/client/apps/web/src/generated/api.ts",
  );
  const schemaTypes = path.join(
    root,
    "workspaces/client/apps/web/src/generated/apiSchema.ts",
  );
  const before = [readFileSync(apiTypes), readFileSync(schemaTypes)];
  const workingTreeBefore = [
    execFileSync("git", ["status", "--porcelain", "--untracked-files=all"], {
      cwd: root,
    }),
    execFileSync("git", ["diff", "--binary", "HEAD"], {
      cwd: root,
      maxBuffer: 32 * 1024 * 1024,
    }),
  ];
  const directory = mkdtempSync(
    path.join(os.tmpdir(), "codex-contract-drift-"),
  );
  try {
    const script = path.join(directory, "check.py");
    writeFileSync(
      path.join(directory, ".oxfmtrc.json"),
      readFileSync(path.join(root, ".oxfmtrc.json")),
    );
    writeFileSync(
      script,
      `from pathlib import Path\nfrom studio_api import generate_types as g\ndirectory = Path(${JSON.stringify(directory)})\ndoc = g.openapi_document()\nrequest_id = doc["components"]["schemas"]["AcceptInvite"]["properties"]["requestId"]\nrequest_id["type"] = "integer"\nrequest_id.pop("maxLength", None)\nrequest_id.pop("minLength", None)\ng.openapi_document = lambda: doc\ng.ROOT = directory\ng.OUTPUT = directory / "api.ts"\ng.SCHEMA_OUTPUT = directory / "apiSchema.ts"\ng.OUTPUT.write_bytes(Path(${JSON.stringify(apiTypes)}).read_bytes())\ng.SCHEMA_OUTPUT.write_bytes(Path(${JSON.stringify(schemaTypes)}).read_bytes())\nsnapshot = (g.OUTPUT.read_bytes(), g.SCHEMA_OUTPUT.read_bytes())\nresult = g.main(["--check"])\nassert result == 1, result\nassert snapshot == (g.OUTPUT.read_bytes(), g.SCHEMA_OUTPUT.read_bytes()), "check mode rewrote generated files"\n`,
    );
    const runner = path.join(server, "codex_python.py");
    execFileSync("python3", [runner, "--exec", script], {
      cwd: root,
      stdio: "pipe",
    });
    assert.deepEqual(
      [readFileSync(apiTypes), readFileSync(schemaTypes)],
      before,
    );
    assert.deepEqual(
      [
        execFileSync(
          "git",
          ["status", "--porcelain", "--untracked-files=all"],
          { cwd: root },
        ),
        execFileSync("git", ["diff", "--binary", "HEAD"], {
          cwd: root,
          maxBuffer: 32 * 1024 * 1024,
        }),
      ],
      workingTreeBefore,
      "check mode changed working tree bytes",
    );
  } finally {
    rmSync(directory, { recursive: true, force: true });
  }
});
