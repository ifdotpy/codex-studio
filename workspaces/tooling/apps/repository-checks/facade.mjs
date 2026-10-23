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
const UNSAFE_RUST_ALLOWLIST = [
  [
    "workspaces/runtime/packages/runtime-priority-sys/src/lib.rs",
    "OS priority syscall.",
  ],
  [
    "workspaces/runtime/packages/runtime-proc-sys/src/lib.rs",
    "Read-only macOS libproc and sysctl process identity queries.",
  ],
  [
    "workspaces/runtime/packages/runtime-services/src/image_workspace_macos/fsevents_ffi.rs",
    "FSEvents (lead's W9 decision).",
  ],
  [
    "workspaces/runtime/packages/runtime-services/src/host_exec_service/clone_ffi.rs",
    "APFS clonefile(2) for host_exec slot clones.",
  ],
  [
    "workspaces/runtime/packages/runtime-services/src/diagnostics/allocator.rs",
    "Opt-in GNU allocator summary through a bounded private diagnostic buffer.",
  ],
  [
    "workspaces/client/apps/native/src-tauri/src/lib.rs",
    "Single-instance flock on the native shell lock file.",
  ],
  [
    "workspaces/client/packages/client-core/examples/voice_playback_check/check.rs",
    "Offline AVAudioEngine render check for the voice playback path.",
  ],
  [
    "workspaces/client/packages/client-core/src/desktop.rs",
    "C ABI entry point, getentropy, and AppKit pasteboard and NSError bindings.",
  ],
  [
    "workspaces/client/packages/client-core/src/media.rs",
    "AVFoundation capture authorization bindings.",
  ],
  [
    "workspaces/client/packages/client-core/src/native.rs",
    "Single-instance flock and SO_NOSIGPIPE on the UI socket.",
  ],
  [
    "workspaces/client/packages/client-core/src/notifications.rs",
    "UserNotifications delegate class and center bindings.",
  ],
  [
    "workspaces/client/packages/client-core/src/pdf.rs",
    "CoreGraphics PDF page render context.",
  ],
  [
    "workspaces/client/packages/client-core/src/platform.rs",
    "getentropy for client random identifiers.",
  ],
  [
    "workspaces/client/packages/client-core/src/quick_look.rs",
    "Quick Look panel through AppKit initializers.",
  ],
  [
    "workspaces/client/packages/client-core/src/rich_document.rs",
    "WebKit rich document renderer delegate and bindings.",
  ],
  [
    "workspaces/client/packages/client-core/src/startup.rs",
    "Process liveness probe, setsid before exec, and fixture process cleanup.",
  ],
  [
    "workspaces/client/packages/client-core/src/voice.rs",
    "libopus encoder and decoder state.",
  ],
  [
    "workspaces/client/packages/client-core/src/voice_audio.rs",
    "CoreAudio and AVAudioEngine realtime render callbacks.",
  ],
];
const UNSAFE_RUST_ALLOWLIST_BY_FILE = new Map(UNSAFE_RUST_ALLOWLIST);
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
    specifier: "../web/src/desktop",
    reason:
      "The desktop bridge declaration re-exports the renderer's single bridge owner until it has a public package owner.",
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
    importer: "codex-studio-tooling",
    file: "workspaces/tooling/apps/repository-checks/tests/release-binaries.test.mjs",
    specifier: "../../../../client/apps/**",
    reason:
      "The fast tier checks the release-binaries commit and SHA-256 manifest contract of the desktop packagers and the native package flow, which have no fast-tier test runner of their own.",
  },
  {
    importer: "codex-studio-tooling",
    file: "workspaces/tooling/apps/repository-checks/tests/signing-check.test.mjs",
    specifier: "../../../../client/apps/desktop/**",
    reason:
      "The fast tier checks the signing check and the unlock error contract of the desktop packager with a fake security tool, because the desktop package has no fast-tier test runner of its own.",
  },
  {
    importer: "codex-agents-desktop",
    file: "profile-export-test.mjs",
    specifier: "../web/src/native/importBrowserProfile.mjs",
    reason:
      "The profile export scenario checks its output with the renderer's own browser profile import code.",
  },
  {
    importer: "codex-agents-desktop",
    file: "profile-export-test.mjs",
    specifier: "../web/src/native/combineBrowserProfiles.mjs",
    reason:
      "The profile export scenario checks its output with the renderer's own browser profile import code.",
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
        docTests: item.targets?.some((target) => target.doctest) ?? false,
      });
    }
  }
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
  return packageTarget(discover(root), pkg);
}

export function packageTarget(packages, pkg) {
  const found = packages.find((item) => item.name === pkg);
  if (!found)
    fail(
      `Unknown package "${pkg}". Valid packages: ${packages.map((item) => item.name).join(", ")}`,
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
      if (options.script) {
        const manifest = manifestFor(item);
        if (!manifest.scripts?.[options.script])
          fail(
            `Package ${item.name} does not declare script ${options.script}`,
          );
        return [["pnpm", ["--filter", item.name, "run", options.script]]];
      }
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
    if (action === "test") {
      const filters = [caseName, options.name].filter(Boolean);
      const filterset = filters
        .map((filter) => `test(~${escapeFilterMatcher(filter)})`)
        .join(" and ");
      return [
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
            ...(options.cargoTargets ?? []),
            ...(filterset ? ["--filterset", filterset] : []),
            ...(options.passthrough?.length
              ? ["--", ...options.passthrough]
              : []),
          ],
        ],
      ];
    }
    return [
      ["cargo", ["fmt", "--check", "-p", item.name]],
      [
        "cargo",
        ["clippy", "-p", item.name, "--all-targets", "--", "-D", "warnings"],
      ],
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
        ],
      ],
      ...(item.docTests
        ? [["cargo", ["test", "--doc", "--locked", "-p", item.name]]]
        : []),
    ];
  }
  fail(`Unsupported package ecosystem: ${item.ecosystem}`);
}

function escapeFilterMatcher(value) {
  return value
    .replaceAll("\\", "\\\\")
    .replaceAll("\n", "\\n")
    .replaceAll("\r", "\\r")
    .replaceAll("\t", "\\t")
    .replaceAll(")", "\\)")
    .replaceAll(",", "\\,");
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
        : "Required package tools available";
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
            : "package tests",
    workingDirectory,
    invocationDirectory: root,
    command: reproduction(root, commands),
    selector:
      action === "check"
        ? "mapped package checks (no test filter)"
        : selectorDescription(item, caseName, options),
    requiredSetup: setup,
    reason: options.reason
      ? options.reason
      : item.ecosystem === "pnpm"
        ? "Run only the selected package's manifest-owned test or check command."
        : item.ecosystem === "cargo"
          ? "Compile and run only the selected Cargo package and its declared dependencies."
          : "Run the selected package tests.",
    notStarted: NOT_STARTED,
    costCategory: options.costCategory
      ? options.costCategory
      : action === "check"
        ? item.ecosystem === "pnpm"
          ? "package-local type and unit checks"
          : item.ecosystem === "cargo"
            ? "package-local fmt, clippy, and unit checks"
            : "package checks"
        : COST[item.ecosystem],
    executesNothing: true,
  };
}

