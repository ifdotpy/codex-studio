import test from "node:test";
import assert from "node:assert/strict";
import {
  affectedPlan,
  boundaryFindings,
  commandFor,
  execute,
  nextestError,
  nextestSelectionError,
  packageTarget,
  parseCounts,
  parseSelection,
  pruneLogs,
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
  };
  process.env.PATH = `${bin}:${original.PATH}`;
  process.env.CARGO_TARGET_DIR = path.join(root, "target");
  process.env.CODEX_STUDIO_CHECKS_CACHE = path.join(root, "logs");
  const originalWrite = process.stdout.write;
  process.stdout.write = () => true;
  const item = { name: "fixture", ecosystem: "cargo" };
  try {
    process.env.FIXTURE_CARGO_MODE = "empty";
    const empty = await execute(root, item, "missing_case", true);
    assert.equal(empty.exitCode, 1);
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
    execFileSync("git", ["diff", "--binary", "HEAD"], { cwd: root }),
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
        execFileSync("git", ["diff", "--binary", "HEAD"], { cwd: root }),
      ],
      workingTreeBefore,
      "check mode changed working tree bytes",
    );
  } finally {
    rmSync(directory, { recursive: true, force: true });
  }
});
