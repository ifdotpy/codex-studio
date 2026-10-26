#!/usr/bin/env node
// Print the TypeScript declarations of an OpenAPI document: openapi-typescript
// output with the recursive JsonValue alias and the *EntityDto aliases,
// formatted by oxfmt. `studio-contracts` (runtime-schema) runs this script,
// appends the Rust-owned constants, and writes or checks `api.ts`.
import ts from "typescript";
import { spawnSync } from "node:child_process";
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), "../../../..");
const BIN = join(ROOT, "node_modules", ".bin");
const JSON_VALUE_ALIAS =
  "export type JsonValue = null | boolean | number | string | JsonValue[] " +
  "| { [key: string]: JsonValue };";

function run(command: string, args: string[]) {
  const result = spawnSync(join(BIN, command), args, {
    cwd: ROOT,
    stdio: ["ignore", "ignore", "inherit"],
  });
  if (result.error) {
    throw new Error(
      `${command} failed to start (${result.error.message}); run \`pnpm install --frozen-lockfile\``,
    );
  }
  if (result.status !== 0)
    throw new Error(`${command} exited ${result.status}`);
}

// Break openapi-typescript's indexed self-reference for the recursive JSON schema.
function normalizeRecursiveJsonValue(generated: string) {
  const failure =
    "openapi-typescript output has no unique recursive JsonValue component";
  const start = /^([ \t]*)JsonValue: /m.exec(generated);
  if (!start) throw new Error(failure);
  const indent = start[1] ?? "";
  const afterStart = start.index + start[0].length;
  const ending = new RegExp(
    `^${indent.replace(/[.*+?^${}()|[\]\\]/g, "\\$&")}}[^;\\n]*;`,
    "m",
  ).exec(generated.slice(afterStart));
  if (!ending) throw new Error(failure);
  const end = afterStart + ending.index + ending[0].length;
  if (
    !generated
      .slice(start.index, end)
      .includes('components["schemas"]["JsonValue"]')
  ) {
    throw new Error(failure);
  }
  return `${generated.slice(0, start.index)}${indent}JsonValue: JsonValue;${generated.slice(end)}`;
}

function entityAliases(document: import("openapi-typescript").OpenAPI3) {
  const schemas = document.components?.schemas ?? {};
  const names = Object.keys(schemas)
    .filter((name) => name.endsWith("EntityDto"))
    .sort();
  if (names.length === 0)
    throw new Error("OpenAPI document has no *EntityDto component schemas");
  if ("SyncEntityPayload" in schemas) names.push("SyncEntityPayload");
  return names
    .map((name) => `\nexport type ${name} = components["schemas"]["${name}"];`)
    .join("");
}

// JSON omits undefined members. Keep that same input contract with exact optional types.
function optionalPropertyInputs(generated: string) {
  const source = ts.createSourceFile(
    "api.ts",
    generated,
    ts.ScriptTarget.Latest,
    true,
    ts.ScriptKind.TS,
  );
  const insertions: { position: number; text: string }[] = [];
  const visit = (node: ts.Node) => {
    if (ts.isPropertySignature(node) && node.questionToken && node.type) {
      const type = node.type;
      const types = ts.isUnionTypeNode(type) ? type.types : [type];
      if (
        !types.some((member) => member.kind === ts.SyntaxKind.UndefinedKeyword)
      ) {
        const grouped =
          ts.isFunctionTypeNode(type) ||
          ts.isConstructorTypeNode(type) ||
          ts.isConditionalTypeNode(type);
        if (grouped)
          insertions.push({ position: type.getStart(source), text: "(" });
        insertions.push({
          position: type.end,
          text: grouped ? ") | undefined" : " | undefined",
        });
      }
    }
    ts.forEachChild(node, visit);
  };
  visit(source);
  for (const { position, text } of insertions.sort(
    (a, b) => b.position - a.position,
  ))
    generated = generated.slice(0, position) + text + generated.slice(position);
  return generated;
}

const input = process.argv[2];
if (!input)
  throw new Error(
    "usage: generate-api-types.ts <openapi.json | - | --rust-owned>",
  );
const rustOwned = input === "--rust-owned";
const fromStdin = rustOwned || input === "-";
const document: import("openapi-typescript").OpenAPI3 = JSON.parse(
  readFileSync(fromStdin ? 0 : input, "utf8"),
);
const temp = mkdtempSync(join(tmpdir(), "studio-api-types-"));
try {
  const output = join(temp, "api.ts");
  const schemaInput = fromStdin ? join(temp, "openapi.json") : input;
  if (fromStdin) writeFileSync(schemaInput, JSON.stringify(document));
  run("openapi-typescript", [
    schemaInput,
    "--output",
    output,
    "--alphabetize",
    "--default-non-nullable=false",
  ]);
  const generated = optionalPropertyInputs(readFileSync(output, "utf8"));
  if (!generated.includes("export interface components")) {
    throw new Error(
      "openapi-typescript output is missing the components export",
    );
  }
  writeFileSync(
    output,
    rustOwned
      ? `${generated}\n${Object.keys(document.components?.schemas ?? {})
          .sort()
          .map(
            (name) => `export type ${name} = components["schemas"]["${name}"];`,
          )
          .join("\n")}\n`
      : `${normalizeRecursiveJsonValue(generated)}\n\n${JSON_VALUE_ALIAS}${entityAliases(document)}\n`,
  );
  run("oxfmt", ["--write", `--config=${join(ROOT, ".oxfmtrc.json")}`, output]);
  process.stdout.write(readFileSync(output, "utf8"));
} finally {
  rmSync(temp, { recursive: true, force: true });
}
