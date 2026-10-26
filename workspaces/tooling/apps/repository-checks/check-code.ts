#!/usr/bin/env node

import {
  JAVASCRIPT_EXCEPTIONS,
  isJavaScript,
  isTypeScriptProject,
  javascriptFindings,
} from "./javascript-policy.ts";

import { execFileSync, spawnSync } from "node:child_process";
import {
  existsSync,
  mkdtempSync,
  realpathSync,
  rmSync,
  writeFileSync,
  mkdirSync,
} from "node:fs";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";

const scriptDirectory = path.dirname(fileURLToPath(import.meta.url));
const defaultRoot = path.resolve(scriptDirectory, "../../../..");
const mode = process.argv[2];
const check = process.argv[3];
const repositoryRoot = defaultRoot;
const codeExtensions = new Set([
  ".js",
  ".jsx",
  ".mjs",
  ".cjs",
  ".ts",
  ".tsx",
  ".cts",
  ".mts",
]);
const formatExtensions = new Set([
  ...codeExtensions,
  ".css",
  ".html",
  ".json",
  ".jsonc",
  ".md",
  ".yaml",
  ".yml",
]);

function git(
  args: string[],
  options: import("node:child_process").ExecFileSyncOptionsWithBufferEncoding = {},
) {
  return execFileSync("git", args, {
    cwd: repositoryRoot,
    encoding: "buffer",
    maxBuffer: 64 * 1024 * 1024,
    ...options,
  });
}

function listPaths(output: Buffer) {
  return output.toString("utf8").split("\0").filter(Boolean);
}

function isCheckable(relativePath: string, extensions: ReadonlySet<string>) {
  return extensions.has(path.extname(relativePath).toLowerCase());
}

function getPaths() {
  return listPaths(
    git(["diff", "--cached", "--name-only", "-z", "--diff-filter=ACMR"]),
  );
}

function getIndexPaths() {
  return new Set(listPaths(git(["ls-files", "-z"])));
}

function run(binary: string, args: string[], cwd: string) {
  const result = spawnSync(binary, args, { cwd, stdio: "inherit" });
  if (result.error) {
    throw result.error;
  }
  return result.status ?? 1;
}

function main() {
  if (
    mode !== "staged" ||
    (check !== undefined && check !== "lint" && check !== "format")
  ) {
    throw new Error(
      "Usage: node workspaces/tooling/apps/repository-checks/check-code.ts staged [lint|format]",
    );
  }

  const selectedPaths = getPaths();
  const changedPaths = listPaths(
    git(["diff", "--cached", "--name-only", "-z"]),
  );
  const projectsChanged = changedPaths.some(
    (file) => isTypeScriptProject(file) || file === "tsconfig.base.json",
  );
  const lintPaths = selectedPaths.filter((relativePath) =>
    isCheckable(relativePath, codeExtensions),
  );
  const formatPaths = selectedPaths.filter((relativePath) =>
    isCheckable(relativePath, formatExtensions),
  );
  if (check === "lint") {
    formatPaths.length = 0;
  } else if (check === "format") {
    lintPaths.length = 0;
  }

  if (lintPaths.length === 0 && formatPaths.length === 0 && !projectsChanged) {
    console.log("No staged source or supported text files to check.");
    return 0;
  }

  // The real path: on macOS the temporary folder is below the /var link, and Oxfmt matches
  // ignorePatterns only when a file path starts with the folder of its configuration.
  const temporaryRoot = realpathSync(
    mkdtempSync(path.join(os.tmpdir(), "codex-staged-check-")),
  );
  try {
    const indexPaths = getIndexPaths();
    const policyPaths = [
      ...new Set([
        ...selectedPaths.filter(isJavaScript),
        ...(projectsChanged
          ? JAVASCRIPT_EXCEPTIONS.map((entry) => entry.file).filter((file) =>
              indexPaths.has(file),
            )
          : []),
      ]),
    ];
    for (const relativePath of new Set([
      ...lintPaths,
      ...formatPaths,
      ...policyPaths,
      ...[...indexPaths].filter(
        (file) => isTypeScriptProject(file) || file === "tsconfig.base.json",
      ),
    ])) {
      const destination = path.join(temporaryRoot, relativePath);
      mkdirSync(path.dirname(destination), { recursive: true });
      const content = git(["cat-file", "blob", `:${relativePath}`]);
      writeFileSync(destination, content);
    }
    for (const relativePath of [".oxlintrc.json", ".oxfmtrc.json"]) {
      if (!indexPaths.has(relativePath)) continue;
      writeFileSync(
        path.join(temporaryRoot, relativePath),
        git(["cat-file", "blob", `:${relativePath}`]),
      );
    }

    const policyFindings = javascriptFindings(
      temporaryRoot,
      [...indexPaths],
      policyPaths,
    );
    if (policyFindings.length) {
      for (const finding of policyFindings)
        console.error(`JavaScript policy: ${finding}`);
      return 1;
    }

    const binDirectory = path.join(repositoryRoot, "node_modules", ".bin");
    const oxlint = path.join(binDirectory, "oxlint");
    const oxfmt = path.join(binDirectory, "oxfmt");
    if (
      (lintPaths.length > 0 && !existsSync(oxlint)) ||
      (formatPaths.length > 0 && !existsSync(oxfmt))
    ) {
      throw new Error(
        "Oxlint/Oxfmt dependencies are missing; install them with `pnpm install --frozen-lockfile` before committing.",
      );
    }

    if (lintPaths.length > 0) {
      console.log("Checking staged JavaScript/TypeScript with Oxlint...");
      const status = run(
        oxlint,
        [
          "--deny-warnings",
          "--disable-nested-config",
          ...lintPaths.map((relativePath) =>
            path.join(temporaryRoot, relativePath),
          ),
        ],
        temporaryRoot,
      );
      if (status !== 0) return status;
    }

    if (formatPaths.length > 0) {
      console.log("Checking staged supported text files with Oxfmt...");
      const status = run(
        oxfmt,
        [
          "--check",
          "--disable-nested-config",
          "--no-error-on-unmatched-pattern",
          ...formatPaths.map((relativePath) =>
            path.join(temporaryRoot, relativePath),
          ),
        ],
        temporaryRoot,
      );
      if (status !== 0) return status;
    }
    return 0;
  } finally {
    rmSync(temporaryRoot, { recursive: true, force: true });
  }
}

try {
  process.exitCode = main();
} catch (error) {
  console.error(
    `Pre-commit checks failed: ${error instanceof Error ? error.message : String(error)}`,
  );
  process.exitCode = 1;
}
