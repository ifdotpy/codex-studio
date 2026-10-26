import type { ViteDevServer } from "vite";
import type { ApiPostPath, PostBody } from "../../src/apiTypes.ts";

export function fixtureUrl(server: ViteDevServer): string {
  const url = server.resolvedUrls?.local[0];
  if (!url) throw new Error("Fixture server has no local URL");
  return url;
}

/** Requests come from the renderer under test, which owns these route bodies. */
export function fixturePostBody<Path extends ApiPostPath>(
  body: unknown,
): PostBody<Path> {
  return body as PostBody<Path>;
}

export async function usageFixtureServer(
  entry: string,
  options: {
    html?: string;
    middleware?: import("vite").Connect.NextHandleFunction;
  } = {},
) {
  const { createFixtureViteServer } = await import("../playwright.ts");
  const { fileURLToPath } = await import("node:url");
  return createFixtureViteServer({
    configFile: false,
    root: fileURLToPath(new URL("../../", import.meta.url)),
    server: { host: "127.0.0.1", port: 0 },
    plugins: [
      {
        name: entry,
        configureServer(server) {
          if (options.middleware) server.middlewares.use(options.middleware);
          server.middlewares.use((req, res, next) => {
            if (req.url?.split("?")[0] !== "/") return next();
            res.setHeader("Content-Type", "text/html");
            res.end(
              options.html ??
                `<meta name="viewport" content="width=device-width, initial-scale=1"><div id="root"></div><script type="module" src="/tests/usage/${entry}.fixture.tsx"></script>`,
            );
          });
        },
      },
    ],
  });
}
