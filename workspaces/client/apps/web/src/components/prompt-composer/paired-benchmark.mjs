import { spawnSync } from "node:child_process";
import { existsSync } from "node:fs";
import {
  copyFile,
  mkdir,
  mkdtemp,
  readFile,
  rm,
  symlink,
  writeFile,
} from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";

const BASELINE_REVISION = "7066718";
const commandTimeoutMs = 90_000;
const benchmarkGlobalTimeoutMs = 360_000;
const benchmarkCommandTimeoutMs = 390_000;
const repo = fileURLToPath(new URL("../../../../../../../", import.meta.url));
const web = join(repo, "workspaces/client/apps/web");
const tests = join(web, "tests");
const playwrightConfig = join(web, "playwright.config.ts");
const requiredPaths = [
  join(web, "dist/index.html"),
  playwrightConfig,
  join(tests, "playwright.mjs"),
  join(web, "src/components/prompt-composer/renderProbe.ts"),
  join(repo, "workspaces/runtime/apps/server/tests/simple-ui-fixture.py"),
  join(repo, "workspaces/runtime/apps/server/tests/runtime-contract.py"),
  join(tests, "performance/draft-render-performance-browser.spec.mjs"),
  join(tests, "performance/ui-responsiveness-browser.spec.mjs"),
];
const missingPaths = requiredPaths.filter((path) => !existsSync(path));
if (missingPaths.length)
  throw new Error(
    `Paired prompt composer benchmark is missing: ${missingPaths.join(", ")}`,
  );
const temporaryRoot = await mkdtemp(
  join(tmpdir(), "studio-prompt-composer-pair-"),
);
const baseline = join(temporaryRoot, "baseline");
const baselineWeb = join(baseline, "web");
let baselineWorktreeAdded = false;

function command(
  program,
  args,
  cwd,
  extraEnv = {},
  timeoutMs = commandTimeoutMs,
) {
  const result = spawnSync(program, args, {
    cwd,
    env: { ...process.env, ...extraEnv },
    stdio: "inherit",
    timeout: timeoutMs,
  });
  if (result.error) throw result.error;
  if (result.signal) throw new Error(`${program} ${args.join(" ")} timed out`);
  if (result.status !== 0)
    throw new Error(
      `${program} ${args.join(" ")} exited with ${result.status}`,
    );
}

async function insertOnce(path, anchor, addition, prepend = false) {
  let source = await readFile(path, "utf8");
  if (source.includes(addition.trim())) return;
  if (prepend) source = `${addition}\n${source}`;
  else {
    if (!source.includes(anchor))
      throw new Error(`Benchmark probe anchor is missing in ${path}`);
    source = source.replace(anchor, `${addition}\n${anchor}`);
  }
  await writeFile(path, source);
}

