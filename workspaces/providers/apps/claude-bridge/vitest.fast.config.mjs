import base from "./vitest.config.mjs";

// Single owner of this package's short-tier witnesses; preserve file isolation.
export default {
  ...base,
  test: {
    ...base.test,
    name: "fast",
    include: [
      // Test-side fixture waits must preserve the worker's IPC lifetime.
      "file-signals.test.mjs",
      // Preserve unconfirmed outcomes and bounded, read-only repair evidence without a model query.
      "restart-reconcile.test.mjs",
      "lifecycle-safety.test.mjs",
      "journal-repair.test.mjs",
      // Check zero idle timers, exact deadline recovery, and rearming on progress, results and Studio waits.
      "stall-watch.test.mjs",
      // SDK UUID coverage, composite history controls and authoritative session transitions.
      "history-index.test.mjs",
      "history-recovery.test.mjs",
      "session-identity.test.mjs",
      // Proven unread/cancelled transition inputs must release their guards.
      "steer.test.mjs",
      // Final stream accounting, deferred result totals and explicit unfinished-response gaps.
      "usage.test.mjs",
      "usage-bridge.test.mjs",
      // Portable image codecs, bounded compression and visible pre-input refusals.
      "images.test.mjs",
      "images-bridge.test.mjs",
    ],
  },
};
