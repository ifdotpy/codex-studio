import { readFileSync } from "node:fs";
import { expect, test } from "vitest";
import ts from "typescript";
import { inspectCapabilities } from "../scripts/nativeImportGraph.mjs";

test("App delegates UI scheduling to its explicit focus adapter", () => {
  const file = new URL("./App.tsx", import.meta.url);
  const compiled = ts.transpileModule(readFileSync(file, "utf8"), {
    fileName: file.pathname,
    reportDiagnostics: true,
    compilerOptions: {
      module: ts.ModuleKind.ESNext,
      target: ts.ScriptTarget.ESNext,
      jsx: ts.JsxEmit.ReactJSX,
    },
  });
  expect(
    compiled.diagnostics.filter(
      (item) => item.category === ts.DiagnosticCategory.Error,
    ),
  ).toEqual([]);
  expect(inspectCapabilities(compiled.outputText, file.pathname, {})).toEqual(
    [],
  );
});
