#!/usr/bin/env node
import { spawn, spawnSync } from "node:child_process";
import { appendFileSync, mkdirSync, mkdtempSync, readFileSync } from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";

const MIN_JUST = [1, 58, 0];
const COST = {
  pnpm: "focused; package test only",
  cargo: "focused; package compile and test",
  server: "focused; isolated Python suite runner",
};
const NOT_STARTED = "backend, browser, Electron, provider";

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
  const script = manifest.scripts?.["test:unit"]
    ? "test:unit"
    : manifest.scripts?.test
      ? "test"
      : null;
  if (!script)
    fail(`Package ${item.name} has no focused test script in ${item.manifest}`);
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
  if (caseName && /\bvitest\b/.test(scriptText)) {
    const config = scriptText.match(/--config(?:=|\s+)([^\s]+)/)?.[1];
    const args = ["--filter", item.name, "exec", "vitest", "run"];
    if (config) args.push("--config", config);
    if (
      /\.(?:test|spec)\.[cm]?[jt]sx?$/.test(caseName) ||
      caseName.includes("/") ||
      caseName.includes("\\")
    )
      args.push(caseName);
    else args.push("-t", caseName);
    return [["pnpm", args]];
  }
  if (caseName && !/\bvitest\b/.test(scriptText))
    fail(
      `Package ${item.name} does not expose a native Vitest filter through its test script`,
    );
  const args = ["--filter", item.name, "run", script];
  return [["pnpm", args]];
}

export function commandFor(root, item, action, caseName = "") {
  if (item.ecosystem === "pnpm") {
    if (action === "test") return pnpmTestCommand(item, caseName);
    const manifest = manifestFor(item);
    if (manifest.scripts?.check)
      return [["pnpm", ["--filter", item.name, "run", "check"]]];
    const commands = [];
    if (
      manifest.dependencies?.typescript ||
      manifest.devDependencies?.typescript
    )
      commands.push([
        "pnpm",
        ["--filter", item.name, "exec", "tsc", "--noEmit"],
      ]);
    for (const script of ["lint", "typecheck", "test:unit"]) {
      if (manifest.scripts?.[script])
        commands.push(["pnpm", ["--filter", item.name, "run", script]]);
    }
    if (!commands.length && manifest.scripts?.test)
      commands.push(["pnpm", ["--filter", item.name, "run", "test"]]);
    if (!commands.length)
      fail(
        `Package ${item.name} does not define local check scripts or a TypeScript compiler`,
      );
    return commands;
  }
  if (item.ecosystem === "cargo") {
    if (action === "test")
      return [
        ["cargo", ["test", "-p", item.name, ...(caseName ? [caseName] : [])]],
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
            "-B",
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
          ? `CARGO_TARGET_DIR=${shellQuote(path.join(root, "target"))} `
          : "";
      return `cd ${shellQuote(root)} && ${targetEnv}${[bin, ...args].map(shellQuote).join(" ")}`;
    })
    .join(" && ");
}

function plan(root, item, action, caseName, commands) {
  const workingDirectory =
    item.ecosystem === "pnpm" ? path.resolve(root, item.dir) : root;
  const setup =
    item.ecosystem === "pnpm"
      ? "Pinned pnpm and workspace dependencies installed"
      : item.ecosystem === "cargo"
        ? `Pinned Rust toolchain and required crates available; target output uses ${path.join(root, "target")}`
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
    requiredSetup: setup,
    reason:
      item.ecosystem === "pnpm"
        ? "Run only the selected package's manifest-owned test or check command."
        : item.ecosystem === "cargo"
          ? "Compile and run only the selected Cargo package and its declared dependencies."
          : "Pass the selected suite filter to the server runner, which owns discovery and isolation.",
    notStarted: NOT_STARTED,
    costCategory: COST[item.ecosystem],
    executesNothing: true,
  };
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

export async function execute(root, item, caseName, asJson = false) {
  const commands = commandFor(root, item, "test", caseName);
  const run = plan(root, item, "test", caseName, commands);
  const logDir = mkdtempSync(path.join(tmpdir(), "codex-studio-check-"));
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
        ? { ...process.env, CARGO_TARGET_DIR: path.join(root, "target") }
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
    "Usage: facade.mjs packages | test <package> [case] [--json] | test-plan <package> [case] [--json] | check <package> [--dry-run] [--json]\n",
  );
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
  if (!["test", "test-plan", "check"].includes(action)) {
    usage();
    return action ? 2 : 0;
  }
  const root = rootFrom();
  const item = target(root, packageName);
  const caseName =
    action === "check" || requestedCase === "--json"
      ? ""
      : [
          requestedCase,
          ...rest.filter((arg) => arg !== "--json" && arg !== "--dry-run"),
        ]
          .filter(Boolean)
          .join(" ");
  const effective = action === "test-plan" ? "test" : action;
  const commands = commandFor(root, item, effective, caseName);
  if (action === "test-plan" || (action === "check" && dryRun)) {
    reportPlan(plan(root, item, effective, caseName, commands), json);
    return 0;
  }
  if (action === "check") {
    const dir = mkdtempSync(path.join(tmpdir(), "codex-studio-check-plan-"));
    const log = path.join(dir, "check.log");
    appendFileSync(log, "");
    const started = performance.now();
    let output = "";
    let exitCode = 0;
    for (const [bin, args] of commands) {
      const env =
        bin === "cargo"
          ? { ...process.env, CARGO_TARGET_DIR: path.join(root, "target") }
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
  const result = await execute(root, item, caseName, json);
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
