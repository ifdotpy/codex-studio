import path from "node:path";
import { readFileSync } from "node:fs";
import ts from "typescript";

// Runtime entrypoints loaded unchanged, without a TypeScript loader.
export const JAVASCRIPT_EXCEPTIONS = [
  {
    file: "workspaces/client/apps/desktop/preload.cjs",
    purpose: "Sandboxed Electron preload has no TypeScript loader.",
  },
  {
    file: "workspaces/providers/apps/claude-bridge/bridge.mjs",
    purpose:
      "Stable VM launcher checks Node TypeScript support before loading the bridge.",
  },
  {
    file: "workspaces/client/apps/native/src-tauri/src/native-check.js",
    purpose: "Rust include_str! embeds these WebView bytes unchanged.",
  },
  {
    file: "workspaces/client/apps/web/public/studio-startup.js",
    purpose: "Browser startup script is served unchanged.",
  },
  {
    file: "workspaces/client/apps/web/public/studio-sw.js",
    purpose: "Browser service worker is served unchanged.",
  },
  {
    file: "workspaces/client/apps/web/stryker.config.mjs",
    purpose: "Installed Stryker loads JavaScript or JSON configuration only.",
  },
];

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
    if (!JAVASCRIPT_EXCEPTIONS.some((entry) => entry.file === file)) {
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
