import path from "node:path";
import { readFileSync } from "node:fs";
import ts from "typescript";

// Runtime entrypoints loaded unchanged, without a TypeScript loader.
export const JAVASCRIPT_EXCEPTIONS_FILE =
  "workspaces/tooling/apps/repository-checks/javascript-exceptions.json";
export interface JavaScriptException {
  file: string;
  purpose: string;
}
// The repository owns this document; the hook passes its staged version explicitly.
export function readJavaScriptExceptions(): JavaScriptException[] {
  return JSON.parse(
    readFileSync(
      new URL("./javascript-exceptions.json", import.meta.url),
      "utf8",
    ),
  );
}

export function isJavaScript(file: string) {
  return /\.(?:[cm]?js|jsx)$/.test(file);
}

export function isTypeScriptProject(file: string) {
  return (
    /^tsconfig.*\.json$/.test(path.basename(file)) &&
    file !== "tsconfig.base.json"
  );
}

// Generated JavaScript has no blanket exemption: tracked outputs follow the same list.
export function javascriptFindings(
  root: string,
  tracked: string[],
  selected = tracked,
  exceptions = readJavaScriptExceptions(),
): string[] {
  const javascript = selected.filter(isJavaScript);
  if (!javascript.length) return [];
  const included = new Set<string>();
  const findings: string[] = [];
  for (const project of tracked.filter(isTypeScriptProject)) {
    const parsed = ts.getParsedCommandLineOfConfigFile(
      path.resolve(root, project),
      {},
      {
        ...ts.sys,
        onUnRecoverableConfigFileDiagnostic(diagnostic) {
          findings.push(
            `${project}: ${ts.flattenDiagnosticMessageText(diagnostic.messageText, " ")}`,
          );
        },
      },
    );
    for (const file of parsed?.options.allowJs ? parsed.fileNames : [])
      included.add(path.resolve(file));
  }
  for (const file of javascript) {
    if (!exceptions.some((entry) => entry.file === file)) {
      findings.push(
        `${file}: tracked JavaScript is not a documented runtime exception`,
      );
      continue;
    }
    if (
      !readFileSync(path.join(root, file), "utf8").match(
        /^\/\/ @ts-check(?:\r?\n|$)/,
      )
    )
      findings.push(
        `${file}: JavaScript exception must start with // @ts-check`,
      );
    if (!included.has(path.resolve(root, file)))
      findings.push(
        `${file}: JavaScript exception is not included by a tracked tsconfig project`,
      );
  }
  return findings;
}
