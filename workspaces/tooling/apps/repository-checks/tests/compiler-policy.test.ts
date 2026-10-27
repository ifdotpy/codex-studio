import assert from "node:assert/strict";
import { execFileSync, spawnSync } from "node:child_process";
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";
import test from "node:test";
import ts from "typescript";
import { gateSteps, typescriptProjects } from "../facade.ts";

const root = process.cwd();
const strictOptions = [
  "strict",
  "noImplicitAny",
  "noImplicitThis",
  "strictNullChecks",
  "strictFunctionTypes",
  "strictBindCallApply",
  "strictPropertyInitialization",
  "strictBuiltinIteratorReturn",
  "useUnknownInCatchVariables",
  "alwaysStrict",
  "noImplicitReturns",
  "noImplicitOverride",
  "noFallthroughCasesInSwitch",
  "exactOptionalPropertyTypes",
  "noUncheckedIndexedAccess",
] as const satisfies readonly (keyof ts.CompilerOptions)[];

const strictSubflags = new Set<keyof ts.CompilerOptions>(
  strictOptions.slice(1, 10),
);
function effectiveOption(
  options: ts.CompilerOptions,
  option: (typeof strictOptions)[number],
) {
  return (
    options[option] ?? (strictSubflags.has(option) ? options.strict : undefined)
  );
}

function compilerOptions(file: string): ts.CompilerOptions {
  const parsed = ts.getParsedCommandLineOfConfigFile(
    path.resolve(root, file),
    {},
    {
      ...ts.sys,
      onUnRecoverableConfigFileDiagnostic(diagnostic) {
        assert.fail(
          ts.flattenDiagnosticMessageText(diagnostic.messageText, " "),
        );
      },
    },
  );
  assert.ok(parsed, `${file}: compiler configuration must parse`);
  assert.deepEqual(parsed.errors, [], `${file}: compiler configuration errors`);
  return parsed.options;
}

function assertStrictParity(
  owner: ts.CompilerOptions,
  options: ts.CompilerOptions,
  project: string,
) {
  for (const option of strictOptions)
    assert.equal(
      effectiveOption(options, option),
      effectiveOption(owner, option),
      `${project} weakens ${option} from tsconfig.base.json`,
    );
}

test("every TypeScript program preserves the strict compiler owner", () => {
  const owner = compilerOptions("tsconfig.base.json");
  for (const option of strictOptions)
    assert.equal(
      effectiveOption(owner, option),
      true,
      `tsconfig.base.json must enable ${option}`,
    );
  const projects = execFileSync(
    "git",
    ["ls-files", "--", "*tsconfig.json", "*tsconfig.*.json"],
    { cwd: root, encoding: "utf8" },
  )
    .trim()
    .split("\n")
    .filter((file) => file !== "tsconfig.base.json");
  assert.ok(projects.length > 0);
  for (const project of projects)
    assertStrictParity(owner, compilerOptions(project), project);
});

test("strict subflag override cannot weaken a project", () => {
  const owner = compilerOptions("tsconfig.base.json");
  assert.throws(
    () =>
      assertStrictParity(owner, { ...owner, noImplicitAny: false }, "fixture"),
    /fixture weakens noImplicitAny/,
  );
});

test("local and hosted gate compositions enforce root lint", () => {
  for (const gate of [
    "test-fast",
    "test-comprehensive",
    "test-fast-rest-non-rust",
  ]) {
    const lint = gateSteps(root, gate).filter(
      (step) => step.id === "root-lint",
    );
    assert.equal(lint.length, 1, `${gate} must run root lint exactly once`);
    assert.equal(lint[0]?.options?.script, "lint:all");
    assert.equal(lint[0]?.requires, "wasm");
  }
});

test("contract gates retain generated parity and every TypeScript program", () => {
  for (const gate of ["test-fast", "test-comprehensive"]) {
    const steps = gateSteps(root, gate);
    const contracts = steps.filter((step) => step.id === "contracts");
    assert.equal(contracts.length, 1, `${gate}: generated contract parity`);
    assert.deepEqual(contracts[0]?.commands?.[0]?.[1], ["--check"]);
    assert.equal(contracts[0]?.requires, "workspace");
    const types = steps.filter((step) => step.id === "TypeScript projects");
    assert.equal(types.length, 1, `${gate}: TypeScript compilation`);
    assert.deepEqual(
      types[0]?.commands?.map(([command, args]) => {
        assert.equal(command, "pnpm");
        assert.deepEqual(args.slice(0, 3), ["exec", "tsc", "--project"]);
        return args[3];
      }),
      typescriptProjects(root),
    );
  }
});

