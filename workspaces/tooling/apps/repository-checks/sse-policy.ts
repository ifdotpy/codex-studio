import { readdirSync, readFileSync } from "node:fs";
import path from "node:path";
import ts from "typescript";

export type Site = { file: string; callee: string };

// Owners of explicit user actions, bounded resync and the stream transport.
const requestOwners = new Set([
  "api.ts",
  "commands.ts",
  "sync/resync.ts",
  "servers/eventSource.ts",
]);
// The sole owner of browser persistence.
const storageOwner = "browserPersistence.ts";

export function violations(file: string, source: string): Site[] {
  const tree = ts.createSourceFile(
    file,
    source,
    ts.ScriptTarget.Latest,
    true,
    file.endsWith(".tsx") ? ts.ScriptKind.TSX : ts.ScriptKind.TS,
  );
  const result: Site[] = [];
  const add = (callee: string) => result.push({ file, callee });
  const visit = (node: ts.Node) => {
    if (ts.isCallExpression(node) && !requestOwners.has(file)) {
      const callee = node.expression;
      const name = ts.isIdentifier(callee)
        ? callee.text
        : ts.isPropertyAccessExpression(callee)
          ? callee.name.text
          : ts.isElementAccessExpression(callee) &&
              ts.isStringLiteral(callee.argumentExpression)
            ? callee.argumentExpression.text
            : "";
      if (
        [
          "fetch",
          "syncGet",
          "syncPost",
          "apiGet",
          "apiPost",
          "GET",
          "POST",
        ].includes(name)
      )
        add(callee.getText(tree));
    }
    if (file !== storageOwner && ts.isPropertyAccessExpression(node)) {
      const name = node.name.text;
      if (name === "localStorage" || name === "indexedDB")
        add(node.getText(tree));
      else if (
        ts.isIdentifier(node.expression) &&
        ["localStorage", "indexedDB"].includes(node.expression.text)
      )
        add(node.getText(tree));
    }
    if (
      file !== storageOwner &&
      ts.isElementAccessExpression(node) &&
      ts.isStringLiteral(node.argumentExpression) &&
      ["localStorage", "indexedDB"].includes(node.argumentExpression.text)
    )
      add(node.getText(tree));
    if (
      file !== storageOwner &&
      ts.isIdentifier(node) &&
      ["localStorage", "indexedDB"].includes(node.text) &&
      !ts.isPropertyAccessExpression(node.parent) &&
      !(ts.isVariableDeclaration(node.parent) && node.parent.name === node) &&
      !(ts.isParameter(node.parent) && node.parent.name === node) &&
      !ts.isImportSpecifier(node.parent) &&
      !ts.isBindingElement(node.parent) &&
      !ts.isPropertySignature(node.parent) &&
      !(ts.isPropertyAssignment(node.parent) && node.parent.name === node)
    )
      add(node.text);
    ts.forEachChild(node, visit);
  };
  visit(tree);
  return result;
}

export function checkBaseline(actual: Site[], baseline: Site[]): string[] {
  const counts = (sites: Site[]) => {
    const result = new Map<string, number>();
    for (const site of sites) {
      const key = `${site.file}: ${site.callee}`;
      result.set(key, (result.get(key) ?? 0) + 1);
    }
    return result;
  };
  const current = counts(actual),
    previous = counts(baseline);
  return [...new Set([...current.keys(), ...previous.keys()])]
    .sort()
    .flatMap((key) => {
      const difference = (current.get(key) ?? 0) - (previous.get(key) ?? 0);
      return difference
        ? [
            `${difference > 0 ? "new" : "stale"} ${key} (${Math.abs(difference)})`,
          ]
        : [];
    });
}

export function scan(directory: string): Site[] {
  const walk = (folder: string): string[] =>
    readdirSync(path.join(directory, folder), { withFileTypes: true }).flatMap(
      (entry) => {
        const name = path.posix.join(folder, entry.name);
        if (name === "wasm/generated") return [];
        return entry.isDirectory()
          ? walk(name)
          : /\.[cm]?[jt]sx?$/.test(name)
            ? [name]
            : [];
      },
    );
  return walk("")
    .sort()
    .flatMap((file) =>
      violations(file, readFileSync(path.join(directory, file), "utf8")),
    );
}
