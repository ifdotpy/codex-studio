// Build cumulative variants without changing the working tree or its normal dist.
import { execFileSync } from "node:child_process";
import { mkdir, readFile, writeFile } from "node:fs/promises";
import { isAbsolute, join, relative } from "node:path";
import { fileURLToPath } from "node:url";
import { createHash } from "node:crypto";
import type { InlineConfig } from "vite";
import { required } from "./performance-fixture.ts";
import { build, loadConfigFromFile, mergeConfig } from "vite";

const root = fileURLToPath(new URL("../../", import.meta.url));
const repo = execFileSync("git", ["rev-parse", "--show-toplevel"], {
  cwd: root,
  encoding: "utf8",
}).trim();
const [ref, output] = process.argv.slice(2);
if (
  !ref ||
  !output ||
  !isAbsolute(output) ||
  !relative(repo, output).startsWith("..")
)
  throw new Error(
    "Usage: node tests/performance/build-interaction-variants.mjs BASELINE_COMMIT ABSOLUTE_OUTPUT_OUTSIDE_REPO",
  );
const commit = execFileSync(
  "git",
  ["rev-parse", "--verify", `${ref}^{commit}`],
  { cwd: repo, encoding: "utf8" },
).trim();
const groups = [
  ["src/api.ts", "src/native/preferenceState.ts"],
  ["src/native/NativeMarkdown.tsx"],
  ["src/components/conversation/transcript/HistoryWindow.tsx"],
  ["src/App.tsx"],
];
const files = groups.flat();
const before = new Map(
  files.map((path) => [
    path,
    execFileSync(
      "git",
      ["show", `${commit}:${relative(repo, join(root, path))}`],
      { cwd: repo, encoding: "utf8" },
    ),
  ]),
);
const current = new Map(
  await Promise.all(
    files.map(async (path): Promise<[string, string]> => [
      path,
      await readFile(join(root, path), "utf8"),
    ]),
  ),
);
const supportPath = "src/components/shell/DeferredWorkspace.tsx";
const supportHash = createHash("sha256")
  .update(await readFile(join(root, supportPath)))
  .digest("hex");
const loaded = await loadConfigFromFile(
  { command: "build", mode: "production" },
  join(root, "vite.config.ts"),
);
const labels = [
  "baseline",
  "save-dedup",
  "completed-markdown",
  "scroll-checkpoint",
  "workspace-open",
];
const variants = [];
for (const [stage, label] of labels.entries()) {
  if (
    process.env.STUDIO_BENCHMARK_BUILD_LABEL &&
    label !== process.env.STUDIO_BENCHMARK_BUILD_LABEL
  )
    continue;
  const changed = new Set(groups.slice(0, stage).flat());
  const sources = new Map(
    files.map((path) => [
      join(root, path),
      required((changed.has(path) ? current : before).get(path)),
    ]),
  );
  const dist = join(output, label);
  await build(
    mergeConfig(required(loaded).config, {
      configFile: false,
      root,
      build: { outDir: dist, emptyOutDir: false },
      plugins: [
        {
          name: "interaction-variant",
          enforce: "pre",
          transform(_code, id) {
            const code = sources.get(id.split("?")[0] ?? id);
            if (code !== undefined) return { code, map: null };
          },
        },
      ],
    } satisfies InlineConfig),
  );
  variants.push({
    label,
    dist,
    baselineCommit: commit,
    supportSources: { [supportPath]: supportHash },
    sources: Object.fromEntries(
      [...sources].map(([path, code]) => [
        relative(root, path),
        createHash("sha256").update(code).digest("hex"),
      ]),
    ),
  });
}
await mkdir(output, { recursive: true });
await writeFile(
  join(output, "variants.json"),
  JSON.stringify(variants, null, 2),
);
