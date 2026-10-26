import { readFileSync } from "node:fs";
import { createRequire } from "node:module";
import { fileURLToPath } from "node:url";
import ts from "typescript";

const require = createRequire(import.meta.url);
const declarations = require
  .resolve("@anthropic-ai/claude-agent-sdk")
  .replace(/sdk\.mjs$/, "sdk.d.ts");
const sdk = readFileSync(declarations, "utf8");
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
) {
  const host = ts.createCompilerHost(options);
  const read = host.readFile.bind(host);
  host.readFile = (name) =>
    name === declarations && mutation ? mutation(sdk) : read(name);
  const program = ts.createProgram(
    (Array.isArray(file) ? file : [file]).map((file) => directory + file),
    options,
    host,
  );
  return ts
    .getPreEmitDiagnostics(program)
    .filter((error) => !error.file?.isDeclarationFile);
}
