import base from "./vitest.config.mjs";

// Single owner of this package's short-tier witnesses; preserve file isolation.
export default {
  ...base,
  test: {
    ...base.test,
    name: "fast",
    include: [
      // Check startup without Python, binary build identity, existing-owner attachment, and uncertain connection refusals.
      "backend.test.mjs",
      "backend-selection.test.mjs",
      "renderer-recovery.test.mjs",
    ],
  },
};
