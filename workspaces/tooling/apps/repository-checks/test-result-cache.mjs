// A passing gate step is a build artifact: its result is reused while its
// inputs keep the same content, and any input change runs it again.
import { execFileSync } from "node:child_process";
import { createHash } from "node:crypto";
import fs from "node:fs";
import path from "node:path";

/** Paths every cached step depends on, in addition to its own inputs. */
export const SHARED_INPUTS = [
  "package.json",
  "pnpm-lock.yaml",
  "pnpm-workspace.yaml",
  "Cargo.lock",
  "rust-toolchain.toml",
  "justfile",
  "workspaces/tooling/apps/repository-checks",
];

/** Folder of passing results. It is under target/, which layr keeps as a warm
 * cache, so a result made in one line reaches the lines that start from it. */
export const RESULT_DIRECTORY = path.join("target", "studio-test-results");

/** Content key of a step, or undefined when the step must run. */
export function stepKey(root, step, env = process.env) {
  if (env.STUDIO_TEST_REUSE === "0" || !step.inputs?.length) return undefined;
  const paths = [...SHARED_INPUTS, ...step.inputs];
  const git = (args) =>
    execFileSync("git", args, { cwd: root, encoding: "utf8" });
  try {
    // Uncommitted input changes have no tree id; such a step always runs.
    if (git(["status", "--porcelain", "--", ...paths]).trim()) return undefined;
    const trees = git(["rev-parse", ...paths.map((p) => `HEAD:${p}`)]);
    return createHash("sha256")
      .update(
        JSON.stringify([
          step.id,
          step.commands ?? step.options?.script,
          trees,
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

const resultFile = (root, step, key) =>
  path.join(root, RESULT_DIRECTORY, `${step.id}-${key}.json`);

/** The saved passing result for this key, marked as reused. */
export function reusedResult(root, step, key) {
  try {
    const saved = JSON.parse(
      fs.readFileSync(resultFile(root, step, key), "utf8"),
    );
    return { ...saved, durationSeconds: 0, reused: true };
  } catch {
    return undefined;
  }
}

/** Saves a passing result. Failures are never saved. */
export function recordPass(root, step, key, result) {
  if (result.exitCode !== 0 || result.status !== "passed") return;
  const file = resultFile(root, step, key);
  fs.mkdirSync(path.dirname(file), { recursive: true });
  const { status, exitCode, passed, failed, skipped, executed } = result;
  fs.writeFileSync(
    file,
    JSON.stringify({ status, exitCode, passed, failed, skipped, executed }),
  );
}
