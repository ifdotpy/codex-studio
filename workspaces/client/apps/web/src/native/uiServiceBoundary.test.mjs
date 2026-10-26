import { fileURLToPath } from "node:url";
import { createServer } from "vite";
import ts from "typescript";
import { afterAll, beforeAll, expect, it } from "vitest";
import { nativePlatformAliases } from "../../scripts/nativePlatformAliases.mjs";

const callers = [
  "components/CodexSignIn.tsx",
  "components/useFormDraft.ts",
  "components/useRecoveredLimit.ts",
  "components/useTurnErrors.ts",
  "components/useMessageReceipts.ts",
  "components/useConversationScroll.ts",
  "components/agents/UnifiedAgentSettings.tsx",
  "components/agents/WorkerCard.tsx",
  "components/conversation/transcript/HistoryWindow.tsx",
  "components/conversation/transcript/TurnHistory.tsx",
  "components/shell/messages/TeamChats.tsx",
  "servers/ProjectGroupsSidebar.tsx",
  "sync/useSyncedVisualState.ts",
];

let server;
beforeAll(async () => {
  server = await createServer({
    root: fileURLToPath(new URL("../../", import.meta.url)),
    configFile: false,
    logLevel: "silent",
    resolve: { alias: nativePlatformAliases },
    optimizeDeps: { noDiscovery: true, include: [] },
    server: { middlewareMode: true, watch: null, hmr: false },
  });
});
afterAll(async () => {
  await server?.close();
});

it.each(callers)(
  "%s resolves captured service ports without browser owner imports",
  async (caller) => {
    const forbidden = new Set([
      "/src/api.ts",
      "/src/sync/send.ts",
      "/src/sync/localDraft.ts",
      "/src/sync/resourceEvents.ts",
      "/src/servers/ServerServicesContext.tsx",
    ]);
    const id = `/src/${caller}`;
    const transformed = await server.transformRequest(id);
    expect(transformed, id).not.toBeNull();
    const source = ts.createSourceFile(
      id,
      transformed.code,
      ts.ScriptTarget.Latest,
      true,
      ts.ScriptKind.JS,
    );
    const imports = source.statements.flatMap((node) =>
      ts.isImportDeclaration(node) && ts.isStringLiteral(node.moduleSpecifier)
        ? [node.moduleSpecifier.text.split("?")[0]]
        : [],
    );
    expect(imports, id).toContain("/src/native/servicesContext.tsx");
    expect(
      imports.filter((path) => forbidden.has(path)),
      id,
    ).toEqual([]);
  },
);
