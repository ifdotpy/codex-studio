import { readFileSync } from "node:fs";
import { createRequire } from "node:module";
import { fileURLToPath } from "node:url";
import ts from "typescript";

const require = createRequire(import.meta.url);
const declarations = require
  .resolve("@anthropic-ai/claude-agent-sdk")
  .replace(/sdk\.mjs$/, "sdk.d.ts");
const sdk = readFileSync(declarations, "utf8");
const toolDeclarations = declarations.replace(/sdk\.d\.ts$/, "sdk-tools.d.ts");
const tools = readFileSync(toolDeclarations, "utf8");
const directory = fileURLToPath(new URL("..", import.meta.url));
const config = ts.readConfigFile(directory + "tsconfig.json", ts.sys.readFile);
const { options } = ts.parseJsonConfigFileContent(
  config.config,
  ts.sys,
  directory,
);

export function sdkDiagnostics(
  file: string | string[],
  mutation?: (source: string) => string,
  owner: "sdk" | "tools" = "sdk",
) {
  const host = ts.createCompilerHost(options);
  const read = host.readFile.bind(host);
  host.readFile = (name) =>
    mutation && name === (owner === "sdk" ? declarations : toolDeclarations)
      ? mutation(owner === "sdk" ? sdk : tools)
      : read(name);
  const program = ts.createProgram(
    (Array.isArray(file) ? file : [file]).map((file) => directory + file),
    options,
    host,
  );
  return ts
    .getPreEmitDiagnostics(program)
    .filter((error) => !error.file?.isDeclarationFile);
}

// Mutate the actual published declaration node, without changing dependencies or
// relying on a handwritten substitute type in a negative compiler witness.
export function mutateSdkField(
  source: string,
  ownerName: string,
  path: string[],
  change: (type: string) => string | null,
): string {
  const ast = ts.createSourceFile(
    "sdk.d.ts",
    source,
    ts.ScriptTarget.ESNext,
    true,
  );
  const declaration = ast.statements.find(
    (node) =>
      (ts.isTypeAliasDeclaration(node) || ts.isInterfaceDeclaration(node)) &&
      node.name.text === ownerName,
  );
  if (!declaration) throw new Error(`Missing SDK owner ${ownerName}`);
  let owner: ts.Node = declaration;
  let field: ts.PropertySignature | undefined;
  function findField(
    node: ts.Node,
    name: string,
  ): ts.PropertySignature | undefined {
    if (ts.isPropertySignature(node) && node.name.getText(ast) === name)
      return node;
    return ts.forEachChild(node, (child) => findField(child, name));
  }
  for (const name of path) {
    field = findField(owner, name);
    if (!field || !field.type)
      throw new Error(`Missing SDK field ${ownerName}.${path.join(".")}`);
    owner = field.type;
  }
  if (!field?.type) throw new Error("Empty SDK mutation path");
  const next = change(field.type.getText(ast));
  return next === null
    ? source.slice(0, field.getStart(ast)) + source.slice(field.getEnd())
    : source.slice(0, field.type.getStart(ast)) +
        next +
        source.slice(field.type.getEnd());
}
