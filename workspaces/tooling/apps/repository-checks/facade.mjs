#!/usr/bin/env node
import { spawn, spawnSync } from "node:child_process";
import { builtinModules, createRequire } from "node:module";
import {
  appendFileSync,
  existsSync,
  mkdirSync,
  mkdtempSync,
  readFileSync,
  readdirSync,
  rmSync,
  statSync,
} from "node:fs";
import { homedir } from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";

const MIN_JUST = [1, 58, 0];
const COST = {
  pnpm: "focused; package test only",
  cargo: "focused; package compile and test",
  server: "focused; isolated Python suite runner",
};
const NOT_STARTED = "backend, browser, Electron, provider";
const LOG_RETENTION_MS = 14 * 24 * 60 * 60 * 1000;
const SOURCE_EXTENSIONS = /\.(?:[cm]?js|[cm]?ts|jsx|tsx)$/;
const SOURCE_SKIP = new Set([
  ".git",
  "node_modules",
  "dist",
  "target",
  "coverage",
]);
const DECLARATION_SECTIONS = [
  "dependencies",
  "devDependencies",
  "peerDependencies",
  "optionalDependencies",
];
const BOUNDARY_ALLOWLIST = [
  {
    importer: "codex-agents-desktop",
    file: "bridge.d.ts",
    specifier: "../web/src/servers/desktopCredentials",
    reason:
      "The desktop bridge declaration shares the renderer's credential wire types until they have a public package owner.",
  },
  {
    importer: "codex-agents-desktop",
    file: "server-access-native.mjs",
    specifier: "../web/tests/servers/multi-server-fixture.mjs",
    reason:
      "The native desktop integration scenario reuses the renderer's local multi-server test fixture.",
  },
  {
    importer: "codex-agents-desktop",
    file: "test.mjs",
    specifier: "../web/tests/playwright.mjs",
    reason:
      "The native desktop scenario reuses the renderer's browser test-state helper.",
  },
  {
    importer: "codex-agents-desktop",
    file: "multi-server-memory.mjs",
    specifier: "../web/tests/servers/multi-server-fixture.mjs",
    reason:
      "A desktop integration scenario reuses the renderer's local multi-server test fixture.",
  },
  {
    importer: "codex-agents-desktop",
    file: "server-frames-native.mjs",
    specifier: "../web/tests/servers/multi-server-fixture.mjs",
    reason:
      "A desktop integration scenario reuses the renderer's local multi-server test fixture.",
  },
  {
    importer: "codex-agents-web",
    file: "tests/**",
    specifier: "../../../../../runtime/apps/server/tests/**",
    reason:
      "Renderer browser tests reuse local setup fixtures maintained with the server test runner.",
  },
  {
    importer: "server",
    file: "tests/claude-*-contract.mjs",
    specifier: "../../../../providers/apps/claude-bridge/**",
    reason:
      "Server bridge contract tests import the actual bridge implementation under test.",
  },
  {
    importer: "server",
    file: "tests/fixtures/ux-*.tsx",
    specifier: "../../../../../client/apps/web/**",
    reason:
      "Server-owned visual fixtures reuse renderer components and styles for isolated UX validation.",
  },
];

function fail(message) {
  throw new Error(message);
}

function rootFrom(start = process.cwd()) {
  if (process.env.CODEX_STUDIO_CHECKS_ROOT)
    return path.resolve(process.env.CODEX_STUDIO_CHECKS_ROOT);
  const result = spawnSync("git", ["rev-parse", "--show-toplevel"], {
    cwd: start,
    encoding: "utf8",
  });
  if (result.status === 0) return result.stdout.trim();
  let current = path.resolve(start);
  while (current !== path.dirname(current)) {
    if (exists(path.join(current, "pnpm-workspace.yaml"))) return current;
    current = path.dirname(current);
  }
  fail(`Cannot locate repository root from ${start}`);
}

function exists(file) {
  try {
    readFileSync(file);
    return true;
  } catch {
    return false;
  }
}

function justVersion() {
  const output = spawnSync("just", ["--version"], { encoding: "utf8" });
  if (output.status !== 0)
    fail(
      "Just is required. Install Just >= 1.58.0; no tools will be installed by this command.",
    );
  const match = output.stdout.match(/just (\d+)\.(\d+)\.(\d+)/);
  if (!match || compareVersions(match.slice(1).map(Number), MIN_JUST) < 0) {
    fail(
      `Just >= 1.58.0 is required; found ${output.stdout.trim() || "unknown version"}.`,
    );
  }
}

function compareVersions(a, b) {
  for (let i = 0; i < b.length; i += 1) if (a[i] !== b[i]) return a[i] - b[i];
  return 0;
}

function runSync(command, args, cwd, label) {
  const result = spawnSync(command, args, {
    cwd,
    encoding: "utf8",
    maxBuffer: 16 * 1024 * 1024,
  });
  if (result.status !== 0)
    fail(
      `${label} failed (exit ${result.status ?? result.signal}): ${(result.stderr || result.stdout).trim()}`,
    );
  return result.stdout;
}

