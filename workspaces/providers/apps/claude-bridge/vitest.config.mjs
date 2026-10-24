import { fileURLToPath } from "node:url";
import { scrubAgentEnvironment } from "../../../tooling/apps/repository-checks/gate-config.mjs";

scrubAgentEnvironment(process.env);

export default {
  root: fileURLToPath(new URL(".", import.meta.url)),
  test: {
    include: ["*.test.mjs"],
    environment: "node",
    testTimeout: 30_000,
    hookTimeout: 30_000,
    maxWorkers: 2,
  },
};
