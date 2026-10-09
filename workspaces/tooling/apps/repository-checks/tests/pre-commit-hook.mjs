import assert from "node:assert/strict";
import {
  chmodSync,
  copyFileSync,
  mkdirSync,
  mkdtempSync,
  readFileSync,
  rmSync,
  symlinkSync,
  unlinkSync,
  writeFileSync,
} from "node:fs";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { spawnSync } from "node:child_process";

const repositoryRoot = path.resolve(
  path.dirname(fileURLToPath(import.meta.url)),
  "../../../../..",
);
const hookSource = path.join(repositoryRoot, ".githooks", "pre-commit");
const checkSource = path.join(
  repositoryRoot,
  "workspaces/tooling/apps/repository-checks/check-code.mjs",
);
const temporaryRoots = [];

function command(program, args, cwd, options = {}) {
  const result = spawnSync(program, args, {
    cwd,
    encoding: "utf8",
    ...options,
  });
  if (result.error) throw result.error;
  return result;
}

function git(root, ...args) {
  const result = command("git", args, root);
  assert.equal(result.status, 0, result.stderr || result.stdout);
  return result.stdout.trim();
}

function fixture() {
  const root = mkdtempSync(path.join(os.tmpdir(), "codex-hook-fixture-"));
  temporaryRoots.push(root);
  mkdirSync(path.join(root, ".githooks"), { recursive: true });
  const checkerPath = path.join(
    root,
    "workspaces/tooling/apps/repository-checks/check-code.mjs",
  );
  mkdirSync(path.dirname(checkerPath), { recursive: true });
  mkdirSync(path.join(root, "node_modules", ".bin"), { recursive: true });
  mkdirSync(path.join(root, "node_modules"), { recursive: true });
  copyFileSync(hookSource, path.join(root, ".githooks", "pre-commit"));
  chmodSync(path.join(root, ".githooks", "pre-commit"), 0o755);
  copyFileSync(checkSource, checkerPath);
  for (const config of [".oxlintrc.json", ".oxfmtrc.json"]) {
    copyFileSync(path.join(repositoryRoot, config), path.join(root, config));
  }
  for (const dependency of ["oxlint", "oxfmt"]) {
    symlinkSync(
      path.join(repositoryRoot, "node_modules", dependency),
      path.join(root, "node_modules", dependency),
      process.platform === "win32" ? "junction" : "dir",
    );
    symlinkSync(
      path.join(repositoryRoot, "node_modules", ".bin", dependency),
      path.join(root, "node_modules", ".bin", dependency),
      process.platform === "win32" ? "junction" : "dir",
    );
  }
  git(root, "init", "--quiet");
  git(root, "config", "user.name", "Hook Fixture");
  git(root, "config", "user.email", "hook-fixture@example.invalid");
  git(root, "add", "--", ".oxlintrc.json", ".oxfmtrc.json");
  git(root, "commit", "--quiet", "-m", "fixture tooling baseline");
  return root;
}

function stage(root, relativePath, content) {
  const filePath = path.join(root, relativePath);
  mkdirSync(path.dirname(filePath), { recursive: true });
  if (content === null) {
    command("git", ["rm", "--", relativePath], root);
  } else {
    writeFileSync(filePath, content);
    git(root, "add", "--", relativePath);
  }
}

function runHook(root) {
  return command(path.join(root, ".githooks", "pre-commit"), [], root);
}

function commitFixture(root) {
  git(root, "add", "-A");
  git(root, "commit", "--quiet", "-m", "fixture baseline");
}

