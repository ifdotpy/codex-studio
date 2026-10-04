import { readFile, writeFile, mkdir } from "node:fs/promises";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import Ajv2020 from "ajv/dist/2020.js";
import standaloneCode from "ajv/dist/standalone/index.js";
import { build } from "esbuild";

const schemaId = "urn:codex-studio:stream-contract";
const schemaExports = {
  isResourceRef: "ResourceRef",
  isResourceChangeEvent: "ResourceChangeEvent",
  isResourceHeartbeatEvent: "ResourceHeartbeatEvent",
};
const banner = "// Generated from the Python OpenAPI contract. Do not edit.\n";

function localReferences(value) {
  if (Array.isArray(value)) return value.map(localReferences);
  if (value !== null && typeof value === "object") {
    return Object.fromEntries(
      Object.entries(value).map(([key, item]) => [
        key,
        key === "$ref" && typeof item === "string"
          ? item.replace(/^#\/components\/schemas\//, "#/$defs/")
          : localReferences(item),
      ]),
    );
  }
  return value;
}

/** Compile only the named wire contracts, never a second handwritten schema. */
export async function renderStreamValidators(document) {
  const schemas = document.components?.schemas;
  for (const name of Object.values(schemaExports)) {
    if (!schemas?.[name]) throw new Error(`Missing stream schema: ${name}`);
  }
  const ajv = new Ajv2020({
    code: { source: true, esm: true, lines: true },
    coerceTypes: false,
    useDefaults: false,
    removeAdditional: false,
    validateFormats: false,
  });
  // OpenAPI's discriminator is an annotation. The emitted oneOf enforces it.
  ajv.addKeyword({
    keyword: "discriminator",
    schemaType: "object",
    valid: true,
  });
  ajv.addSchema({ $id: schemaId, $defs: localReferences(schemas) });
  const refs = Object.fromEntries(
    Object.entries(schemaExports).map(([name, schema]) => [
      name,
      `${schemaId}#/$defs/${schema}`,
    ]),
  );
  const compiled = standaloneCode(ajv, refs);
  // Bundle helpers: the browser needs neither Ajv's compiler nor unsafe-eval.
  const bundled = await build({
    stdin: {
      contents: compiled,
      sourcefile: "stream-validators.js",
      resolveDir: fileURLToPath(new URL("../..", import.meta.url)),
    },
    bundle: true,
    platform: "browser",
    format: "esm",
    target: "es2022",
    write: false,
    legalComments: "inline",
  });
  const declarations = Object.entries(schemaExports)
    .map(
      ([name, schema]) =>
        `export declare function ${name}(value: unknown): value is components["schemas"]["${schema}"];`,
    )
    .join("\n");
  return {
    // Ajv emits shared context arguments and a break after an early return.
    // Keep compiler output intact; these two authoring rules do not apply to it.
    "stream-validators.js":
      banner +
      "/* oxlint-disable no-unused-vars, no-unreachable -- unmodified Ajv compiler output */\n" +
      bundled.outputFiles[0].text,
    "stream-validators.d.ts":
      banner +
      'import type { components } from "./api";\n' +
      declarations +
      "\n",
  };
}

async function main() {
  const [input, output] = process.argv.slice(2);
  if (!input || !output)
    throw new Error("Expected OpenAPI file and output directory");
  const result = await renderStreamValidators(
    JSON.parse(await readFile(input, "utf8")),
  );
  for (const [name, text] of Object.entries(result)) {
    const path = resolve(output, name);
    await mkdir(dirname(path), { recursive: true });
    await writeFile(path, text);
  }
}

if (
  process.argv[1] &&
  resolve(process.argv[1]) === fileURLToPath(import.meta.url)
) {
  await main();
}
