import { fileURLToPath } from "node:url";

export default {
  root: fileURLToPath(new URL(".", import.meta.url)),
  test: {
    include: ["*.test.ts"],
    exclude: ["*-helper-witness.test.ts"],
    environment: "node",
    testTimeout: 30_000,
    hookTimeout: 30_000,
    maxWorkers: 2,
  },
} satisfies Parameters<typeof import("vitest/config").defineConfig>[0];
