// Generate only consumed Codex wire shapes. Unsupported schema constructs fail
// here instead of silently becoming an untyped or invented protocol variant.
import { readFileSync, writeFileSync, mkdtempSync, rmSync } from "node:fs";
import { execFileSync } from "node:child_process";
import { createHash } from "node:crypto";
import { tmpdir } from "node:os";
import path from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";
import { codexContractSource as source } from "./provider-contract-sources.ts";

type ObjectValue = Record<string, unknown>;
type Reader = (file: string) => string;
interface Rendered {
  path: string;
  text: string;
}
const root = fileURLToPath(new URL("../../../..", import.meta.url));
const rustPath =
  "workspaces/runtime/packages/runtime-schema/src/codex_generated.rs";
const bridge = "workspaces/providers/apps/claude-bridge/codex-generated";
const provenance = `Owner: codex-cli ${source.version}; app-server generate-json-schema --experimental.`;
const read: Reader = (file) => readFileSync(file, "utf8");
function object(value: unknown): ObjectValue {
  if (!value || typeof value !== "object" || Array.isArray(value))
    throw new Error("Expected vendor schema object");
  return value as ObjectValue;
}
function strings(value: unknown): string[] {
  if (
    !Array.isArray(value) ||
    !value.every((item): item is string => typeof item === "string")
  )
    throw new Error("Expected vendor string list");
  return value;
}
function parse(text: string): ObjectValue {
  return object(JSON.parse(text));
}
const pascal = (name: string) => name[0]?.toUpperCase() + name.slice(1);
const snake = (name: string) =>
  name.replace(/[A-Z]/g, (letter) => "_" + letter.toLowerCase());