function selectorDescription(item, caseName, options) {
  if (options.selector) return options.selector;
  if (item.ecosystem === "cargo")
    return `Cargo test-name substring${options.cargoTargets?.length ? `; Cargo target selection: ${options.cargoTargets.join(" ")}` : ""}${options.passthrough?.length ? `; test harness args after --: ${options.passthrough.join(" ")}` : ""}`;
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

function browserTestOptions() {
  return {
    script: "test:browser",
    playwrightJsonReport: true,
    selector: "full client browser project",
    reason:
      "Run the web package's isolated browser launcher with its 900 s global cap.",
    costCategory: "full browser project; 900 s cap",
  };
}

function browserSmokeTestOptions() {
  return {
    script: "test:browser:smoke",
    playwrightJsonReport: true,
    selector: "explicit browser smoke cases",
    reason:
      "Run the fast browser smoke subset through the isolated Playwright launcher.",
    costCategory: "browser smoke subset; under 120 s target",
    label: "browser smoke subset",
  };
}

function browserPlan(root, smoke = false) {
  const item = packageTarget(discover(root), "codex-agents-web");
  const options = smoke ? browserSmokeTestOptions() : browserTestOptions();
  const value = plan(
    root,
    item,
    "test",
    "",
    commandFor(root, item, "test", "", options),
    options,
  );
  value.notStarted = "backend, Electron, provider";
  return value;
}

function webWasmPreparation(root) {
  return {
    item: { name: "web WASM preparation", ecosystem: "pnpm" },
    label: "web WASM preparation",
    commands: [
      [
        "node",
        [
          path.join(
            root,
            "workspaces/client/apps/web/scripts/build-client-wasm.mjs",
          ),
        ],
      ],
    ],
    options: { allowZeroTests: true },
  };
}

// A fast-tier nextest filter with one witness per line, so independent changes
// add separate lines instead of rewriting one shared filter string.
function nextestAnyOf(terms) {
  return terms.join(" | ");
}

function fastTierCommands(root) {
  const packages = discover(root);
  const bridge = packageTarget(packages, "studio-claude-bridge");
  const web = packageTarget(packages, "codex-agents-web");
  const tooling = packageTarget(packages, "codex-studio-tooling");
  const facade = path.join(
    root,
    "workspaces/tooling/apps/repository-checks/facade.mjs",
  );
  return [
    webWasmPreparation(root),
    {
      item: { name: "studio-runtime-server", ecosystem: "cargo" },
      label: "Rust operator CLI witnesses",
      commands: [
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
            "studio-runtime-server",
            "-E",
            "test(operator_cli::)",
          ],
        ],
      ],
      options: {
        selector: "Rust service and command installation without Python",
        reason:
          "Check isolated user service managers, delivery layout, safe link migration, and idempotent retries without touching host services.",
      },
    },
    {
      item: { name: "studio-runtime-services", ecosystem: "cargo" },
      label: "VM tools, host_exec tool and Mac service witnesses",
      commands: [
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
            "studio-runtime-services",
            "--lib",
            "-E",
            "test(/^(host_exec(_service)?|layr_read)::/) | test(/^thread_prepare::vm_catalog_tests::/) | test(/::context::tests::the_tools_topic_lists_the_tools_of_a_vm_agent$/) | test(/::a_vm_agent_on_an_old_thread_gets_the_current_catalog$/)",
          ],
        ],
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
            "studio-runtime-server",
            "-E",
            "test(/^host_exec_service::/)",
          ],
        ],
      ],
      options: {
        selector:
          "host_exec identities, refusals, Mac service receipts, start rules, and the guest broker",
        reason:
          "Check that a lost answer never repeats a Mac command, that only VM writers run commands, and that the unchanged guest broker works with the service process.",
      },
    },
    {
      item: { name: "studio-runtime-services", ecosystem: "cargo" },
      label: "Linux VM start, watchdog and share mount witnesses",
      commands: [
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
            "studio-runtime-services",
            "--lib",
            "-E",
            "test(/^linux_vm_(runtime|share)::/)",
          ],
        ],
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
            "studio-runtime-server",
            "-E",
            "test(/^linux_vm_start::/)",
          ],
        ],
      ],
      options: {
        selector:
          "VM start after a Mac restart, stops on purpose, restart limits and the helper session",
        reason:
          "Check that only a VM that the Mac restart stopped starts by itself, with a fake helper and guest, and that the helper runs in its own session.",
      },
    },
    {
      item: { name: "studio-runtime-services", ecosystem: "cargo" },
      label: "retained provider disconnect witnesses",
      commands: [
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
            "studio-runtime-services",
            "--lib",
            "-E",
            "test(/^account_worker::tests::(a_replaced_worker_keeps_the_live_turn_of_its_retained_provider|a_supervised_provider_exit_is_still_a_disconnect|provider_exit_while_the_server_runs_is_a_disconnect_reported_once)$/)",
          ],
        ],
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
            "studio-runtime-store",
            "--lib",
            "-E",
            "test(/::a_retained_host_provider_disconnects_only_the_vm_actors$/)",
          ],
        ],
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
            "studio-runtime-process",
            "-E",
            "binary(integration) & test(/^provider_task::(a_cancelled_attachment_leaves_its_supervised_child_retained|terminal_eof_serves_already_queued_final_ack_before_completion)$/)",
          ],
        ],
      ],
      options: {
        selector:
          "account worker end with a supervised provider that outlives it",
        reason:
          "Check that a replaced worker leaves the live turn of a retained provider running and tells no parent, while a provider exit stays a disconnect.",
      },
    },
    {
      item: { name: "retained Python helper boundaries", ecosystem: "pnpm" },
      label: "retained Python helper boundaries",
      commands: [
        [
          "node",
          [
            "--test",
            path.join(
              root,
              "workspaces/client/apps/desktop/retained-helper-witness.test.cjs",
            ),
          ],
        ],
      ],
      options: {
        selector: "retained helper import and launch boundaries",
        reason:
          "Keep retained desktop and guest Python independent of the removed backend.",
      },
    },
    {
      item: { name: "Mac sync chunk reads", ecosystem: "pnpm" },
      label: "Mac sync chunk read witness",
      commands: [
        [
          "node",
          [
            "--test",
            path.join(
              root,
              "workspaces/runtime/apps/vm-guest/mac-sync-witness.test.mjs",
            ),
          ],
        ],
      ],
      options: {
        selector: "bounded guest Mac sync reads",
        reason:
          "Read at most one chunk under the project lock and preserve file and link responses.",
      },
    },
    {
      // The runner owns the Python version, the supervisor binary, the short macOS
      // temporary root that the guest's Unix sockets need, and the check that no process
      // of the suite outlives it; its own test proves that check.
      item: { name: "VM guest Python suite", ecosystem: "pnpm" },
      label: "VM guest Python suite",
      commands: [
        ["cargo", ["build", "-p", "studio-process-supervisor", "--locked"]],
        [
          "node",
          [
            "--test",
            path.join(
              root,
              "workspaces/runtime/apps/vm-guest/run-guest-suite.test.mjs",
            ),
          ],
        ],
        [
          "node",
          [
            path.join(
              root,
              "workspaces/runtime/apps/vm-guest/run-guest-suite.mjs",
            ),
          ],
        ],
      ],
      options: {
        selector: "every guest unittest module",
        reason:
          "Keep the guest service, bridge, upload, project and update contracts that the VM runs.",
      },
    },
    {
      item: { name: "native WebView fixture", ecosystem: "pnpm" },
      label: "native WebView fixture",
      commands: [
        [
          "node",
          ["--test", "workspaces/client/apps/native/check-native.test.mjs"],
        ],
      ],
      options: {
        selector: "native HTTP fixture contracts",
        reason:
          "Check the hidden native WebView harness without opening a window.",
      },
    },
    {
      item: { name: "schema and dependency guards", ecosystem: "pnpm" },
      label: "schema and dependency guards",
      commands: [
        ["node", [facade, "contracts-check"]],
        ["node", [facade, "boundaries-check"]],
      ],
      options: {
        selector: "generated contract and workspace boundary checks",
        reason: "Run the existing read-only schema and dependency guards.",
        allowZeroTests: true,
      },
    },
    {
      item: { name: "desktop recovery helpers", ecosystem: "pnpm" },
      label: "Rust desktop recovery witnesses",
      commands: [
        [
          "node",
          [
            "--test",
            "workspaces/client/apps/desktop/recovery-helper-witness.test.cjs",
          ],
        ],
      ],
      options: {
        selector:
          "Rust recovery lease, identity and supervisor socket boundaries",
        reason:
          "Check desktop recovery without Python backend imports or supervisor scripts.",
      },
    },
    ...(process.platform === "linux"
      ? [
          {
            item: {
              name: "supervisor protocol boundaries",
              ecosystem: "cargo",
            },
            label: "supervisor protocol and identity witnesses",
            commands: [
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
                  "studio-process-supervisor",
                  "--test",
                  "clients",
                  "-E",
                  "test(supervisor_rejects_retired_protocol_before_admission)",
                ],
              ],
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
                  "studio-runtime-providers",
                  "--lib",
                  "-E",
                  "test(supervisor_rejects_python_identity_adoption)",
                ],
              ],
            ],
            options: {
              selector: "current supervisor protocol and kernel identity",
              reason:
                "Reject retired protocol and Python identity adoption before ownership changes.",
            },
          },
        ]
      : []),
    {
      item: { name: "Rust-owned drafts", ecosystem: "cargo" },
      label: "draft wire and legacy compatibility witnesses",
      commands: [
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
            "studio-client-core",
            "-p",
            "studio-runtime-http",
            "--lib",
            "-E",
            nextestAnyOf([
              "test(draft_receive_normalizes_shared_legacy_fixtures_without_rewriting_storage)",
              "test(named_schema_draft_push_matches_shared_compatibility_fixtures)",
              "test(stored_legacy_drafts_remain_readable_and_replaceable)",
            ]),
          ],
        ],
      ],
      options: {
        selector: "shared strict push and legacy receive fixtures",
        reason:
          "Keep new draft payloads compatible with native receive without rewriting stored legacy drafts.",
      },
    },
    {
      item: { name: "HTTP Brotli responses", ecosystem: "cargo" },
      label: "JSON compression and native decoding witnesses",
      commands: [
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
            "studio-runtime-http",
            "-p",
            "studio-client-core",
            "--lib",
            "-E",
            "test(brotli_)",
          ],
        ],
      ],
      options: {
        selector: "Brotli JSON bytes, ETags, SSE exclusion and native decoding",
        reason:
          "Preserve uncompressed representation hashes and SSE while reducing large JSON transfer bytes.",
      },
    },
    {
      item: { name: "studio-client-core", ecosystem: "cargo" },
      label: "native client boundary witnesses",
      commands: [
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
            "studio-client-core",
            "--lib",
            "-E",
            nextestAnyOf([
              "test(draft_store::tests::synchronization_decodes_only_the_keyed_draft_per_incoming_document)",
              "test(terminal::tests::resized_grid_keeps_cursor_output_and_truncation_dimensions)",
              "test(server_transport::tests::https_transport_rejects_untrusted_certificate_before_sending_request)",
              "test(server_transport::tests::one_server_cannot_hold_the_slots_of_another_server)",
              "test(server_transport::tests::https_transport_signs_exact_bytes_and_streams_without_forwarding_local_auth)",
              "test(svg_normalization_has_no_script_file_or_network_references)",
            ]),
          ],
        ],
      ],
      options: {
        selector:
          "draft synchronization, terminal resize, TLS and SVG boundaries",
        reason:
          "Preserve keyed draft decoding, terminal dimensions, certificate rejection, exact signed HTTP bytes and SVG resource isolation across dependency updates.",
      },
    },
    {
      item: { name: "studio-runtime-providers", ecosystem: "cargo" },
      label: "provider request body ownership witnesses",
      commands: [
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
            "studio-runtime-providers",
            "--lib",
            "-E",
            "test(/^connection::request_body_tests::/)",
          ],
        ],
      ],
      options: {
        selector: "connection request body allocation transfer and retention",
        reason:
          "Check that emitted requests retain only metadata, queued bodies move on first emission, and uncertainty, late replies and unsent cancellation preserve correlation.",
      },
    },
    {
      item: bridge,
      label: "Claude restart and journal evidence witnesses",
      commands: [
        [
          "pnpm",
          [
            "--filter",
            "studio-claude-bridge",
            "exec",
            "vitest",
            "run",
            "--config",
            "vitest.config.mjs",
            "restart-reconcile.test.mjs",
            "journal-repair.test.mjs",
          ],
        ],
      ],
      options: {
        selector: "restart-reconcile and journal-repair",
        reason:
          "Preserve unconfirmed outcomes and bounded, read-only repair evidence without a model query.",
      },
    },
    {
      item: { name: "studio-runtime-services", ecosystem: "cargo" },
      label: "journal agent publication witnesses",
      commands: [
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
            "studio-runtime-services",
            "--lib",
            "-E",
            "test(account_worker::tests::journal_publication::)",
          ],
        ],
      ],
      options: {
        selector:
          "journal_publication per-agent call counts, rollback and replay",
        reason:
          "Check that notification and stream-flush batches project and validate each changed agent once inside their atomic journal transaction.",
      },
    },
    {
      item: { name: "studio-runtime-services", ecosystem: "cargo" },
      label: "agent decode ownership witness",
      commands: [
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
            "studio-runtime-services",
            "--lib",
            "-E",
            nextestAnyOf([
              "test(account_worker::tests::journal_publication::changed_agents_decode_once_through_final_publication)",
            ]),
          ],
        ],
      ],
      options: {
        selector: "journal final publication decode ownership",
        reason:
          "Check one decode and volatile merge per changed agent, no intermediate text round trip, no unrelated reads, and no replay work.",
      },
    },
    {
      item: { name: "studio-runtime-services", ecosystem: "cargo" },
      label: "transaction diagnostics ownership witnesses",
      commands: [
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
            "studio-runtime-services",
            "--lib",
            "-E",
            nextestAnyOf([
              "test(writer_transaction_acquires_one_diagnostics_lock)",
              "test(writer_transaction_views_preserve_)",
            ]),
          ],
        ],
      ],
      options: {
        selector: "writer transaction diagnostics lock count and output parity",
        reason:
          "Check one collector lock per writer transaction while preserving all waits, caller sites, thresholds and sample windows.",
      },
    },
    {
      item: { name: "studio-runtime-store", ecosystem: "cargo" },
      label: "Rust state migration and SQL callback witnesses",
      commands: [
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
            "studio-runtime-store",
            "--lib",
            "--test",
            "integration",
            "-E",
            nextestAnyOf([
              "test(agent_columns::tests::migration_2026_10_20_columns_round_trips_an_old_state_once)",
              "test(schema_compat::migration_2026_10_22_drops_python_cost_projection_once)",
              "test(schema_compat::cost_projection_migration_rolls_back_if_rust_backfill_fails)",
              "test(agent_columns::tests::migration_2026_10_21_removes_capacity_column_and_replaces_wake_index_once)",
              "test(/^sql_callback_guard::/)",
              "test(fresh_state::rust_fresh_state_catalog_is_deterministic_and_survives_reopen)",
              "test(cross_runtime::fixture_records_are_readable_and_writable_in_rust)",
              "test(sql_guard_tests::)",
              "test(account_limits_ignore_python_single_account_fallback)",
              "test(python_payload_marker_layout_is_refused_without_upgrade)",
              "test(repositories::workspace_views::bulk_read_tests::)",
            ]),
          ],
        ],
      ],
      options: {
        selector:
          "runtime-store agent_columns::tests::migration_ and SQL callback guard",
        reason:
          "Check that older states migrate to current agent columns and wake indexes once, and that SQL callbacks reject filesystem and process work.",
      },
    },
    {
      item: { name: "studio-runtime-services", ecosystem: "cargo" },
      label: "server command snapshot witnesses",
      commands: [
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
            "studio-runtime-services",
            "--lib",
            "-E",
            nextestAnyOf([
              "test(peer_updates::tests::desktop_installer_contract_matches_rust_owner)",
              "test(server_exec::tests::snapshots_)",
              "test(server_exec::tests::snapshot_write_failure_)",
              "test(server_exec::tests::existing_start_fence_)",
              "test(server_exec::tests::synchronous_start_)",
              "test(server_exec::tests::output_tail_append_does_not_move_retained_bytes)",
              "test(payload_blobs::tests::payload_preview_projects_only_retained_string_bytes)",
              "test(notification_dispatch::stream::tests::analytics_delta_size_)",
            ]),
          ],
        ],
      ],
      options: {
        selector:
          "server_exec snapshot coalescing, write retry and start fence",
        reason:
          "Check that idle commands do not rewrite snapshots, changed output coalesces, failed writes retry, and an existing start fence prevents another process.",
      },
    },
    {
      item: { name: "studio-runtime-services", ecosystem: "cargo" },
      label:
        "provider callback, task submission and monitor projection witnesses",
      commands: [
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
            "studio-runtime-services",
            "--lib",
            "-E",
            nextestAnyOf([
              "test(callback_blob_read_allows_an_unrelated_writer)",
              "test(callback_blob_actor_epoch_change_refuses_settlement)",
              "test(callback_blob_changed_result_refuses_settlement)",
              "test(callback_blob_late_result_refuses_stale_recovery)",
              "test(callback_blob_abandoned_response_does_not_replay)",
              "test(callback_blob_recovery_write_precedes_referencing_commit)",
              "test(callback_blob_missing_file_does_not_settle_recovery)",
              "test(callback_blob_corrupt_file_does_not_settle_recovery)",
              "test(callback_blob_symlink_does_not_settle_recovery)",
              "test(callback_panel_uses_one_file_snapshot_per_callback)",
              "test(callback_request_expires_a_terminal_inline_result)",
              "test(callback_request_invalid_action_preserves_tool_refusal)",
              "test(account_worker::tests::callback_handoff::)",
              "test(account_worker::tests::auth_hold::held_start_wakes_nothing_until_an_account_change)",
              "test(account_worker::tests::callback_head_of_line)",
              "test(account_worker::tests::native_requests::)",
              "test(stale_stamp_of_an_undelivered_steer_batch_does_not_block_the_next_turn)",
              "test(stamp_tests::)",
              "test(start_submission_replaces_an_earlier_unsent_attempt)",
              "test(transcript_reasoning_decodes_each_row_before_fetching_the_next)",
              "test(sync_policy::tests::monitor_)",
              "test(work_write::actor_tests::submission_manifest_boundaries_use_shared_limits)",
              "test(work_write::actor_tests::nonempty_file_submission_rechecks_cwd_and_replays_without_filesystem_work)",
              "test(ancestry_reads_only_the_checked_parent_chain)",
              "test(send_to_descendant_is_followup_input_while_parent_and_broadcast_are_chat)",
              "test(provider_session_task::tests::payload_readiness_and_retained_delivery_do_not_clone_before_eligibility)",
              "test(account_worker::retained::runtime::tests::payload_delivery_)",
              "test(account_worker::tests::retained::automatic_recovery_)",
              "test(account_worker::tests::retained::startup_first_pass_reconciles_exact_history_without_replaying_unknown_input)",
              "test(account_worker::tests::retained::retained_actor_request_answer_after_same_generation_restart_is_recorded_once)",
              "test(account_worker::tests::restart_held_first_input_is_reconciled_by_the_first_worker)",
            ]),
          ],
        ],
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
            "studio-runtime-services",
            "--test",
            "integration",
            "-E",
            nextestAnyOf([
              "test(/^provider_session_task::active_barrier_holds_new_requests_but_not_callback_answers_or_interrupts$/)",
              "test(provider_callbacks_unjournaled_card_is_refused_without_writes)",
              "test(/^provider_session_task::expiry_batch_preserves_all_queued_request_outcomes_in_order$/)",
              "test(/^recovery_store::restart_continues_only_a_confirmed_interruption$/)",
              "test(/^analytics_capture_corpus::uncertain_turn_keeps_its_outcome_and_reason_in_analytics$/)",
              "test(/^turn_lifecycle_corpus::fast_witnesses_cover_transitions_states_and_review_findings$/)",
              "test(wave7_p10_lifecycle::committed_delete_)",
              "test(wave7_p10_lifecycle::lost_wake_preserves_committed_stop_for_restart)",
              "test(/^wave9_safety_buffering_executor::comparison_normalizes_only_generated_identity_and_role_source_roots$/)",
            ]),
          ],
        ],
      ],
      options: {
        selector:
          "account_worker::tests::callback_handoff, callback_head_of_line, native_requests, stale batch stamp recovery, automatic turn recovery, the session callback lane, monitor publication work and work_write submission boundaries",
        reason:
          "Check that a provider request is durably accepted with its journal batch, that its record owns the answer through every crash point without a repeated answer, and that an unanswered request never stops provider events or new work, that a native approval or question of a lead waits as a card and its user answer reaches the provider once, also after a worker reattach, and that a stamp left by an undelivered steer batch is replaced instead of holding the agent. Monitor output must publish exactly one monitor projection while preserving window membership and rollback. Task submissions keep limits, workspace ownership and replay at the shared manifest bound. Native-history recovery continues authorized workers once, preserves explicit Stop, and never replays uncertain input.",
      },
    },
    // The reaping witnesses are Linux only (#[cfg(target_os = "linux")]), so
    // the step runs only on Linux. The publication gate also runs its reattach
    // witnesses on macOS.
    ...(process.platform === "linux"
      ? [
          {
            item: { name: "provider process exit", ecosystem: "cargo" },
            label: "independent provider reaping witnesses",
            commands: [
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
                  "studio-runtime-process",
                  "-p",
                  "studio-runtime-server",
                  "--test",
                  "integration",
                  "-E",
                  nextestAnyOf([
                    "test(/^provider_stdio::killed_provider_is_reaped_with_full_journal_and_unobserved_write$/)",
                    "test(/^provider_stdio::cancellation_stops_the_entire_owned_process_group$/)",
                    "test(/^wave7_p01_factory::running_child_started_with_a_previous_command_reattaches$/)",
                    "test(/^wave7_p01_registry::running_app_server_started_with_the_frozen_command_reattaches$/)",
                    "test(/^wave7_p01_registry::direct_child_sigkill_settles_unknown_without_resend$/)",
                    "test(/^wave7_p01_registry::rust_direct_claude_launch_runs_the_scripted_bridge_turn$/)",
                    "test(/^state_check::state_check_reads_fixture_rows_without_changing_or_disclosing_them$/)",
                  ]),
                ],
              ],
            ],
            options: {
              selector: "full stdio lanes and direct provider SIGKILL recovery",
              reason:
                "Check that child reaping and account session loss do not wait for blocked I/O, that the registry starts a new generation without resending an uncertain turn, and that a changed launch command reattaches a running child with the earlier command it lists.",
            },
          },
        ]
      : []),
    {
      item: { name: "project preparation", ecosystem: "cargo" },
      label: "project preparation conflict witnesses",
      commands: [
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
            "studio-runtime-services",
            "-p",
            "studio-runtime-http",
            "--test",
            "integration",
            "-E",
            "test(/^project_preparation::/) | test(committed_delete_route_succeeds_with_unrelated_failing_cleanup) | test(real_rxdb_single_writer_draft_ack_echo_ordering)",
          ],
        ],
      ],
      options: {
        selector:
          "project preparation structural conflicts and idle async answer",
        reason:
          "Check project writes across agent execution changes and real structural conflicts.",
      },
    },
    {
      item: { name: "studio-runtime-store", ecosystem: "cargo" },
      label: "entity commit invalidation witnesses",
      commands: [
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
            "studio-runtime-http",
            "-p",
            "studio-runtime-store",
            "--lib",
            "-E",
            nextestAnyOf([
              "test(entity_only_commits_preserve_volatile_resources_and_refresh_entity_refs)",
              "test(resource_change_classifier_tests::)",
              "test(entity_resource_hints_cover_tombstones_deletes_and_unknown_commits)",
              "test(tracked_observer_tables_must_keep_rowid_storage)",
            ]),
          ],
        ],
      ],
      options: {
        selector:
          "typed entity-only commit hints and conservative broad refresh",
        reason:
          "Entity-only writes must wake entity-backed refs without advancing volatile resource versions; unknown and mixed writes retain broad invalidation.",
      },
    },
    {
      item: { name: "studio-runtime-http", ecosystem: "cargo" },
      label: "resource content invalidation witnesses",
      commands: [
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
            "studio-runtime-http",
            "-p",
            "studio-runtime-services",
            "--lib",
            "-E",
            nextestAnyOf([
              "test(unrelated_commits_do_not_reread_five_unchanged_resources)",
              "test(cost_content_ignores_account_listing_order)",
              "test(subscribed_content_is_computed_once_per_coalesced_revision)",
              "test(equal_racing_content_keeps_version_and_checks_the_pending_change)",
              "test(limits_content_reads_only_cached_account_snapshots)",
              "test(unavailable_and_racing_content_keep_the_safe_refresh)",
              "test(provider_versions::tests::preference_read_error_fails_closed_without_an_upstream_request)",
              "test(token_publications_do_not_repeat_or_invalidate_content_projections)",
              "test(drained_notification_effects_use_committed_state_without_resetting_entities)",
              "test(analytics_receipts_preserve_entity_delivery_but_recovery_still_resets)",
              "test(publication_bound_keeps_state_invalidation_and_token_rate_usage)",
            ]),
          ],
        ],
      ],
      options: {
        selector:
          "subscribed resource content and shared coalesced projections",
        reason:
          "Unrelated commits must not trigger resource re-reads or duplicate server projections; unknown and racing reads retain safe refreshes.",
      },
    },
    {
      item: { name: "studio-runtime-http", ecosystem: "cargo" },
      label: "shared named schema validation witnesses",
      commands: [
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
            "studio-runtime-http",
            "--lib",
            "--test",
            "integration",
            "-E",
            "test(validation::tests::named_schema_) | test(/^claude_repair_report::/)",
          ],
        ],
      ],
      options: {
        selector:
          "shared named validator startup and invalid-value error parity",
        reason:
          "Preserve named-schema validation and error locations while HTTP reuses the compiled schema owner.",
      },
    },
    {
      item: { name: "studio-runtime-http", ecosystem: "cargo" },
      label: "branch turn metadata lookup witnesses",
      commands: [
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
            "studio-runtime-http",
            "--lib",
            "-E",
            "test(routes::branches::tests::branch_)",
          ],
        ],
      ],
      options: {
        selector: "branch predecessor metadata and bounded history reads",
        reason:
          "Avoid decoding turn bodies and reading past the predecessor while preserving branch cutoffs and unknown fork outcomes.",
      },
    },
    {
      item: { name: "studio-runtime-http", ecosystem: "cargo" },
      label: "task page HTTP projection witness",
      commands: [
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
            "studio-runtime-http",
            "--lib",
            "-E",
            "test(task_page_http_keeps_summary_and_ignores_invalid_lookahead)",
          ],
        ],
      ],
      options: {
        selector: "task page summary and lookahead validation",
        reason:
          "Keep task summaries and pagination unchanged while selecting records once per page.",
      },
    },
    {
      item: { name: "studio-runtime-services", ecosystem: "cargo" },
      label:
        "monitor ownership, history lock boundaries, and recovery witnesses",
      commands: [
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
            "studio-runtime-services",
            "-p",
            "studio-runtime-server",
            "--bin",
            "studio-runtime-server",
            "--test",
            "integration",
            "--lib",
            "-E",
            nextestAnyOf([
              "test(exit_logging_tests::continuation_)",
              "test(child_result_release_consumes_)",
              "test(/^history_runtime::tests::slow_filesystem_metadata_does_not_block_a_store_observer$/)",
              "test(/^analytics_history_checkpoint::terminal_error_repair_is_idempotent_for_exact_and_ambiguous_notices$/)",
              "test(/^analytics_history_schedule::tests::/)",
              "test(/^monitor_start_preparation::/)",
              "test(monitor_port::tests::startup_resources_refuse_replacement)",
              "test(model_catalog::tests::)",
              "test(/^monitor_wake_guarantees::/)",
              "test(/^monitor_runtime_wiring::tests::monitor_scan_checks_each_unchanged_monitor_once_per_cycle$/)",
              "test(/^monitor_runtime_wiring::tests::monitor_scan_restarts_after_persistent_probe_failure$/)",
              "(not binary(integration) & (test(monitor_completion_) | test(reattached_exit_)))",
            ]),
          ],
        ],
      ],
      options: {
        selector:
          "monitor start ownership, exactly-once terminal continuation, and history lock boundaries",
        reason:
          "Check monitor starts across actor progress and turn boundaries, and bounded fair continuation retries, while preserving permission and ownership checks, and keep history metadata and completed repairs outside writer admission.",
      },
    },
    {
      item: { name: "studio-runtime-services", ecosystem: "cargo" },
      label: "Rust SQL boundary and agent progress witnesses",
      commands: [
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
            "studio-runtime-services",
            "-p",
            "studio-runtime-schema",
            "--lib",
            "-E",
            nextestAnyOf([
              "test(journal_fixture_directory_is_removed_on_return_and_unwind)",
              "test(nonempty_file_submission_rechecks_cwd_and_replays_without_filesystem_work)",
              "test(unrelated_reservation_started_after_path_capture_does_not_refuse_ready_token)",
              "test(room_membership_uses_symlink_paths_resolved_by_the_creation_owner)",
              "test(agent_progress::tests::)",
              "test(token_rate::tests::)",
              "test(completed_child_turn_reports_unsubmitted_assignment_once_with_final_text)",
              "test(account_worker::tests::continuation_start::capacity_retry_restart_unknown_reports_once_without_resending)",
              "test(agent_progress_schema_matches_strict_serde_contract)",
              "test(contracts::tests::generated_contracts_do_not_recreate_the_removed_backend)",
              "test(contracts::tests::markdown_output_matches_rust_owned_schema)",
              "test(disk_space::tests::)",
              "test(archive_finished_)",
              "test(archive_finished_reason_survives_dispatch_and_receipt_replay)",
              "test(committed_archive_)",
              "test(lead_archive_and_restore_run_through_the_lifecycle_owner)",
              "test(workspace_cleanup_runtime::tests::missing_archive_worktree_settles_without_external_effects)",
              "test(workspace_cleanup_runtime::tests::missing_worktree_subdirectory_does_not_settle_a_live_root)",
              "test(workspace_cleanup_runtime::tests::cleanup_failure_does_not_block_unrelated_queued_intents)",
              "test(status_snapshot_tracks_capacity_finished_pages_and_revision_replay)",
              "test(workspace_pending_reuses_reserved_attempt)",
              "test(long_answers_use_canonical_bounded_agent_sync_projection)",
            ]),
          ],
        ],
      ],
      options: {
        selector:
          "SQL callback boundaries and agent-list progress/turn-result witnesses",
        reason:
          "Keep filesystem work outside SQLite and verify typed progress, idle assignments, and one parent result per completed turn.",
      },
    },
    {
      item: { name: "studio-runtime-services", ecosystem: "cargo" },
      label: "task authority, worker limits, and Claude input witnesses",
      commands: [
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
            "studio-runtime-services",
            "-p",
            "studio-runtime-store",
            "-p",
            "studio-runtime-schema",
            "--lib",
            "-E",
            "test(authenticated_task_caller_) | test(acceptance_gate_) | test(task_result_schema) | test(capability_) | test(claude_child_) | test(workspace_fence) | test(input_origin) | test(stop_receipt_) | test(live_limits_) | test(archive_keeps_submodule) | test(turn_prepare::budget_tests::)",
          ],
        ],
      ],
      options: {
        selector:
          "task authority, acceptance gates, worker capability limits, and Claude input provenance",
        reason:
          "Check that workers cannot act as the lead, gates and limits hold, and Claude input keeps its origin and queue receipts.",
      },
    },
    {
      item: web,
      label: "web focused type check",
      options: { script: "typecheck:fast", allowZeroTests: true },
    },
    {
      item: web,
      label: "web API contract type check",
      options: { script: "test:api-contract-types", allowZeroTests: true },
    },
    {
      item: web,
      label: "Single token record footer and team witnesses",
      commands: [
        [
          "pnpm",
          [
            "--filter",
            "codex-agents-web",
            "exec",
            "vitest",
            "run",
            "--config",
            "vitest.unit.config.ts",
            "src/usage/tokenRate.records.test.ts",
            "src/usage/tokenRate.test.mjs",
          ],
        ],
      ],
      options: {
        selector: "one record, root moves, removals, strict bounds and replay",
        reason:
          "Token snapshots carry each agent once and derive team membership without entity projections.",
      },
    },
    {
      item: web,
      label: "web unit safety witnesses",
      options: { script: "test:unit:fast" },
    },
    {
      item: { name: "desktop disk status", ecosystem: "cargo" },
      label: "Desktop content identity and native attach witnesses",
      commands: [
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
            "studio-runtime-http",
            "-p",
            "studio-client-core",
            "--lib",
            "-E",
            nextestAnyOf([
              "test(desktop_content_ignores_healthy_disk_writes_but_delivers_visible_changes)",
              "test(desktop_http_)",
              "test(existing_backend_must_supply_a_valid_build_identity)",
            ]),
          ],
        ],
      ],
      options: {
        selector: "desktop disk projection, content filtering and Rust attach",
        reason:
          "Healthy disk writes must not cause renderer reads; visible changes and native identity checks remain live.",
      },
    },
    {
      item: web,
      label: "Desktop notice reader witnesses",
      commands: [
        [
          "pnpm",
          [
            "--filter",
            "codex-agents-web",
            "exec",
            "vitest",
            "run",
            "--config",
            "vitest.unit.config.ts",
            "src/components/SupervisorRecoveryNotice.test.ts",
            "src/components/desktopNotices.test.ts",
            "src/components/watchResourceReads.test.ts",
          ],
        ],
      ],
      options: {
        selector: "mounted desktop reader and retained disk notices",
        reason:
          "Read a stable baseline once and retain the full-disk notice across failed reads.",
      },
    },
    {
      item: { name: "studio-runtime-http", ecosystem: "cargo" },
      label: "Primary state stream and conversation revision witnesses",
      commands: [
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
            "studio-runtime-http",
            "--lib",
            "-E",
            "test(routes::sync_stream::tests::state_stream_) | test(unchanged_open_chat_ignores_unrelated_agent_commits)",
          ],
        ],
      ],
      options: {
        selector:
          "typed complete stream intervals, bounded flush and per-chat content revision",
        reason:
          "Deliver state without extra pulls while retaining gap recovery and unchanged-chat suppression.",
      },
    },
    {
      item: { name: "studio-runtime-http", ecosystem: "cargo" },
      label: "Draft stream content witness",
      commands: [
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
            "studio-runtime-http",
            "--lib",
            "-E",
            "test(unchanged_drafts_ignore_unrelated_commits_and_publications)",
          ],
        ],
      ],
      options: {
        selector: "unchanged draft revisions and retained tombstones",
        reason:
          "Unrelated commits must not request draft pulls; draft changes still notify.",
      },
    },
    {
      item: web,
      label: "Draft stream acceptance and durable recovery witnesses",
      commands: [
        [
          "pnpm",
          [
            "--filter",
            "codex-agents-web",
            "exec",
            "vitest",
            "run",
            "--config",
            "vitest.unit.config.ts",
            "src/sync/client.draftStream.test.ts",
          ],
        ],
      ],
      options: {
        selector:
          "draft stream acceptance, tombstones, gaps, reconnects and failed persistence",
        reason:
          "Normal draft changes use RxDB's master stream; recovery reads its durable checkpoint.",
      },
    },
    {
      item: web,
      label: "Draft stream transport ordering witness",
      commands: [
        [
          "pnpm",
          [
            "--filter",
            "codex-agents-web",
            "exec",
            "vitest",
            "run",
            "--config",
            "vitest.unit.config.ts",
            "src/sync/resourceEvents.test.ts",
            "-t",
            "consecutive draft batches",
          ],
        ],
      ],
      options: {
        selector:
          "lossless draft intervals and equal-version reconnect recovery",
        reason:
          "Coalescing hints must not discard draft data or mask a recovery baseline.",
      },
    },
    {
      item: web,
      label: "State stream persistence and recovery witnesses",
      commands: [
        [
          "pnpm",
          [
            "--filter",
            "codex-agents-web",
            "exec",
            "vitest",
            "run",
            "--config",
            "vitest.unit.config.ts",
            "src/sync/client.dispatchGate.test.ts",
            "src/sync/client.entityReset.test.ts",
            "src/sync/resourceEntityDelivery.test.ts",
            "src/sync/resourceEvents.test.ts",
            "src/servers/eventSource.test.ts",
          ],
        ],
      ],
      options: {
        selector:
          "persist stream data before dispatch and retain pull recovery",
        reason:
          "A complete state batch advances the durable projection with no new pull; storage failures, gaps, reconnects and epochs still reconcile.",
      },
    },
    {
      item: web,
      label: "UI preference projection ownership witnesses",
      commands: [
        [
          "pnpm",
          [
            "--filter",
            "codex-agents-web",
            "exec",
            "vitest",
            "run",
            "--config",
            "vitest.unit.config.ts",
            "src/servers/useUiPreferenceSync.test.ts",
            "src/sync/uiPreferenceBridge.test.ts",
          ],
        ],
      ],
      options: {
        selector: "UI preferences reuse each mounted server projection",
        reason:
          "Prevent duplicate entity pulls and preserve server storage and API ownership.",
      },
    },
    {
      item: web,
      label: "Native draft notification witnesses",
      commands: [
        [
          "pnpm",
          [
            "--filter",
            "codex-agents-web",
            "exec",
            "vitest",
            "run",
            "--config",
            "vitest.unit.config.ts",
            "src/native/useNativeDrafts.test.ts",
          ],
        ],
      ],
      options: {
        selector: "Native drafts idle and refresh work counts",
        reason:
          "Prevent idle polling and lost draft notifications during a refresh.",
      },
    },
    {
      item: web,
      label: "Delete readback generation witnesses",
      commands: [
        [
          "pnpm",
          [
            "--filter",
            "codex-agents-web",
            "exec",
            "vitest",
            "run",
            "--config",
            "vitest.unit.config.ts",
            "src/hooks/snapshotDeleteReadback.test.ts",
            "src/chatDeleteScope.test.ts",
          ],
        ],
      ],
      options: {
        selector:
          "Delete readback across superseded session and projection requests",
        reason:
          "An authoritative delete readback must settle even when another refresh owns the current render generation.",
      },
    },
    {
      item: { name: "codex-agents-desktop", ecosystem: "pnpm" },
      label: "Rust-only desktop startup witnesses",
      commands: [
        [
          "pnpm",
          [
            "--filter",
            "codex-agents-desktop",
            "exec",
            "vitest",
            "run",
            "--config",
            "vitest.config.mjs",
            "backend.test.mjs",
            "backend-selection.test.mjs",
          ],
        ],
      ],
      options: {
        selector: "Rust runtime startup and identity",
        reason:
          "Check startup without Python, binary build identity, existing-owner attachment, and uncertain connection refusals.",
      },
    },
    {
      item: packageTarget(packages, "studio-claude-bridge"),
      label: "Claude watchdog deadline witnesses",
      commands: [
        [
          "pnpm",
          [
            "--filter",
            "studio-claude-bridge",
            "exec",
            "vitest",
            "run",
            "--config",
            "vitest.config.mjs",
            "stall-watch.test.mjs",
          ],
        ],
      ],
      options: {
        selector: "idle watchdog and nearest tool deadline",
        reason:
          "Check zero idle timers, exact deadline recovery, and rearming on progress, results and Studio waits.",
      },
    },
    {
      item: tooling,
      label: "retired server action witness",
      commands: [
        [
          "node",
          [
            "--test",
            "workspaces/tooling/apps/repository-checks/tests/retired-server-action.test.mjs",
          ],
        ],
      ],
    },
    { item: tooling, label: "tooling tests" },
  ];
}

