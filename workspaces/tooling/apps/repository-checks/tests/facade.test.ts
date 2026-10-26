import { spawnSync } from "node:child_process";
import test from "node:test";
import assert from "node:assert/strict";
import {
  affectedPlan,
  boundaryFindings,
  commandFor,
  discover,
  execute,
  executeRecipe,
  evaluatePlaywrightReport,
  nextestError,
  nextestSelectionError,
  packageTarget,
  parseCounts,
  parseSelection,
  pruneLogs,
  recipePlan,
  unsafeRustFindings,
} from "../facade.ts";
import {
  mkdtempSync,
  mkdirSync,
  chmodSync,
  statSync,
  rmSync,
  utimesSync,
  writeFileSync,
  readFileSync,
  readdirSync,
  existsSync,
} from "node:fs";
import os from "node:os";
import path from "node:path";

test("short and comprehensive recipes preserve their owned test boundaries", () => {
  const fast = recipePlan(process.cwd(), "test-fast");
  const comprehensive = recipePlan(process.cwd(), "test-comprehensive");
  for (const recipe of [fast, comprehensive]) {
    assert.match(recipe.command, /build:wasm/);
    assert.match(recipe.command, /build:bundle:prepared/);
    for (const once of [
      /build:wasm/g,
      /build:bundle:prepared/g,
      /'nextest' 'run'/g,
    ])
      assert.equal([...recipe.command.matchAll(once)].length, 1);
    assert.match(recipe.command, /'nextest' 'run' '--workspace'/);
    assert.match(recipe.command, /studio-contracts/);
    assert.match(recipe.command, /boundaries-check/);
    assert.match(recipe.command, /vm-guest\/run-guest-suite\.mjs/);
    assert.doesNotMatch(
      recipe.command,
      /--release|test-server-load|runtime_load|mutations/,
    );
  }
  assert.match(fast.command, /'--profile' 'fast'/);
  assert.match(fast.command, /test:unit:fast/);
  assert.match(fast.command, /native\/check-native\.test\.mjs/);
  assert.doesNotMatch(fast.command, /test:browser/);
  assert.match(comprehensive.command, /'--profile' 'ci'/);
  assert.match(comprehensive.command, /'clippy' '--workspace'/);
  assert.match(comprehensive.command, /test:unit:prepared/);
  assert.match(comprehensive.command, /test:browser:smoke:prepared/);
});

test("real IndexedDB staging belongs to provisioned browser smoke, not Node units", () => {
  const web = "workspaces/client/apps/web";
  const spec = readFileSync(
    `${web}/tests/sync/entity-transfer-storage-browser.spec.mjs`,
    "utf8",
  );
  const title = spec.match(/test\(\s*"([^"]+)"/)?.[1];
  assert.ok(title, "the real IndexedDB witness must remain registered");
  const scripts = JSON.parse(
    readFileSync(`${web}/package.json`, "utf8"),
  ).scripts;
  const smoke = scripts["test:browser:smoke:prepared"];
  const grep = smoke.match(/--grep "([^"]+)"/)?.[1];
  assert.ok(grep, "package.json owns the smoke selector");
  assert.match(title, new RegExp(grep));
  assert.match(spec, /from "\.\.\/playwright\.mjs"/);
  assert.equal(
    existsSync(`${web}/src/sync/entityTransfer.storage.test.ts`),
    false,
  );
  const fast = readFileSync(`${web}/vitest.fast.config.ts`, "utf8");
  assert.doesNotMatch(fast, /entityTransfer\.storage\.test/);
  assert.match(fast, /"src\/sync\/entityTransfer\.test\.ts"/);
  const workflow = readFileSync(".github/workflows/ci.yml", "utf8");
  const browserJob = workflow.split("  fast-browser-smoke:")[1];
  assert.match(
    browserJob,
    /playwright install chromium[\s\S]+just test-browser-smoke --prepared/,
  );
});