export function discover(root = rootFrom()) {
  const packages = new Map();
  const pnpm = JSON.parse(
    runSync(
      "pnpm",
      ["-r", "list", "--depth", "-1", "--json"],
      root,
      "pnpm workspace discovery",
    ),
  );
  for (const item of pnpm) {
    if (!item.name || !item.path) continue;
    const dir = path.relative(root, item.path).split(path.sep).join("/") || ".";
    packages.set(item.name, {
      name: item.name,
      ecosystem: "pnpm",
      dir,
      manifest: path.join(item.path, "package.json"),
    });
  }
  const cargoFile = path.join(root, "Cargo.toml");
  if (exists(cargoFile)) {
    assertPinnedCargoToolchain(root);
    const metadata = JSON.parse(
      runSync(
        "cargo",
        ["metadata", "--no-deps", "--format-version", "1"],
        root,
        "Cargo workspace discovery",
      ),
    );
    for (const item of metadata.packages ?? []) {
      const relativeManifest = path
        .relative(root, item.manifest_path)
        .split(path.sep)
        .join("/");
      const dir = relativeManifest.replace(/(?:^|\/)Cargo\.toml$/, "") || ".";
      packages.set(item.name, {
        name: item.name,
        ecosystem: "cargo",
        dir,
        manifest: item.manifest_path,
      });
    }
  }
  const serverDir = "workspaces/runtime/apps/server";
  packages.set("server", {
    name: "server",
    ecosystem: "server",
    dir: serverDir,
    manifest: null,
  });
  return [...packages.values()].sort((a, b) => a.name.localeCompare(b.name));
}

function assertPinnedCargoToolchain(root) {
  const configPath = path.join(root, "rust-toolchain.toml");
  if (!exists(configPath)) return;
  const channel = readFileSync(configPath, "utf8").match(
    /^\s*channel\s*=\s*"([^"]+)"/m,
  )?.[1];
  if (!channel) return;
  let installed;
  try {
    installed = runSync(
      "rustup",
      ["toolchain", "list"],
      root,
      "Rust toolchain check",
    );
  } catch {
    fail(
      `Pinned Rust toolchain ${channel} must already be installed; Just will not install tools.`,
    );
  }
  if (
    !installed
      .split("\n")
      .some(
        (line) =>
          line.startsWith(`${channel}-`) ||
          line === channel ||
          line.startsWith(`${channel} `),
      )
  )
    fail(
      `Pinned Rust toolchain ${channel} must already be installed; Just will not install tools.`,
    );
}

function target(root, pkg) {
  const entry = discover(root);
  const found = entry.find((item) => item.name === pkg);
  if (!found)
    fail(
      `Unknown package "${pkg}". Valid packages: ${entry.map((item) => item.name).join(", ")}`,
    );
  return found;
}

function manifestFor(item) {
  return item.manifest ? JSON.parse(readFileSync(item.manifest, "utf8")) : {};
}

function pnpmTestCommand(item, caseName) {
  const manifest = manifestFor(item);
  const script = manifest.codexStudioChecks?.test;
  if (!script)
    fail(
      `Package ${item.name} must declare codexStudioChecks.test in ${item.manifest}`,
    );
  if (!manifest.scripts?.[script])
    fail(`Package ${item.name} declares missing test script ${script}`);
  const scriptText = manifest.scripts[script];
  const nodeTest = scriptText.match(/node --test(?:\s+(.*))?/);
  if (nodeTest && caseName) {
    const args = [
      "--filter",
      item.name,
      "exec",
      "node",
      "--test",
      "--test-reporter=tap",
    ];
    const isFile =
      /\.(?:test|spec)\.[cm]?[jt]sx?$/.test(caseName) ||
      caseName.includes("/") ||
      caseName.includes("\\");
    if (!isFile) args.push("--test-name-pattern", caseName);
    args.push(...(isFile ? [caseName] : (nodeTest[1]?.split(/\s+/) ?? [])));
    return [["pnpm", args]];
  }
  if (/\bvitest\b/.test(scriptText)) {
    const config = scriptText.match(/--config(?:=|\s+)([^\s]+)/)?.[1];
    const args = ["--filter", item.name, "exec", "vitest", "run"];
    if (config) args.push("--config", config);
    if (caseName) args.push(caseName);
    return [["pnpm", args]];
  }
  if (caseName && !/\bvitest\b/.test(scriptText) && !nodeTest)
    fail(
      `Package ${item.name} does not expose a native Vitest filter through its test script`,
    );
  const args = ["--filter", item.name, "run", script];
  return [["pnpm", args]];
}

