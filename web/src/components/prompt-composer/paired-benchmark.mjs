import { spawnSync } from "node:child_process";
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
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const BASELINE_REVISION = "2470fb5ac877559c70a74a8c76b3617db66cf141";
const repo = dirname(
  dirname(dirname(dirname(dirname(fileURLToPath(import.meta.url))))),
);
const tests = join(repo, "tests");
const temporaryRoot = await mkdtemp(
  join(tmpdir(), "studio-prompt-composer-pair-"),
);
const baseline = join(temporaryRoot, "baseline");
const baselineWeb = join(baseline, "web");
let baselineWorktreeAdded = false;

function command(program, args, cwd, extraEnv = {}) {
  const result = spawnSync(program, args, {
    cwd,
    env: { ...process.env, ...extraEnv },
    stdio: "inherit",
    timeout: 90_000,
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
      "/tests/draft-render-performance-browser.mjs",
      "/tests/ui-responsiveness-browser.mjs",
      "/tests/simple-ui-fixture.py",
      "/tests/runtime-contract.py",
      "/scripts/claude_bridge/**",
      "/scripts/**",
    ],
    repo,
  );
  command(
    "git",
    ["-C", baseline, "checkout", "--detach", BASELINE_REVISION],
    repo,
  );
  await symlink(
    join(repo, "web/node_modules"),
    join(baselineWeb, "node_modules"),
    "dir",
  );
  const probeDir = join(baselineWeb, "src/components/prompt-composer");
  await mkdir(probeDir, { recursive: true });
  await copyFile(
    join(repo, "web/src/components/prompt-composer/renderProbe.ts"),
    join(probeDir, "renderProbe.ts"),
  );
  for (const name of [
    "draft-render-performance-browser.mjs",
    "ui-responsiveness-browser.mjs",
  ])
    await copyFile(join(tests, name), join(baseline, "tests", name));

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
}

async function runPair(round) {
  process.stdout.write(`\nBaseline draft fixture, round ${round}\n`);
  command(
    process.execPath,
    [join(baseline, "tests/draft-render-performance-browser.mjs")],
    baseline,
    { RENDER_ISOLATION: "baseline" },
  );
  process.stdout.write(`Current draft fixture, round ${round}\n`);
  command(
    process.execPath,
    [join(tests, "draft-render-performance-browser.mjs")],
    repo,
  );
}

try {
  await prepareBaseline();
  await runPair(1);
  await runPair(2);
  if (process.env.DRAFT_HOOK_ONLY !== "1") {
    process.stdout.write("\nBaseline production UI fixture\n");
    command(
      process.execPath,
      [join(baseline, "tests/ui-responsiveness-browser.mjs")],
      baseline,
      { RENDER_ISOLATION: "baseline" },
    );
    process.stdout.write("Current production UI fixture\n");
    command(
      process.execPath,
      [join(tests, "ui-responsiveness-browser.mjs")],
      repo,
    );
  }
} finally {
  if (baselineWorktreeAdded)
    command("git", ["worktree", "remove", "--force", baseline], repo);
  await rm(temporaryRoot, { recursive: true, force: true });
}