function identifier(name: string): string {
  if (!/^[a-zA-Z_][a-zA-Z0-9_]*$/.test(name))
    throw new Error(`Unsupported vendor identifier ${name}`);
  return name;
}
function rustType(value: unknown): string {
  const schema = object(value);
  if (
    typeof schema.$ref === "string" &&
    schema.$ref.startsWith("#/definitions/")
  )
    return "Codex" + identifier(schema.$ref.slice("#/definitions/".length));
  if (Array.isArray(schema.type)) {
    const types = strings(schema.type);
    const nonNull = types.filter((type) => type !== "null");
    if (types.length !== 2 || nonNull.length !== 1)
      throw new Error("Unsupported vendor type union");
    return `Option<${rustType({ ...schema, type: nonNull[0] })}>`;
  }
  switch (schema.type) {
    case "string":
      return "String";
    case "integer":
      if (schema.format !== "int64")
        throw new Error("Unsupported vendor integer format");
      return "i64";
    case "boolean":
      return "bool";
    case "array":
      return `Vec<${rustType(schema.items)}>`;
    default:
      throw new Error(`Unsupported vendor field ${JSON.stringify(schema)}`);
  }
}
function tsType(value: unknown): string {
  const schema = object(value);
  if (
    typeof schema.$ref === "string" &&
    schema.$ref.startsWith("#/definitions/")
  )
    return identifier(schema.$ref.slice("#/definitions/".length));
  if (Array.isArray(schema.type))
    return strings(schema.type)
      .map((type) => (type === "null" ? "null" : tsType({ ...schema, type })))
      .join(" | ");
  switch (schema.type) {
    case "string":
      return "string";
    case "integer":
      return "number";
    case "boolean":
      return "boolean";
    case "array":
      return `Array<${tsType(schema.items)}>`;
    default:
      throw new Error(`Unsupported vendor field ${JSON.stringify(schema)}`);
  }
}
function fields(schema: ObjectValue, excluded?: string): string {
  const required =
    schema.required === undefined ? [] : strings(schema.required);
  return Object.entries(object(schema.properties))
    .filter(([name]) => name !== excluded)
    .map(([name, value]) => {
      const property = object(value);
      let type = rustType(property);
      const optional = !required.includes(name);
      const hasDefault = Object.hasOwn(property, "default");
      if (hasDefault && property.default !== 0)
        throw new Error(`Unsupported vendor default ${name}`);
      if (
        (hasDefault && !optional) ||
        (optional && !hasDefault && !type.startsWith("Option<"))
      )
        throw new Error(`Unsupported vendor presence codec: ${name}`);
      if (!optional && type.startsWith("Option<"))
        throw new Error(
          `Required nullable vendor field needs an explicit presence codec: ${name}`,
        );
      return `${hasDefault ? "    #[serde(default)]\n" : ""}    #[serde(rename = ${JSON.stringify(name)})]\n    pub ${identifier(snake(name))}: ${type},`;
    })
    .join("\n");
}
function enumStrings(schema: ObjectValue): string[] | null {
  if (schema.enum !== undefined) return strings(schema.enum);
  if (
    Array.isArray(schema.oneOf) &&
    schema.oneOf.every((entry) => object(entry).type === "string")
  )
    return schema.oneOf.flatMap((entry) => strings(object(entry).enum));
  return null;
}
function renderType(
  name: string,
  schema: ObjectValue,
): { rust: string; ts: string } {
  const values = enumStrings(schema);
  const derive =
    "#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]";
  const rustName = "Codex" + name;
  if (values)
    return {
      rust: `#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]\n#[schemars(rename = ${JSON.stringify(name)})]\npub enum ${rustName} {\n${values.map((value) => `    #[serde(rename = ${JSON.stringify(value)})]\n    ${identifier(pascal(value))},`).join("\n")}\n}\n`,
      ts: `export type ${name} = ${values.map((value) => JSON.stringify(value)).join(" | ")};\n`,
    };
  if (Array.isArray(schema.oneOf)) {
    const taggedDerive = schema.oneOf.every((value) =>
      Object.keys(object(object(value).properties)).every(
        (name) => name === "type",
      ),
    )
      ? derive.replace("Clone, Debug", "Clone, Copy, Debug")
      : derive;
    const variants = schema.oneOf.map((value) => {
      const variant = object(value);
      const properties = object(variant.properties);
      const tag = strings(object(properties.type).enum);
      if (tag.length !== 1 || !tag[0])
        throw new Error("Unsupported vendor discriminant");
      const payload = fields(variant, "type").replaceAll("    pub ", "    ");
      return `    #[serde(rename = ${JSON.stringify(tag[0])})]\n    ${identifier(pascal(tag[0]))}${payload ? ` {\n${payload.replaceAll("\n", "\n    ")}\n    }` : ""},`;
    });
    const ts = schema.oneOf
      .map((value) => {
        const variant = object(value);
        const required = strings(variant.required);
        return `{ ${Object.entries(object(variant.properties))
          .map(
            ([key, property]) =>
              `${key}${required.includes(key) ? "" : "?"}: ${
                key === "type"
                  ? strings(object(property).enum)
                      .map((value) => JSON.stringify(value))
                      .join(" | ")
                  : tsType(property)
              }`,
          )
          .join("; ")} }`;
      })
      .join(" | ");
    return {
      rust: `${taggedDerive}\n#[serde(tag = "type")]\n#[schemars(rename = ${JSON.stringify(name)})]\npub enum ${rustName} {\n${variants.join("\n")}\n}\n`,
      ts: `export type ${name} = ${ts};\n`,
    };
  }
  if (schema.type !== "object")
    throw new Error(`Unsupported vendor shape ${name}`);
  return {
    rust: `${derive}\n#[schemars(rename = ${JSON.stringify(name)})]\npub struct ${rustName} {\n${fields(schema)}\n}\n`,
    ts: "",
  };
}
export function renderCodexContracts(
  directory = root,
  reader: Reader = read,
  rustOnly = false,
): Rendered[] {
  const lifecycleText = reader(
    path.join(directory, source.directory, "lifecycle.schema.json"),
  );
  const usageText = reader(
    path.join(
      directory,
      source.directory,
      "ThreadTokenUsageUpdatedNotification.json",
    ),
  );
  const errorText = reader(
    path.join(directory, source.directory, "CodexErrorInfo.json"),
  );
  for (const text of [lifecycleText, usageText, errorText]) {
    const comment = parse(text).$comment;
    if (typeof comment !== "string" || !comment.startsWith(provenance))
      throw new Error(
        "Codex vendor schema provenance differs from provider-contract-sources.ts; refresh the pinned source",
      );
  }
  const digest = createHash("sha256")
    .update(JSON.stringify(source))
    .update(lifecycleText)
    .update(usageText)
    .update(errorText)
    .digest("hex");
  const header = `// Generated from Codex CLI ${source.version} selected JSON Schema; do not edit.\n// Source: provider-contract-sources.ts; SHA-256: ${digest}\n`;
  const lifecycle = object(parse(lifecycleText).definitions);
  const usage = parse(usageText);
  const threadStatus = object(lifecycle.ThreadStatus);
  if (!Array.isArray(threadStatus.oneOf))
    throw new Error("Missing vendor thread status variants");
  // Recovery consumes only the discriminator. Derive that projection from the
  // same owner; it neither invents active flags nor validates the full status.
  const activity = {
    oneOf: threadStatus.oneOf.map((value) => ({
      type: "object",
      properties: { type: object(object(value).properties).type },
      required: ["type"],
    })),
  };
  const shapes = {
    ...lifecycle,
    ThreadActivity: activity,
    ...object(usage.definitions),
    ThreadTokenUsageUpdatedNotification: usage,
  };
  const output: Rendered[] = [];
  let rust = header + "\n";
  for (const [name, value] of Object.entries(shapes)) {
    const rendered = renderType(identifier(name), object(value));
    rust += rendered.rust + "\n";
    if (!rustOnly && Object.hasOwn(lifecycle, name)) {
      const imports =
        name === "ThreadStatus"
          ? 'import type { ThreadActiveFlag } from "./ThreadActiveFlag.ts";\n\n'
          : "";
      const target = `${bridge}/${name}.ts`;
      output.push({
        path: target,
        text: execFileSync(
          path.join(directory, "node_modules/.bin/oxfmt"),
          [
            `--stdin-filepath=${target}`,
            `--config=${path.join(directory, ".oxfmtrc.json")}`,
          ],
          { input: header + imports + rendered.ts, encoding: "utf8" },
        ),
      });
    }
  }
  const errors = parse(errorText);
  if (!Array.isArray(errors.anyOf))
    throw new Error("Missing vendor error variants");
  const kinds: string[] = [];
  for (const value of errors.anyOf) {
    const variant = object(value);
    if (variant.enum !== undefined) kinds.push(...strings(variant.enum));
    else if (variant.properties !== undefined)
      kinds.push(...Object.keys(object(variant.properties)));
    else if (
      JSON.stringify(variant.type) !== JSON.stringify(["string", "object"])
    )
      throw new Error("Unsupported vendor error extension");
  }
  rust += `#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]\npub enum KnownCodexErrorKind {\n${kinds.map((kind) => `    #[serde(rename = ${JSON.stringify(kind)})]\n    ${identifier(pascal(kind))},`).join("\n")}\n}\n`;
  output.unshift({
    path: rustPath,
    text: execFileSync("rustfmt", ["--edition", "2024", "--emit", "stdout"], {
      input: rust,
      encoding: "utf8",
    }),
  });
  return output;
}
export function checkCodexContracts(
  directory = root,
  reader: Reader = read,
  rustOnly = false,
): void {
  for (const entry of renderCodexContracts(directory, read, rustOnly)) {
    if (rustOnly && entry.path !== rustPath) continue;
    if (reader(path.join(directory, entry.path)) !== entry.text)
      throw new Error(
        `Stale Codex contract: ${entry.path}; run pnpm run contracts:vendor:generate`,
      );
  }
}

