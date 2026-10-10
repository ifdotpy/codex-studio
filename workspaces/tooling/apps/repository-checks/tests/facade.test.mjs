import test from "node:test";
import assert from "node:assert/strict";
import {
  commandFor,
  parseCounts,
  parseSelection,
  pruneLogs,
} from "../facade.mjs";
import { mkdtempSync, mkdirSync, statSync, rmSync, utimesSync } from "node:fs";
import os from "node:os";
import path from "node:path";

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

test("Cargo uses manifest package filters and its native summary detects zero matches", () => {
  const item = { name: "studio-diagnostics", ecosystem: "cargo" };
  assert.deepEqual(commandFor("/repo", item, "test", "cli::tests::help"), [
    ["cargo", ["test", "-p", item.name, "cli::tests::help"]],
  ]);
  assert.deepEqual(
    commandFor("/repo", item, "test", "cli::tests::help", {
      passthrough: ["--exact", "--test", "integration"],
    }),
    [
      [
        "cargo",
        [
          "test",
          "-p",
          item.name,
          "cli::tests::help",
          "--",
          "--exact",
          "--test",
          "integration",
        ],
      ],
    ],
  );
  assert.deepEqual(
    parseCounts(
      item,
      "test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 12 filtered out",
    ),
    {
      passed: 0,
      failed: 0,
      skipped: 0,
      executed: 0,
      summaryFound: true,
    },
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
      options: { name: "renders readable error", passthrough: [] },
    },
  );
  assert.deepEqual(parseSelection("case", ["--", "--exact", "--test", "x"]), {
    caseName: "case",
    options: { name: "", passthrough: ["--exact", "--test", "x"] },
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