export function commandFor(root, item, action, caseName = "", options = {}) {
  if (item.ecosystem === "pnpm") {
    if (action === "test") {
      const commands = pnpmTestCommand(item, caseName);
      if (options.name) {
        const args = commands[0][1];
        if (!args.includes("vitest"))
          fail("--name is supported only for Vitest packages");
        args.push("-t", options.name);
      }
      return commands;
    }
    const manifest = manifestFor(item);
    const scripts = manifest.codexStudioChecks?.check;
    if (!Array.isArray(scripts) || scripts.length === 0)
      fail(
        `Package ${item.name} must declare a non-empty codexStudioChecks.check array in ${item.manifest}`,
      );
    return scripts.map((script) => {
      if (!manifest.scripts?.[script])
        fail(`Package ${item.name} declares missing check script ${script}`);
      return ["pnpm", ["--filter", item.name, "run", script]];
    });
  }
  if (item.ecosystem === "cargo") {
    if (action === "test")
      return [
        [
          "cargo",
          [
            "test",
            "-p",
            item.name,
            ...(options.cargoTargets ?? []),
            ...(caseName ? [caseName] : []),
            ...(options.passthrough?.length
              ? ["--", ...options.passthrough]
              : []),
          ],
        ],
      ];
    return [
      ["cargo", ["fmt", "--check", "-p", item.name]],
      [
        "cargo",
        ["clippy", "-p", item.name, "--all-targets", "--", "-D", "warnings"],
      ],
      ["cargo", ["test", "-p", item.name]],
    ];
  }
  if (item.name === "server") {
    const python = path.join(
      root,
      "workspaces/runtime/apps/server/src/codex_python.py",
    );
    if (action === "test")
      return [
        [
          "python3",
          [
            python,
            "--exec",
            path.join(
              root,
              "workspaces/runtime/apps/server/src/test-server.py",
            ),
            ...(caseName ? ["--filter", caseName] : []),
          ],
        ],
      ];
    return [["python3", [python, "--mypy"]]];
  }
  fail(`Unsupported package ecosystem: ${item.ecosystem}`);
}

function shellQuote(value) {
  return `'${String(value).replaceAll("'", "'\\''")}'`;
}

function reproduction(root, commands) {
  return commands
    .map(([bin, args]) => {
      const targetEnv =
        bin === "cargo"
          ? `CARGO_TARGET_DIR=${shellQuote(cargoTarget(root))} `
          : "";
      return `cd ${shellQuote(root)} && ${targetEnv}${[bin, ...args].map(shellQuote).join(" ")}`;
    })
    .join(" && ");
}

function cargoTarget(root) {
  return process.env.CARGO_TARGET_DIR || path.join(root, "target");
}

function cacheDirectory() {
  return path.resolve(
    process.env.CODEX_STUDIO_CHECKS_CACHE ||
      path.join(
        process.env.XDG_CACHE_HOME || path.join(homedir(), ".cache"),
        "codex-studio",
        "checks",
      ),
  );
}

export function pruneLogs(logRoot, now = Date.now()) {
  mkdirSync(logRoot, { recursive: true });
  for (const entry of readdirSync(logRoot)) {
    if (!entry.startsWith("run-")) continue;
    const fullPath = path.join(logRoot, entry);
    try {
      if (now - statSync(fullPath).mtimeMs > LOG_RETENTION_MS)
        rmSync(fullPath, { recursive: true, force: true });
    } catch {
      // A concurrent cleanup or non-directory entry does not block this run.
    }
  }
}

function makeLogDirectory() {
  const root = cacheDirectory();
  pruneLogs(root);
  return mkdtempSync(path.join(root, `run-${Date.now()}-`));
}

function plan(root, item, action, caseName, commands, options = {}) {
  const workingDirectory = root;
  const setup =
    item.ecosystem === "pnpm"
      ? "Pinned pnpm and workspace dependencies installed"
      : item.ecosystem === "cargo"
        ? `Pinned Rust toolchain and required crates available; target output uses ${cargoTarget(root)}`
        : "Managed Python runtime and server development dependencies prepared";
  return {
    package: item.name,
    case: caseName || null,
    target:
      action === "check"
        ? `${item.ecosystem} package checks for ${item.name}`
        : item.ecosystem === "pnpm"
          ? "package test script (Vitest for codex-agents-web)"
          : item.ecosystem === "cargo"
            ? `Cargo package ${item.name}`
            : "Python server runner suite filter",
    workingDirectory,
    invocationDirectory: root,
    command: reproduction(root, commands),
    selector:
      action === "check"
        ? "mapped package checks (no test filter)"
        : selectorDescription(item, caseName, options),
    requiredSetup: setup,
    reason:
      item.ecosystem === "pnpm"
        ? "Run only the selected package's manifest-owned test or check command."
        : item.ecosystem === "cargo"
          ? "Compile and run only the selected Cargo package and its declared dependencies."
          : "Pass the selected suite filter to the server runner, which owns discovery and isolation.",
    notStarted: NOT_STARTED,
    costCategory:
      action === "check"
        ? item.ecosystem === "pnpm"
          ? "package-local type and unit checks"
          : item.ecosystem === "cargo"
            ? "package-local fmt, clippy, and unit checks"
            : "server runtime type check"
        : COST[item.ecosystem],
    executesNothing: true,
  };
}

function selectorDescription(item, caseName, options) {
  if (item.ecosystem === "cargo")
    return `Cargo test-name substring${options.cargoTargets?.length ? `; Cargo target selection: ${options.cargoTargets.join(" ")}` : ""}${options.passthrough?.length ? `; test harness args after --: ${options.passthrough.join(" ")}` : ""}`;
  if (item.ecosystem === "server")
    return "server runner --filter suite substring";
  const manifest = manifestFor(item);
  const script = manifest.scripts?.[manifest.codexStudioChecks?.test] ?? "";
  if (/\bvitest\b/.test(script))
    return `${caseName ? `Vitest positional file-path substring: ${caseName}` : "none (package default)"}${options.name ? `; Vitest -t name substring: ${options.name}` : ""}`;
  return caseName
    ? `Node native name filter: ${caseName}`
    : "none (package default)";
}