async function prepareBaseline() {
  command(
    "git",
    ["worktree", "add", "--detach", "--no-checkout", baseline],
    repo,
  );
  baselineWorktreeAdded = true;
  command(
    "git",
    ["-C", baseline, "sparse-checkout", "init", "--no-cone"],
    repo,
  );
  command(
    "git",
    [
      "-C",
      baseline,
      "sparse-checkout",
      "set",
      "--no-cone",
      "/web/**",
      "/web/playwright.config.ts",
      "/tests/client/performance/draft-render-performance-browser.spec.mjs",
      "/tests/client/performance/ui-responsiveness-browser.spec.mjs",
      "/tests/client/playwright.mjs",
      "/tests/client/playwright.d.mts",
      "/tests/simple-ui-fixture.py",
      "/tests/test_isolation.py",
      "/tests/runtime-contract.py",
      "/scripts/claude_bridge/**",
      "/scripts/codex_canvas.py",
      "/scripts/codex_runtime.py",
      "/scripts/**",
      "/package.json",
      "/package-lock.json",
      "/requirements.txt",
      "/requirements-dev.txt",
      "/.oxfmtrc.json",
    ],
    repo,
  );
  command(
    "git",
    ["-C", baseline, "checkout", "--detach", BASELINE_REVISION],
    repo,
  );
  await symlink(
    join(web, "node_modules"),
    join(baselineWeb, "node_modules"),
    "dir",
  );
  await symlink(
    join(repo, "node_modules"),
    join(baseline, "node_modules"),
    "dir",
  );
  command("npm", ["run", "api:generate"], baseline);
  const probeDir = join(baselineWeb, "src/components/prompt-composer");
  await mkdir(probeDir, { recursive: true });
  await copyFile(
    join(web, "src/components/prompt-composer/renderProbe.ts"),
    join(probeDir, "renderProbe.ts"),
  );
  await insertOnce(
    join(baselineWeb, "src/App.tsx"),
    'import { useSyncedDrafts } from "./sync/drafts";',
    'import { reportPromptComposerRender } from "./components/prompt-composer/renderProbe";',
  );
  await insertOnce(
    join(baselineWeb, "src/App.tsx"),
    "  const outbox = useOutbox();",
    '  reportPromptComposerRender("app");',
  );
  await insertOnce(
    join(baselineWeb, "src/components/Sidebar.tsx"),
    "",
    'import { reportPromptComposerRender } from "./prompt-composer/renderProbe";',
    true,
  );
  await insertOnce(
    join(baselineWeb, "src/components/Sidebar.tsx"),
    '  const compact = useMediaQuery("(max-width: 760px)");',
    '  reportPromptComposerRender("sidebar");',
  );
  await insertOnce(
    join(baselineWeb, "src/components/Conversation.tsx"),
    "",
    'import { reportPromptComposerRender } from "./prompt-composer/renderProbe";',
    true,
  );
  await insertOnce(
    join(baselineWeb, "src/components/Conversation.tsx"),
    '  const mobileClient = useMediaQuery("(max-width: 760px)");',
    '  reportPromptComposerRender("conversation");',
  );
  command("npm", ["run", "build"], baselineWeb);

  const baselinePerformanceTests = join(baseline, "tests/performance");
  await mkdir(baselinePerformanceTests, { recursive: true });
  for (const name of [
    "draft-render-performance-browser.spec.mjs",
    "ui-responsiveness-browser.spec.mjs",
  ])
    await copyFile(
      join(tests, "performance", name),
      join(baselinePerformanceTests, name),
    );
  await copyFile(
    join(tests, "playwright.mjs"),
    join(baseline, "tests/playwright.mjs"),
  );
  await copyFile(
    join(web, "playwright.config.ts"),
    join(baselineWeb, "playwright.config.ts"),
  );
}

function runCurrentSpec(spec) {
  command(
    "pnpm",
    [
      "exec",
      "playwright",
      "test",
      "--config",
      playwrightConfig,
      "--project=performance",
      "--global-timeout",
      String(benchmarkGlobalTimeoutMs),
      join(tests, "performance", spec),
    ],
    web,
    { PLAYWRIGHT_INCLUDE_SPECIAL: "1" },
    benchmarkCommandTimeoutMs,
  );
}

function runBaselineSpec(spec) {
  command(
    process.execPath,
    [
      join(baselineWeb, "node_modules/@playwright/test/cli.js"),
      "test",
      "--config",
      join(baselineWeb, "playwright.config.ts"),
      "--project=performance",
      "--global-timeout",
      String(benchmarkGlobalTimeoutMs),
      join(baseline, "tests/performance", spec),
    ],
    baselineWeb,
    {
      PLAYWRIGHT_INCLUDE_SPECIAL: "1",
      RENDER_ISOLATION: "baseline",
    },
    benchmarkCommandTimeoutMs,
  );
}

async function runPair(round) {
  process.stdout.write(`\nBaseline draft fixture, round ${round}\n`);
  runBaselineSpec("draft-render-performance-browser.spec.mjs");
  process.stdout.write(`Current draft fixture, round ${round}\n`);
  runCurrentSpec("draft-render-performance-browser.spec.mjs");
}

try {
  await prepareBaseline();
  await runPair(1);
  await runPair(2);
  process.stdout.write("\nBaseline production UI fixture\n");
  runBaselineSpec("ui-responsiveness-browser.spec.mjs");
  process.stdout.write("Current production UI fixture\n");
  runCurrentSpec("ui-responsiveness-browser.spec.mjs");
} finally {
  if (baselineWorktreeAdded)
    command("git", ["worktree", "remove", "--force", baseline], repo);
  await rm(temporaryRoot, { recursive: true, force: true });
}
