import test from "node:test";
import type { GateStep, PackageManifest } from "../facade.ts";
import assert from "node:assert/strict";

import { commandFor, discover, executeRecipe, gateSteps } from "../facade.ts";
import { runLogged } from "../owned-process.ts";
import {
  chmodSync,
  mkdtempSync,
  realpathSync,
  writeFileSync,
  readFileSync,
  mkdirSync,
  rmSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { spawnSync } from "node:child_process";
import path from "node:path";

const fakeStep = (
  id: string,
  lane: string,
  extra: Partial<GateStep> = {},
): GateStep => ({
  id,
  label: id,
  lane,
  item: { name: id, ecosystem: "pnpm" },
  ...extra,
});

test("worker isolation preserves PATH tools and discovers the selected Node", () => {
  const directory = mkdtempSync(path.join(tmpdir(), "studio-worker-tools-"));
  try {
    const node = path.join(directory, "node");
    writeFileSync(node, "#!/bin/sh\nexit 0\n");
    chmodSync(node, 0o755);
    const run = spawnSync(
      "bash",
      [
        "-c",
        `
      mkdir() { :; }
      CS_NAME=fixture
      source workspaces/tooling/worker-env.sh
      test "$CODEX_NODE" = "$EXPECTED_NODE"
    `,
      ],
      {
        env: {
          ...process.env,
          PATH: directory + path.delimiter + process.env.PATH,
          EXPECTED_NODE: node,
        },
        encoding: "utf8",
      },
    );
    assert.equal(run.status, 0, run.stderr);
  } finally {
    rmSync(directory, { recursive: true, force: true });
  }
});

test("bridge config removes ambient agent settings before workers inherit them", () => {
  const result = spawnSync(
    process.execPath,
    [
      "--input-type=module",
      "-e",
      `
    await import(${JSON.stringify(new URL("../../../../providers/apps/claude-bridge/vitest.config.ts", import.meta.url).href)});
    const assert = await import('node:assert/strict');
    for (const key of ['CLAUDE_CONFIG_DIR', 'CLAUDECODE', 'STUDIO_CLAUDE_OPTIONS', 'AI_AGENT'])
      assert.equal(process.env[key], undefined, key);
    assert.equal(process.env.PATH, ${JSON.stringify(process.env.PATH)});
  `,
    ],
    {
      encoding: "utf8",
      env: {
        ...process.env,
        CLAUDE_CONFIG_DIR: "ambient",
        CLAUDECODE: "1",
        STUDIO_CLAUDE_OPTIONS: "{",
        AI_AGENT: "ambient",
      },
    },
  );
  assert.equal(result.status, 0, result.stderr);
});

test("gate children discard ambient agent settings and keep explicit fixture settings", async () => {
  const { execute } = await import("../facade.ts");
  const previous = process.env.STUDIO_CLAUDE_OPTIONS;
  process.env.STUDIO_CLAUDE_OPTIONS = "{";
  try {
    const result = await execute(
      process.cwd(),
      { name: "environment fixture", ecosystem: "pnpm" },
      "",
      false,
      {
        commands: [
          [
            process.execPath,
            [
              "-e",
              `
        const assert = require('node:assert/strict');
        assert.equal(process.env.STUDIO_CLAUDE_OPTIONS, undefined);
        assert.equal(process.env.STUDIO_SUPERVISOR_BINARY, '/explicit/fixture');
      `,
            ],
          ],
        ],
        env: { STUDIO_SUPERVISOR_BINARY: "/explicit/fixture" },
        allowZeroTests: true,
        write: () => {},
      },
    );
    assert.equal(result.exitCode, 0, readFileSync(result.logPath, "utf8"));
  } finally {
    if (previous === undefined) delete process.env.STUDIO_CLAUDE_OPTIONS;
    else process.env.STUDIO_CLAUDE_OPTIONS = previous;
  }
});

test("fixed lanes overlap, gate the guest build, and summarize failures in declaration order", async () => {
  const trace: string[] = [];
  const releaseBuild = Promise.withResolvers<void>();
  const steps = [
    fakeStep("build", "rust", { provides: "workspace" }),
    fakeStep("rust-failure", "rust", { requires: "workspace" }),
    fakeStep("js-failure", "js"),
    fakeStep("js-independent", "js"),
    fakeStep("types-independent", "types"),
    fakeStep("guest", "guest", { requires: "workspace" }),
  ];
  let printed = "";
  const result = await executeRecipe(process.cwd(), "test-fast", steps, {
    write: (text) => {
      printed += text;
    },
    execute: async (_root, item, _case, _json, options) => {
      trace.push(item.name);
      if (item.name === "build") await releaseBuild.promise;
      if (item.name === "js-failure") releaseBuild.resolve();
      mkdirSync(options.logDirectory, { recursive: true });
      const logPath = path.join(options.logDirectory, "full.log");
      writeFileSync(logPath, item.name + "\n");
      const exitCode = item.name.endsWith("failure") ? 1 : 0;
      return {
        exitCode,
        status: exitCode ? "failed" : "passed",
        durationSeconds: 0,
        logPath,
      };
    },
  });
  assert.equal(result.failedStep, "rust-failure");
  assert.ok(trace.indexOf("js-failure") < trace.indexOf("rust-failure"));
  assert.ok(trace.indexOf("guest") > trace.indexOf("build"));
  assert.ok(trace.includes("js-independent"));
  assert.ok(trace.includes("types-independent"));
  assert.deepEqual(
    result.steps.filter((x) => x.exitCode).map((x) => x.label),
    ["rust-failure", "js-failure"],
  );
  assert.ok(printed.indexOf("rust-failure:") < printed.indexOf("js-failure:"));
  assert.match(
    readFileSync(result.logPath, "utf8"),
    /rust-failure[\s\S]*js-failure/,
  );
});

test("failed preparation skips consumers while independent lanes still run", async () => {
  const trace: string[] = [];
  const result = await executeRecipe(
    process.cwd(),
    "test-fast",
    [
      fakeStep("wasm", "rust", { provides: "wasm" }),
      fakeStep("build", "rust", { requires: "wasm", provides: "workspace" }),
      fakeStep("web", "js", { requires: "wasm" }),
      fakeStep("bridge", "js"),
      fakeStep("guest", "guest", { requires: "workspace" }),
    ],
    {
      write: () => {},
      execute: async (_root, item) => {
        trace.push(item.name);
        return {
          status: item.name === "wasm" ? "failed" : "passed",
          exitCode: item.name === "wasm" ? 1 : 0,
          durationSeconds: 0,
        };
      },
    },
  );
  assert.deepEqual(trace, ["wasm", "bridge"]);
  assert.deepEqual(
    result.steps.map((x) => x.status),
    ["failed", "skipped", "skipped", "passed", "skipped"],
  );
});

test("interrupt waits for running children and cancels unstarted dependents", async () => {
  const controller = new AbortController();
  let stopped = false;
  const result = await executeRecipe(
    process.cwd(),
    "test-fast",
    [
      fakeStep("build", "rust", { provides: "workspace" }),
      fakeStep("interrupt", "js"),
      fakeStep("guest", "guest", { requires: "workspace" }),
    ],
    {
      signal: controller.signal,
      write: () => {},
      execute: async (_root, item, _case, _json, options) => {
        if (item.name === "build") {
          await new Promise((resolve) =>
            options.signal.addEventListener("abort", resolve, { once: true }),
          );
          stopped = true;
          return { status: "cancelled", exitCode: 143, durationSeconds: 0 };
        }
        controller.abort();
        return { status: "passed", exitCode: 0, durationSeconds: 0 };
      },
    },
  );
  assert.ok(stopped);
  const [, , cancelled] = result.steps;
  assert.ok(cancelled);
  assert.equal(cancelled.status, "cancelled");
  assert.equal(result.exitCode, 1);
});

test("the real HTTP web fixture does not overlap Rust witness execution", async () => {
  const steps = gateSteps(process.cwd(), "test-fast").filter((step) =>
    ["wasm", "renderer", "workspace", "rust-tests", "web unit tests"].includes(
      step.id ?? "",
    ),
  );
  const started = Promise.withResolvers<void>();
  const release = Promise.withResolvers<void>();
  const trace: (string | undefined)[] = [];
  const result = executeRecipe(process.cwd(), "test-fast", steps, {
    write: () => {},
    execute: async (_root, _item, _case, _json, options) => {
      if (options.label === "Rust nextest") {
        started.resolve();
        await release.promise;
        trace.push("Rust finished");
      }
      trace.push(options.label);
      return { status: "passed", exitCode: 0, durationSeconds: 0 };
    },
  });
  await started.promise;
  try {
    assert.ok(
      !trace.includes("web unit tests"),
      "the draft HTTP fixture must not compete with Rust witnesses",
    );
  } finally {
    release.resolve();
    await result;
  }
  assert.ok(trace.indexOf("Rust finished") < trace.indexOf("web unit tests"));
  for (const name of ["test-fast", "test-comprehensive", "test-fast-rest"]) {
    assert.equal(
      gateSteps(process.cwd(), name).find(
        (step) => step.id === "web unit tests",
      )!.lane,
      "rust",
    );
  }
  const nonRust = gateSteps(process.cwd(), "test-fast-rest-non-rust");
  assert.equal(
    nonRust.find((step) => step.id === "web unit tests")!.requires,
    "workspace",
  );
  assert.ok(nonRust.every((step) => !step.requiresCargo));
});

test(
  "owned process groups clean nested children after failure and cancellation",
  { skip: process.platform === "win32" },
  async () => {
    const directory = mkdtempSync(path.join(tmpdir(), "studio-gate-children-"));
    try {
      for (const cancel of [false, true]) {
        const controller = new AbortController();
        let pid: number | undefined;
        let captured = "";
        const source = `const {spawn}=require('node:child_process');
        const child=spawn(process.execPath,['-e','setInterval(()=>{},1000)'],{stdio:'ignore'});
        child.once('spawn',()=>{console.log('READY '+child.pid); ${cancel ? "setInterval(()=>{},1000)" : "process.exit(1)"}});`;
        const result = await runLogged(
          process.execPath,
          ["-e", source],
          directory,
          path.join(directory, cancel ? "cancel.log" : "fail.log"),
          process.env,
          {
            signal: controller.signal,
            timeoutMs: 10_000,
            onOutput: (text) => {
              captured += text;
              const match = captured.match(/READY (\d+)/);
              if (match) {
                pid = Number(match[1]);
                if (cancel) controller.abort();
              }
            },
          },
        );
        assert.ok(pid);
        assert.ok(result.code !== 0 || result.signal);
        // An exited orphan may briefly be a zombie until init reaps it; it cannot run.
        const state = spawnSync("ps", ["-o", "stat=", "-p", String(pid)], {
          encoding: "utf8",
        }).stdout.trim();
        assert.ok(
          !state || state.startsWith("Z"),
          `nested child ${pid} survived: ${state}`,
        );
      }
    } finally {
      rmSync(directory, { recursive: true, force: true });
    }
  },
);

test("Cargo compilation is confined to the fixed Rust lane and both tiers use one universe", () => {
  for (const name of ["test-fast", "test-comprehensive", "test-fast-rest"]) {
    const steps = gateSteps(process.cwd(), name);
    // Tools must start while type checks wait for WASM and web tests wait for Rust.
    const lane = (id: string) => steps.find((step) => step.id === id)!.lane;
    assert.notEqual(lane("tooling"), lane("TypeScript projects"));
    assert.notEqual(lane("tooling"), lane("web unit tests"));
    assert.notEqual(lane("TypeScript projects"), lane("web unit tests"));
    assert.ok(
      steps.filter((s) => s.requiresCargo).every((s) => s.lane === "rust"),
    );
    const providerContracts = steps.find((s) => s.id === "provider-contracts");
    assert.ok(
      providerContracts,
      "provider source mutations must run in both tiers",
    );
    assert.equal(providerContracts.lane, "rust");
    assert.equal(providerContracts.requiresCargo, true);
    assert.equal(providerContracts.requires, "workspace");
    assert.deepEqual(providerContracts.commands, [
      ["pnpm", ["--dir", process.cwd(), "run", "test:provider-contracts"]],
    ]);
    const rust = steps.find((s) => s.id === "rust-tests");
    const [rustCommand] = rust?.commands ?? [];
    assert.ok(rustCommand);
    assert.deepEqual(rustCommand[1].slice(0, 4), [
      "nextest",
      "run",
      "--workspace",
      "--profile",
    ]);
    assert.ok(!JSON.stringify(steps).includes("--release"));
    assert.equal(steps.find((s) => s.id === "guest")!.requires, "workspace");
    assert.equal(
      steps.find((s) => s.id === "web unit tests")!.requires,
      "workspace",
    );
    const [workspaceCommand] =
      steps.find((s) => s.id === "workspace")?.commands ?? [];
    assert.ok(workspaceCommand);
    assert.ok(workspaceCommand[1].includes("--examples"));
  }
  assert.ok(
    gateSteps(process.cwd(), "test-fast-rest-non-rust").every(
      (s) => !s.requiresCargo,
    ),
  );
});

test("profile contract parity runs in the short tier", () => {
  const step = gateSteps(process.cwd(), "test-fast").find(
    (item) => item.id === "profile-contract",
  );
  assert.deepEqual(step?.commands, [
    [
      "node",
      [
        "--test",
        "workspaces/client/apps/native/src-tauri/src/profile-contract.test.ts",
      ],
    ],
  ]);
});

test("Rust formatting is checked by the fast and comprehensive gates", () => {
  for (const name of ["test-fast", "test-comprehensive"]) {
    const step = gateSteps(process.cwd(), name).find(
      (item) => item.id === "rust-fmt",
    );
    assert.deepEqual(step?.commands, [
      ["cargo", ["fmt", "--all", "--", "--check"]],
    ]);
  }
});

test("compiler policy witnesses run after dependency preparation outside the JS lane", () => {
  for (const name of ["test-fast", "test-comprehensive", "test-fast-rest"]) {
    const steps = gateSteps(process.cwd(), name);
    const policy = steps.find((step) => step.id === "rust-lint-policy");
    assert.ok(policy);
    assert.equal(policy.lane, "rust");
    assert.equal(policy.requires, "workspace");
    assert.equal(policy.requiresCargo, true);
    assert.deepEqual(policy.commands, [
      ["pnpm", ["--dir", process.cwd(), "run", "test:rust-lint-policy"]],
    ]);
  }
  const nonRust = gateSteps(process.cwd(), "test-fast-rest-non-rust");
  assert.ok(nonRust.every((step) => step.id !== "rust-lint-policy"));
  assert.equal(
    nonRust.find((step) => step.id === "tooling")?.options?.script,
    "test:tooling:js",
  );
  const workflow = readFileSync(".github/workflows/ci.yml", "utf8");
  const rust = workflow.split("  rust:\n")[1]?.split("  rust-macos:\n")[0];
  assert.ok(rust);
  assert.match(rust, /run: pnpm run test:rust-lint-policy/);
  assert.ok(
    rust.indexOf("clippy-ratchet.ts") <
      rust.indexOf("run: pnpm run test:rust-lint-policy"),
  );
  const manifest: PackageManifest = JSON.parse(
    readFileSync("package.json", "utf8"),
  );
  const files = (script: string) => {
    const command = manifest.scripts?.[script];
    assert.ok(command, script);
    assert.ok(command.startsWith("node --test "), script);
    return command.slice("node --test ".length).split(/\s+/);
  };
  // test:tooling owns the complete list; its two gate consumers must partition it.
  const policyFiles = files("test:rust-lint-policy");
  assert.deepEqual(policyFiles, [
    "workspaces/tooling/apps/repository-checks/tests/rust-lint-policy.test.ts",
  ]);
  assert.deepEqual(
    files("test:tooling:js"),
    files("test:tooling").filter((file) => !policyFiles.includes(file)),
  );
  const tooling = discover(process.cwd()).find(
    (item) => item.name === "codex-studio-tooling",
  );
  assert.ok(tooling);
  assert.deepEqual(
    commandFor(process.cwd(), tooling, "test", "native Clippy"),
    [
      [
        "pnpm",
        [
          "--filter",
          tooling.name,
          "exec",
          "node",
          "--test",
          "--test-reporter=tap",
          "--test-name-pattern",
          "native Clippy",
          ...files("test:tooling"),
        ],
      ],
    ],
  );
});

test("archived CI partitions reuse prepared binaries without Cargo build options", () => {
  // just reports the real justfile directory, for example /private/var on macOS.
  const root = realpathSync(
    mkdtempSync(path.join(tmpdir(), "studio-gate-archive-")),
  );
  try {
    const owner = "workspaces/tooling/apps/repository-checks/gate-config.ts";
    mkdirSync(path.dirname(path.join(root, owner)), { recursive: true });
    writeFileSync(path.join(root, owner), readFileSync(owner));
    writeFileSync(path.join(root, "justfile"), readFileSync("justfile"));
    const tools = path.join(root, "tools");
    mkdirSync(tools);
    const capture = path.join(root, "capture.json");
    writeFileSync(
      path.join(tools, "cargo"),
      `#!${process.execPath}
require('node:fs').appendFileSync(process.env.CAPTURE, JSON.stringify({args: process.argv.slice(2), supervisor: process.env.STUDIO_SUPERVISOR_BINARY}));
`,
    );
    chmodSync(path.join(tools, "cargo"), 0o755);
    const result = spawnSync(
      "just",
      ["test-fast-rust-partition", "1", "4", "archive.tar.zst"],
      {
        cwd: root,
        env: {
          ...process.env,
          PATH: tools + path.delimiter + process.env.PATH,
          CAPTURE: capture,
        },
        encoding: "utf8",
        timeout: 10_000,
      },
    );
    assert.equal(result.status, 0, result.stderr);
    const recorded: { args: string[]; supervisor: string } = JSON.parse(
      readFileSync(capture, "utf8"),
    );
    assert.deepEqual(recorded.args, [
      "nextest",
      "run",
      "--archive-file",
      "archive.tar.zst",
      "--workspace-remap",
      ".",
      "--extract-to",
      ".",
      "--extract-overwrite",
      "--profile",
      "ci",
      "--partition",
      "hash:1/4",
    ]);
    assert.equal(
      recorded.supervisor,
      path.join(
        realpathSync(root),
        "target",
        "debug",
        "codex-process-supervisor",
      ),
    );
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});

test("hosted shards omit native binaries while the prepared builder runs them", () => {
  const workflow = readFileSync(".github/workflows/ci.yml", "utf8");
  const builder = workflow
    .split("  rust-build-tests:\n")[1]
    ?.split("  fast-rest:\n")[0];
  const shards = workflow
    .split("  fast-rust:\n")[1]
    ?.split("  fast-browser-smoke:\n")[0];
  assert.ok(builder);
  assert.ok(shards);
  assert.match(builder, /run: just test-native-ci/);
  assert.doesNotMatch(shards, /native-linux-deps|apt-get/);

  const nativePackage = /^name = "([^"]+)"/m.exec(
    readFileSync("workspaces/client/apps/native/src-tauri/Cargo.toml", "utf8"),
  )?.[1];
  assert.ok(nativePackage);
  const filter = `package(=${nativePackage})`;
  for (const [recipe, expected] of [
    [["test-native-ci"], `-E '${filter}'`],
    [["build-rust-test-archive", "archive.tar.zst"], `-E 'not ${filter}'`],
  ] as const) {
    const result = spawnSync("just", ["--dry-run", ...recipe], {
      encoding: "utf8",
      timeout: 10_000,
    });
    assert.equal(result.status, 0, result.stderr);
    assert.ok(result.stderr.includes(expected), result.stderr);
    assert.match(result.stderr, /--workspace --locked/);
    assert.doesNotMatch(result.stderr, /--exclude|--package| -p /);
  }
});

