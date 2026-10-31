// A passing gate step is a build artifact: its result is reused while its
// inputs keep the same content, and any input change runs it again.
import { execFileSync } from "node:child_process";
import { createHash } from "node:crypto";
import fs from "node:fs";
import path from "node:path";
import type { GateStep, StepResult } from "./facade.ts";

/** The step fields that key a cached result. */
type CachedStep = {
  [K in "commands" | "inputs" | "options"]?: GateStep[K] | undefined;
} & {
  id?: string | undefined;
};

/** The `cargo metadata --format-version 1` fields used for crate keys. */
export interface CargoMetadata {
  workspace_members: string[];
  packages: { id: string; name: string; manifest_path: string }[];
  resolve: { nodes: { id: string; deps: { pkg: string }[] }[] };
}

/** Fields of a passing result kept on disk. */
type SavedResult = StepResult & {
  passed?: number;
  failed?: number;
  skipped?: number;
  executed?: number;
};

/** Paths every cached step depends on, in addition to its own inputs. */
export const SHARED_INPUTS = [
  "package.json",
  "pnpm-lock.yaml",
  "pnpm-workspace.yaml",
  "Cargo.lock",
  "rust-toolchain.toml",
  "justfile",
  "workspaces/tooling/apps/repository-checks",
  // Files that Rust tests and build scripts read from outside their crate folder.
  "clippy.toml",
  ".agents",
  "workspaces/client/apps/desktop/desktop_vm.py",
];

/** Folder of passing results. It is under target/, which layr keeps as a warm
 * cache, so a result made in one line reaches the lines that start from it. */
export const RESULT_DIRECTORY = path.join("target", "studio-test-results");

/** Environment variable naming an absolute folder of passing results shared by
 * every checkout. Keys hash input content, so a result is valid in any checkout. */
export const SHARED_RESULT_DIRECTORY_ENV = "STUDIO_TEST_RESULTS_DIR";

const contentHashes = new Map<string, string>();

/** SHA-256 of a file, kept for the process by path, size and modification time.
 * A deleted file hashes to a fixed marker. */
function fileHash(file: string): string {
  try {
    const stat = fs.statSync(file);
    const memo = `${file}:${stat.size}:${stat.mtimeMs}`;
    let hash = contentHashes.get(memo);
    if (!hash) {
      hash = createHash("sha256").update(fs.readFileSync(file)).digest("hex");
      contentHashes.set(memo, hash);
    }
    return hash;
  } catch {
    return "absent";
  }
}

/** Content key of a step, or undefined when the step must run. The key hashes the
 * working-tree content of every tracked or unignored file under the inputs, so it is the same
 * for a saved and an unsaved tree with equal content. */
export function stepKey(
  root: string,
  step: CachedStep,
  env: NodeJS.ProcessEnv = process.env,
): string | undefined {
  if (env.STUDIO_TEST_REUSE === "0" || !step.inputs?.length) return undefined;
  const paths = [...SHARED_INPUTS, ...step.inputs];
  try {
    const files = [
      ...new Set(
        execFileSync(
          "git",
          [
            "ls-files",
            "-z",
            "--cached",
            "--others",
            "--exclude-standard",
            "--",
            ...paths,
          ],
          { cwd: root, encoding: "utf8", maxBuffer: 1 << 28 },
        )
          .split("\0")
          .filter(Boolean),
      ),
    ].sort();
    return createHash("sha256")
      .update(
        JSON.stringify([
          step.id,
          step.commands ?? step.options?.script,
          files.map((file) => [file, fileHash(path.join(root, file))]),
          process.version,
          process.platform,
          process.arch,
        ]),
      )
      .digest("hex");
  } catch {
    return undefined;
  }
}

/** Key of a gate step. A step without declared inputs depends on the whole tree. A step that
 * provides an artifact to later steps always runs. */
export function cachedStepKey(
  root: string,
  step: CachedStep & { provides?: string | undefined },
  env: NodeJS.ProcessEnv = process.env,
): string | undefined {
  return step.provides
    ? undefined
    : stepKey(root, { ...step, inputs: step.inputs ?? ["."] }, env);
}

/** Crates of a nextest JUnit report that have a test suite and no failed or errored test. */
export function passedCrates(report: string): Set<string> {
  const passed = new Set<string>();
  const failed = new Set<string>();
  for (const [, name, attributes] of report.matchAll(
    /<testsuite name="([^"]+)"([^>]*)>/g,
  )) {
    const crate = name!.split("::")[0]!;
    const count = (attribute: string) =>
      Number(new RegExp(`${attribute}="(\\d+)"`).exec(attributes!)?.[1] ?? 1);
    (count("failures") || count("errors") ? failed : passed).add(crate);
  }
  for (const crate of failed) passed.delete(crate);
  return passed;
}

const resultFile = (root: string, step: { id?: string }, key: string) =>
  path.join(
    process.env[SHARED_RESULT_DIRECTORY_ENV] ||
      path.join(root, RESULT_DIRECTORY),
    `${step.id}-${key}.json`,
  );

/** The saved passing result for this key, marked as reused. */
export function reusedResult(
  root: string,
  step: { id?: string },
  key: string,
): StepResult | undefined {
  try {
    const saved = JSON.parse(
      fs.readFileSync(resultFile(root, step, key), "utf8"),
    ) as StepResult;
    return { ...saved, durationSeconds: 0, reused: true };
  } catch {
    return undefined;
  }
}

/** Saves a passing result. Failures are never saved. */
export function recordPass(
  root: string,
  step: { id?: string },
  key: string,
  result: SavedResult,
) {
  if (result.exitCode !== 0 || result.status !== "passed") return;
  const file = resultFile(root, step, key);
  fs.mkdirSync(path.dirname(file), { recursive: true });
  const { status, exitCode, passed, failed, skipped, executed } = result;
  fs.writeFileSync(
    file,
    JSON.stringify({ status, exitCode, passed, failed, skipped, executed }),
  );
}

/** Per-crate keys for a Cargo test step whose command has `--workspace`. A crate's key covers
 * its own folder and the folders of every workspace crate it depends on, transitively.
 * Returns undefined when reuse is off. A crate with uncommitted inputs has no key. */
export function crateKeys(
  root: string,
  metadata: CargoMetadata,
  step: CachedStep,
  env: NodeJS.ProcessEnv = process.env,
): Map<string, string | undefined> | undefined {
  if (env.STUDIO_TEST_REUSE === "0") return undefined;
  const members = new Set(metadata.workspace_members);
  const packages = new Map(metadata.packages.map((p) => [p.id, p]));
  const deps = new Map<string, string[]>(
    metadata.resolve.nodes
      .filter((node) => members.has(node.id))
      .map((node) => [
        node.id,
        node.deps.map((dep) => dep.pkg).filter((id) => members.has(id)),
      ]),
  );
  const folder = (id: string) =>
    path.relative(root, path.dirname(packages.get(id)!.manifest_path));
  const keys = new Map<string, string | undefined>();
  for (const id of members) {
    const seen = new Set([id]);
    const stack = [id];
    while (stack.length)
      for (const dep of deps.get(stack.pop()!) ?? [])
        if (!seen.has(dep)) {
          seen.add(dep);
          stack.push(dep);
        }
    const name = packages.get(id)!.name;
    keys.set(
      name,
      stepKey(
        root,
        {
          id: `${step.id}-${name}`,
          commands: step.commands,
          inputs: [".config/nextest.toml", ...[...seen].map(folder).sort()],
        },
        env,
      ),
    );
  }
  return keys;
}