test("root Oxlint rejects type erasure and unchecked compiler suppressions", () => {
  const directory = mkdtempSync(path.join(tmpdir(), "studio-type-policy-"));
  try {
    const lint = (file: string, source: string) => {
      const target = path.join(directory, file);
      writeFileSync(target, source);
      const result = spawnSync(
        process.execPath,
        [
          path.join(root, "node_modules/oxlint/bin/oxlint"),
          "--config",
          path.join(root, ".oxlintrc.json"),
          "--deny-warnings",
          target,
        ],
        { cwd: root, encoding: "utf8", timeout: 10_000 },
      );
      assert.ifError(result.error);
      return result;
    };
    for (const file of ["boundary.ts", "boundary.test.ts"]) {
      const valid = lint(file, "export const receipt: unknown = {};\n");
      assert.equal(valid.status, 0, valid.stdout + valid.stderr);
      for (const [source, diagnostic] of [
        ["export const receipt: any = {};\n", /no-explicit-any/],
        [
          "// @ts-ignore: pretend the contract matches\nexport const receipt: string = 1;\n",
          /ban-ts-comment/,
        ],
        [
          "// @ts-nocheck\nexport const receipt: string = 1;\n",
          /ban-ts-comment/,
        ],
      ] as const) {
        const rejected = lint(file, source);
        assert.notEqual(rejected.status, 0, `${file}: type erasure must fail`);
        assert.match(rejected.stdout + rejected.stderr, diagnostic);
      }
    }
    const source =
      "// @ts-expect-error: deliberately invalid contract\nexport const receipt: string = 1;\n";
    const production = lint("boundary.ts", source);
    assert.notEqual(
      production.status,
      0,
      "production must not suppress contract mismatches",
    );
    assert.match(production.stdout + production.stderr, /ban-ts-comment/);
    const witness = lint("boundary.test.ts", source);
    assert.equal(witness.status, 0, witness.stdout + witness.stderr);
  } finally {
    rmSync(directory, { recursive: true, force: true });
  }
});

test("generated enum owners reject raw literals in typed positions", async () => {
  const { enumLiteralFindings } = await import("../enum-literals.ts");
  const directory = mkdtempSync(path.join(tmpdir(), "studio-enum-policy-"));
  try {
    const owner = path.join(directory, "generated.ts");
    const target = path.join(directory, "consumer.ts");
    writeFileSync(
      owner,
      'export const Reason = { slot: "slot", budget: "budget" } as const;\nexport type Reason = typeof Reason[keyof typeof Reason];\nexport interface components { schemas: { Reason: "slot" | "budget"; Dto: { reason: components["schemas"]["Reason"] } } }\nexport type Dto = components["schemas"]["Dto"];\n',
    );
    const check = (body: string) => {
      writeFileSync(
        target,
        `import { Reason, type Dto } from "./generated";\n${body}`,
      );
      const program = ts.createProgram([owner, target], {
        strict: true,
        noEmit: true,
      });
      return enumLiteralFindings(program, [target], [owner]);
    };
    const prefix =
      "declare const reason: Reason; declare function use(value: Reason): void;\n";
    for (const body of [
      'if (reason === "slot") {}',
      'switch (reason) { case "slot": break; }',
      'use("slot");',
      'const object: { reason: Reason } = { reason: "slot" };',
      'declare const dto: Dto; switch (dto.reason) { case "slot": break; }',
      'const object: Dto = { reason: "slot" };',
      'let chosen: Reason = Reason.slot; if (chosen === "slot") {}',
    ]) {
      const findings = check(prefix + body);
      assert.equal(findings.length, 1, body);
      assert.match(findings[0] ?? "", /import and use Reason\["slot"\]/);
    }
    assert.deepEqual(
      check(
        prefix +
          'use(Reason.slot); const text: string = "slot"; const data: unknown = "slot"; type Text = "slot" | "budget"; const textChoice: Text = "slot";',
      ),
      [],
    );
  } finally {
    rmSync(directory, { recursive: true, force: true });
  }
});

test("contract gates execute schema compile-fail doctests", () => {
  for (const gate of ["test-fast", "test-comprehensive"]) {
    const steps = gateSteps(root, gate).filter(
      (step) => step.id === "schema-doctests",
    );
    assert.equal(steps.length, 1, `${gate}: schema doctest owner`);
    assert.deepEqual(steps[0]?.commands, [
      ["cargo", ["test", "--locked", "-p", "studio-runtime-schema", "--doc"]],
    ]);
  }
});

