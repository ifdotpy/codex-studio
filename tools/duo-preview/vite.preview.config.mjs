// Localhost-only preview of the feat/duo-ios-shell web build for the simulator.
const target = "https://macbook-pro-lumina.tailf00fa0.ts.net";
import { appendFileSync, statSync } from "node:fs";
const LOG = "/tmp/duo-ios-preview/device-log.jsonl";
// Disk guard: entries over 8 KB are cut; writes stop above 200 MB; a
// failed write (full disk) never stops the preview.
const ENTRY_MAX = 8192;
const LOG_MAX = 200 * 1024 * 1024;
// Preview-only: the device logger (dist/duo-log.js) posts here.
const deviceLog = {
  name: "duo-device-log",
  configurePreviewServer(server) {
    server.middlewares.use("/__duo-log", (req, res) => {
      let body = "";
      req.on("data", (chunk) => (body += chunk));
      req.on("end", () => {
        try {
          let size = 0;
          try { size = statSync(LOG).size; } catch {}
          if (size < LOG_MAX)
            for (const entry of JSON.parse(body || "[]")) {
              let line = JSON.stringify(entry);
              if (line.length > ENTRY_MAX) line = JSON.stringify({ cut: line.length, head: line.slice(0, ENTRY_MAX) });
              appendFileSync(LOG, line + "\n");
            }
        } catch (error) {
          console.error("duo-log write skipped:", error.code || error.message);
        }
        res.statusCode = 204;
        res.end();
      });
    });
  },
};
export default {
  plugins: [deviceLog],
  base: "./",
  preview: {
    host: "127.0.0.1",
    port: 8793,
    strictPort: true,
    // tailscale serve :8444 forwards the tailnet name to this loopback port.
    allowedHosts: ["macbook-pro-lumina.tailf00fa0.ts.net"],
    proxy: {
      "/api": {
        target,
        changeOrigin: true,
        ws: true,
        configure(proxy) {
          proxy.on("proxyReq", (request) => request.setHeader("origin", target));
        },
      },
    },
  },
};