function reportPlan(value, json) {
  if (json) process.stdout.write(`${JSON.stringify(value)}\n`);
  else
    process.stdout.write(
      [
        `Target: ${value.target}${value.case ? ` (${value.case})` : ""}`,
        `Working directory: ${value.workingDirectory}`,
        `Invocation directory: ${value.invocationDirectory}`,
        `Command: ${value.command}`,
        `Selector: ${value.selector}`,
        `Required setup: ${value.requiredSetup}`,
        `Not started: ${value.notStarted}`,
        `Cost: ${value.costCategory}`,
        "Execution: none (plan only)",
      ].join("\n") + "\n",
    );
}

export function parseCounts(item, output) {
  if (item.ecosystem === "cargo") {
    const matches = [
      ...output.matchAll(
        /test result: (?:ok|FAILED)\. (\d+) passed; (\d+) failed; (\d+) ignored; (\d+) measured; (\d+) filtered out/g,
      ),
    ];
    if (!matches.length)
      return {
        passed: 0,
        failed: 0,
        skipped: 0,
        executed: 0,
        summaryFound: false,
      };
    const rows = matches.map((match) => ({
      passed: +match[1],
      failed: +match[2],
      skipped: +match[3],
      executed: +match[1] + +match[2],
    }));
    const result = rows.reduce(
      (sum, row) =>
        Object.fromEntries(
          Object.keys(row).map((key) => [key, (sum[key] ?? 0) + row[key]]),
        ),
      { summaryFound: true },
    );
    result.summaryFound = true;
    return result;
  }
  if (item.ecosystem === "server") {
    const suite = output.match(
      /Server suites: (\d+) passed, (\d+) failed, (\d+) skipped \(opt-in\), (\d+) skipped \(environment\)/,
    );
    const testOutcomes = output.match(/Test outcomes: (\{[^\n]*\})/);
    const outcomes = testOutcomes ? JSON.parse(testOutcomes[1]) : {};
    const passed = (outcomes.passed ?? 0) + (suite ? +suite[1] : 0);
    const failed =
      (outcomes.failed ?? 0) + (outcomes.error ?? 0) + (suite ? +suite[2] : 0);
    const skipped =
      (outcomes.skipped ?? 0) + (suite ? +suite[3] + +suite[4] : 0);
    return {
      passed,
      failed,
      skipped,
      executed: passed + failed,
      summaryFound: Boolean(suite),
    };
  }
  const testLines = output
    .split("\n")
    .filter((line) => /^\s*Tests\s+/.test(line));
  if (testLines.length) {
    const rows = testLines.map((line) => ({
      passed: +(line.match(/(\d+) passed/)?.[1] ?? 0),
      failed: +(line.match(/(\d+) failed/)?.[1] ?? 0),
      skipped: +(line.match(/(\d+) skipped/)?.[1] ?? 0),
    }));
    const result = rows.reduce(
      (sum, row) => ({
        passed: sum.passed + row.passed,
        failed: sum.failed + row.failed,
        skipped: sum.skipped + row.skipped,
        executed: sum.executed + row.passed + row.failed,
      }),
      { passed: 0, failed: 0, skipped: 0, executed: 0, summaryFound: true },
    );
    result.summaryFound = true;
    return result;
  }
  if (/^TAP version \d+\n1\.\.0$/m.test(output))
    return {
      passed: 0,
      failed: 0,
      skipped: 0,
      executed: 0,
      summaryFound: true,
    };
  const tap = output.match(
    /^# tests (\d+)\n(?:# suites \d+\n)?# pass (\d+)\n# fail (\d+)\n# cancelled (\d+)\n# skipped (\d+)/m,
  );
  if (tap)
    return {
      passed: +tap[2],
      failed: +tap[3],
      skipped: +tap[5],
      executed: +tap[2] + +tap[3],
      summaryFound: true,
    };
  const nodeSpec = output.match(
    /^ℹ tests (\d+)\nℹ suites \d+\nℹ pass (\d+)\nℹ fail (\d+)\nℹ cancelled \d+\nℹ skipped (\d+)/m,
  );
  if (nodeSpec)
    return {
      passed: +nodeSpec[2],
      failed: +nodeSpec[3],
      skipped: +nodeSpec[4],
      executed: +nodeSpec[2] + +nodeSpec[3],
      summaryFound: true,
    };
  return { passed: 0, failed: 0, skipped: 0, executed: 0, summaryFound: false };
}

function runLogged(command, args, cwd, log, environment = process.env) {
  return new Promise((resolve) => {
    const child = spawn(command, args, {
      cwd,
      stdio: ["inherit", "pipe", "pipe"],
      env: environment,
    });
    let output = "";
    let interrupted = null;
    const capture = (chunk) => {
      output += chunk.toString();
      appendFileSync(log, chunk);
    };
    child.stdout.on("data", capture);
    child.stderr.on("data", capture);
    const forward = (signal) => {
      interrupted = signal;
      child.kill(signal);
    };
    const handlers = new Map(
      ["SIGINT", "SIGTERM", "SIGHUP"].map((signal) => {
        const handler = () => forward(signal);
        process.once(signal, handler);
        return [signal, handler];
      }),
    );
    child.on("error", (error) =>
      resolve({
        code: 127,
        signal: null,
        output: `${output}\n${error.message}`,
      }),
    );
    child.on("close", (code, signal) => {
      for (const [name, handler] of handlers)
        process.removeListener(name, handler);
      resolve({ code, signal: signal || interrupted, output });
    });
  });
}

