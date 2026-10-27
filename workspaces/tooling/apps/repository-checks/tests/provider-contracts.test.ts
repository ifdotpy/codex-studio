import assert from "node:assert/strict";
import { readFileSync, mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";
import test from "node:test";
import {
  renderCodexContracts,
  checkCodexContracts,
  refreshCodexSchemas,
  checkClaudeRateContract,
} from "../generate-provider-contracts.ts";
import { codexContractSource } from "../provider-contract-sources.ts";

const root = fileURLToPath(new URL("../../../../..", import.meta.url));

test("the SDK-backed rate parser owns the committed Rust validation schema", async () => {
  await checkClaudeRateContract(root);
  await assert.rejects(
    () =>
      checkClaudeRateContract(root, (file) => {
        const schema = JSON.parse(readFileSync(file, "utf8"));
        schema.properties.status.enum.push("future");
        return JSON.stringify(schema);
      }),
    /Stale Claude rate contract/,
  );
});

test("committed Rust and TypeScript consume the same vendor schema", () => {
  assert.doesNotThrow(() => checkCodexContracts(root));
});

test("a vendor refresh fails explicitly when the pinned CLI is absent", () => {
  const directory = mkdtempSync(path.join(tmpdir(), "studio-no-provider-"));
  try {
    assert.throws(
      () => refreshCodexSchemas(root, path.join(directory, "codex"), true),
      /ENOENT/,
    );
  } finally {
    rmSync(directory, { recursive: true, force: true });
  }
});

test("regeneration cannot relabel stale vendor schemas as a new pinned version", () => {
  assert.throws(
    () =>
      renderCodexContracts(root, (file) => {
        const text = readFileSync(file, "utf8");
        return file.endsWith("lifecycle.schema.json")
          ? text.replace(
              `codex-cli ${codexContractSource.version}`,
              "codex-cli future",
            )
          : text;
      }),
    /provenance differs/,
  );
});

test("the selected schema generates added variants and changed fields", () => {
  const render = renderCodexContracts(root, (file) => {
    const text = readFileSync(file, "utf8");
    if (file.endsWith("lifecycle.schema.json")) {
      const value = JSON.parse(text);
      value.definitions.TurnStatus.enum.push("futureStatus");
      return JSON.stringify(value);
    }
    return text;
  });
  const rust = render.find((entry) =>
    entry.path.endsWith("codex_generated.rs"),
  );
  assert.ok(rust?.text.includes("FutureStatus"));
  assert.ok(
    render
      .find((entry) => entry.path.endsWith("/TurnStatus.ts"))
      ?.text.includes('"futureStatus"'),
  );
});

test("freshness refuses a stale owner rather than compiling the old types", () => {
  assert.throws(
    () =>
      checkCodexContracts(
        root,
        (file) =>
          readFileSync(file, "utf8") +
          (file.endsWith("codex_generated.rs") ? "// stale\n" : ""),
      ),
    /Stale Codex contract/,
  );
});

for (const mutation of ["requiredDefault", "optionalNonNullable"] as const) {
  test(`generation refuses the unsupported ${mutation} presence codec`, () => {
    assert.throws(
      () =>
        renderCodexContracts(root, (file) => {
          const text = readFileSync(file, "utf8");
          if (!file.endsWith("ThreadTokenUsageUpdatedNotification.json"))
            return text;
          const schema = JSON.parse(text);
          if (mutation === "requiredDefault")
            schema.definitions.TokenUsageBreakdown.required.push(
              "cacheWriteInputTokens",
            );
          else
            schema.definitions.ThreadTokenUsage.properties.modelContextWindow.type =
              "integer";
          return JSON.stringify(schema);
        }),
      /Unsupported vendor presence codec/,
    );
  });
}
