import { createRequire } from "node:module";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test, expect } from "../../../tests/playwright.mjs";

test("progress cache retains good content through a panel read error", async ({
  browser,
}) => {
  const repo = join(import.meta.dirname, "../../../../../../../");
  const webRoot = join(repo, "workspaces/client/apps/web");
  const require = createRequire(join(webRoot, "package.json"));
  const { createServer } = await import(require.resolve("vite"));
  const cacheDir = await mkdtemp(join(tmpdir(), "studio-progress-error-"));
  const entry = join(webRoot, "progress-cache-error-fixture.ts");
  const server = await createServer({
    configFile: false,
    root: webRoot,
    cacheDir,
    server: { host: "127.0.0.1", port: 0 },
    optimizeDeps: { noDiscovery: true },
    plugins: [
      {
        name: "progress-cache-error-fixture",
        configureServer(vite) {
          vite.middlewares.use("/check", (_request, response) => {
            response.setHeader("Content-Type", "text/html");
            response.end(
              '<script type="module" src="/progress-cache-error-fixture.ts"></script>',
            );
          });
        },
        resolveId(id) {
          if (id === "/progress-cache-error-fixture.ts") return entry;
        },
        load(id) {
          if (id === entry)
            return "import * as cache from '/src/components/agents/progressCache.ts'; window.progressCache = cache;";
        },
      },
    ],
  });
  try {
    await server.listen();
    const page = await browser.newPage();
    await page.addInitScript(() => {
      window.panelResponse = {
        markdown: "Last good progress",
        revision: "good-1",
        error: null,
      };
      window.fetch = async (input) => {
        const requestUrl = input instanceof Request ? input.url : String(input);
        const url = new URL(requestUrl, location.href);
        if (
          url.pathname !== "/api/panel" ||
          url.searchParams.get("agent") !== "agent-a"
        )
          throw new Error("Unexpected progress request");
        const value = {
          id: "agent-a",
          agent: "agent-a",
          format: "markdown",
          markdown: panelResponse.markdown,
          path: "/fixture/PROGRESS.md",
          revision: panelResponse.revision,
          updated: 1,
          exists: true,
          error: panelResponse.error,
        };
        return new Response(JSON.stringify(value), {
          status: 200,
          headers: { "Content-Type": "application/json" },
        });
      };
    });
    await page.goto(
      `http://127.0.0.1:${server.httpServer.address().port}/check`,
    );
    await page.waitForFunction(() => !!window.progressCache);

    const outcome = await page.evaluate(async () => {
      const first = await progressCache.readProgress("state-a", "agent-a");
      panelResponse.markdown = "";
      panelResponse.revision = null;
      panelResponse.error = "PROGRESS.md must contain valid UTF-8 text";
      const error = await progressCache.readProgress("state-a", "agent-a").then(
        () => null,
        (failure) => failure.message,
      );
      const duringError = progressCache.peekProgress("state-a", "agent-a");
      panelResponse.markdown = "Recovered progress";
      panelResponse.revision = "good-2";
      panelResponse.error = null;
      const recovered = await progressCache.readProgress("state-a", "agent-a");
      return {
        first: first.markdown,
        error,
        duringError: duringError?.markdown,
        recovered: recovered.markdown,
        cached: progressCache.peekProgress("state-a", "agent-a")?.markdown,
      };
    });
    expect(outcome).toEqual({
      first: "Last good progress",
      error: "PROGRESS.md must contain valid UTF-8 text",
      duringError: "Last good progress",
      recovered: "Recovered progress",
      cached: "Recovered progress",
    });
  } finally {
    await server.close();
    await rm(cacheDir, { recursive: true, force: true });
  }
});