function comprehensiveTierCommands(root) {
  const packages = discover(root);
  const web = packageTarget(packages, "codex-agents-web");
  const bridge = packageTarget(packages, "studio-claude-bridge");
  const tooling = packageTarget(packages, "codex-studio-tooling");
  return [
    webWasmPreparation(root),
    {
      item: { name: "Rust HTTP fixture preparation", ecosystem: "cargo" },
      label: "Rust HTTP fixture preparation",
      commands: [
        [
          "cargo",
          ["build", "--locked", "--release", "-p", "studio-runtime-server"],
        ],
      ],
      options: { allowZeroTests: true, buildOnly: true },
    },
    { item: bridge, label: "provider bridge" },
    {
      // The native client crate embeds web/dist at compile time.
      item: { name: "renderer build", ecosystem: "pnpm" },
      label: "renderer build",
      commands: [
        ["pnpm", ["--filter", "codex-agents-web", "run", "build:bundle"]],
      ],
      options: { allowZeroTests: true, buildOnly: true },
    },
    {
      // The publication gate owns the same lint the CI Clippy job runs; the
      // justfile recipe is its single command owner.
      item: { name: "Rust Clippy", ecosystem: "cargo" },
      label: "Rust Clippy",
      commands: [["just", ["clippy-runtime"]]],
      options: { allowZeroTests: true, buildOnly: true },
    },
    {
      item: { name: "Rust workspace", ecosystem: "cargo" },
      label: "Rust nextest",
      commands: [
        ["cargo", ["build", "-p", "studio-process-supervisor", "--locked"]],
        [
          "cargo",
          [
            "nextest",
            "run",
            "--workspace",
            "--profile",
            "ci",
            "--locked",
            "--message-format",
            "libtest-json-plus",
          ],
        ],
      ],
    },
    {
      item: web,
      label: "web focused type check",
      options: { script: "typecheck:focused", allowZeroTests: true },
    },
    {
      item: web,
      label: "web API contract type check",
      options: { script: "test:api-contract-types", allowZeroTests: true },
    },
    {
      item: { name: "client helper type check", ecosystem: "pnpm" },
      label: "client helper type check",
      commands: [["pnpm", ["run", "typecheck:client-helper"]]],
      options: { allowZeroTests: true },
    },
    { item: web, label: "web unit tests" },
    { item: tooling, label: "tooling tests" },
    {
      item: web,
      label: "browser smoke subset",
      options: browserSmokeTestOptions(),
    },
  ];
}

