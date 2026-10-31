import { fileURLToPath } from "node:url";
import { defineConfig } from "vitest/config";

export default defineConfig({
  test: {
    name: "unit",
    root: fileURLToPath(new URL(".", import.meta.url)),
    environment: "node",
    // Bundle the large component barrels once instead of importing hundreds of
    // dependency modules again for every isolated renderer test.
    deps: {
      optimizer: {
        ssr: {
          enabled: true,
          include: [
            "@mantine/core",
            "lucide-react",
            // RxDB and its plugins share one registration singleton.
            "rxdb",
            "rxdb/plugins/leader-election",
            "rxdb/plugins/replication",
            "rxdb/plugins/storage-memory",
          ],
        },
      },
    },
    // Reuse bounded thread workers while keeping each file's globals isolated.
    pool: "threads",
    isolate: true,
    maxWorkers: 2,
    include: ["src/**/*.test.ts", "src/**/*.test.mjs"],
  },
});