test("consolidated Rust targets register every integration test file once", () => {
  const consolidated = discover(process.cwd()).filter(
    (item) =>
      item.ecosystem === "cargo" &&
      existsSync(
        path.join(path.dirname(item.manifest), "tests/integration.rs"),
      ),
  );
  assert.ok(consolidated.length > 0);
  for (const item of consolidated) {
    const crate = path.dirname(item.manifest);
    const manifest = readFileSync(item.manifest, "utf8");
    assert.match(manifest, /^autotests = false$/m, item.name);
    assert.match(
      manifest,
      /\[\[test\]\]\s+name = "integration"\s+path = "tests\/integration.rs"/,
      item.name,
    );
    const facade = readFileSync(
      path.join(crate, "tests/integration.rs"),
      "utf8",
    );
    const modules = [...facade.matchAll(/^mod (\w+);$/gm)]
      .map((match) => `${match[1]}.rs`)
      .sort();
    const files = readdirSync(path.join(crate, "tests"))
      .filter((name) => name.endsWith(".rs") && name !== "integration.rs")
      .sort();
    assert.deepEqual(
      modules,
      files,
      `${item.name}: register each test file in tests/integration.rs`,
    );
  }
});

test("unsafe Rust is limited to the purpose-listed modules", () => {
  const root = mkdtempSync(path.join(os.tmpdir(), "codex-unsafe-rust-policy-"));
  const exception = path.join(
    root,
    "workspaces/runtime/packages/runtime-services/src/image_workspace_macos/fsevents_ffi.rs",
  );
  const violation = path.join(root, "workspaces/runtime/src/unsafe_probe.rs");
  const quotedViolation = path.join(
    root,
    "workspaces/runtime/src/quoted_unsafe_probe.rs",
  );
  const attributeViolation = path.join(
    root,
    "workspaces/runtime/src/attribute_unsafe_probe.rs",
  );
  const safeProbe = path.join(root, "workspaces/runtime/src/safe_probe.rs");
  mkdirSync(path.dirname(exception), { recursive: true });
  mkdirSync(path.dirname(violation), { recursive: true });
  writeFileSync(exception, "#![allow(unsafe_code)]\nunsafe { example(); }\n");
  writeFileSync(violation, "fn probe() { unsafe { example(); } }\n");
  writeFileSync(
    quotedViolation,
    String.raw`let quote = '"';
fn probe() { unsafe { example(); } }
`,
  );
  writeFileSync(attributeViolation, "#![allow(unsafe_code)]\n");
  writeFileSync(safeProbe, '// "unsafe" in comments and strings is not code\n');

  try {
    assert.deepEqual(
      unsafeRustFindings(root, [
        exception,
        violation,
        quotedViolation,
        attributeViolation,
        safeProbe,
      ]).map((finding) => finding.file),
      [
        "workspaces/runtime/src/unsafe_probe.rs",
        "workspaces/runtime/src/quoted_unsafe_probe.rs",
        "workspaces/runtime/src/attribute_unsafe_probe.rs",
      ],
    );
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});

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
          "workspaces/tooling/apps/repository-checks/tests/gates.test.ts",
          "workspaces/tooling/apps/repository-checks/tests/facade.test.ts",
          "workspaces/tooling/apps/repository-checks/tests/release-binaries.test.ts",
          "workspaces/tooling/apps/repository-checks/tests/scale-verifier-teardown.test.ts",
          "workspaces/tooling/apps/repository-checks/tests/native-style-policy.test.ts",
          "workspaces/tooling/apps/installed-smoke/tests/smoke.test.mjs",
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

test("hosted non-Rust checks consume prepared inputs and cannot compile Cargo", () => {
  const workflow = readFileSync(".github/workflows/ci.yml", "utf8");
  const remaining = workflow
    .split("  fast-rest:\n")[1]
    .split("  fast-rust:\n")[0];
  assert.match(remaining, /needs: rust-build-tests/);
  assert.match(remaining, /name: prepared-check-inputs/);
  assert.doesNotMatch(remaining, /client-build-setup|cargo build|clippy/);
  assert.doesNotMatch(
    recipePlan(process.cwd(), "test-fast-rest-non-rust").command,
    /'cargo'|clippy|build:wasm|build:bundle/,
  );
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

  const identifiedFailure = evaluatePlaywrightReport(
    JSON.stringify({
      stats: { expected: 4, unexpected: 1, flaky: 0, skipped: 0 },
      suites: [
        {
          title: "runtime smoke",
          specs: [
            {
              title: "loads the agent list",
              tests: [
                {
                  status: "unexpected",
                  results: [
                    {
                      status: "failed",
                      errors: [
                        { message: "locator did not appear\nstack trace" },
                      ],
                    },
                  ],
                },
              ],
            },
          ],
        },
      ],
    }),
    1,
  );
  assert.deepEqual(identifiedFailure.failures, [
    {
      title: "runtime smoke › loads the agent list",
      error: "locator did not appear",
    },
  ]);

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
  process.env.CODEX_STUDIO_CHECKS_CACHE = path.join(root, "cache");
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
    if (originalCache === undefined)
      delete process.env.CODEX_STUDIO_CHECKS_CACHE;
    else process.env.CODEX_STUDIO_CHECKS_CACHE = originalCache;
    rmSync(root, { recursive: true, force: true });
  }
});