async function renderClaudeRateContract(directory: string): Promise<Rendered> {
  const owner: typeof import("../../../providers/apps/claude-bridge/rate-limit-input.ts") =
    await import(
      pathToFileURL(
        path.join(
          directory,
          "workspaces/providers/apps/claude-bridge/rate-limit-input.ts",
        ),
      ).href
    );
  const manifest = parse(
    read(
      path.join(
        directory,
        "workspaces/providers/apps/claude-bridge/package.json",
      ),
    ),
  );
  const version = object(manifest.dependencies)[
    "@anthropic-ai/claude-agent-sdk"
  ];
  const target =
    "workspaces/runtime/packages/runtime-schema/src/schemas/claude-rate-limit.schema.json";
  const text =
    JSON.stringify(
      {
        ...owner.rateLimitJsonSchema(),
        $comment: `Owner: rate-limit-input.ts SDK projection, @anthropic-ai/claude-agent-sdk ${version}.`,
      },
      null,
      2,
    ) + "\n";
  return {
    path: target,
    text: execFileSync(
      path.join(directory, "node_modules/.bin/oxfmt"),
      [
        `--stdin-filepath=${target}`,
        `--config=${path.join(directory, ".oxfmtrc.json")}`,
      ],
      { input: text, encoding: "utf8" },
    ),
  };
}
export async function checkClaudeRateContract(
  directory = root,
  reader = read,
): Promise<void> {
  const entry = await renderClaudeRateContract(directory);
  if (reader(path.join(directory, entry.path)) !== entry.text)
    throw new Error(
      `Stale Claude rate contract: ${entry.path}; run pnpm run contracts:vendor:generate`,
    );
}
export function refreshCodexSchemas(
  directory = root,
  executable = process.env.CODEX_BIN || "codex",
  check = false,
): void {
  const actual = execFileSync(executable, ["--version"], {
    encoding: "utf8",
  }).trim();
  if (actual !== `codex-cli ${source.version}`)
    throw new Error(`Expected codex-cli ${source.version}; received ${actual}`);
  const temporary = mkdtempSync(
    path.join(tmpdir(), "studio-vendor-contracts-"),
  );
  try {
    execFileSync(executable, [
      "app-server",
      "generate-json-schema",
      "--experimental",
      "--out",
      temporary,
    ]);
    const generated = (file: string) => parse(read(path.join(temporary, file)));
    const lifecycle = Object.fromEntries(
      Object.entries(source.lifecycle).map(([name, file]) => [
        name,
        object(generated(file).definitions)[name],
      ]),
    );
    const errors = generated(source.errors);
    const errorLeaf = object(object(errors.definitions).CodexErrorInfo);
    const outputs = [
      {
        path: "lifecycle.schema.json",
        value: {
          $comment: `${provenance} Selected definitions, unchanged.`,
          definitions: lifecycle,
        },
      },
      {
        path: "ThreadTokenUsageUpdatedNotification.json",
        value: { ...generated(source.usage), $comment: provenance },
      },
      {
        path: "CodexErrorInfo.json",
        value: {
          $schema: errors.$schema,
          $comment: `${provenance} definitions/CodexErrorInfo from ${source.errors}. Refresh with pnpm run contracts:vendor:refresh.`,
          title: "CodexErrorInfo",
          ...errorLeaf,
          definitions: {
            NonSteerableTurnKind: object(errors.definitions)
              .NonSteerableTurnKind,
          },
        },
      },
    ];
    for (const output of outputs) {
      const target = path.join(directory, source.directory, output.path);
      const text = execFileSync(
        path.join(directory, "node_modules/.bin/oxfmt"),
        [
          `--stdin-filepath=${target}`,
          `--config=${path.join(directory, ".oxfmtrc.json")}`,
        ],
        {
          input: JSON.stringify(output.value, null, 2) + "\n",
          encoding: "utf8",
        },
      );
      if (check) {
        if (read(target) !== text)
          throw new Error(`Stale vendor source ${output.path}`);
      } else writeFileSync(target, text);
    }
  } finally {
    rmSync(temporary, { recursive: true, force: true });
  }
}
if (
  process.argv[1] &&
  path.resolve(process.argv[1]) === fileURLToPath(import.meta.url)
) {
  if (process.argv.includes("--refresh"))
    refreshCodexSchemas(root, undefined, process.argv.includes("--check"));
  const rustOnly = process.argv.includes("--rust-only");
  if (process.argv.includes("--write"))
    for (const entry of renderCodexContracts(root, read, rustOnly))
      writeFileSync(path.join(root, entry.path), entry.text);
  else checkCodexContracts(root, read, rustOnly);
  if (!rustOnly) {
    if (process.argv.includes("--write")) {
      const entry = await renderClaudeRateContract(root);
      writeFileSync(path.join(root, entry.path), entry.text);
    } else await checkClaudeRateContract(root);
  }
}
