import { fileURLToPath } from "node:url";
import { createServer } from "vite";
import ts from "typescript";
import { expect, it } from "vitest";
import { nativePlatformAliases } from "../../scripts/nativePlatformAliases.mjs";

it("the actual result preview caller and file service resolve native file, document, clipboard and Markdown owners", async () => {
  const server = await createServer({
    root: fileURLToPath(new URL("../../", import.meta.url)),
    configFile: false,
    logLevel: "silent",
    resolve: { alias: nativePlatformAliases },
    optimizeDeps: { noDiscovery: true, include: [] },
    server: { middlewareMode: true, watch: null, hmr: false },
  });
  try {
    const queue = [
      "/src/components/conversation/transcript/ConversationResults.tsx",
      "/src/native/services/files.ts",
    ];
    const modules = new Map();
    const forbidden = [
      "/src/api.ts",
      "/src/servers/ServerServicesContext.tsx",
      "/src/servers/serverServices.ts",
      "/src/servers/transport.ts",
      "/src/clipboard/clipboard.ts",
      "/src/fileActions.ts",
      "/src/components/FilePreview.tsx",
      "/src/components/RichPreview.tsx",
      "/src/components/MarkdownImage.tsx",
      "/src/wasm/client.ts",
    ];
    for (const id of queue) {
      expect(forbidden, id).not.toContain(id);
      const transformed = await server.transformRequest(id);
      expect(transformed, id).not.toBeNull();
      modules.set(id, transformed.code);
      const source = ts.createSourceFile(
        id,
        transformed.code,
        ts.ScriptTarget.Latest,
        true,
        ts.ScriptKind.JS,
      );
      const accept = (path) => {
        if (typeof path !== "string" || !path.startsWith("/src/")) return;
        const clean = path.split("?")[0];
        if (!queue.includes(clean)) queue.push(clean);
      };
      const visit = (node) => {
        if (ts.isImportDeclaration(node) || ts.isExportDeclaration(node))
          accept(node.moduleSpecifier?.text);
        if (
          ts.isCallExpression(node) &&
          node.expression.kind === ts.SyntaxKind.ImportKeyword
        )
          accept(node.arguments[0]?.text);
        ts.forEachChild(node, visit);
      };
      visit(source);
    }
    for (const [id, code] of modules) {
      expect(code, id).not.toMatch(
        /\bnew\s+(?:Blob|File|FileReader|AudioContext|MediaRecorder)\s*\(|\bURL\.(?:createObjectURL|revokeObjectURL)\s*\(|\bnavigator\.(?:clipboard|mediaDevices)\b|\b(?:fetch|XMLHttpRequest|WebSocket|EventSource)\s*\(/,
      );
    }
    for (const expected of [
      "/src/native/FilePreview.tsx",
      "/src/native/RichPreview.tsx",
      "/src/native/preview.ts",
      "/src/native/PreparedMarkdown.tsx",
    ])
      expect([...modules.keys()]).toContain(expected);
  } finally {
    await server.close();
  }
}, 20_000);
