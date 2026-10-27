import assert from "node:assert/strict";
import { execFileSync, spawnSync } from "node:child_process";
import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";
import test from "node:test";
import ts from "typescript";
import { gateSteps, typescriptProjects } from "../facade.ts";

const root = process.cwd();
const strictOptions = [
  "strict",
  "noImplicitReturns",
  "noImplicitOverride",
  "noFallthroughCasesInSwitch",
  "exactOptionalPropertyTypes",
  "noUncheckedIndexedAccess",
] as const satisfies readonly (keyof ts.CompilerOptions)[];

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

test("every TypeScript program preserves the strict compiler owner", () => {
  const owner = compilerOptions("tsconfig.base.json");
  for (const option of strictOptions)
    assert.equal(
      owner[option],
      true,
      `tsconfig.base.json must enable ${option}`,
    );
  const projects = execFileSync(
    "git",
    ["ls-files", "--", "*tsconfig.json", "*tsconfig.*.json"],
    {
      cwd: root,
      encoding: "utf8",
    },
  )
    .trim()
    .split("\n")
    .filter((file) => file !== "tsconfig.base.json");
  assert.ok(projects.length > 0);
  for (const project of projects) {
    const options = compilerOptions(project);
    for (const option of strictOptions)
      assert.equal(
        options[option],
        owner[option],
        `${project} weakens ${option} from tsconfig.base.json`,
      );
  }
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
    assert.equal(lint[0]?.requires, undefined);
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
