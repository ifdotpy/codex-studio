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
  "workspaces/tooling/apps/repository-checks/check-code.ts",
);
const temporaryRoots: string[] = [];

function command(
  program: string,
  args: string[],
  cwd: string,
  options: Omit<
    import("node:child_process").SpawnSyncOptionsWithStringEncoding,
    "encoding"
  > = {},
) {
  const result = spawnSync(program, args, {
    cwd,
    encoding: "utf8",
    ...options,
  });
  if (result.error) throw result.error;
  return result;
}

function git(root: string, ...args: string[]) {
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
    "workspaces/tooling/apps/repository-checks/check-code.ts",
  );
  mkdirSync(path.dirname(checkerPath), { recursive: true });
  mkdirSync(path.join(root, "node_modules", ".bin"), { recursive: true });
  mkdirSync(path.join(root, "node_modules"), { recursive: true });
  copyFileSync(hookSource, path.join(root, ".githooks", "pre-commit"));
  chmodSync(path.join(root, ".githooks", "pre-commit"), 0o755);
  copyFileSync(checkSource, checkerPath);
  copyFileSync(
    path.join(path.dirname(checkSource), "enum-literals.ts"),
    path.join(path.dirname(checkerPath), "enum-literals.ts"),
  );
  copyFileSync(
    path.join(path.dirname(checkSource), "javascript-policy.ts"),
    path.join(path.dirname(checkerPath), "javascript-policy.ts"),
  );
  copyFileSync(
    path.join(path.dirname(checkSource), "javascript-exceptions.json"),
    path.join(path.dirname(checkerPath), "javascript-exceptions.json"),
  );
  for (const config of [".oxlintrc.json", ".oxfmtrc.json"]) {
    copyFileSync(path.join(repositoryRoot, config), path.join(root, config));
  }
  for (const dependency of ["oxlint", "oxfmt", "typescript"]) {
    symlinkSync(
      path.join(repositoryRoot, "node_modules", dependency),
      path.join(root, "node_modules", dependency),
      process.platform === "win32" ? "junction" : "dir",
    );
    if (dependency === "typescript") continue;
    symlinkSync(
      path.join(repositoryRoot, "node_modules", ".bin", dependency),
      path.join(root, "node_modules", ".bin", dependency),
      process.platform === "win32" ? "junction" : "dir",
    );
  }
  git(root, "init", "--quiet");
  git(root, "config", "user.name", "Hook Fixture");
  git(root, "config", "user.email", "hook-fixture@example.invalid");
  git(
    root,
    "add",
    "--",
    ".oxlintrc.json",
    ".oxfmtrc.json",
    "workspaces/tooling/apps/repository-checks/javascript-exceptions.json",
  );
  git(root, "commit", "--quiet", "-m", "fixture tooling baseline");
  return root;
}

function stage(root: string, relativePath: string, content: string | null) {
  const filePath = path.join(root, relativePath);
  mkdirSync(path.dirname(filePath), { recursive: true });
  if (content === null) {
    command("git", ["rm", "--", relativePath], root);
  } else {
    writeFileSync(filePath, content);
    git(root, "add", "--", relativePath);
  }
}

function runHook(root: string) {
  return command(path.join(root, ".githooks", "pre-commit"), [], root);
}

function commitFixture(root: string) {
  git(root, "add", "-A");
  git(root, "commit", "--quiet", "-m", "fixture baseline");
}