test("native dependency setup selects the Ubuntu archive before every apt operation", () => {
  const actionPath = path.resolve(".github/actions/native-linux-deps");
  const action = readFileSync(path.join(actionPath, "action.yml"), "utf8");
  const script = action.split("      run: |\n")[1];
  assert.ok(script);
  const directory = mkdtempSync(path.join(tmpdir(), "studio-apt-mirror-"));
  try {
    writeFileSync(
      path.join(directory, "apt-mirrors.txt"),
      "http://azure.archive.ubuntu.com/ubuntu/\tpriority:1\nhttps://archive.ubuntu.com/ubuntu/\tpriority:2\n",
    );
    // Run the real action with isolated filesystem and apt stand-ins. No sudo or
    // package manager on the test host is invoked.
    const result = spawnSync(
      "bash",
      [
        "-euc",
        `
        sudo() {
          if [ "$1" = install ]; then
            test "$2" = -m
            test "$3" = 644
            test "$5" = /etc/apt/apt-mirrors.txt
            command install -m "$3" "$4" "$MIRROR_CAPTURE"
          elif [ "$1" = apt-get ]; then
            test "$(cat "$MIRROR_CAPTURE")" = https://archive.ubuntu.com/ubuntu/
            printf '%s\\n' "$*" >> "$APT_CAPTURE"
          else
            return 1
          fi
        }
        ${script}
        `,
      ],
      {
        encoding: "utf8",
        timeout: 10_000,
        env: {
          ...process.env,
          GITHUB_ACTION_PATH: actionPath,
          MIRROR_CAPTURE: path.join(directory, "apt-mirrors.txt"),
          APT_CAPTURE: path.join(directory, "apt.log"),
        },
      },
    );
    assert.equal(result.status, 0, result.stderr);
    assert.deepEqual(
      readFileSync(path.join(directory, "apt.log"), "utf8").trim().split("\n"),
      [
        "apt-get update",
        "apt-get install -y --no-install-recommends libwebkit2gtk-4.1-dev libayatana-appindicator3-dev librsvg2-dev patchelf",
      ],
    );
  } finally {
    rmSync(directory, { recursive: true, force: true });
  }
});