try {
  {
    const root = fixture();
    stage(root, "desktop/staged-broken.js", "const = ;\n");
    const tree = git(root, "write-tree");
    const result = runHook(root);
    assert.notEqual(
      result.status,
      0,
      "staged syntax error should reject a commit",
    );
    assert.match(result.stderr + result.stdout, /Oxlint/i);
    assert.equal(
      git(root, "write-tree"),
      tree,
      "rejected hook must leave the index unchanged",
    );
  }

  {
    const root = fixture();
    stage(root, "web/src/partial.ts", "export const ready = true;\n");
    commitFixture(root);
    stage(root, "web/src/partial.ts", "export const ready = false;\n");
    writeFileSync(path.join(root, "web/src/partial.ts"), "const = ;\n");
    assert.equal(
      git(root, "diff", "--cached", "--name-only"),
      "web/src/partial.ts",
      "the staged version must remain a real partial change",
    );
    const tree = git(root, "write-tree");
    const workingBytes = readFileSync(path.join(root, "web/src/partial.ts"));
    const result = runHook(root);
    assert.equal(result.status, 0, result.stderr || result.stdout);
    assert.match(
      result.stdout,
      /Checking staged JavaScript\/TypeScript with Oxlint/,
      "the staged delta must reach Oxlint despite dirty working bytes",
    );
    assert.equal(
      git(root, "write-tree"),
      tree,
      "successful hook must leave the index unchanged",
    );
    assert.deepEqual(
      readFileSync(path.join(root, "web/src/partial.ts")),
      workingBytes,
    );
  }

  {
    const root = fixture();
    const strictConfig = {
      $schema: "./node_modules/oxlint/configuration_schema.json",
      rules: { "no-console": "error" },
    };
    const relaxedConfig = {
      $schema: "./node_modules/oxlint/configuration_schema.json",
      rules: { "no-console": "off" },
    };
    stage(root, ".oxlintrc.json", `${JSON.stringify(strictConfig, null, 2)}\n`);
    stage(root, "web/src/configured.js", 'console.log("staged rule");\n');
    writeFileSync(
      path.join(root, ".oxlintrc.json"),
      `${JSON.stringify(relaxedConfig, null, 2)}\n`,
    );
    const result = runHook(root);
    assert.notEqual(
      result.status,
      0,
      "staged lint config should be authoritative",
    );
    assert.match(result.stderr + result.stdout, /no-console/);
  }

  {
    const root = fixture();
    const stagedConfig = {
      $schema: "./node_modules/oxfmt/configuration_schema.json",
      printWidth: 80,
      semi: false,
      singleQuote: false,
      trailingComma: "all",
    };
    const workingConfig = { ...stagedConfig, semi: true };
    stage(root, ".oxfmtrc.json", `${JSON.stringify(stagedConfig, null, 2)}\n`);
    stage(root, "web/src/format-config.js", 'export const value = "ok"\n');
    writeFileSync(
      path.join(root, ".oxfmtrc.json"),
      `${JSON.stringify(workingConfig, null, 2)}\n`,
    );
    const result = runHook(root);
    assert.equal(result.status, 0, result.stderr || result.stdout);
  }

  {
    const root = fixture();
    stage(root, "desktop/source outside web.js", "export const valid = 1;\n");
    const result = runHook(root);
    assert.equal(result.status, 0, result.stderr || result.stdout);
  }

  {
    const root = fixture();
    stage(root, "tests/unformatted.ts", "export const value=1;\n");
    const result = runHook(root);
    assert.notEqual(
      result.status,
      0,
      "unformatted staged content should reject a commit",
    );
    assert.match(result.stderr + result.stdout, /Oxfmt/i);
  }

  {
    const root = fixture();
    stage(root, "README.md", "# Heading #\n");
    const result = runHook(root);
    assert.notEqual(
      result.status,
      0,
      "unformatted staged Markdown should reject a commit",
    );
    assert.match(result.stderr + result.stdout, /Oxfmt/i);
  }

  {
    const root = fixture();
    stage(root, ".github/workflows/check.yml", "name:  check\n");
    const result = runHook(root);
    assert.notEqual(
      result.status,
      0,
      "unformatted staged YAML should reject a commit",
    );
    assert.match(result.stderr + result.stdout, /Oxfmt/i);
  }

  {
    const root = fixture();
    stage(root, "web/src/renamed.ts", "export const renamed = 1;\n");
    commitFixture(root);
    git(root, "mv", "web/src/renamed.ts", "web/src/renamed with spaces.ts");
    const result = runHook(root);
    assert.equal(result.status, 0, result.stderr || result.stdout);
  }

  {
    const root = fixture();
    stage(root, "desktop/to-delete.js", "const = ;\n");
    commitFixture(root);
    command("git", ["rm", "--", "desktop/to-delete.js"], root);
    const result = runHook(root);
    assert.equal(result.status, 0, result.stderr || result.stdout);
  }

  {
    const root = fixture();
    unlinkSync(path.join(root, "node_modules", ".bin", "oxlint"));
    unlinkSync(path.join(root, "node_modules", ".bin", "oxfmt"));
    stage(root, "web/src/no-tools.js", "export const value = 1;\n");
    const result = runHook(root);
    assert.notEqual(
      result.status,
      0,
      "missing local dependencies should reject a commit",
    );
    assert.match(
      result.stderr + result.stdout,
      /missing.*pnpm install --frozen-lockfile/i,
    );
  }

  console.log("Pre-commit staged-content fixtures passed.");
} finally {
  for (const root of temporaryRoots)
    rmSync(root, { recursive: true, force: true });
}
