import { fileURLToPath } from "node:url";
import { readFile } from "node:fs/promises";
import { relative } from "node:path";
import { createServer } from "vite";
import ts from "typescript";
import { expect, it } from "vitest";
import { nativePlatformAliases } from "../../scripts/nativePlatformAliases.mjs";

it("the actual prompt and conversation helper imports exclude browser business owners", async () => {
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
    const queue = [];
    for (const [hook, importer] of [
      ["useSkillAutocomplete", "../components/prompt-composer/PromptInput.tsx"],
      ["useSendingMessageRemoval", "../components/Conversation.tsx"],
      ["useRemovedMessages", "../components/Conversation.tsx"],
    ]) {
      const path = fileURLToPath(new URL(importer, import.meta.url));
      const caller = ts.createSourceFile(
        path,
        await readFile(path, "utf8"),
        ts.ScriptTarget.Latest,
        true,
        ts.ScriptKind.TSX,
      );
      if (path.endsWith("/Conversation.tsx")) {
        const modules = caller.statements
          .filter(ts.isImportDeclaration)
          .map((item) => item.moduleSpecifier.text);
        expect(modules).toContain("./removeSendingMessagesCore");
        expect(modules).not.toContain("./removeSendingMessages");
      }
      const imported = caller.statements.find(
        (item) =>
          ts.isImportDeclaration(item) &&
          item.importClause?.namedBindings &&
          ts.isNamedImports(item.importClause.namedBindings) &&
          item.importClause.namedBindings.elements.some(
            (name) => name.name.text === hook,
          ),
      );
      expect(imported).toBeDefined();
      const target = await server.environments.client.pluginContainer.resolveId(
        imported.moduleSpecifier.text,
        path,
      );
      expect(target).not.toBeNull();
      queue.push("/" + relative(root, target.id));
    }
    const seen = new Set(queue);
    for (const id of queue) {
      const transformed = await server.transformRequest(id);
      expect(transformed).not.toBeNull();
      const code = ts.createSourceFile(
        id,
        transformed.code,
        ts.ScriptTarget.Latest,
        true,
        ts.ScriptKind.JS,
      );
      for (const item of code.statements)
        if (ts.isImportDeclaration(item) || ts.isExportDeclaration(item)) {
          const child = item.moduleSpecifier?.text;
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
    for (const expected of [
      "/src/native/useSkillAutocomplete.ts",
      "/src/native/useSendingMessageRemoval.ts",
      "/src/native/removedMessages.ts",
    ])
      expect(seen).toContain(expected);
    for (const forbidden of [
      "/src/api.ts",
      "/src/servers/storage.ts",
      "/src/hooks.ts",
      "/src/sync/send.ts",
      "/src/components/removeSendingMessages.ts",
      "/src/components/useSkillAutocomplete.ts",
      "/src/components/removedMessages.ts",
    ])
      expect(seen).not.toContain(forbidden);
  } finally {
    await server.close();
  }
});