test("enum diagnostics distinguish equal generated domains and computed keys", async () => {
  const { enumLiteralFindings } = await import("../enum-literals.ts");
  const directory = mkdtempSync(path.join(tmpdir(), "studio-enum-owners-"));
  try {
    const owner = path.join(directory, "generated.ts");
    const target = path.join(directory, "consumer.ts");
    writeFileSync(
      owner,
      `export interface components { schemas: { ExecutionAction: "review" | "compact"; NativeAction: "review" | "compact"; Special: "__proto__" | "other" } }
export const ExecutionAction = { review: "review", compact: "compact" } as const;
export const NativeAction = { review: "review", compact: "compact" } as const;
export const Special = { ["__proto__"]: "__proto__", other: "other" } as const;
export type ExecutionAction = components["schemas"]["ExecutionAction"];
export type NativeAction = components["schemas"]["NativeAction"];
export type Special = components["schemas"]["Special"];
`,
    );
    writeFileSync(
      target,
      `import type {ExecutionAction, NativeAction, Special} from "./generated";
declare const execution: ExecutionAction; declare const native: NativeAction; declare function useSpecial(value: Special): void;
if (execution === "review") {} if (native === "review") {} useSpecial("__proto__");
`,
    );
    const program = ts.createProgram([owner, target], {
      strict: true,
      noEmit: true,
    });
    const findings = enumLiteralFindings(program, [target], [owner]);
    assert.equal(findings.length, 3);
    assert.match(findings[0] ?? "", /use ExecutionAction\["review"\]/);
    assert.doesNotMatch(findings[0] ?? "", /NativeAction/);
    assert.match(findings[1] ?? "", /use NativeAction\["review"\]/);
    assert.doesNotMatch(findings[1] ?? "", /ExecutionAction/);
    assert.match(findings[2] ?? "", /use Special\["__proto__"\]/);
  } finally {
    rmSync(directory, { recursive: true, force: true });
  }
});

test("whole-repository lint runs the owned enum checker", () => {
  const packageJson = JSON.parse(
    readFileSync(path.join(root, "package.json"), "utf8"),
  );
  assert.match(packageJson.scripts["lint:all"], /check-code\.ts lint-all/);
});

test("generated component aliases and computed keys reach the enum checker", async () => {
  const { enumLiteralFindings } = await import("../enum-literals.ts");
  const document = {
    openapi: "3.1.0",
    info: { title: "enum witness", version: "1" },
    components: {
      schemas: {
        "special-name": { type: "string", enum: ["__proto__", "other"] },
        Dto: {
          type: "object",
          properties: { reason: { $ref: "#/components/schemas/special-name" } },
          required: ["reason"],
        },
      },
    },
  };
  const generated = spawnSync(
    process.execPath,
    [
      process.env.STUDIO_TEST_ENUM_GENERATOR ??
        path.join(
          root,
          "workspaces/tooling/apps/repository-checks/generate-api-types.ts",
        ),
      "--rust-owned",
    ],
    { cwd: root, input: JSON.stringify(document), encoding: "utf8" },
  );
  assert.equal(generated.status, 0, generated.stderr);
  const directory = mkdtempSync(path.join(tmpdir(), "studio-generated-owner-"));
  try {
    const owner = path.join(directory, "generated.ts");
    const target = path.join(directory, "consumer.ts");
    writeFileSync(owner, generated.stdout);
    writeFileSync(
      target,
      'import type { components } from "./generated"; declare function use(value: components["schemas"]["special-name"]): void; use("__proto__"); declare const dto: components["schemas"]["Dto"]; if (dto.reason === "__proto__") {}',
    );
    const program = ts.createProgram([owner, target], {
      strict: true,
      noEmit: true,
    });
    const findings = enumLiteralFindings(program, [target], [owner]);
    assert.equal(findings.length, 2, findings.join("\n"));
    for (const finding of findings)
      assert.match(finding, /special_name\["__proto__"\]/);
  } finally {
    rmSync(directory, { recursive: true, force: true });
  }
});

