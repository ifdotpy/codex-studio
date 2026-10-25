import base from "./vitest.config.mjs";

// Single owner of this package's short-tier witnesses; preserve file isolation.
export default {
  ...base,
  test: {
    ...base.test,
    name: "fast",
    include: [
      // Preserve unconfirmed outcomes and bounded, read-only repair evidence without a model query.
      "restart-reconcile.test.mjs",
      "journal-repair.test.mjs",
      // Check zero idle timers, exact deadline recovery, and rearming on progress, results and Studio waits.
      "stall-watch.test.mjs",
      // Final stream accounting, deferred result totals and explicit unfinished-response gaps.
      "usage.test.mjs",
      "usage-bridge.test.mjs",
    ],
  },
};
