import { defineConfig } from "vite";
import { readFile } from "node:fs/promises";
const target = "http://127.0.0.1:4620";
export default defineConfig({
  base: "./",
  plugins: [
    {
      name: "studio-panel-bundle",
      configureServer(server) {
        server.middlewares.use(
          "/assets/panel-ui.js",
          async (_request, response) => {
            try {
              const script = await readFile(
                new URL("./dist/assets/panel-ui.js", import.meta.url),
              );
              response.setHeader("Content-Type", "text/javascript");
              response.setHeader("Cache-Control", "no-store");
              response.end(script);
            } catch {
              response.statusCode = 503;
              response.end(
                "Build the Studio panel renderer before starting the development server.",
              );
            }
          },
        );
      },
    },
  ],
  build: {
    rollupOptions: {
      input: { main: "index.html", panel: "panel-preview.html" },
    },
  },
  server: {
    proxy: {
      "/api": {
        target,
        changeOrigin: true,
        configure(proxy) {
          proxy.on("proxyReq", (request) =>
            request.setHeader("origin", target),
          );
        },
      },
    },
  },
});
