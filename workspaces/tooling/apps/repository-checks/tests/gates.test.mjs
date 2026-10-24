import test from "node:test";
import assert from "node:assert/strict";
import { recipePlan } from "../facade.mjs";

test("fast gate has one workspace Rust run and prepares the native renderer once", () => {
  const command = recipePlan(process.cwd(), "test-fast").command;
  assert.equal((command.match(/'nextest' 'run'/g) ?? []).length, 1);
  assert.match(command, /--workspace/);
  assert.equal((command.match(/build:wasm/g) ?? []).length, 1);
  assert.equal((command.match(/build:bundle:prepared/g) ?? []).length, 1);
});

test("non-Rust remainder has no compiler, WASM generator, Clippy, or nested cargo check", () => {
  const command = recipePlan(process.cwd(), "test-fast-rest-non-rust").command;
  assert.doesNotMatch(
    command,
    /'cargo'|clippy-runtime|build-client-wasm|contracts-check|build:bundle/,
  );
});

import { executeRecipe, gateSteps } from "../facade.mjs";
import { runLogged } from "../owned-process.mjs";
import {
  mkdtempSync,
  writeFileSync,
  readFileSync,
  mkdirSync,
  rmSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { spawnSync } from "node:child_process";
import path from "node:path";

const fakeStep = (id, lane, extra = {}) => ({
  id,
  label: id,
  lane,
  item: { name: id, ecosystem: "pnpm" },
  ...extra,
});

test("bridge config removes ambient agent settings before workers inherit them", () => {
  const result = spawnSync(
    process.execPath,
    [
      "--input-type=module",
      "-e",
      `
    await import(${JSON.stringify(new URL("../../../../providers/apps/claude-bridge/vitest.config.mjs", import.meta.url).href)});
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
  const { execute } = await import("../facade.mjs");
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
  const trace = [];
  const releaseBuild = Promise.withResolvers();
  const steps = [
    fakeStep("build", "rust", { provides: "workspace" }),
    fakeStep("rust-failure", "rust", { requires: "workspace" }),
    fakeStep("js-failure", "js"),
    fakeStep("js-independent", "js"),
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
  const trace = [];
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
  assert.equal(result.steps[2].status, "cancelled");
  assert.equal(result.exitCode, 1);
});

test(
  "owned process groups clean nested children after failure and cancellation",
  { skip: process.platform === "win32" },
  async () => {
    const directory = mkdtempSync(path.join(tmpdir(), "studio-gate-children-"));
    try {
      for (const cancel of [false, true]) {
        const controller = new AbortController();
        let pid;
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
                pid = +match[1];
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
    assert.ok(
      steps.filter((s) => s.requiresCargo).every((s) => s.lane === "rust"),
    );
    const rust = steps.find((s) => s.id === "rust-tests");
    assert.deepEqual(rust.commands[0][1].slice(0, 4), [
      "nextest",
      "run",
      "--workspace",
      "--profile",
    ]);
    assert.ok(!JSON.stringify(steps).includes("--release"));
    assert.equal(steps.find((s) => s.id === "guest").requires, "workspace");
    assert.equal(
      steps.find((s) => s.id === "web unit tests").requires,
      "workspace",
    );
    assert.ok(
      steps
        .find((s) => s.id === "workspace")
        .commands[0][1].includes("--examples"),
    );
  }
  assert.ok(
    gateSteps(process.cwd(), "test-fast-rest-non-rust").every(
      (s) => !s.requiresCargo,
    ),
  );
});

test("native platform filter keeps every common clause identical", () => {
  const source = readFileSync(".config/nextest.toml", "utf8");
  const [common, linux] = [
    ...source.matchAll(/default-filter = '''\n([\s\S]*?)\n'''/g),
  ].map((match) => match[1]);
  assert.ok(linux.startsWith(common + "\n    | "));
  assert.match(source, /platform = 'cfg\(target_os = "linux"\)'/);
  assert.match(source, /inherits = "ci"/);
});

test("non-Cargo output may quote nextest installation guidance without failing the gate", async () => {
  const { execute } = await import("../facade.mjs");
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