test("native platform filter keeps every common clause identical", () => {
  const source = readFileSync(".config/nextest.toml", "utf8");
  const [common, linux] = [
    ...source.matchAll(/default-filter = '''\n([\s\S]*?)\n'''/g),
  ].map((match) => match[1]);
  assert.ok(linux!.startsWith(common + "\n    | "));
  assert.match(source, /platform = 'cfg\(target_os = "linux"\)'/);
  assert.match(source, /inherits = "ci"/);
});

test("only the four exact Git process fixtures reserve all fast runner slots", () => {
  const source = readFileSync(".config/nextest.toml", "utf8");
  const expectedFilter =
    "package(=studio-runtime-services) & kind(lib) & (test(=agent_management_store::archive_finished_tests::archive_finished_removes_accepted_worktree_and_target_and_keeps_dirty_work) | test(=task_gate::tests::acceptance_gate_command_checks_revision_source_changes_and_exit_code) | test(=bounded_child::tests::fixture_git_config_does_not_read_the_supplied_hostile_global_file) | test(=bounded_child::tests::fixture_git_config_isolates_nested_git_and_preserves_other_request_inputs))";
  const reservations = [
    ...source.matchAll(
      /\[\[profile\.([^\].]+)\.overrides\]\]\n([\s\S]*?)(?=\n\[|$)/g,
    ),
  ]
    .map((match) => {
      const [, profile, body] = match;
      if (profile === undefined || body === undefined)
        throw new Error("expected a profile override with a body");
      return { profile, body };
    })
    .filter(({ body }) => /^threads-required\s*=/m.test(body))
    .map(({ profile, body }) => ({
      profile,
      filter: body.match(/^filter = '([^']+)'$/m)?.[1],
      slots: body.match(/^threads-required = "([^"]+)"$/m)?.[1],
      settings: [...body.matchAll(/^([a-z-]+)\s*=/gm)].map(
        (setting) => setting[1],
      ),
    }));
  assert.deepEqual(reservations, [
    {
      profile: "fast",
      filter: expectedFilter,
      slots: "num-test-threads",
      settings: ["filter", "threads-required"],
    },
  ]);
  assert.equal(
    [...source.matchAll(/^threads-required\s*=/gm)].length,
    1,
    "the reservation must not become a profile-wide default",
  );
  assert.doesNotMatch(source, /^test-threads\s*=\s*1\s*$/m);
});

test("non-Cargo output may quote nextest installation guidance without failing the gate", async () => {
  const { execute } = await import("../facade.ts");
  const output =
    "cargo-nextest 0.9.146 is required. Install it with: cargo install cargo-nextest --version 0.9.146 --locked\nTAP version 13\n# tests 1\n# pass 1\n# fail 0\n# cancelled 0\n# skipped 0\n";
  const result = await execute(
    process.cwd(),
    { name: "tooling fixture", ecosystem: "pnpm" },
    "",
    false,
    {
      commands: [
        ["node", ["-e", `process.stdout.write(${JSON.stringify(output)})`]],
      ],
      write: () => {},
    },
  );
  assert.equal(result.exitCode, 0);
  assert.equal(result.executed, 1);
});
