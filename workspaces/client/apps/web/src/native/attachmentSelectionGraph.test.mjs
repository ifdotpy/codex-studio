import { fileURLToPath } from "node:url";
import { createServer } from "vite";
import ts from "typescript";
import { expect, it } from "vitest";
import { nativePlatformAliases } from "../../scripts/nativePlatformAliases.mjs";

it("the real native attachment callers contain no browser byte selection", async () => {
  const server = await createServer({
    root: fileURLToPath(new URL("../../", import.meta.url)),
    configFile: false,
    logLevel: "silent",
    resolve: { alias: nativePlatformAliases },
    optimizeDeps: { noDiscovery: true, include: [] },
    server: {
      middlewareMode: true,
      watch: null,
      hmr: false,
      preTransformRequests: false,
    },
  });
  try {
    const violations = [];
    for (const id of [
      "/src/components/Conversation.tsx",
      "/src/components/ComposerAttachments.tsx",
      "/src/components/prompt-composer/PromptInput.tsx",
    ]) {
      const transformed = await server.transformRequest(id);
      expect(transformed).not.toBeNull();
      const source = ts.createSourceFile(
        id,
        transformed.code,
        ts.ScriptTarget.Latest,
        true,
        ts.ScriptKind.JS,
      );
      const inspect = (node) => {
        if (
          ts.isNewExpression(node) &&
          node.expression.getText(source) === "File"
        )
          violations.push(`${id}:new File`);
        if (
          ts.isPropertyAccessExpression(node) &&
          node.name.text === "files" &&
          /\.(?:dataTransfer|clipboardData|currentTarget)$/.test(
            node.expression.getText(source),
          )
        )
          violations.push(`${id}:${node.getText(source)}`);
        ts.forEachChild(node, inspect);
      };
      inspect(source);
    }
    expect(violations).toEqual([]);
  } finally {
    await server.close();
  }
});
