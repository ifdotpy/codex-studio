import { defineConfig } from "vite";
import { readFile } from "node:fs/promises";
import { createHash } from "node:crypto";
const target = "http://127.0.0.1:4620";
export default defineConfig({
  base: "./",
  plugins: [
    {
      name: "studio-offline-shell",
      enforce: "post",
      async generateBundle(_options, bundle) {
        const html = bundle["index.html"];
        if (!html || html.type !== "asset") return;
        const bootstrap = await readFile(
          new URL("./public/studio-startup.js", import.meta.url),
          "utf8",
        );
        const worker = await readFile(
          new URL("./public/studio-sw.js", import.meta.url),
          "utf8",
        );
        const manifest = await readFile(
          new URL("./public/manifest.webmanifest", import.meta.url),
          "utf8",
        );
        const digest = (value: string) =>
          createHash("sha256").update(value).digest("hex").slice(0, 16);
        const bootstrapName = `assets/studio-startup-${digest(bootstrap)}.js`;
        this.emitFile({
          type: "asset",
          fileName: bootstrapName,
          source: bootstrap,
        });
        const build = digest(
          String(html.source) +
            Object.keys(bundle).sort().join("\n") +
            bootstrap +
            worker +
            manifest,
        );
        html.source = String(html.source)
          .replace(/(?:\.\/|\/)studio-startup\.js/, `./${bootstrapName}`)
          .replace(
            "</head>",
            `<meta name="studio-build" content="${build}" /></head>`,
          );
        const initial = new Set<string>([
          "/",
          "/manifest.webmanifest",
          "/apple-touch-icon.png",
          `/${bootstrapName}`,
        ]);
        const visit = (name: string) => {
          if (initial.has(`/${name}`)) return;
          initial.add(`/${name}`);
          const output = bundle[name];
          if (output?.type === "chunk") output.imports.forEach(visit);
        };
        for (const match of String(html.source).matchAll(
          /(?:src|href)="\.\/([^"?#]+)"/g,
        ))
          visit(match[1]);
        // Only content-addressed assets belong to this build. The panel's separate,
        // unversioned renderer and every API response stay on the network.
        const assets = Object.keys(bundle).filter((name) =>
          /^assets\/.+-[\w-]{8,}\.[\w]+$/.test(name),
        );
        const allowed = [
          ...new Set([...initial, ...assets.map((name) => `/${name}`)]),
        ];
        this.emitFile({
          type: "asset",
          fileName: "studio-sw.js",
          source: `self.STUDIO_SHELL = ${JSON.stringify({ build, initial: [...initial], allowed })};\n${worker}`,
        });
      },
    },
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
