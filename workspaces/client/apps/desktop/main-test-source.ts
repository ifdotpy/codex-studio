import { readFileSync } from "node:fs";
import { stripTypeScriptTypes, createRequire } from "node:module";
import * as backend from "./backend.ts";
import * as recovery from "./recovery.ts";
import * as windowState from "./window-state.ts";
import * as rendererRecovery from "./renderer-recovery.ts";
import * as frameOwner from "./frame-owner.ts";
import * as approvedPair from "./approved-pair.ts";
import * as uiHost from "./ui-host.ts";
import * as installMode from "./install-mode.ts";
import * as vaultKeys from "./vault-keys.ts";
import * as speech from "./speech.ts";
import * as serverCredentials from "./server-credentials.ts";

const modules: Record<string, unknown> = {
  "./backend.ts": backend,
  "./recovery.ts": recovery,
  "./window-state.ts": windowState,
  "./renderer-recovery.ts": rendererRecovery,
  "./frame-owner.ts": frameOwner,
  "./approved-pair.ts": approvedPair,
  "./ui-host.ts": uiHost,
  "./install-mode.ts": installMode,
  "./vault-keys.ts": vaultKeys,
  "./speech.ts": speech,
  "./server-credentials.ts": serverCredentials,
};
const require = createRequire(import.meta.url);
export function mainDependency(name: string): unknown {
  if (name in modules) return modules[name];
  if (name.startsWith("./"))
    throw new Error(`Unknown main dependency: ${name}`);
  return require(name);
}
// Run the real ESM body with VM-injected dependencies. Only import bindings and
// import.meta need adapting to the script context; Node owns type removal.
export function mainTestSource() {
  return stripTypeScriptTypes(
    readFileSync(new URL("./main.ts", import.meta.url), "utf8"),
  )
    .replace(
      /import\s+\*\s+as\s+(\w+)\s+from\s+"([^"]+)";/g,
      (_match, name: string, specifier: string) =>
        `const ${name} = require(${JSON.stringify(specifier)});`,
    )
    .replace(
      /import\s*\{([^}]+)\}\s*from\s*"([^"]+)";/g,
      (_match, names: string, specifier: string) =>
        `const {${names.replaceAll(/\bas\b/g, ":")}} = require(${JSON.stringify(specifier)});`,
    )
    .replaceAll("import.meta.dirname", "__dirname")
    .replace(/export\s*\{([^}]+)\};/g, "module.exports = {$1};");
}
