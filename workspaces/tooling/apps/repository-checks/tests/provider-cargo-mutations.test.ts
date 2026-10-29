// Compile the real owning crate in a disposable source copy. This is a focused
// compiler witness, not a provider process or a second runtime/server.
import assert from "node:assert/strict";
import {
  cpSync,
  mkdirSync,
  mkdtempSync,
  readFileSync,
  rmSync,
  existsSync,
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
  { timeout: 900000 },
  async (context) => {
    const copy = mkdtempSync(path.join(tmpdir(), "studio-contract-compile-"));
    try {
      for (const file of [
        "Cargo.toml",
        "Cargo.lock",
        "rust-toolchain.toml",
        "package.json",
        "pnpm-lock.yaml",
        ".oxfmtrc.json",
        "workspaces/runtime/packages/runtime-schema",
        "workspaces/client/packages/client-domain",
        "workspaces/providers/apps/claude-bridge/codex-generated",
        "workspaces/providers/apps/claude-bridge/package.json",
        "workspaces/providers/apps/claude-bridge/rate-limit-input.ts",
        "workspaces/providers/apps/claude-bridge/usage-policy.ts",
        "workspaces/runtime/packages/runtime-http/openapi.json",
        "workspaces/tooling/apps/repository-checks/generate-provider-contracts.ts",
        "workspaces/tooling/apps/repository-checks/provider-contract-sources.ts",
      ]) {
        const target = path.join(copy, file);
        mkdirSync(path.dirname(target), { recursive: true });
        cpSync(path.join(root, file), target, { recursive: true });
      }
      assert.equal(existsSync(path.join(copy, "node_modules")), false);
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
        // Own target folder: the gate's Rust lane holds the lock of the shared one for
        // minutes, and each blocked check here waited out its whole timeout. It stays in
        // the checkout's target/ so dependency builds stay warm between runs.
        CARGO_TARGET_DIR: path.join(
          root,
          "target",
          "provider-contract-compile",
        ),
      };
      const run = (command: string, args: string[]) =>
        spawnSync(command, args, {
          cwd: copy,
          env,
          encoding: "utf8",
          timeout: 300000,
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
          "--rust-only",
        ]);
        assert.equal(result.status, 0, result.stdout + result.stderr);
      };
      const baseline = check();
      assert.equal(baseline.status, 0, baseline.stdout + baseline.stderr);
      for (const [name, relative, mutate] of [
        [
          "parser source",
          "workspaces/providers/apps/claude-bridge/rate-limit-input.ts",
          (text: string) => text + "\n// changed consumed source\n",
        ],
        [
          "generated validator",
          "workspaces/runtime/packages/runtime-schema/src/schemas/claude-rate-limit.schema.json",
          (text: string) => {
            const schema = JSON.parse(text);
            schema.properties.status.enum.push("future_status");
            return JSON.stringify(schema);
          },
        ],
      ] as const) {
        await context.test(
          `offline Cargo refuses stale Claude ${name} without node_modules`,
          () => {
            const file = path.join(copy, relative);
            const original = readFileSync(file, "utf8");
            try {
              writeFileSync(file, mutate(original));
              const result = check();
              assert.notEqual(
                result.status,
                0,
                "Offline Cargo must reject stale Claude evidence contracts",
              );
              assert.match(
                result.stdout + result.stderr,
                /Stale Claude rate provenance/,
              );
            } finally {
              writeFileSync(file, original);
            }
          },
        );
      }
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
      await context.test(
        "the real usage reader rejects a changed model context window type",
        () => {
          try {
            const window = JSON.parse(originalUsage);
            window.definitions.ThreadTokenUsage.properties.modelContextWindow =
              { type: ["string", "null"] };
            writeFileSync(usage, JSON.stringify(window));
            generate();
            const result = check();
            assert.notEqual(
              result.status,
              0,
              "The real usage reader must consume its generated window field",
            );
            assert.match(result.stderr, /usage\.rs/);
          } finally {
            writeFileSync(usage, originalUsage);
            generate();
          }
        },
      );
      await context.test(
        "the real idle decision rejects an added vendor activity discriminator",
        () => {
          try {
            const status = JSON.parse(originalLifecycle);
            status.definitions.ThreadStatus.oneOf.push({
              type: "object",
              properties: {
                type: { type: "string", enum: ["futureActivity"] },
              },
              required: ["type"],
            });
            writeFileSync(lifecycle, JSON.stringify(status));
            generate();
            const result = check();
            assert.notEqual(
              result.status,
              0,
              "Recovery must consume the vendor-owned activity discriminators",
            );
            assert.match(result.stderr, /provider_lifecycle\.rs/);
            assert.match(result.stderr, /FutureActivity.*not covered/);
          } finally {
            writeFileSync(lifecycle, originalLifecycle);
            generate();
          }
        },
      );
      const final = check();
      assert.equal(final.status, 0, final.stdout + final.stderr);
    } finally {
      rmSync(copy, { recursive: true, force: true });
    }
  },
);
