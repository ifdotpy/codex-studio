import base from "./vitest.config.ts";

// Single owner of this package's short-tier witnesses; preserve file isolation.
export default {
  ...base,
  test: {
    ...base.test,
    name: "fast",
    include: [
      // Check startup without Python, binary build identity, existing-owner attachment, and uncertain connection refusals.
      "backend.test.ts",
      "backend-selection.test.ts",
      "renderer-recovery.test.ts",
    ],
  },
};
