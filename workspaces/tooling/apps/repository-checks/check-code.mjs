#!/usr/bin/env node

import { execFileSync, spawnSync } from "node:child_process";
import {
  existsSync,
  mkdtempSync,
  rmSync,
  writeFileSync,
  mkdirSync,
} from "node:fs";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";

const scriptDirectory = path.dirname(fileURLToPath(import.meta.url));
const defaultRoot = path.resolve(scriptDirectory, "..");
const mode = process.argv[2];
const check = process.argv[3];
const repositoryRoot = defaultRoot;
const codeExtensions = new Set([".js", ".jsx", ".mjs", ".cjs", ".ts", ".tsx"]);
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

function git(args, options = {}) {
  return execFileSync("git", args, {
    cwd: repositoryRoot,
    encoding: "buffer",
    maxBuffer: 64 * 1024 * 1024,
    ...options,
  });
}

function listPaths(output) {
  return output.toString("utf8").split("\0").filter(Boolean);
}

function isCheckable(relativePath, extensions) {
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

function run(binary, args, cwd) {
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
    throw new Error("Usage: node scripts/check-code.mjs staged [lint|format]");
  }

  const selectedPaths = getPaths();
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

  if (lintPaths.length === 0 && formatPaths.length === 0) {
    console.log("No staged source or supported text files to check.");
    return 0;
  }

  const temporaryRoot = mkdtempSync(
    path.join(os.tmpdir(), "codex-staged-check-"),
  );
  try {
    const indexPaths = getIndexPaths();
    for (const relativePath of new Set([...lintPaths, ...formatPaths])) {
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

    const packageDirectory = path.join(repositoryRoot, "node_modules");
    const oxlint = path.join(packageDirectory, "oxlint", "bin", "oxlint");
    const oxfmt = path.join(packageDirectory, "oxfmt", "bin", "oxfmt");
    if (
      (lintPaths.length > 0 && !existsSync(oxlint)) ||
      (formatPaths.length > 0 && !existsSync(oxfmt))
    ) {
      throw new Error(
        "Oxlint/Oxfmt dependencies are missing; install them with `npm ci` before committing.",
      );
    }

    if (lintPaths.length > 0) {
      console.log("Checking staged JavaScript/TypeScript with Oxlint...");
      const status = run(
        process.execPath,
        [
          oxlint,
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
        process.execPath,
        [
          oxfmt,
          "--check",
          "--disable-nested-config",
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
  console.error(`Pre-commit checks failed: ${error.message}`);
  process.exitCode = 1;
}