test("contract gates select runtime-services typed owner doctests", () => {
  const fast = gateSteps(root, "test-fast").filter((step) =>
    step.id?.startsWith("services-doctests"),
  );
  assert.deepEqual(
    fast.map((step) => step.commands),
    [
      [
        [
          "cargo",
          [
            "test",
            "--locked",
            "-p",
            "studio-runtime-services",
            "--doc",
            "ControlInvocation",
          ],
        ],
      ],
      [
        [
          "cargo",
          [
            "test",
            "--locked",
            "-p",
            "studio-runtime-services",
            "--doc",
            "provider_receipt",
          ],
        ],
      ],
    ],
  );
  const comprehensive = gateSteps(root, "test-comprehensive").filter((step) =>
    step.id?.startsWith("services-doctests"),
  );
  assert.deepEqual(
    comprehensive.map((step) => step.commands),
    [
      [
        [
          "cargo",
          ["test", "--locked", "-p", "studio-runtime-services", "--doc"],
        ],
      ],
    ],
  );
});

test("generated types reject duplicate names and handwritten union copies", async () => {
  const { generatedTypeDiagnostics } = await import("../enum-literals.ts");
  const directory = mkdtempSync(path.join(tmpdir(), "studio-generated-types-"));
  try {
    const owner = path.join(directory, "generated.ts");
    const target = path.join(directory, "consumer.ts");
    writeFileSync(
      owner,
      'export type State = "open" | "closed"; export const KINDS = ["one", "two"] as const; export type Kind = typeof KINDS[number]; export interface components { schemas: { Message: { id: string }; Unrelated: { wire: string }; Derived: { id: string }; Wrapped: { id: string }; Reexport: { id: string } } } export type Reexport = components["schemas"]["Reexport"];',
    );
    writeFileSync(
      target,
      'import type { components } from "./generated"; export type { Reexport } from "./generated"; type GetResult<T extends keyof components["schemas"]> = components["schemas"][T]; type Message = { id: string }; type Other = "closed" | "open"; type Copy = "two" | "one"; type Local = "other" | "local"; type Unrelated = { local: number }; type Derived = components["schemas"]["Derived"]; type Wrapped = Pick<GetResult<"Wrapped">, "id">;',
    );
    const program = ts.createProgram([owner, target], {
      strict: true,
      noEmit: true,
    });
    const findings = generatedTypeDiagnostics(program, [target], [owner]);
    assert.equal(findings.length, 3);
    assert.match(findings[0]?.message ?? "", /Message/);
    assert.match(findings[1]?.message ?? "", /State/);
  } finally {
    rmSync(directory, { recursive: true, force: true });
  }
});

test("union switches require a terminating or never-checked default", async () => {
  const { switchDefaultDiagnostics } = await import("../enum-literals.ts");
  const directory = mkdtempSync(path.join(tmpdir(), "studio-switch-policy-"));
  try {
    const target = path.join(directory, "consumer.ts");
    writeFileSync(
      target,
      `
      type State = "open" | "closed";
      declare const item: { state: State; count: number; mode: 1 | 2 };
      declare function assertNever(value: never): never;
      declare const unrelated: never;
      switch (item.state) { case "open": break; default: console.log(item.state); }
      switch (item.state) { case "open": break; default: returnValue(); }
      switch (item.state) { case "open": break; default: throw new Error(); }
      function done() { switch (item.state) { default: return; } }
      function checked(value: { state: State }) { switch (value.state) { case "open": break; case "closed": break; default: assertNever(value.state); } }
      function binding(value: { state: State }) { switch (value.state) { case "open": break; case "closed": break; default: { const exhaustive: never = value.state; } } }
      function objectBinding(value: { state: "open"; a: number } | { state: "closed"; b: number }) { while (true) { switch (value.state) { case "open": break; case "closed": break; default: { const exhaustive: never = value; void exhaustive; continue; } } break; } }
      function wrongObject(value: { state: "open"; a: number } | { state: "closed"; b: number }) { switch (value.state) { case "open": break; default: { const exhaustive: never = value; void exhaustive; } } }
      switch (item.count) { default: console.log(item.count); }
      switch (item.mode) { default: console.log(item.mode); }
      function wrong(value: { state: State }) { switch (value.state) { default: assertNever(unrelated); } }
      function returnValue() { return 1; }
    `,
    );
    const program = ts.createProgram([target], { strict: true, noEmit: true });
    const findings = switchDefaultDiagnostics(program, [target]);
    assert.equal(
      findings.length,
      5,
      findings.map((item) => item.text).join("\n"),
    );
  } finally {
    rmSync(directory, { recursive: true, force: true });
  }
});
