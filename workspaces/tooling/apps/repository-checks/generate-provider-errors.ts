// Retain only the Codex error leaf and its referenced definition. The installed
// generator owns the schema; studio-contracts owns its TypeScript consumers.
import { execFileSync } from "node:child_process";
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const root = resolve(dirname(fileURLToPath(import.meta.url)), "../../../..");
const codex = process.env.CODEX_BIN || "codex";
const version = "0.164.0";
const actual = execFileSync(codex, ["--version"], { encoding: "utf8" }).trim();
if (actual !== `codex-cli ${version}`)
  throw new Error(`Expected codex-cli ${version}; received ${actual}`);
const directory = mkdtempSync(join(tmpdir(), "studio-error-contract-"));
try {
  execFileSync(codex, [
    "app-server",
    "generate-json-schema",
    "--experimental",
    "--out",
    directory,
  ]);
  const schema = JSON.parse(
    readFileSync(join(directory, "v2/ErrorNotification.json"), "utf8"),
  );
  const leaf = {
    $schema: schema.$schema,
    $comment: `Codex CLI ${version}: app-server generate-json-schema --experimental; definitions/CodexErrorInfo from v2/ErrorNotification.json. Refresh with pnpm run contracts:vendor-errors.`,
    title: "CodexErrorInfo",
    ...schema.definitions.CodexErrorInfo,
    definitions: {
      NonSteerableTurnKind: schema.definitions.NonSteerableTurnKind,
    },
  };
  const target = join(
    root,
    "workspaces/runtime/packages/runtime-schema/src/schemas/codex/CodexErrorInfo.json",
  );
  const output = execFileSync(
    join(root, "node_modules/.bin/oxfmt"),
    [`--stdin-filepath=${target}`, `--config=${join(root, ".oxfmtrc.json")}`],
    { input: JSON.stringify(leaf, null, 2) + "\n", encoding: "utf8" },
  );
  if (process.argv.includes("--check")) {
    if (readFileSync(target, "utf8") !== output)
      throw new Error(
        "Codex error leaf differs; run pnpm run contracts:vendor-errors",
      );
  } else writeFileSync(target, output);
} finally {
  rmSync(directory, { recursive: true, force: true });
}
