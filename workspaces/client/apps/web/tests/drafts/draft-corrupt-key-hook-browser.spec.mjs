import assert from "node:assert/strict";
import { fileURLToPath } from "node:url";
import { test } from "../playwright.mjs";

test("malformed current-draft key does not crash hook initialization", async ({
  browser,
}) => {
  const { createServer } = await import(
    new URL(
      "../../../web/node_modules/vite/dist/node/index.js",
      import.meta.url,
    )
  );
  const server = await createServer({
    configFile: false,
    root: fileURLToPath(new URL("../../../web", import.meta.url)),
    server: { host: "127.0.0.1", port: 0 },
  });
  server.middlewares.use("/draft-corrupt-check", (_request, response) => {
    response.setHeader("Content-Type", "text/html");
    response.end("<!doctype html><title>Corrupt draft key</title>");
  });
  await server.listen();
  const context = await browser.newContext();
  try {
    const page = await context.newPage();
    const errors = [];
    page.on("pageerror", (error) => errors.push(error.message));
    const workspaceId = "a".repeat(32);
    await context.route("**/api/sync/identity", (route) =>
      route.fulfill({ json: { workspaceId } }),
    );
    await context.route("**/api/sync/pull?*", (route) =>
      route.fulfill({
        json: { workspaceId, documents: [], checkpoint: { seq: 0 } },
      }),
    );
    const origin = `http://127.0.0.1:${server.httpServer.address().port}`;
    await page.goto(`${origin}/draft-corrupt-check`);
    await page.evaluate((workspaceId) => {
      localStorage.setItem("codex-sync-workspace", JSON.stringify(workspaceId));
      localStorage.setItem(
        `codex-chat-draft:${workspaceId}:healthy`,
        JSON.stringify({
          version: 1,
          workspace: workspaceId,
          session: "healthy",
          text: "Healthy chat remains visible",
          deleted: false,
          source: "local",
        }),
      );
      localStorage.setItem(`codex-chat-draft:${workspaceId}:%E0%A4%A`, "{}");
    }, workspaceId);
    await page.evaluate(async () => {
      const { useSyncedDrafts } = await import("/src/sync/drafts.ts");
      const r = await import("/node_modules/.vite/deps/react.js");
      const d = await import("/node_modules/.vite/deps/react-dom_client.js");
      const React = r.default || r;
      const { createRoot } = d.default || d;
      const node = document.createElement("div");
      document.body.appendChild(node);
      createRoot(node).render(
        React.createElement(function Harness() {
          window.draft = useSyncedDrafts();
          return null;
        }),
      );
    });
    await page.waitForFunction(
      () => window.draft?.drafts.healthy === "Healthy chat remains visible",
    );
    assert.match(
      await page.evaluate(() => window.draft.error),
      /saved drafts could not be read/i,
    );
    assert.deepEqual(errors, []);
  } finally {
    await context.close();
    await server.close();
  }
});
