import { readdirSync, readFileSync } from "node:fs";
import { join, relative } from "node:path";
import { fileURLToPath } from "node:url";
import { expect, it } from "vitest";

// Each file that reads browser storage directly, with what it keeps. The target
// architecture (docs/decisions/2026-10-28-target-client-server-architecture.md)
// leaves the device only a thin cache, unsent input, and the device key. A new
// entry needs an owner. A removed use must leave this list, so the list always
// matches the source.
const allowed: Record<string, string> = {
  "App.tsx": "workspace id of each server view, read by the server list",
  "components/shell/BackgroundTasks.tsx":
    "request identity of a pending background task control, kept for a lost reply",
  "components/shell/McpInventory.tsx":
    "request identity of a pending MCP action, kept for a lost reply",
  "components/shell/MonitorRestart.tsx":
    "request body of a pending monitor restart, kept for a lost reply",
  "components/shell/workspacePanelWidth.ts":
    "workspace panel width of this device",
  "components/sidebar/SidebarFrame.tsx": "sidebar width of this device",
  "native/importBrowserProfile.ts":
    "one-time import of the old browser profile into the desktop app",
  "native/preferences.ts":
    "replaces window.localStorage with the native preference store",
  "push/signInPush.ts":
    "Web Push subscription id of this device for each server key",
  "servers/MultiServerApp.tsx": "selected server and Studio preferences",
  "servers/ServerManager.tsx":
    "local server alias and the pending pairing attempt",
  "servers/automaticAccessStore.ts":
    "servers removed from the automatic UI list",
  "servers/frameBridge.ts": "Studio preferences sent from a server frame",
  "servers/preferenceServerOrder.ts":
    "workspace id of each server, for the preference order",
  "servers/projectLocations.ts": "paired server list, for project locations",
  "servers/registry.ts":
    "paired server list and the local server name and alias",
  "servers/routes.ts": "routes that a paired server reported, for each server",
  "servers/serverServices.ts": "server-scoped storage for each server view",
  "servers/storage.ts":
    "server-scoped storage that drafts, queues, uploads, and caches use",
  "servers/useUiPreferenceSync.ts":
    "workspace id and local server name, for preference sync",
  "sync/uiPreferenceStore.ts": "UI preferences of this user",
};

const root = fileURLToPath(new URL(".", import.meta.url));
const pattern =
  /(?:globalThis|window)\.(?:localStorage|sessionStorage)\b|(?<![.\w$])(localStorage|sessionStorage)\.(?:getItem|setItem|removeItem|key|clear|length)\b/g;
const binding =
  /(?:const|let|var)\s+(?:localStorage|sessionStorage)\b|\{[^}]*:\s*(?:localStorage|sessionStorage)\s*(?=[,}])[^}]*\}\s*=|import\s[^;]*\b(?:localStorage|sessionStorage)\b[^;]*from|\b(?:localStorage|sessionStorage)\s*[:,)=]|\bas\s+(?:localStorage|sessionStorage)\b/;

function sources(directory: string): string[] {
  return readdirSync(directory, { withFileTypes: true }).flatMap((entry) => {
    const path = join(directory, entry.name);
    if (entry.isDirectory()) return sources(path);
    return /\.(ts|tsx)$/.test(entry.name) &&
      !/\.(test|spec|stories|fixture)\./.test(entry.name)
      ? [path]
      : [];
  });
}

it("lists every web source file that reads browser storage directly", () => {
  const found = sources(root)
    .filter((path) => {
      const text = readFileSync(path, "utf8");
      if (!pattern.test(text)) {
        pattern.lastIndex = 0;
        return false;
      }
      pattern.lastIndex = 0;
      // A file that declares its own storage name (for example through
      // serverLocalStorage as localStorage) uses that binding, not the global.
      const direct = [...text.matchAll(pattern)].filter(
        (match) => match[1] === undefined || !binding.test(text),
      );
      return direct.length > 0;
    })
    .map((path) => relative(root, path))
    .sort();
  expect(found).toEqual(Object.keys(allowed).sort());
  for (const [file, reason] of Object.entries(allowed))
    expect(reason, file).not.toBe("");
});
