import { fileURLToPath } from "node:url";
import { defineConfig } from "vitest/config";

export default defineConfig({
  test: {
    name: "unit",
    root: fileURLToPath(new URL(".", import.meta.url)),
    environment: "node",
    maxWorkers: 2,
    include: ["src/**/*.test.ts", "src/**/*.test.mjs"],
  },
});
