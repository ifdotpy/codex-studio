import { readFile } from "node:fs/promises";
import ts from "typescript";
import { expect, it } from "vitest";

it("the actual App uses its injected preference owner and no browser preference store", async () => {
  const code = await readFile(new URL("./App.tsx", import.meta.url), "utf8");
  const source = ts.createSourceFile(
    "App.tsx",
    code,
    ts.ScriptTarget.Latest,
    true,
    ts.ScriptKind.TSX,
  );
  const imports = source.statements
    .filter(ts.isImportDeclaration)
    .map((item) => item.moduleSpecifier.text);
  expect(imports).not.toContain("./sync/uiPreferenceStore");
  expect(imports).not.toContain("./sync/uiPreferenceBridge");
  expect(imports).not.toContain("./servers/storage");
  const calls = [];
  const globals = [];
  function visit(node) {
    if (ts.isCallExpression(node)) calls.push(node.expression.getText(source));
    if (
      ts.isPropertyAccessExpression(node) &&
      ["window", "globalThis"].includes(node.expression.getText(source)) &&
      node.name.text === "localStorage"
    )
      globals.push(node.getText(source));
    ts.forEachChild(node, visit);
  }
  visit(source);
  expect(globals).toEqual([]);
  expect(calls).toContain("useServerAppPreferences");
  expect(calls).toContain("appPreferences.editPreference");
  expect(calls).toContain("appPreferences.publishWorkspace");
  expect(calls).toContain("preferenceEvents.subscribe");
});
