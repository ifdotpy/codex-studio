import { defineConfig } from "vite";
const target = "http://127.0.0.1:4620";
export default defineConfig({
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