try {
  {
    const root = fixture();
    stage(root, "unlisted.js", "// @ts-check\nexport {};\n");
    stage(
      root,
      "tsconfig.json",
      '{\n  "compilerOptions": {\n    "allowJs": true\n  },\n  "include": ["*.js"]\n}\n',
    );
    const policy = path.join(
      root,
      "workspaces/tooling/apps/repository-checks/javascript-exceptions.json",
    );
    const exceptions: { file: string; purpose: string }[] = JSON.parse(
      readFileSync(policy, "utf8"),
    );
    writeFileSync(
      policy,
      JSON.stringify(
        [...exceptions, { file: "unlisted.js", purpose: "unstaged bypass" }],
        null,
        2,
      ) + "\n",
    );
    const result = runHook(root);
    assert.notEqual(
      result.status,
      0,
      "working-tree exception changes must not authorize staged JavaScript",
    );
    assert.match(
      result.stderr + result.stdout,
      /not a documented runtime exception/,
    );
  }
  {
    const root = fixture();
    stage(root, "unlisted.js", "// @ts-check\nexport {};\n");
    const result = runHook(root);
    assert.notEqual(result.status, 0);
    assert.match(
      result.stderr + result.stdout,
      /not a documented runtime exception/,
    );
  }
  {
    const root = fixture();
    const file = "workspaces/client/apps/desktop/preload.cjs";
    stage(
      root,
      "tsconfig.json",
      '{\n  "compilerOptions": {\n    "allowJs": true\n  },\n  "include": ["**/*.cjs"]\n}\n',
    );
    stage(root, file, "module.exports = {};\n");
    writeFileSync(
      path.join(root, file),
      "// @ts-check\nmodule.exports = {};\n",
    );
    assert.match(runHook(root).stderr, /must start with/);
    stage(root, file, "// @ts-check\nmodule.exports = {};\n");
    stage(
      root,
      "tsconfig.json",
      '{\n  "compilerOptions": {\n    "allowJs": true\n  },\n  "include": ["missing.cjs"]\n}\n',
    );
    writeFileSync(
      path.join(root, "tsconfig.json"),
      '{\n  "compilerOptions": {\n    "allowJs": true\n  },\n  "include": ["**/*.cjs"]\n}\n',
    );
    assert.match(runHook(root).stderr, /not included by a tracked tsconfig/);
    stage(
      root,
      "tsconfig.json",
      '{\n  "compilerOptions": {\n    "allowJs": true\n  },\n  "include": ["**/*.cjs"]\n}\n',
    );
    const accepted = runHook(root);
    assert.equal(accepted.status, 0, accepted.stderr + accepted.stdout);
    commitFixture(root);
    stage(root, "tsconfig.json", null);
    assert.match(
      runHook(root).stderr,
      /not included by a tracked tsconfig/,
      "deleting the covering project must not leave an unchecked exception",
    );
  }

  for (const source of [
    "export type Unsafe = any;\n",
    "// @ts-ignore reason\nexport const value = 1;\n",
    "// @ts-nocheck reason\nexport const value = 1;\n",
    "// @ts-expect-error\nexport const value = 1;\n",
    "// @ts-expect-error production code must not suppress a type mismatch\nexport const value: string = 1;\n",
  ]) {
    const root = fixture();
    stage(root, "invalid.ts", source);
    assert.notEqual(runHook(root).status, 0, source);
  }
  {
    const root = fixture();
    stage(
      root,
      "negative.type-test.ts",
      "// @ts-expect-error value deliberately violates its declared string type\nexport const value: string = 1;\n",
    );
    const result = runHook(root);
    assert.equal(result.status, 0, result.stderr + result.stdout);
  }

  for (const extension of ["ts", "cts", "mts"]) {
    const root = fixture();
    stage(root, `desktop/staged-broken.${extension}`, "const = ;\n");
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
    stage(root, "web/src/configured.ts", 'console.log("staged rule");\n');
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
    stage(root, "web/src/format-config.ts", 'export const value = "ok"\n');
    writeFileSync(
      path.join(root, ".oxfmtrc.json"),
      `${JSON.stringify(workingConfig, null, 2)}\n`,
    );
    const result = runHook(root);
    assert.equal(result.status, 0, result.stderr || result.stdout);
  }

  {
    const root = fixture();
    // Vendored sources keep their upstream bytes; the Oxfmt ignorePatterns skip them.
    stage(
      root,
      "workspaces/runtime/apps/layr-desk/crates/vectors.json",
      '{"a":[1,\n2]}\n',
    );
    const result = runHook(root);
    assert.equal(result.status, 0, result.stderr || result.stdout);
  }

  {
    const root = fixture();
    stage(root, "desktop/source outside web.ts", "export const valid = 1;\n");
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
    stage(root, "pnpm-lock.yaml", "lockfileVersion: '9.0'\n");
    const result = runHook(root);
    assert.equal(result.status, 0, result.stderr || result.stdout);
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
    stage(root, "web/src/no-tools.ts", "export const value = 1;\n");
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

  {
    const root = fixture();
    stage(
      root,
      "tsconfig.base.json",
      '{"compilerOptions":{"strict":true,"module":"ESNext","moduleResolution":"Bundler"}}',
    );
    stage(
      root,
      "workspaces/client/apps/web/tsconfig.json",
      '{"extends":"../../../../tsconfig.base.json"}',
    );
    stage(
      root,
      "workspaces/client/apps/web/src/generated/api.ts",
      'export const Reason = { slot: "slot", budget: "budget" } as const;\nexport type Reason = typeof Reason[keyof typeof Reason];\n',
    );
    stage(
      root,
      "workspaces/client/apps/web/src/generated/providerKind.ts",
      'export const ProviderKind = { Codex: "codex", Claude: "claude" } as const;\n',
    );
    stage(
      root,
      "workspaces/client/apps/web/package.json",
      '{"name":"fixture-web"}',
    );
    const target = "workspaces/client/apps/web/src/startWait.ts";
    stage(
      root,
      target,
      'import type { Reason } from "./generated/api";\nexport function waiting(reason: Reason) { return reason === "slot"; }\n',
    );
    const lint = () =>
      command(
        process.execPath,
        [
          "workspaces/tooling/apps/repository-checks/check-code.ts",
          "staged",
          "lint",
        ],
        root,
      );
    stage(root, target, 'import { value } from "web-only-package"; value();\n');
    const missing = lint();
    assert.notEqual(missing.status, 0, missing.stdout + missing.stderr);
    assert.match(
      missing.stdout + missing.stderr,
      /Cannot resolve TypeScript dependencies/,
    );
    const dependency = path.join(
      root,
      "workspaces/client/apps/web/node_modules/web-only-package",
    );
    mkdirSync(dependency, { recursive: true });
    writeFileSync(
      path.join(dependency, "index.d.ts"),
      "export declare function value(): void;\n",
    );
    stage(
      root,
      target,
      'import { value } from "web-only-package"; value();\nimport type { Reason } from "./generated/api";\nexport function waiting(reason: Reason) { return reason === "slot"; }\n',
    );
    const rejected = lint();
    assert.notEqual(rejected.status, 0, rejected.stdout + rejected.stderr);
    assert.match(rejected.stdout + rejected.stderr, /import and use Reason/);
    stage(
      root,
      target,
      'import { value } from "web-only-package"; value();\nimport { Reason } from "./generated/api";\nexport function waiting(reason: Reason) { return reason === Reason.slot; }\n',
    );
    writeFileSync(path.join(root, target), "const broken = ;\n");
    const accepted = lint();
    assert.equal(accepted.status, 0, accepted.stdout + accepted.stderr);
    const ambient = "workspaces/client/apps/web/src/assets.d.ts";
    stage(
      root,
      ambient,
      'declare module "*.css";\ndeclare module "*?url" { const url: string; export default url; }\ndeclare module "*?raw" { const text: string; export default text; }\n',
    );
    git(root, "commit", "--quiet", "-m", "prepare ambient module declarations");
    stage(
      root,
      target,
      'import "./view.css";\nimport workerURL from "./worker.ts?url";\nimport text from "./contract.json?raw";\nimport { Reason } from "./generated/api";\nexport function waiting(reason: Reason) { return reason === Reason.slot && workerURL.length > 0 && text.length > 0; }\n',
    );
    writeFileSync(path.join(root, ambient), "export {};\n");
    const ambientAccepted = lint();
    assert.equal(
      ambientAccepted.status,
      0,
      ambientAccepted.stdout + ambientAccepted.stderr,
    );
    const bridgeRoot = "workspaces/providers/apps/fixture-bridge";
    stage(
      root,
      `${bridgeRoot}/package.json`,
      '{"name":"fixture-bridge","type":"module","exports":"./index.ts"}',
    );
    const bridge = `${bridgeRoot}/index.ts`;
    const enumBridge =
      'export type { Reason } from "../../../client/apps/web/src/generated/api";\n';
    stage(root, bridge, enumBridge);
    symlinkSync(
      path.join(root, bridgeRoot),
      path.join(root, "workspaces/client/apps/web/node_modules/fixture-bridge"),
      "dir",
    );
    stage(
      root,
      target,
      'import type { Reason } from "fixture-bridge";\nexport function waiting(reason: Reason) { return reason === "slot"; }\n',
    );
    writeFileSync(path.join(root, bridge), "export type Reason = string;\n");
    const indexRejects = lint();
    assert.notEqual(
      indexRejects.status,
      0,
      indexRejects.stdout + indexRejects.stderr,
    );
    assert.match(
      indexRejects.stdout + indexRejects.stderr,
      /import and use Reason/,
    );
    stage(root, bridge, "export type Reason = string;\n");
    writeFileSync(path.join(root, bridge), enumBridge);
    const indexAccepts = lint();
    assert.equal(
      indexAccepts.status,
      0,
      indexAccepts.stdout + indexAccepts.stderr,
    );
    const facade = "workspaces/client/apps/web/src/wasm/client.ts";
    const realFacade = readFileSync(path.join(repositoryRoot, facade), "utf8");
    stage(root, facade, realFacade);
    const rustOwned = "workspaces/client/apps/web/src/generated/rustOwned.ts";
    stage(
      root,
      rustOwned,
      readFileSync(path.join(repositoryRoot, rustOwned), "utf8"),
    );
    git(root, "commit", "--quiet", "-m", "prepare staged wasm facade");
    stage(
      root,
      target,
      'import { sha256 } from "./wasm/client";\nexport async function check(bytes: Uint8Array): Promise<Uint8Array> { return sha256(bytes); }\n',
    );
    const cleanWasm = lint();
    assert.equal(cleanWasm.status, 0, cleanWasm.stdout + cleanWasm.stderr);
    stage(root, facade, realFacade + "\n");
    const facadeNeedsBinding = lint();
    assert.notEqual(
      facadeNeedsBinding.status,
      0,
      facadeNeedsBinding.stdout + facadeNeedsBinding.stderr,
    );
    assert.match(
      facadeNeedsBinding.stdout + facadeNeedsBinding.stderr,
      /Cannot resolve TypeScript dependencies/,
    );
    stage(root, facade, realFacade);
    const wholeLint = () =>
      command(
        process.execPath,
        [
          "workspaces/tooling/apps/repository-checks/check-code.ts",
          "lint-all",
          target,
        ],
        root,
      );
    writeFileSync(
      path.join(root, target),
      'import { Reason } from "./generated/api";\nexport function waiting(reason: Reason) { return reason === Reason.slot; }\n',
    );
    const wholeAccepted = wholeLint();
    assert.equal(
      wholeAccepted.status,
      0,
      wholeAccepted.stdout + wholeAccepted.stderr,
    );
    writeFileSync(
      path.join(root, target),
      'import type { Reason } from "./generated/api";\nexport function waiting(reason: Reason) { return reason === "slot"; }\n',
    );
    const wholeRejected = wholeLint();
    assert.notEqual(
      wholeRejected.status,
      0,
      wholeRejected.stdout + wholeRejected.stderr,
    );
    assert.match(
      wholeRejected.stdout + wholeRejected.stderr,
      /import and use Reason/,
    );
  }
  console.log("Pre-commit staged-content fixtures passed.");
} finally {
  for (const root of temporaryRoots)
    rmSync(root, { recursive: true, force: true });
}
