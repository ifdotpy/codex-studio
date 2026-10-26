import { readFile } from "node:fs/promises";
import { fileURLToPath } from "node:url";
import { createServer } from "vite";
import ts from "typescript";
import { expect, it } from "vitest";
import { nativePlatformAliases } from "../../scripts/nativePlatformAliases.mjs";

it("the real project-save helper resolves no browser default owner in the native config", async () => {
  const root = fileURLToPath(new URL("../../", import.meta.url));
  const server = await createServer({
    root,
    configFile: false,
    logLevel: "silent",
    resolve: { alias: nativePlatformAliases },
    optimizeDeps: { noDiscovery: true, include: [] },
    server: { middlewareMode: true, watch: null, hmr: false },
  });
  try {
    const queue = ["/src/components/useProjectSave.ts"];
    const seen = new Set(queue);
    for (const id of queue) {
      const transformed = await server.transformRequest(id);
      expect(transformed).not.toBeNull();
      const source = ts.createSourceFile(
        id,
        transformed.code,
        ts.ScriptTarget.Latest,
        true,
        ts.ScriptKind.JS,
      );
      for (const statement of source.statements)
        if (
          ts.isImportDeclaration(statement) ||
          ts.isExportDeclaration(statement)
        ) {
          const child = statement.moduleSpecifier?.text;
          if (
            typeof child === "string" &&
            child.startsWith("/src/") &&
            !seen.has(child)
          ) {
            seen.add(child);
            queue.push(child);
          }
        }
    }
    expect(seen).toContain("/src/native/sidebarServices.tsx");
    expect(seen).toContain("/src/components/sidebar/serviceContext.tsx");
    for (const forbidden of [
      "/src/api.ts",
      "/src/servers/storage.ts",
      "/src/sync/uiPreferenceStore.ts",
      "/src/sync/subscriptions/events.ts",
    ])
      expect(seen).not.toContain(forbidden);
  } finally {
    await server.close();
  }
});

it("the shared Sidebar keeps its real UI but requires explicit service and display owners", async () => {
  const code = await readFile(
    new URL("../components/SidebarView.tsx", import.meta.url),
    "utf8",
  );
  expect(code).not.toMatch(
    /localSidebarBackend|localSidebarServices|readServerAliases|useServerAliases/,
  );
  expect(code).toContain("defaultBackendOwnerId");
  expect(code).toContain("SidebarBackendProvider");
  expect(code).toContain('aria-label="Conversations"');
});
