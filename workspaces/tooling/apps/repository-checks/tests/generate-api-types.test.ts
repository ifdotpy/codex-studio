import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { fileURLToPath } from "node:url";
import test from "node:test";
import ts from "typescript";
import type { OpenAPI3 } from "openapi-typescript";

const schema = {
  openapi: "3.1.0",
  info: { title: "Optional property witness", version: "1" },
  components: {
    schemas: {
      Witness: {
        type: "object",
        required: ["required"],
        properties: {
          required: { type: "string" },
          optional: { type: "string" },
          nested: {
            type: "object",
            properties: { optionalChild: { type: "number" } },
          },
          nullable: { type: ["string", "null"] },
        },
      },
    },
  },
} satisfies OpenAPI3;

test("generated optional properties allow undefined and required properties stay required", () => {
  const result = spawnSync(
    process.execPath,
    [
      fileURLToPath(new URL("../generate-api-types.ts", import.meta.url)),
      "--rust-owned",
    ],
    { input: JSON.stringify(schema), encoding: "utf8" },
  );
  assert.equal(result.status, 0, result.stderr);
  const source = ts.createSourceFile(
    "generated.ts",
    result.stdout,
    ts.ScriptTarget.Latest,
    true,
    ts.ScriptKind.TS,
  );
  const properties = new Map<string, ts.PropertySignature>();
  const visit = (node: ts.Node) => {
    if (ts.isPropertySignature(node))
      properties.set(node.name.getText(source), node);
    ts.forEachChild(node, visit);
  };
  visit(source);
  for (const name of ["optional", "optionalChild", "nullable", "nested"]) {
    const property = properties.get(name);
    assert.ok(property?.questionToken, `${name} stays optional`);
    assert.ok(
      property.type &&
        ts.isUnionTypeNode(property.type) &&
        property.type.types.some(
          (type) => type.kind === ts.SyntaxKind.UndefinedKeyword,
        ),
      `${name} explicitly allows undefined`,
    );
  }
  const required = properties.get("required");
  assert.ok(required?.type);
  assert.equal(required.questionToken, undefined);
  assert.equal(required.type.kind, ts.SyntaxKind.StringKeyword);
});
