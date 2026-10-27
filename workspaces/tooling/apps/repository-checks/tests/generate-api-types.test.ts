import assert from "node:assert/strict";
import { createReadStream, readFileSync } from "node:fs";
import { spawn, spawnSync } from "node:child_process";
import { once } from "node:events";
import { pipeline } from "node:stream/promises";
import { fileURLToPath } from "node:url";
import test from "node:test";
import ts from "typescript";
import type { OpenAPI3 } from "openapi-typescript";

const schema = {
  openapi: "3.1.0",
  info: { title: "Optional property witness", version: "1" },
  components: {
    schemas: {
      RequestStatus: { type: "string", enum: ["pending", "answered"] },
      JsonValue: { type: "string" },
      WitnessEntityDto: {
        additionalProperties: { $ref: "#/components/schemas/JsonValue" },
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
  assert.match(result.stdout, /export const RequestStatus = \{/);
  assert.match(result.stdout, /pending: "pending"/);
  assert.match(result.stdout, /answered: "answered"/);
  const source = ts.createSourceFile(
    "generated.ts",
    result.stdout,
    ts.ScriptTarget.Latest,
    true,
    ts.ScriptKind.TS,
  );
  const properties = new Map<string, ts.PropertySignature>();
  const indexes: ts.IndexSignatureDeclaration[] = [];
  const visit = (node: ts.Node) => {
    if (ts.isPropertySignature(node))
      properties.set(node.name.getText(source), node);
    if (ts.isIndexSignatureDeclaration(node)) indexes.push(node);
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
  assert.equal(indexes.length, 1);
  assert.ok(
    indexes[0]?.type &&
      ts.isUnionTypeNode(indexes[0].type) &&
      indexes[0].type.types.some(
        (type) => type.kind === ts.SyntaxKind.UndefinedKeyword,
      ),
    "extension map allows undefined optional members",
  );
  const required = properties.get("required");
  assert.ok(required?.type);
  assert.equal(required.questionToken, undefined);
  assert.equal(required.type.kind, ts.SyntaxKind.StringKeyword);
});

test("the recursive JsonValue alias stays strict", () => {
  // The checked-in Rust OpenAPI output owns the recursive schema used by this fixture.
  const document: OpenAPI3 = JSON.parse(
    readFileSync(
      new URL(
        "../../../../runtime/packages/runtime-http/openapi.json",
        import.meta.url,
      ),
      "utf8",
    ),
  );
  const jsonValue = document.components?.schemas?.JsonValue;
  assert.ok(jsonValue);
  const input = {
    ...schema,
    components: {
      schemas: { ...schema.components.schemas, JsonValue: jsonValue },
    },
  } satisfies OpenAPI3;
  for (const mode of ["-", "--rust-owned"]) {
    const result = spawnSync(
      process.execPath,
      [
        fileURLToPath(new URL("../generate-api-types.ts", import.meta.url)),
        mode,
      ],
      { input: JSON.stringify(input), encoding: "utf8" },
    );
    assert.equal(result.status, 0, result.stderr);
    const source = ts.createSourceFile(
      "generated.ts",
      result.stdout,
      ts.ScriptTarget.Latest,
      true,
      ts.ScriptKind.TS,
    );
    const alias = source.statements.find(
      (node) =>
        ts.isTypeAliasDeclaration(node) && node.name.text === "JsonValue",
    );
    assert.ok(alias && ts.isTypeAliasDeclaration(alias));
    const visit = (node: ts.Node) => {
      assert.notEqual(node.kind, ts.SyntaxKind.UndefinedKeyword);
      ts.forEachChild(node, visit);
    };
    assert.ok(
      ts.isUnionTypeNode(alias.type),
      "JsonValue has an independent recursive alias",
    );
    visit(alias.type);
  }
});

test("recursive generation handles array-only schemas and properties named JsonValue", () => {
  const document = {
    ...schema,
    components: {
      schemas: {
        AEntityDto: {
          type: "object",
          required: ["JsonValue"],
          properties: { JsonValue: { type: "string" } },
        },
        JsonValue: {
          type: ["null", "string", "array"],
          anyOf: [
            { type: "null" },
            { type: "string" },
            {
              type: "array",
              items: { $ref: "#/components/schemas/JsonValue" },
            },
          ],
        },
      },
    },
  } satisfies OpenAPI3;
  for (const mode of ["-", "--rust-owned"]) {
    const result = spawnSync(
      process.execPath,
      [
        fileURLToPath(new URL("../generate-api-types.ts", import.meta.url)),
        mode,
      ],
      { input: JSON.stringify(document), encoding: "utf8" },
    );
    assert.equal(result.status, 0, result.stderr);
    const file = "/generated-api-witness.ts";
    const input = `${result.stdout}
const entity: AEntityDto = { JsonValue: "preserved" };
const json: JsonValue = { nested: [null, "value", false, 1] };
// @ts-expect-error Required properties remain required.
const missing: AEntityDto = {};
// @ts-expect-error A same-named property keeps its original type.
const wrong: AEntityDto = { JsonValue: null };
// @ts-expect-error Recursive JSON cannot contain undefined.
const invalidJson: JsonValue = { nested: [undefined] };
`;
    const host = ts.createCompilerHost({});
    const readSource = host.getSourceFile.bind(host);
    host.getSourceFile = (name, ...args) =>
      name === file
        ? ts.createSourceFile(name, input, ts.ScriptTarget.Latest, true)
        : readSource(name, ...args);
    const program = ts.createProgram(
      [file],
      {
        strict: true,
        exactOptionalPropertyTypes: true,
        noEmit: true,
        skipLibCheck: true,
        target: ts.ScriptTarget.ESNext,
      },
      host,
    );
    assert.deepEqual(
      ts
        .getPreEmitDiagnostics(program)
        .map((diagnostic) =>
          ts.flattenDiagnosticMessageText(diagnostic.messageText, "\n"),
        ),
      [],
    );
  }
});

test("the repository OpenAPI preserves committed declarations without format churn", () => {
  const result = spawnSync(
    process.execPath,
    [
      fileURLToPath(new URL("../generate-api-types.ts", import.meta.url)),
      fileURLToPath(
        new URL(
          "../../../../runtime/packages/runtime-http/openapi.json",
          import.meta.url,
        ),
      ),
    ],
    { encoding: "utf8", maxBuffer: Infinity },
  );
  assert.equal(result.status, 0, result.stderr);
  const declarations = (text: string) => {
    const source = ts.createSourceFile(
      "api.ts",
      text,
      ts.ScriptTarget.Latest,
      true,
    );
    return source.statements
      .filter(
        (node) =>
          ts.isInterfaceDeclaration(node) || ts.isTypeAliasDeclaration(node),
      )
      .map((node) => node.getText(source));
  };
  assert.deepEqual(
    declarations(result.stdout),
    declarations(
      readFileSync(
        new URL(
          "../../../../client/apps/web/src/generated/api.ts",
          import.meta.url,
        ),
        "utf8",
      ),
    ),
  );
});

test("runtime enum exports use schema-owned strings and reject non-string enums", async () => {
  const statuses = {
    RequestStatus: { type: "string", enum: ["pending", "uncertain"] },
    TaskStatus: { type: "string", enum: ["running", "lost"] },
    TransferStatus: { type: "string", enum: ["pending", "completed"] },
  } satisfies NonNullable<NonNullable<OpenAPI3["components"]>["schemas"]>;
  const document = {
    ...schema,
    components: { schemas: statuses },
  } satisfies OpenAPI3;
  const generate = (input: OpenAPI3) =>
    spawnSync(
      process.execPath,
      [
        fileURLToPath(new URL("../generate-api-types.ts", import.meta.url)),
        "--rust-owned",
      ],
      { input: JSON.stringify(input), encoding: "utf8" },
    );
  const result = generate(document);
  assert.equal(result.status, 0, result.stderr);
  const javascript = ts.transpileModule(result.stdout, {
    compilerOptions: { module: ts.ModuleKind.ESNext },
  }).outputText;
  const exports = await import(
    `data:text/javascript;base64,${Buffer.from(javascript).toString("base64")}`
  );
  for (const [name, definition] of Object.entries(statuses))
    assert.deepEqual(
      exports[name],
      Object.fromEntries(definition.enum.map((value) => [value, value])),
    );
  const invalid = generate({
    ...document,
    components: { schemas: { TransferStatus: { type: "number", enum: [1] } } },
  });
  assert.notEqual(invalid.status, 0);
  assert.match(invalid.stderr, /TransferStatus must be a closed string enum/);
});

test("API generation rejects a document without its recursive JSON owner", () => {
  const result = spawnSync(
    process.execPath,
    [fileURLToPath(new URL("../generate-api-types.ts", import.meta.url)), "-"],
    { input: JSON.stringify(schema), encoding: "utf8" },
  );
  assert.notEqual(result.status, 0);
  assert.match(result.stderr, /must define a recursive JsonValue component/);
});

test("streamed OpenAPI stdin preserves file-input generation across pipe backpressure", async () => {
  const generator = fileURLToPath(
    new URL("../generate-api-types.ts", import.meta.url),
  );
  const input = fileURLToPath(
    new URL(
      "../../../../runtime/packages/runtime-http/openapi.json",
      import.meta.url,
    ),
  );
  const expected = spawnSync(process.execPath, [generator, input], {
    encoding: "utf8",
    maxBuffer: Infinity,
  });
  assert.equal(expected.status, 0, expected.stderr);
  const child = spawn(process.execPath, [generator, "-"], {
    stdio: ["pipe", "pipe", "pipe"],
  });
  let output = "";
  let errors = "";
  child.stdout.setEncoding("utf8").on("data", (chunk: string) => {
    output += chunk;
  });
  child.stderr.setEncoding("utf8").on("data", (chunk: string) => {
    errors += chunk;
  });
  const completed = once(child, "close");
  const [transferred, exited] = await Promise.allSettled([
    pipeline(createReadStream(input, { highWaterMark: 4096 }), child.stdin),
    completed,
  ]);
  assert.ok(exited.status === "fulfilled", errors);
  assert.equal(exited.value[0], 0, errors);
  assert.equal(transferred.status, "fulfilled", errors);
  assert.equal(output, expected.stdout);
});
