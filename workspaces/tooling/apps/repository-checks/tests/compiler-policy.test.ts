import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import path from "node:path";
import test from "node:test";
import ts from "typescript";
import { gateSteps } from "../facade.ts";

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