export async function execute(
  root,
  item,
  caseName,
  asJson = false,
  options = {},
) {
  const commands = commandFor(root, item, "test", caseName, options);
  const run = plan(root, item, "test", caseName, commands, options);
  const logDir = makeLogDirectory();
  const logPath = path.join(logDir, "full.log");
  mkdirSync(logDir, { recursive: true });
  appendFileSync(logPath, "");
  const started = performance.now();
  let counts = { passed: null, failed: null, skipped: null, executed: null };
  let exitCode = 0;
  let signal = null;
  let output = "";
  for (const [bin, args] of commands) {
    const env =
      bin === "cargo"
        ? { ...process.env, CARGO_TARGET_DIR: cargoTarget(root) }
        : process.env;
    const child = await runLogged(bin, args, root, logPath, env);
    output += child.output;
    if (child.signal) {
      signal = child.signal;
      exitCode =
        128 +
        (child.signal === "SIGINT" ? 2 : child.signal === "SIGTERM" ? 15 : 1);
      break;
    }
    if (child.code !== 0) {
      exitCode = child.code ?? 1;
      break;
    }
  }
  const durationSeconds = Number(
    ((performance.now() - started) / 1000).toFixed(3),
  );
  counts = parseCounts(item, output);
  const errors = [];
  if (exitCode === 0 && item.ecosystem !== "pnpm" && !counts.summaryFound)
    errors.push(
      "native runner summary was missing; refusing to report success",
    );
  if (exitCode === 0 && counts.executed === 0)
    errors.push("native runner executed zero tests");
  if (errors.length) exitCode = 1;
  const result = {
    package: item.name,
    case: caseName || null,
    status:
      exitCode === 0 ? "passed" : signal ? `signaled:${signal}` : "failed",
    ...counts,
    durationSeconds,
    command: run.command,
    workingDirectory: run.workingDirectory,
    invocationDirectory: root,
    logPath,
    requiredSetup: run.requiredSetup,
    costCategory: run.costCategory,
    notStarted: run.notStarted,
    exitCode,
    ...(signal ? { signal } : {}),
    ...(errors.length ? { errors } : {}),
  };
  if (asJson) process.stdout.write(`${JSON.stringify(result)}\n`);
  else
    process.stdout.write(
      `${item.name}${caseName ? ` / ${caseName}` : ""}: ${result.status}${counts.passed === null ? "" : ` — ${counts.passed} passed, ${counts.failed} failed, ${counts.skipped} skipped`}${counts.executed === 0 ? " — zero tests executed" : ""}; ${durationSeconds}s\nCommand: ${run.command}\nFull log: ${logPath}\n`,
    );
  return result;
}

function usage() {
  process.stdout.write(
    "Usage: facade.mjs packages | test <package> [file-or-case] [--name <pattern>] [-- <cargo args>] [--json] | test-plan <package> [file-or-case] [--name <pattern>] [-- <cargo args>] [--json] | check <package> [--dry-run] [--json] | contracts-check | boundaries-check | check-affected <base> [--dry-run]\n",
  );
}

export function parseSelection(requestedCase, rest) {
  let name = "";
  let passthrough = [];
  let cargoTargets = [];
  const flags = [];
  for (let index = 0; index < rest.length; index += 1) {
    const arg = rest[index];
    if (arg === "--") {
      const forwarded = rest
        .slice(index + 1)
        .filter((value) => value !== "--json");
      for (let cursor = 0; cursor < forwarded.length; cursor += 1) {
        const option = forwarded[cursor];
        if (["--test", "--bin", "--example", "--bench"].includes(option)) {
          if (!forwarded[cursor + 1]) fail(`${option} requires a target name`);
          cargoTargets.push(option, forwarded[cursor + 1]);
          cursor += 1;
        } else if (
          [
            "--lib",
            "--bins",
            "--examples",
            "--benches",
            "--all-targets",
          ].includes(option)
        ) {
          cargoTargets.push(option);
        } else passthrough.push(option);
      }
      break;
    }
    if (arg === "--name") {
      const values = [];
      while (
        rest[index + 1] &&
        !["--", "--json", "--dry-run", "--name"].includes(rest[index + 1])
      ) {
        values.push(rest[index + 1]);
        index += 1;
      }
      name = values.join(" ");
      if (!name) fail("--name requires a pattern");
    } else if (arg !== "--json" && arg !== "--dry-run") flags.push(arg);
  }
  return {
    caseName: [requestedCase, ...flags].filter(Boolean).join(" "),
    options: { name, passthrough, cargoTargets },
  };
}

