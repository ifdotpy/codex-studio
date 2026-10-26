// Compile the real owning crate in a disposable source copy. This is a focused
// compiler witness, not a provider process or a second runtime/server.
import assert from "node:assert/strict";
import {
  cpSync,
  mkdirSync,
  mkdtempSync,
  readFileSync,
  rmSync,
  symlinkSync,
  writeFileSync,
} from "node:fs";
import { spawnSync } from "node:child_process";
import { tmpdir } from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";
import test from "node:test";

const root = fileURLToPath(new URL("../../../../..", import.meta.url));
test(
  "Cargo refuses stale sources and regenerated incompatible vendor types",
  { timeout: 180000 },
  () => {
    const copy = mkdtempSync(path.join(tmpdir(), "studio-contract-compile-"));
    try {
      for (const file of [
        "Cargo.toml",
        "Cargo.lock",
        "rust-toolchain.toml",
        "package.json",
        ".oxfmtrc.json",
        "workspaces/runtime/packages/runtime-schema",
        "workspaces/client/packages/client-domain",
        "workspaces/providers/apps/claude-bridge/codex-generated",
        "workspaces/runtime/packages/runtime-http/openapi.json",
        "workspaces/tooling/apps/repository-checks/generate-provider-contracts.ts",
        "workspaces/tooling/apps/repository-checks/provider-contract-sources.ts",
      ]) {
        const target = path.join(copy, file);
        mkdirSync(path.dirname(target), { recursive: true });
        cpSync(path.join(root, file), target, { recursive: true });
      }
      symlinkSync(
        path.join(root, "node_modules"),
        path.join(copy, "node_modules"),
        "dir",
      );
      const manifest = readFileSync(
        path.join(copy, "Cargo.toml"),
        "utf8",
      ).replace(
        /members = \[[\s\S]*?\]/,
        'members = ["workspaces/runtime/packages/runtime-schema", "workspaces/client/packages/client-domain"]',
      );
      writeFileSync(path.join(copy, "Cargo.toml"), manifest);
      const env = {
        ...process.env,
        CARGO_TARGET_DIR:
          process.env.CARGO_TARGET_DIR ||
          path.join(tmpdir(), "studio-provider-contract-compile"),
      };
      const run = (command: string, args: string[]) =>
        spawnSync(command, args, {
          cwd: copy,
          env,
          encoding: "utf8",
          timeout: 120000,
        });
      const check = () =>
        run("cargo", [
          "check",
          "--offline",
          "-j",
          "4",
          "-p",
          "studio-runtime-schema",
        ]);
      const generate = () => {
        const result = run(process.execPath, [
          "workspaces/tooling/apps/repository-checks/generate-provider-contracts.ts",
          "--write",
        ]);
        assert.equal(result.status, 0, result.stdout + result.stderr);
      };
      const lifecycle = path.join(
        copy,
        "workspaces/runtime/packages/runtime-schema/src/schemas/codex/lifecycle.schema.json",
      );
      const originalLifecycle = readFileSync(lifecycle, "utf8");
      const changed = JSON.parse(originalLifecycle);
      changed.definitions.TurnStatus.enum.push("futureStatus");
      writeFileSync(lifecycle, JSON.stringify(changed));
      const stale = check();
      assert.notEqual(stale.status, 0);
      assert.match(stale.stdout + stale.stderr, /Stale Codex contract/);
      generate();
      const variant = check();
      assert.notEqual(variant.status, 0);
      assert.match(variant.stderr, /provider_lifecycle\.rs/);
      assert.match(variant.stderr, /FutureStatus.*not covered/);
      writeFileSync(lifecycle, originalLifecycle);
      const usage = path.join(
        copy,
        "workspaces/runtime/packages/runtime-schema/src/schemas/codex/ThreadTokenUsageUpdatedNotification.json",
      );
      const originalUsage = readFileSync(usage, "utf8");
      const added = JSON.parse(originalUsage);
      added.definitions.TokenUsageBreakdown.properties.futureTokens = {
        type: "integer",
        format: "int64",
      };
      added.definitions.TokenUsageBreakdown.required.push("futureTokens");
      writeFileSync(usage, JSON.stringify(added));
      generate();
      const field = check();
      assert.notEqual(field.status, 0);
      assert.match(field.stderr, /usage\.rs/);
      assert.match(
        field.stderr,
        /pattern does not mention field `future_tokens`/,
      );
      const typed = JSON.parse(originalUsage);
      typed.definitions.TokenUsageBreakdown.properties.inputTokens = {
        type: "string",
      };
      writeFileSync(usage, JSON.stringify(typed));
      generate();
      const incompatible = check();
      assert.notEqual(incompatible.status, 0);
      assert.match(incompatible.stderr, /usage\.rs/);
      writeFileSync(usage, originalUsage);
      generate();
      const restored = check();
      assert.equal(restored.status, 0, restored.stdout + restored.stderr);
    } finally {
      rmSync(copy, { recursive: true, force: true });
    }
  },
);
