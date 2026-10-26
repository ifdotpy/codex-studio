import { readFile } from "node:fs/promises";
import ts from "typescript";
import { expect, it } from "vitest";

it("the real shared shell acquires services through its tested per-root owner", async () => {
  const code = await readFile(
    new URL("./MultiServerShell.tsx", import.meta.url),
    "utf8",
  );
  const source = ts.createSourceFile(
    "MultiServerShell.tsx",
    code,
    ts.ScriptTarget.Latest,
    true,
    ts.ScriptKind.TSX,
  );
  const runtimeImports = source.statements
    .filter(ts.isImportDeclaration)
    .filter((item) => !item.importClause?.isTypeOnly)
    .map((item) => item.moduleSpecifier.text);
  expect(runtimeImports).toContain("./shellServicePool");
  expect(runtimeImports).not.toContain("./serverServices");
  expect(runtimeImports).not.toContain("./registry");
  expect(runtimeImports).not.toContain("./ServerServicesContext");
  const calls = [];
  const visit = (node) => {
    if (ts.isCallExpression(node)) calls.push(node.expression.getText(source));
    ts.forEachChild(node, visit);
  };
  visit(source);
  expect(calls).toContain("runtime.openService");
  expect(calls).toContain("runtime.provideServices");
  expect(code).not.toMatch(/globalThis\.localStorage|\blocalStorage\./);
  const wrapper = await readFile(
    new URL("./MultiServerApp.tsx", import.meta.url),
    "utf8",
  );
  expect(wrapper).toContain('from "./MultiServerShell"');
  expect(wrapper).not.toContain('from "../App"');
});
