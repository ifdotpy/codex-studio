#!/usr/bin/env node
// Print the TypeScript declarations of an OpenAPI document: openapi-typescript
// output with the recursive JsonValue alias and the *EntityDto aliases,
// formatted by oxfmt. `studio-contracts` (runtime-schema) runs this script,
// appends the Rust-owned constants, and writes or checks `api.ts`.
import ts from "typescript";
import openapiTS, { astToString, COMMENT_HEADER } from "openapi-typescript";
import { spawnSync } from "node:child_process";
import { readFileSync } from "node:fs";
import { text } from "node:stream/consumers";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), "../../../..");
const BIN = join(ROOT, "node_modules", ".bin");
const JSON_VALUE_ALIAS =
  "export type JsonValue = null | boolean | number | string | JsonValue[] " +
  "| { [key: string]: JsonValue };";

// Runtime consumers use these closed domains from the same schema as their types.
function runtimeEnumValues(document: import("openapi-typescript").OpenAPI3) {
  return ["RequestStatus", "TaskStatus", "TransferStatus"]
    .flatMap((name) => {
      const schema = document.components?.schemas?.[name];
      if (!schema) return [];
      if (
        !("enum" in schema) ||
        !schema.enum?.every((value) => typeof value === "string")
      )
        throw new Error(`${name} must be a closed string enum`);
      const entries = schema.enum.map(
        (value) => `${JSON.stringify(value)}: ${JSON.stringify(value)}`,
      );
      return [`export const ${name} = { ${entries.join(", ")} } as const;`];
    })
    .join("\n");
}

function format(input: string) {
  const result = spawnSync(
    join(BIN, "oxfmt"),
    ["--stdin-filepath=api.ts", `--config=${join(ROOT, ".oxfmtrc.json")}`],
    {
      cwd: ROOT,
      input,
      encoding: "utf8",
      stdio: ["pipe", "pipe", "inherit"],
      maxBuffer: Infinity,
    },
  );
  if (result.error) {
    throw new Error(
      `oxfmt failed to start (${result.error.message}); run \`pnpm install --frozen-lockfile\``,
    );
  }
  if (result.status !== 0) throw new Error(`oxfmt exited ${result.status}`);
  return result.stdout;
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
function optionalPropertyInputs(nodes: ts.Node[]) {
  const result = ts.transform(nodes, [
    (context) => {
      const visit: ts.Visitor = (input) => {
        const node = ts.visitEachChild(input, visit, context);
        const extensionMap =
          ts.isIndexSignatureDeclaration(node) &&
          ts.isIndexedAccessTypeNode(node.type) &&
          ts.isLiteralTypeNode(node.type.indexType) &&
          ts.isStringLiteral(node.type.indexType.literal) &&
          node.type.indexType.literal.text === "JsonValue";
        if (
          (ts.isPropertySignature(node) && node.questionToken && node.type) ||
          extensionMap
        ) {
          const type = ts.factory.createUnionTypeNode([
            ...(ts.isUnionTypeNode(node.type) ? node.type.types : [node.type]),
            ts.factory.createKeywordTypeNode(ts.SyntaxKind.UndefinedKeyword),
          ]);
          return ts.isPropertySignature(node)
            ? ts.factory.updatePropertySignature(
                node,
                node.modifiers,
                node.name,
                node.questionToken,
                type,
              )
            : ts.factory.updateIndexSignature(
                node,
                node.modifiers,
                node.parameters,
                type,
              );
        }
        return node;
      };
      return (node) => ts.visitNode(node, visit) ?? node;
    },
  ]);
  try {
    return astToString(result.transformed);
  } finally {
    result.dispose();
  }
}

const input = process.argv[2];
if (!input)
  throw new Error(
    "usage: generate-api-types.ts <openapi.json | - | --rust-owned>",
  );
const rustOwned = input === "--rust-owned";
const fromStdin = rustOwned || input === "-";
const document: import("openapi-typescript").OpenAPI3 = JSON.parse(
  fromStdin ? await text(process.stdin) : readFileSync(input, "utf8"),
);
const recursiveJsonValue =
  JSON.stringify(document.components?.schemas?.JsonValue)?.includes(
    '"#/components/schemas/JsonValue"',
  ) ?? false;
if (!rustOwned && !recursiveJsonValue)
  throw new Error(
    "OpenAPI document must define a recursive JsonValue component",
  );
const nodes = await openapiTS(
  fromStdin ? document : pathToFileURL(resolve(input)),
  {
    alphabetize: true,
    defaultNonNullable: false,
    transform(_schema, { path }) {
      if (recursiveJsonValue && path === "#/components/schemas/JsonValue")
        return ts.factory.createTypeReferenceNode("JsonValue");
      return undefined;
    },
  },
);
const generated = COMMENT_HEADER + optionalPropertyInputs(nodes);
const declarations = recursiveJsonValue
  ? `${generated}\n${JSON_VALUE_ALIAS}`
  : generated;
const runtimeValues = runtimeEnumValues(document);
const runtimeDeclarations = runtimeValues ? `${runtimeValues}\n` : "";
process.stdout.write(
  format(
    rustOwned
      ? `${declarations}\n${runtimeDeclarations}${Object.keys(
          document.components?.schemas ?? {},
        )
          .filter((name) => name !== "JsonValue" || !recursiveJsonValue)
          .sort()
          .map(
            (name) => `export type ${name} = components["schemas"]["${name}"];`,
          )
          .join("\n")}\n`
      : `${declarations}${entityAliases(document)}\n${runtimeDeclarations}`,
  ),
);