test("browser wrapper rejects a missing JSON report even after process success", async () => {
  const root = mkdtempSync(path.join(os.tmpdir(), "codex-browser-no-report-"));
  const originalCache = process.env.CODEX_STUDIO_CHECKS_CACHE;
  process.env.CODEX_STUDIO_CHECKS_CACHE = path.join(root, "cache");
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
  try {
    const result = await executeRecipe(
      process.cwd(),
      "test-fast",
      [
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
      ],
      {
        write: (text) => {
          printed += text;
        },
      },
    );
    assert.equal(result.exitCode, 1);
    assert.equal(result.failedStep, "fixture type check");
    assert.match(printed, /test-fast: failed at fixture type check/);
    assert.match(
      readFileSync(result.logPath, "utf8"),
      /fixture failure output/,
    );
  } finally {
    // Output is injected; the runner's stdout stays untouched.
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

test("boundary: a script inside a nested Cargo crate belongs to the enclosing pnpm app", () => {
  const fixture = workspaceFixture();
  try {
    const app = fixture.add("native-app", "workspaces/demo/apps/native", {
      dependencies: { declared: "1.0.0" },
    });
    const crate = path.join(app, "src-tauri");
    mkdirSync(path.join(crate, "src"), { recursive: true });
    writeFileSync(
      path.join(crate, "Cargo.toml"),
      '[package]\nname = "native-shell"\n',
    );
    fixture.packages.push({
      name: "native-shell",
      dir: "workspaces/demo/apps/native/src-tauri",
      ecosystem: "cargo",
      manifest: path.join(crate, "Cargo.toml"),
    });
    const declared = path.join(crate, "src", "declared.js");
    writeFileSync(declared, 'import "declared";');
    const undeclared = path.join(crate, "src", "undeclared.js");
    writeFileSync(undeclared, 'import "not-declared";');
    const findings = boundaryFindings(fixture.root, fixture.packages, [
      declared,
      undeclared,
    ]);
    assert.deepEqual(
      findings.map((finding) => [
        finding.rule,
        finding.importer,
        finding.specifier,
      ]),
      [["undeclared-bare-import", "native-app", "not-declared"]],
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
    fixture.add("consumer-c", "workspaces/client/apps/c", {
      dependencies: { "desktop-b": "workspace:*" },
    });
    fixture.add("unrelated-d", "workspaces/client/apps/d");
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
        { package: "consumer-c", reason: "reverse dependency of desktop-b" },
      ],
    );
    const widened = affectedPlan(
      fixture.root,
      "origin/main",
      fixture.packages,
      ["root-unowned.md"],
    );
    assert.equal(widened.length, 4);
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
    const graph = [
      ["a", null],
      ["b", "a"],
      ["c", "b"],
      ["d", null],
    ];
    writeFileSync(
      path.join(root, "Cargo.toml"),
      '[workspace]\nresolver = "3"\nmembers = ["crates/*"]\n',
    );
    const packages = graph.map(([name, dependency]) => {
      const dir = `crates/${name}`;
      mkdirSync(path.join(root, dir, "src"), { recursive: true });
      const manifest = path.join(root, dir, "Cargo.toml");
      writeFileSync(
        manifest,
        `[package]\nname = "crate-${name}"\nversion = "0.1.0"\nedition = "2024"\n${dependency ? `[dependencies]\ncrate-${dependency} = { path = "../${dependency}" }\n` : ""}`,
      );
      writeFileSync(path.join(root, dir, "src/lib.rs"), "pub fn value() {}\n");
      return { name: `crate-${name}`, ecosystem: "cargo", dir, manifest };
    });
    assert.deepEqual(
      affectedPlan(root, "origin/main", packages, ["crates/a/src/lib.rs"]),
      [
        {
          package: "crate-a",
          reason: "changed file crates/a/src/lib.rs belongs to crates/a",
        },
        { package: "crate-b", reason: "reverse dependency of crate-a" },
        { package: "crate-c", reason: "reverse dependency of crate-b" },
      ],
    );
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});

// Exercise the real builder in a disposable checkout with executable witnesses;
// no compiler, generated files, or installed host CLI can affect these cases.
test("WASM preparation locates only the pinned CLI and builds before binding", () => {
  const root = mkdtempSync(path.join(os.tmpdir(), "studio-wasm-preparation-"));
  const script = "workspaces/client/apps/web/scripts/build-client-wasm.mjs";
  const manifest = "workspaces/client/packages/client-wasm/Cargo.toml";
  const version = readFileSync(manifest, "utf8").match(
    /^wasm-bindgen\s*=\s*"=([^"\s]+)"/m,
  )[1];
  const bin = path.join(root, "bin");
  const cache = path.join(root, "cache");
  const cargoHome = path.join(root, "cargo-home");
  const log = path.join(root, "commands.jsonl");
  function executable(file, body) {
    mkdirSync(path.dirname(file), { recursive: true });
    writeFileSync(file, `#!${process.execPath}\n${body}`);
    chmodSync(file, 0o755);
  }
  mkdirSync(path.dirname(path.join(root, script)), { recursive: true });
  mkdirSync(path.dirname(path.join(root, manifest)), { recursive: true });
  writeFileSync(path.join(root, script), readFileSync(script));
  writeFileSync(path.join(root, manifest), readFileSync(manifest));
  executable(
    path.join(bin, "cargo"),
    `require('node:fs').appendFileSync(${JSON.stringify(log)}, JSON.stringify(process.argv.slice(2))+'\\n')`,
  );
  executable(
    path.join(bin, "wasm-bindgen"),
    "console.log('wasm-bindgen wrong-version')",
  );
  const env = {
    ...process.env,
    PATH: bin,
    CARGO_HOME: cargoHome,
    XDG_CACHE_HOME: cache,
    CARGO_TARGET_DIR: path.join(root, "build"),
  };
  const run = () =>
    spawnSync(process.execPath, [path.join(root, script)], {
      env,
      encoding: "utf8",
    });
  try {
    const missing = run();
    assert.notEqual(missing.status, 0);
    assert.ok(
      missing.stderr.includes(
        `cargo install wasm-bindgen-cli --version ${version} --locked`,
      ),
    );
    assert.throws(() => readFileSync(log));
    const cli = path.join(
      cache,
      "codex-studio-rust-ui",
      `wasm-bindgen-${version}`,
      "bin",
      "wasm-bindgen",
    );
    executable(
      cli,
      `if (process.argv[2] === '--version') console.log('wasm-bindgen ${version}'); else require('node:fs').appendFileSync(${JSON.stringify(log)}, JSON.stringify(process.argv.slice(2))+'\\n')`,
    );
    const prepared = run();
    assert.equal(prepared.status, 0, prepared.stderr);
    const calls = readFileSync(log, "utf8").trim().split("\n").map(JSON.parse);
    assert.deepEqual(calls[0], [
      "build",
      "-p",
      "studio-client-wasm",
      "--release",
      "--target",
      "wasm32-unknown-unknown",
      "--locked",
    ]);
    assert.equal(
      calls[1][0],
      path.join(
        env.CARGO_TARGET_DIR,
        "wasm32-unknown-unknown/release/studio_client_wasm.wasm",
      ),
    );
    assert.ok(
      calls[1].includes("workspaces/client/apps/web/src/wasm/generated"),
    );
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});

test("Playwright launcher clears BROWSER and removes state after success and failure", () => {
  const root = mkdtempSync(
    path.join(os.tmpdir(), "studio-browser-environment-"),
  );
  const witness = path.join(root, "pnpm");
  try {
    for (const status of [0, 19]) {
      writeFileSync(
        witness,
        `#!${process.execPath}\nif ('BROWSER' in process.env) process.exit(17); console.log('isolated browser'); process.exit(${status});`,
      );
      chmodSync(witness, 0o755);
      const result = spawnSync(
        process.execPath,
        ["workspaces/client/apps/web/scripts/run-browser-tests.mjs", "--list"],
        {
          encoding: "utf8",
          env: {
            ...process.env,
            PATH: root,
            TMPDIR: root,
            KEEP_TEST_ARTIFACTS: "",
            BROWSER: "/session/studio-browser",
          },
        },
      );
      assert.equal(result.status, status, result.stderr);
      assert.match(result.stdout, /isolated browser/);
      assert.deepEqual(readdirSync(root), ["pnpm"]);
    }
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});

test("tests retain temporary state ownership and follow the platform temp root", async () => {
  const { temporaryStateFindings, repositoryTemporaryStateFindings } =
    await import("../test-temp-state.ts");
  assert.deepEqual(repositoryTemporaryStateFindings(process.cwd()), []);
  for (const source of [
    `let root = tempfile::tempdir().unwrap().${"keep"}();`,
    `let root = dir.\n${"into_path"}();`,
    `tempfile::TempDir::${"keep"}(root);`,
    `root.${"disable_cleanup"}(true);`,
    "static ROOT: OnceLock<tempfile::TempDir> = OnceLock::new();",
    "struct Template { root: tempfile::TempDir } static ROOT: OnceCell<Template> = OnceCell::const_new();",
    `root = tempfile.TemporaryDirectory(dir="${"/" + "tmp"}")`,
  ]) {
    assert.equal(temporaryStateFindings("tests/example.rs", source).length, 1);
  }
  const fixture = `assert_eq!(normalize("${"/" + "tmp"}/example"), "example");`;
  assert.deepEqual(
    temporaryStateFindings("tests/example.rs", fixture, [fixture]),
    [],
  );
  assert.equal(
    temporaryStateFindings("tests/example.rs", `${fixture}\n${fixture}`, [
      fixture,
    ]).length,
    1,
  );
  assert.deepEqual(
    temporaryStateFindings(
      "tests/example.rs",
      "let root = tempfile::tempdir().unwrap();",
    ),
    [],
  );
  assert.deepEqual(
    temporaryStateFindings(
      "src/persistence.rs",
      `let path = file.${"keep"}();`,
    ),
    [],
  );
});

test("temporary state scan skips tracked files removed by the current change", async () => {
  const { repositoryTemporaryStateFindings } =
    await import("../test-temp-state.ts");
  const root = mkdtempSync(
    path.join(os.tmpdir(), "studio-temp-state-deleted-"),
  );
  try {
    for (const args of [
      ["init", "--quiet"],
      ["config", "user.email", "checks@example.invalid"],
      ["config", "user.name", "Repository checks"],
    ]) {
      const result = spawnSync("git", args, { cwd: root, encoding: "utf8" });
      assert.equal(result.status, 0, result.stderr);
    }
    writeFileSync(
      path.join(root, "removed.test.mjs"),
      "const root = '" + "/" + "tmp" + "';\n",
    );
    let result = spawnSync("git", ["add", "removed.test.mjs"], {
      cwd: root,
      encoding: "utf8",
    });
    assert.equal(result.status, 0, result.stderr);
    rmSync(path.join(root, "removed.test.mjs"));
    assert.deepEqual(repositoryTemporaryStateFindings(root), []);
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});

test("installer recipe preserves directory arguments as literal positional values", () => {
  const root = mkdtempSync(path.join(os.tmpdir(), "studio-install-recipe-"));
  try {
    const tools = path.join(root, "tools");
    const target = path.join(root, "target with spaces");
    const binary = path.join(target, "debug/studio-runtime-server");
    const capture = path.join(root, "arguments");
    mkdirSync(tools);
    mkdirSync(path.dirname(binary), { recursive: true });
    writeFileSync(path.join(root, "justfile"), readFileSync("justfile"));
    const budget = "workspaces/tooling/apps/repository-checks/gate-config.ts";
    mkdirSync(path.dirname(path.join(root, budget)), { recursive: true });
    writeFileSync(path.join(root, budget), readFileSync(budget));
    writeFileSync(path.join(tools, "cargo"), "#!/bin/sh\nexit 0\n");
    chmodSync(path.join(tools, "cargo"), 0o755);
    writeFileSync(binary, '#!/bin/sh\nprintf "%s\\0" "$@" > "$CAPTURE"\n');
    chmodSync(binary, 0o755);
    const destination = path.join(root, "commands with spaces $literal");
    const result = spawnSync(
      "just",
      [
        "--justfile",
        path.join(root, "justfile"),
        "install",
        "--bin-dir",
        destination,
      ],
      {
        env: {
          ...process.env,
          PATH: `${tools}:${process.env.PATH}`,
          CARGO_TARGET_DIR: target,
          CAPTURE: capture,
        },
        encoding: "utf8",
        timeout: 10000,
      },
    );
    assert.equal(result.status, 0, result.stderr);
    assert.deepEqual(readFileSync(capture, "utf8").split("\0").slice(0, -1), [
      "install-cli",
      "--project",
      root,
      "--bin-dir",
      destination,
    ]);
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});

test("repository gates use manifest packages without the retired backend", () => {
  assert.ok(
    !discover(process.cwd()).some((item) => item.ecosystem === "server"),
  );
  for (const name of [
    "test-fast",
    "test-comprehensive",
    "test-fast-rest",
    "test-fast-rest-non-rust",
    "test-heavy",
  ]) {
    const recipe = recipePlan(process.cwd(), name);
    assert.doesNotMatch(
      recipe.command,
      /apps\/server|studio-operations-python|codex_python|test-server\.py/,
    );
  }
  const scripts = JSON.parse(readFileSync("package.json", "utf8")).scripts;
  assert.equal(scripts["typecheck:runtime"], undefined);
  assert.equal(scripts["test:server:python"], undefined);
  assert.match(scripts["test:server:bridge"], /studio-claude-bridge/);
  const workflow = readFileSync(".github/workflows/ci.yml", "utf8");
  assert.doesNotMatch(
    workflow,
    /setup-python|install-cli\.py|^  (?:python|fast-server):/m,
  );
  assert.match(workflow, /run: pnpm run contracts:check/);
  for (const action of ["fast-tier-setup", "client-build-setup"]) {
    assert.doesNotMatch(
      readFileSync(`.github/actions/${action}/action.yml`, "utf8"),
      /python3|setup-python|install-cli\.py|studio-operations-python/,
    );
  }
  for (const job of [
    "rust",
    "rust-macos",
    "rust-build-tests",
    "fast-rest",
    "fast-rust",
    "fast-browser-smoke",
  ]) {
    assert.ok(workflow.includes(`  ${job}:`), job);
  }
});

test("short profile commands retain native nextest summary and zero-test guards", () => {
  assert.deepEqual(
    parseCounts(
      { ecosystem: "cargo" },
      "   Summary [ 0.621s] 484 tests run: 483 passed, 1 failed, 3049 skipped",
    ),
    {
      passed: 483,
      failed: 1,
      skipped: 3049,
      executed: 484,
      summaryFound: true,
    },
  );
  assert.equal(
    parseCounts(
      { ecosystem: "cargo" },
      "   Summary [ 0.001s] 0 tests run: 0 passed",
    ).executed,
    0,
  );
});
