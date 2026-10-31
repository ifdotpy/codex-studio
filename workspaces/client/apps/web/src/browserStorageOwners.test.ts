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
  "browserPersistence.ts": "allowed browser persistence and legacy cleanup",
  "servers/storage.ts":
    "server-scoped storage that drafts, queues, uploads, and caches use",
};

const root = fileURLToPath(new URL(".", import.meta.url));
const pattern =
  /(?:globalThis|window)\.(?:localStorage|sessionStorage)\b|(?<![.\w$])(localStorage|sessionStorage)\.(?:getItem|setItem|removeItem|key|clear|length)\b/g;
const binding =
  /(?:const|let|var)\s+(?:localStorage|sessionStorage)\b|\{[^}]*:\s*(?:localStorage|sessionStorage)\s*(?=[,}])[^}]*\}\s*=|import\s[^;]*\b(?:localStorage|sessionStorage)\b[^;]*from|\b(?:localStorage|sessionStorage)\s*(?:[,)=]|:\s*Storage\b)|\bas\s+(?:localStorage|sessionStorage)\b/;

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