function fastTierRestCommands(root) {
  return comprehensiveTierCommands(root).filter(
    ({ label }) =>
      ![
        "server",
        "Rust HTTP fixture preparation",
        "browser smoke subset",
      ].includes(label),
  );
}

function fastTierRestNonRustCommands(root) {
  return fastTierRestCommands(root).filter(
    ({ label }) => label !== "Rust nextest",
  );
}

function heavyTierCommands(root) {
  return [
    {
      label: "full browser project",
      item: packageTarget(discover(root), "codex-agents-web"),
      options: { ...browserTestOptions(), allowZeroTests: true },
    },
  ];
}

export function recipePlan(root, name) {
  const commands =
    name === "test-fast"
      ? fastTierCommands(root)
      : name === "test-comprehensive"
        ? comprehensiveTierCommands(root)
        : name === "test-fast-rest"
          ? fastTierRestCommands(root)
          : name === "test-fast-rest-non-rust"
            ? fastTierRestNonRustCommands(root)
            : heavyTierCommands(root);
  const commandLists = commands.map(
    ({ item, options = {}, commands: override }) =>
      override ?? commandFor(root, item, "test", "", options),
  );
  const fast =
    name === "test-fast" ||
    name === "test-comprehensive" ||
    name === "test-fast-rest" ||
    name === "test-fast-rest-non-rust";
  return {
    package: name,
    case: null,
    target: fast
      ? name === "test-fast"
        ? "repository short safety tier"
        : name === "test-comprehensive"
          ? "repository comprehensive regression tier"
          : name === "test-fast-rest"
            ? "comprehensive remainder excluding browser smoke"
            : "comprehensive non-Rust remainder excluding browser smoke"
      : "full browser project",
    workingDirectory: root,
    invocationDirectory: root,
    command: reproduction(root, commandLists.flat()),
    selector: fast
      ? name === "test-fast"
        ? "Rust retry and recovery witnesses, state schema contracts, web API safety witnesses, tooling and shared contract guards"
        : name === "test-comprehensive"
          ? "provider bridge, Rust workspace, web type and unit checks, tooling tests, browser smoke subset"
          : name === "test-fast-rest"
            ? "provider bridge, Rust workspace nextest, web type checks and unit tests, tooling tests"
            : "provider bridge, web type checks and unit tests, tooling tests"
      : "codex-agents-web test:browser",
    requiredSetup:
      name === "test-fast"
        ? "Frozen pnpm dependencies; Rust toolchain, pinned wasm-bindgen CLI and cargo-nextest; web WASM is generated by the gate; no browser or model provider"
        : "Frozen pnpm dependencies, Rust toolchain and cargo-nextest, browser binaries",
    reason: fast
      ? name === "test-fast"
        ? "Run selected deterministic safety witnesses and read-only contract guards in one sequential command."
        : name === "test-comprehensive"
          ? "Run the previous broad local regression recipe in one sequential command."
          : "Run the remaining checks from the comprehensive tier in one sequential command."
      : "Run the full browser project as an announced manual check.",
    notStarted: "Electron, provider-dependent opt-in suites, load-class suites",
    costCategory: fast
      ? name === "test-fast"
        ? "short safety witnesses; warm 30 s target"
        : "comprehensive repository regression checks"
      : "manual heavy checks; browser cap 900 s",
    executesNothing: true,
  };
}

