import { fileURLToPath } from "node:url";

const vitestEntry = fileURLToPath(
  new URL("../web/node_modules/vitest/dist/index.js", import.meta.url),
);

export default {
  root: fileURLToPath(new URL(".", import.meta.url)),
  resolve: {
    alias: [{ find: /^vitest$/, replacement: vitestEntry }],
  },
  test: {
    include: ["*.test.mjs"],
    environment: "node",
    testTimeout: 30_000,
    hookTimeout: 30_000,
    maxWorkers: 2,
  },
};