function workspaceFiles(root) {
  const files = [];
  function visit(directory) {
    for (const entry of readdirSync(directory, { withFileTypes: true })) {
      if (entry.isDirectory() && SOURCE_SKIP.has(entry.name)) continue;
      const full = path.join(directory, entry.name);
      if (entry.isDirectory()) visit(full);
      else if (SOURCE_EXTENSIONS.test(entry.name)) files.push(full);
    }
  }
  const workspaces = path.join(root, "workspaces");
  if (existsSync(workspaces)) visit(workspaces);
  return files;
}

function packageImports(root, file, source) {
  try {
    const require = createRequire(import.meta.url);
    const ts = require(require.resolve("typescript"));
    const kind = /\.[cm]?tsx?$/.test(file)
      ? ts.ScriptKind.TS
      : ts.ScriptKind.JS;
    const script = ts.createSourceFile(
      file,
      source,
      ts.ScriptTarget.Latest,
      true,
      kind,
    );
    const specs = [];
    function visit(node) {
      if (
        (ts.isImportDeclaration(node) || ts.isExportDeclaration(node)) &&
        ts.isStringLiteral(node.moduleSpecifier)
      )
        specs.push(node.moduleSpecifier.text);
      if (
        ts.isCallExpression(node) &&
        node.arguments.length &&
        ts.isStringLiteral(node.arguments[0])
      ) {
        const expression = node.expression;
        const name = ts.isIdentifier(expression)
          ? expression.text
          : ts.isPropertyAccessExpression(expression)
            ? expression.name.text
            : "";
        if (
          name === "require" ||
          name === "import" ||
          (name === "resolve" &&
            expression.getText(script) === "require.resolve")
        )
          specs.push(node.arguments[0].text);
      }
      ts.forEachChild(node, visit);
    }
    visit(script);
    return specs;
  } catch {
    const specs = [];
    const pattern =
      /\b(?:from\s*|import\s*\(|import\s+|export\s+[^;]*?\sfrom\s*|require\s*\()\s*["']([^"']+)["']/g;
    for (const match of source.matchAll(pattern)) specs.push(match[1]);
    return specs;
  }
}

function packageRootFor(file, packages) {
  const workspacePackage = packages
    .filter((item) => item.dir !== ".")
    .filter((item) => {
      const dir = path.resolve(item.root, item.dir);
      return file === dir || file.startsWith(`${dir}${path.sep}`);
    })
    .sort((a, b) => b.dir.length - a.dir.length)[0];
  if (workspacePackage) return workspacePackage;
  const tooling = packages.find((item) => item.name === "codex-studio-tooling");
  const relative =
    tooling && path.relative(tooling.root, file).split(path.sep).join("/");
  return relative?.startsWith("workspaces/tooling/apps/repository-checks/")
    ? tooling
    : undefined;
}

function packageName(specifier) {
  if (
    !specifier ||
    specifier.startsWith(".") ||
    specifier.startsWith("/") ||
    specifier.startsWith("#") ||
    specifier.startsWith("@/")
  )
    return null;
  return specifier.startsWith("@")
    ? specifier.split("/").slice(0, 2).join("/")
    : specifier.split("/")[0];
}

function declaredDependencies(item) {
  const manifest = item.manifest ? manifestFor(item) : {};
  return new Set(
    DECLARATION_SECTIONS.flatMap((section) =>
      Object.keys(manifest[section] ?? {}),
    ),
  );
}

function allowlisted(importer, file, specifier) {
  return BOUNDARY_ALLOWLIST.some(
    (entry) =>
      entry.importer === importer &&
      globMatches(entry.file, file) &&
      globMatches(entry.specifier, specifier),
  );
}

function globMatches(pattern, value) {
  const regex = pattern
    .split("**")
    .map((part) =>
      part
        .split("*")
        .map((segment) => segment.replace(/[.*+?^${}()|[\]\\]/g, "\\$&"))
        .join("[^/]*"),
    )
    .join(".*");
  return new RegExp(`^${regex}$`).test(value);
}

export function boundaryFindings(root, packages, files = workspaceFiles(root)) {
  const rooted = packages.map((item) => ({ ...item, root }));
  const byName = new Map(rooted.map((item) => [item.name, item]));
  const appPackages = rooted.filter((item) => /\/apps\/[^/]+$/.test(item.dir));
  const builtins = new Set(
    builtinModules.flatMap((name) => [name, `node:${name}`]),
  );
  const findings = [];
  for (const file of files) {
    const importer = packageRootFor(file, rooted);
    if (!importer) continue;
    const relativeFile = path
      .relative(path.resolve(root, importer.dir), file)
      .split(path.sep)
      .join("/");
    const declared = declaredDependencies(importer);
    const source = readFileSync(file, "utf8");
    for (const specifier of packageImports(root, file, source)) {
      if (specifier.startsWith(".")) {
        const targetPath = path.resolve(path.dirname(file), specifier);
        const target = packageRootFor(targetPath, rooted);
        const destinationApp = appPackages.find((app) => {
          const dir = path.resolve(root, app.dir);
          return (
            targetPath === dir || targetPath.startsWith(`${dir}${path.sep}`)
          );
        });
        if (importer.dir.includes("/packages/") && destinationApp)
          findings.push({
            rule: "package-imports-app",
            importer: importer.name,
            file: relativeFile,
            specifier,
            message: `${importer.name} package imports app ${destinationApp.name}`,
          });
        if (
          target &&
          target.name !== importer.name &&
          !declared.has(target.name) &&
          !allowlisted(importer.name, relativeFile, specifier)
        )
          findings.push({
            rule: "relative-workspace-import",
            importer: importer.name,
            file: relativeFile,
            specifier,
            message: `${importer.name} reaches into ${target.name} by relative path without a declared package dependency`,
          });
        continue;
      }
      const name = packageName(specifier);
      if (!name || builtins.has(name) || name.startsWith("node:")) continue;
      const destination = byName.get(name);
      if (
        importer.dir.includes("/packages/") &&
        destination?.dir.includes("/apps/")
      )
        findings.push({
          rule: "package-imports-app",
          importer: importer.name,
          file: relativeFile,
          specifier,
          message: `${importer.name} package imports app ${destination.name}`,
        });
      if (destination && destination.name !== importer.name) continue;
      if (importer.manifest && !declared.has(name))
        findings.push({
          rule: "undeclared-bare-import",
          importer: importer.name,
          file: relativeFile,
          specifier,
          message: `${importer.name} imports undeclared package ${name}`,
        });
    }
  }
  return findings;
}

export function checkBoundaries(root = rootFrom(), packages = discover(root)) {
  const findings = boundaryFindings(root, packages);
  if (findings.length) {
    for (const finding of findings)
      process.stderr.write(
        `${finding.rule}: ${finding.file}: ${finding.message} (${finding.specifier})\n`,
      );
    return 1;
  }
  process.stdout.write(
    `Workspace boundaries passed (${packages.length} packages; ${BOUNDARY_ALLOWLIST.length} documented crossings allowlisted).\n`,
  );
  return 0;
}

export function affectedPlan(root, base, packages, changedFiles) {
  if (!changedFiles.length)
    fail(`No changed files found by git diff --name-only ${base}...HEAD`);
  const reasons = new Map();
  const allPackages = (reason) =>
    packages.forEach((item) =>
      reasons.set(item.name, `conservative widening: ${reason}`),
    );
  const rootPolicy =
    /^(?:package\.json|pnpm-lock\.yaml|pnpm-workspace\.yaml|Cargo\.toml|Cargo\.lock|rust-toolchain\.toml|\.github\/|justfile(?:$|\/))/;
  const policyChange = changedFiles.find((file) => rootPolicy.test(file));
  if (policyChange) allPackages(`root policy file ${policyChange} changed`);
  else {
    for (const file of changedFiles) {
      const owners = packages
        .filter(
          (item) =>
            item.dir !== "." &&
            (file === item.dir || file.startsWith(`${item.dir}/`)),
        )
        .sort((a, b) => b.dir.length - a.dir.length);
      const owner =
        owners[0] ??
        (file.startsWith("workspaces/tooling/")
          ? packages.find((item) => item.name === "codex-studio-tooling")
          : null);
      if (!owner) {
        allPackages(`unowned file ${file} changed`);
        break;
      }
      reasons.set(owner.name, `changed file ${file} belongs to ${owner.dir}`);
    }
  }
  if (
    ![...reasons.values()].some((reason) =>
      reason.startsWith("conservative widening"),
    )
  ) {
    const dependents = new Map(packages.map((item) => [item.name, new Set()]));
    const cargoMetadata = packages.some((item) => item.ecosystem === "cargo")
      ? JSON.parse(
          runSync(
            "cargo",
            ["metadata", "--no-deps", "--format-version", "1"],
            root,
            "Cargo workspace graph",
          ),
        )
      : null;
    for (const item of packages) {
      if (item.ecosystem === "pnpm") {
        for (const dependency of declaredDependencies(item))
          if (byWorkspaceDependency(item.manifest, dependency, packages))
            dependents.get(dependency)?.add(item.name);
      } else if (item.ecosystem === "cargo") {
        for (const dependency of cargoMetadata.packages.find(
          (pkg) => pkg.name === item.name,
        )?.dependencies ?? [])
          if (
            packages.some(
              (candidate) =>
                candidate.name === dependency.name &&
                candidate.ecosystem === "cargo",
            )
          )
            dependents.get(dependency.name)?.add(item.name);
      }
    }
    const queue = [...reasons.keys()];
    while (queue.length) {
      const dependency = queue.shift();
      for (const dependent of dependents.get(dependency) ?? []) {
        if (reasons.has(dependent)) continue;
        reasons.set(dependent, `reverse dependency of ${dependency}`);
        queue.push(dependent);
      }
    }
  }
  return packages
    .filter((item) => reasons.has(item.name))
    .map((item) => ({ package: item.name, reason: reasons.get(item.name) }));
}

function byWorkspaceDependency(manifestPath, dependency, packages) {
  if (!manifestPath) return false;
  const manifest = JSON.parse(readFileSync(manifestPath, "utf8"));
  return DECLARATION_SECTIONS.some((section) => {
    const value = manifest[section]?.[dependency];
    return (
      typeof value === "string" &&
      value.startsWith("workspace:") &&
      packages.some((item) => item.name === dependency)
    );
  });
}

export function runContractsCheck(root = rootFrom()) {
  process.stdout.write(
    "Checking generated API contracts without writing files: pnpm run api:check\n",
  );
  const result = spawnSync("pnpm", ["run", "api:check"], {
    cwd: root,
    stdio: "inherit",
  });
  return result.status ?? 1;
}

function runAffected(root, base, dryRun) {
  const changed = runSync(
    "git",
    ["diff", "--name-only", `${base}...HEAD`],
    root,
    "Changed-file discovery",
  )
    .split("\n")
    .filter(Boolean);
  const packages = discover(root);
  const selection = affectedPlan(root, base, packages, changed);
  process.stdout.write(
    `Affected checks for ${base}...HEAD (${changed.length} changed files):\n`,
  );
  for (const item of selection)
    process.stdout.write(
      `- ${item.package}: ${item.reason}${dryRun ? `; command: just check ${item.package}` : ""}\n`,
    );
  if (dryRun) return 0;
  for (const item of selection) {
    const result = spawnSync("just", ["check", item.package], {
      cwd: root,
      stdio: "inherit",
    });
    if (result.status !== 0) return result.status ?? 1;
  }
  return 0;
}

export async function main(argv = process.argv.slice(2)) {
  justVersion();
  const [action, packageName, requestedCase, ...rest] = argv;
  const json = requestedCase === "--json" || rest.includes("--json");
  const dryRun = requestedCase === "--dry-run" || rest.includes("--dry-run");
  if (action === "packages") {
    const all = discover(rootFrom());
    process.stdout.write(
      all
        .map((item) => `${item.name}\t${item.ecosystem}\t${item.dir}`)
        .join("\n") + "\n",
    );
    return 0;
  }
  if (action === "help" || action === "--help") {
    usage();
    return 0;
  }
  if (action === "contracts-check") return runContractsCheck(rootFrom());
  if (action === "boundaries-check") return checkBoundaries(rootFrom());
  if (action === "check-affected") {
    if (!packageName || packageName.startsWith("-"))
      fail("Usage: check-affected <base> [--dry-run]");
    return runAffected(
      rootFrom(),
      packageName,
      requestedCase === "--dry-run" || rest.includes("--dry-run"),
    );
  }
  if (!["test", "test-plan", "check"].includes(action)) {
    usage();
    return action ? 2 : 0;
  }
  const root = rootFrom();
  const item = target(root, packageName);
  const selection = parseSelection(
    action === "check" ||
      requestedCase === "--json" ||
      requestedCase === "--dry-run" ||
      requestedCase === "--name"
      ? ""
      : requestedCase,
    requestedCase === "--name" ? [requestedCase, ...rest] : rest,
  );
  const { caseName, options } = selection;
  const effective = action === "test-plan" ? "test" : action;
  const commands = commandFor(root, item, effective, caseName, options);
  if (action === "test-plan" || (action === "check" && dryRun)) {
    reportPlan(plan(root, item, effective, caseName, commands, options), json);
    return 0;
  }
  if (action === "check") {
    const dir = makeLogDirectory();
    const log = path.join(dir, "check.log");
    appendFileSync(log, "");
    const started = performance.now();
    let output = "";
    let exitCode = 0;
    for (const [bin, args] of commands) {
      const env =
        bin === "cargo"
          ? { ...process.env, CARGO_TARGET_DIR: cargoTarget(root) }
          : process.env;
      const result = await runLogged(bin, args, root, log, env);
      output += result.output;
      if (result.signal) {
        process.kill(process.pid, result.signal);
      }
      if (result.code !== 0) {
        exitCode = result.code ?? 1;
        break;
      }
    }
    const durationSeconds = Number(
      ((performance.now() - started) / 1000).toFixed(3),
    );
    const counts = parseCounts(item, output);
    if (
      exitCode === 0 &&
      commands.some(
        ([, args]) => args.includes("test") || args.includes("test:unit"),
      ) &&
      counts.executed === 0
    )
      exitCode = 1;
    const checkPlan = plan(root, item, "check", "", commands);
    const result = {
      package: item.name,
      case: null,
      status: exitCode === 0 ? "passed" : "failed",
      ...counts,
      durationSeconds,
      command: checkPlan.command,
      workingDirectory: checkPlan.workingDirectory,
      invocationDirectory: root,
      logPath: log,
      requiredSetup: checkPlan.requiredSetup,
      reason: checkPlan.reason,
      notStarted: checkPlan.notStarted,
      costCategory: checkPlan.costCategory,
      exitCode,
    };
    if (json) process.stdout.write(`${JSON.stringify(result)}\n`);
    else
      process.stdout.write(
        `${item.name} checks: ${result.status}${counts.executed ? ` — ${counts.passed} passed, ${counts.failed} failed, ${counts.skipped} skipped` : ""}; ${durationSeconds}s\nCommand: ${result.command}\nFull log: ${log}\n`,
      );
    return exitCode;
  }
  const result = await execute(root, item, caseName, json, options);
  if (result.signal) process.kill(process.pid, result.signal);
  return result.exitCode;
}

if (
  process.argv[1] &&
  path.resolve(process.argv[1]) === fileURLToPath(import.meta.url)
) {
  main()
    .then((code) => {
      process.exitCode = code;
    })
    .catch((error) => {
      process.stderr.write(`${error.message}\n`);
      process.exitCode = 1;
    });
}