export async function executeRecipe(root, name, steps) {
  const planned = recipePlan(root, name);
  const started = performance.now();
  for (const { item, label, options = {}, commands } of steps) {
    const result = await execute(root, item, "", false, {
      ...options,
      ...(commands ? { commands } : {}),
      label,
    });
    if (result.exitCode !== 0) {
      const durationSeconds = Number(
        ((performance.now() - started) / 1000).toFixed(3),
      );
      process.stdout.write(
        `${name}: failed at ${label}; ${durationSeconds}s\nCommand: ${planned.command}\n`,
      );
      return {
        ...result,
        package: name,
        failedStep: label,
        command: planned.command,
      };
    }
  }
  const durationSeconds = Number(
    ((performance.now() - started) / 1000).toFixed(3),
  );
  process.stdout.write(
    `${name}: passed; ${durationSeconds}s\nCommand: ${planned.command}\n`,
  );
  return {
    package: name,
    status: "passed",
    durationSeconds,
    command: planned.command,
    exitCode: 0,
  };
}

export function parseCounts(item, output) {
  // ANSI CSI sequences contain control characters by design.
  // oxlint-disable-next-line no-control-regex
  const normalizedOutput = output.replace(/\u001b\[[0-?]*[ -/]*[@-~]/g, "");
  if (item.ecosystem === "cargo") {
    return normalizedOutput.split("\n").reduce(
      (counts, line) => {
        let event;
        try {
          event = JSON.parse(line);
        } catch {
          return counts;
        }
        if (event.type !== "suite" || !["ok", "failed"].includes(event.event))
          return counts;
        counts.passed += event.passed ?? 0;
        counts.failed += event.failed ?? 0;
        counts.skipped += event.ignored ?? 0;
        counts.executed += (event.passed ?? 0) + (event.failed ?? 0);
        counts.summaryFound = true;
        return counts;
      },
      { passed: 0, failed: 0, skipped: 0, executed: 0, summaryFound: false },
    );
  }
  const testLines = normalizedOutput
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
  if (/^TAP version \d+\n1\.\.0$/m.test(normalizedOutput))
    return {
      passed: 0,
      failed: 0,
      skipped: 0,
      executed: 0,
      summaryFound: true,
    };
  const tap = normalizedOutput.match(
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
  const nodeSpec = normalizedOutput.match(
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

export function evaluatePlaywrightReport(reportText, processExitCode) {
  const errors = [];
  let failures = [];
  if (processExitCode !== 0)
    errors.push(`Playwright exited with code ${processExitCode}`);
  let report;
  try {
    if (reportText === null || reportText === undefined)
      throw new Error("Playwright JSON report is missing");
    report =
      typeof reportText === "string" ? JSON.parse(reportText) : reportText;
    failures = playwrightFailureSummaries(report);
  } catch (error) {
    errors.push(
      error.message === "Playwright JSON report is missing"
        ? error.message
        : `Playwright JSON report is invalid: ${error.message}`,
    );
  }
  const stats = report?.stats;
  const statNames = ["expected", "unexpected", "flaky", "skipped"];
  if (
    !stats ||
    statNames.some(
      (name) => !Number.isSafeInteger(stats[name]) || stats[name] < 0,
    )
  ) {
    if (report)
      errors.push("Playwright JSON report has invalid or missing stats");
    return {
      counts: {
        passed: null,
        failed: null,
        flaky: null,
        skipped: null,
        executed: null,
        summaryFound: false,
      },
      errors,
      failures,
      exitCode: 1,
    };
  }
  const counts = {
    passed: stats.expected,
    failed: stats.unexpected,
    flaky: stats.flaky,
    skipped: stats.skipped,
    executed: stats.expected + stats.unexpected + stats.flaky,
    summaryFound: true,
  };
  if (stats.unexpected > 0)
    errors.push(`Playwright reported ${stats.unexpected} unexpected test(s)`);
  if (stats.flaky > 0)
    errors.push(`Playwright reported ${stats.flaky} flaky test(s)`);
  if (counts.executed === 0) errors.push("Playwright executed zero tests");
  return {
    counts,
    errors,
    failures,
    exitCode: errors.length ? 1 : 0,
  };
}

function playwrightFailureSummaries(report) {
  const failures = [];
  const visitSuite = (suite, parents = []) => {
    const suiteTitles = suite.title ? [...parents, suite.title] : parents;
    for (const spec of suite.specs ?? []) {
      for (const test of spec.tests ?? []) {
        const results = test.results ?? [];
        if (
          test.status !== "unexpected" &&
          test.status !== "flaky" &&
          !results.some((result) =>
            ["failed", "timedOut"].includes(result.status),
          )
        )
          continue;
        const result = results.find((entry) =>
          ["failed", "timedOut", "interrupted"].includes(entry.status),
        );
        const error = result?.errors?.[0] ?? test.errors?.[0];
        const detail =
          typeof error === "string"
            ? error
            : (error?.message ??
              result?.status ??
              test.status ??
              "unexpected result");
        const firstLine = String(detail).split(/\r?\n/, 1)[0].trim();
        failures.push({
          title: [...suiteTitles, spec.title, test.projectName]
            .filter(Boolean)
            .join(" › "),
          error: firstLine || "no error message reported",
        });
      }
    }
    for (const child of suite.suites ?? []) visitSuite(child, suiteTitles);
  };
  for (const suite of report.suites ?? []) visitSuite(suite);
  return failures;
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
    child.on("error", (error) => {
      const message =
        command === "cargo" && args[0] === "nextest"
          ? `${NEXTTEST_MISSING}\n${error.message}`
          : `${output}\n${error.message}`;
      resolve({ code: 127, signal: null, output: message });
    });
    child.on("close", (code, signal) => {
      for (const [name, handler] of handlers)
        process.removeListener(name, handler);
      resolve({ code, signal: signal || interrupted, output });
    });
  });
}

const NEXTTEST_INSTALL =
  "cargo install cargo-nextest --version 0.9.146 --locked";
const NEXTTEST_MISSING = `cargo-nextest 0.9.146 is required. Install it with: ${NEXTTEST_INSTALL}`;

export function nextestError(output) {
  return output.includes(NEXTTEST_MISSING) ||
    /no such command.*nextest|could not find.*cargo-nextest/i.test(output)
    ? NEXTTEST_MISSING
    : null;
}

export function nextestSelectionError(exitCode) {
  return exitCode === 4
    ? "nextest found no tests matching this selection"
    : null;
}

export async function execute(
  root,
  item,
  caseName,
  asJson = false,
  options = {},
) {
  const commands =
    options.commands ?? commandFor(root, item, "test", caseName, options);
  const run = plan(root, item, "test", caseName, commands, options);
  const logDir = makeLogDirectory();
  const logPath = path.join(logDir, "full.log");
  const playwrightReportPath = options.playwrightJsonReport
    ? path.join(logDir, "playwright-results.json")
    : null;
  mkdirSync(logDir, { recursive: true });
  appendFileSync(logPath, "");
  const started = performance.now();
  let counts = {
    passed: null,
    failed: null,
    flaky: null,
    skipped: null,
    executed: null,
  };
  let exitCode = 0;
  let signal = null;
  let output = "";
  let playwrightFailures = [];
  const errors = [];
  for (const [bin, args] of commands) {
    const baseEnv =
      bin === "cargo"
        ? {
            ...process.env,
            CARGO_TARGET_DIR: cargoTarget(root),
            ...(args[0] === "nextest"
              ? {
                  NEXTEST_EXPERIMENTAL_LIBTEST_JSON: "1",
                  STUDIO_SUPERVISOR_BINARY: path.join(
                    cargoTarget(root),
                    "debug",
                    process.platform === "win32"
                      ? "codex-process-supervisor.exe"
                      : "codex-process-supervisor",
                  ),
                  CODEX_NODE: process.execPath,
                }
              : {}),
          }
        : process.env;
    const env = playwrightReportPath
      ? {
          ...baseEnv,
          CODEX_STUDIO_PLAYWRIGHT_JSON_REPORT: playwrightReportPath,
        }
      : baseEnv;
    const child = await runLogged(bin, args, root, logPath, env);
    const missingNextest =
      bin === "cargo" && args[0] === "nextest"
        ? nextestError(child.output)
        : null;
    if (missingNextest) {
      errors.push(missingNextest);
      appendFileSync(logPath, `${missingNextest}\n`);
    }
    output += missingNextest ?? child.output;
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
  if (playwrightReportPath) {
    const reportText = existsSync(playwrightReportPath)
      ? readFileSync(playwrightReportPath, "utf8")
      : null;
    const reportResult = evaluatePlaywrightReport(reportText, exitCode);
    counts = reportResult.counts;
    playwrightFailures = reportResult.failures;
    errors.push(...reportResult.errors);
  } else {
    counts = parseCounts(item, output);
  }
  const selectionError = nextestSelectionError(exitCode);
  if (selectionError) errors.push(selectionError);
  if (nextestError(output) && !errors.includes(NEXTTEST_MISSING))
    errors.push(NEXTTEST_MISSING);
  if (
    exitCode === 0 &&
    item.ecosystem !== "pnpm" &&
    !options.buildOnly &&
    !counts.summaryFound
  )
    errors.push(
      "native runner summary was missing; refusing to report success",
    );
  if (
    exitCode === 0 &&
    counts.executed === 0 &&
    !options.allowZeroTests &&
    !playwrightReportPath
  )
    errors.push("native runner executed zero tests");
  if (errors.length && (!playwrightReportPath || exitCode === 0)) exitCode = 1;
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
    ...(playwrightReportPath ? { reportPath: playwrightReportPath } : {}),
    ...(playwrightReportPath ? { failures: playwrightFailures } : {}),
    requiredSetup: run.requiredSetup,
    costCategory: run.costCategory,
    notStarted: run.notStarted,
    exitCode,
    ...(signal ? { signal } : {}),
    ...(errors.length ? { errors } : {}),
  };
  const playwrightFailureOutput = playwrightFailures.length
    ? `Failing Playwright tests:\n${playwrightFailures.map(({ title, error }) => `- ${title}: ${error}`).join("\n")}\n`
    : "";
  if (asJson) process.stdout.write(`${JSON.stringify(result)}\n`);
  else
    process.stdout.write(
      `${options.label ?? item.name}${caseName ? ` / ${caseName}` : ""}: ${result.status}${counts.passed === null ? "" : ` — ${counts.passed} passed, ${counts.failed} failed${counts.flaky === null || counts.flaky === undefined ? "" : `, ${counts.flaky} flaky`}, ${counts.skipped} skipped`}${counts.executed === 0 && !options.allowZeroTests ? " — zero tests executed" : ""}; ${durationSeconds}s\nCommand: ${run.command}\nFull log: ${logPath}${playwrightReportPath ? `\nPlaywright JSON report: ${playwrightReportPath}` : ""}\n${playwrightFailureOutput}`,
    );
  return result;
}

function usage() {
  process.stdout.write(
    "Usage: facade.mjs packages | test <package> [file-or-case] [--name <pattern>] [-- <cargo args>] [--json] | test-plan <package> [file-or-case] [--name <pattern>] [-- <cargo args>] [--json] | test-fast | test-comprehensive | test-fast-rest | test-fast-rest-non-rust | test-heavy | test-browser | test-browser-smoke | check <package> [--dry-run] [--json] | contracts-check | boundaries-check | check-affected <base> [--dry-run]\n",
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

function workspaceRustFiles(root) {
  const files = [];
  function visit(directory) {
    for (const entry of readdirSync(directory, { withFileTypes: true })) {
      if (entry.isDirectory() && SOURCE_SKIP.has(entry.name)) continue;
      const full = path.join(directory, entry.name);
      const relative = path.relative(root, full).split(path.sep).join("/");
      if (
        entry.isDirectory() &&
        (relative === "workspaces/runtime/apps/layr" ||
          relative === "workspaces/runtime/apps/layr-desk")
      )
        continue;
      if (entry.isDirectory()) visit(full);
      else if (entry.isFile() && entry.name.endsWith(".rs")) files.push(full);
    }
  }
  const workspaces = path.join(root, "workspaces");
  if (existsSync(workspaces)) visit(workspaces);
  return files;
}

export function unsafeRustFindings(root, files = workspaceRustFiles(root)) {
  const findings = [];
  for (const file of files) {
    const relativeFile = path.relative(root, file).split(path.sep).join("/");
    if (UNSAFE_RUST_ALLOWLIST_BY_FILE.has(relativeFile)) continue;
    const source = maskRustCommentsAndStrings(readFileSync(file, "utf8"));
    if (/\bunsafe\b|\bunsafe_code\b/.test(source))
      findings.push({
        file: relativeFile,
        message: `unsafe code or allow(unsafe_code) is not allowlisted; see the purpose list in this check`,
      });
  }
  return findings;
}

function maskRustCommentsAndStrings(source) {
  const chars = [...source];
  const blank = (start, end) => {
    for (let index = start; index < end; index++)
      if (chars[index] !== "\n") chars[index] = " ";
  };
  for (let index = 0; index < chars.length;) {
    if (chars[index] === "/" && chars[index + 1] === "/") {
      let end = index + 2;
      while (end < chars.length && chars[end] !== "\n") end++;
      blank(index, end);
      index = end;
      continue;
    }
    if (chars[index] === "/" && chars[index + 1] === "*") {
      let depth = 1;
      let end = index + 2;
      while (end < chars.length && depth > 0) {
        if (chars[end] === "/" && chars[end + 1] === "*") {
          depth++;
          end += 2;
        } else if (chars[end] === "*" && chars[end + 1] === "/") {
          depth--;
          end += 2;
        } else end++;
      }
      blank(index, end);
      index = end;
      continue;
    }
    const charQuote =
      chars[index] === "'"
        ? index
        : chars[index] === "b" && chars[index + 1] === "'"
          ? index + 1
          : -1;
    if (charQuote >= 0) {
      let end = charQuote + 1;
      if (chars[end] === "\\") {
        end++;
        if (chars[end] === "u" && chars[end + 1] === "{") {
          end += 2;
          while (end < chars.length && chars[end] !== "}") end++;
          if (chars[end] === "}") end++;
        } else if (end < chars.length) end++;
      } else if (end < chars.length && chars[end] !== "\n") {
        end++;
      }
      if (chars[end] === "'") {
        blank(index, end + 1);
        index = end + 1;
        continue;
      }
    }
    const rawStart =
      chars[index] === "r"
        ? index
        : chars[index] === "b" && chars[index + 1] === "r"
          ? index + 1
          : -1;
    if (rawStart >= 0) {
      let quote = rawStart + 1;
      while (chars[quote] === "#") quote++;
      if (chars[quote] === '"') {
        const hashes = quote - rawStart - 1;
        let end = quote + 1;
        while (
          end < chars.length &&
          !(
            chars[end] === '"' &&
            chars.slice(end + 1, end + 1 + hashes).every((char) => char === "#")
          )
        )
          end++;
        end = Math.min(chars.length, end + 1 + hashes);
        blank(index, end);
        index = end;
        continue;
      }
    }
    const stringStart =
      chars[index] === '"' ||
      (chars[index] === "b" && chars[index + 1] === '"');
    if (stringStart) {
      const start = index;
      if (chars[index] === "b") index++;
      index++;
      while (index < chars.length) {
        if (chars[index] === "\\") index += 2;
        else if (chars[index++] === '"') break;
        else continue;
      }
      blank(start, index);
      continue;
    }
    index++;
  }
  return chars.join("");
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
  // JavaScript files belong to pnpm packages. A Cargo crate inside a pnpm app
  // (such as a Tauri src-tauri crate) has no JavaScript manifest of its own.
  const rooted = packages
    .filter((item) => item.ecosystem !== "cargo")
    .map((item) => ({ ...item, root }));
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
  const unsafeFindings = unsafeRustFindings(root);
  if (findings.length || unsafeFindings.length) {
    for (const finding of findings)
      process.stderr.write(
        `${finding.rule}: ${finding.file}: ${finding.message} (${finding.specifier})\n`,
      );
    for (const finding of unsafeFindings)
      process.stderr.write(
        `unsafe-code-policy: ${finding.file}: ${finding.message}\n`,
      );
    return 1;
  }
  process.stdout.write(
    `Workspace boundaries and unsafe-code policy passed (${packages.length} packages; ${BOUNDARY_ALLOWLIST.length} documented crossings and ${UNSAFE_RUST_ALLOWLIST.length} purpose-listed unsafe modules allowlisted).\n`,
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
  for (const script of ["contracts:check"]) {
    process.stdout.write(
      `Checking generated contracts without writing files: pnpm run ${script}\n`,
    );
    const result = spawnSync("pnpm", ["run", script], {
      cwd: root,
      stdio: "inherit",
    });
    if (result.status !== 0) return result.status ?? 1;
  }
  return 0;
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
  const root = rootFrom();
  if (action === "test-fast") {
    const result = await executeRecipe(
      root,
      "test-fast",
      fastTierCommands(root),
    );
    return result.exitCode;
  }
  if (action === "test-comprehensive") {
    const result = await executeRecipe(
      root,
      "test-comprehensive",
      comprehensiveTierCommands(root),
    );
    return result.exitCode;
  }
  if (action === "test-fast-rest") {
    const result = await executeRecipe(
      root,
      "test-fast-rest",
      fastTierRestCommands(root),
    );
    return result.exitCode;
  }
  if (action === "test-fast-rest-non-rust") {
    const result = await executeRecipe(
      root,
      "test-fast-rest-non-rust",
      fastTierRestNonRustCommands(root),
    );
    return result.exitCode;
  }
  if (action === "test-heavy") {
    const result = await executeRecipe(
      root,
      "test-heavy",
      heavyTierCommands(root),
    );
    return result.exitCode;
  }
  if (action === "test-browser") {
    const item = packageTarget(discover(root), "codex-agents-web");
    return (await execute(root, item, "", json, browserTestOptions())).exitCode;
  }
  if (action === "test-browser-smoke") {
    const item = packageTarget(discover(root), "codex-agents-web");
    return (await execute(root, item, "", json, browserSmokeTestOptions()))
      .exitCode;
  }
  if (!["test", "test-plan", "check"].includes(action)) {
    usage();
    return action ? 2 : 0;
  }
  if (action === "test-plan") {
    if (
      packageName === "test-fast" ||
      packageName === "test-comprehensive" ||
      packageName === "test-heavy"
    ) {
      reportPlan(recipePlan(root, packageName), json);
      return 0;
    }
    if (
      packageName === "test-fast-rest" ||
      packageName === "test-fast-rest-non-rust"
    ) {
      reportPlan(recipePlan(root, packageName), json);
      return 0;
    }
    if (packageName === "test-browser") {
      reportPlan(browserPlan(root), json);
      return 0;
    }
    if (packageName === "test-browser-smoke") {
      reportPlan(browserPlan(root, true), json);
      return 0;
    }
  }
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
    const errors = [];
    for (const [bin, args] of commands) {
      const env =
        bin === "cargo"
          ? {
              ...process.env,
              CARGO_TARGET_DIR: cargoTarget(root),
              ...(args[0] === "nextest"
                ? {
                    NEXTEST_EXPERIMENTAL_LIBTEST_JSON: "1",
                    CODEX_NODE: process.execPath,
                  }
                : {}),
            }
          : process.env;
      const result = await runLogged(bin, args, root, log, env);
      const missingNextest =
        bin === "cargo" && args[0] === "nextest"
          ? nextestError(result.output)
          : null;
      const selectionError =
        bin === "cargo" && args[0] === "nextest"
          ? nextestSelectionError(result.code)
          : null;
      if (missingNextest) errors.push(missingNextest);
      if (selectionError) errors.push(selectionError);
      output += [missingNextest, result.output].filter(Boolean).join("\n");
      if (missingNextest) appendFileSync(log, `${missingNextest}\n`);
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
      ...(errors.length ? { errors } : {}),
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
