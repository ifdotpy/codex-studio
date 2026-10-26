import base from "./vitest.config.ts";

// Single owner of this package's short-tier witnesses; preserve file isolation.
export default {
  ...base,
  test: {
    ...base.test,
    name: "fast",
    include: [
      "launcher.test.ts",
      "request-regressions.test.ts",
      "output-compatibility.test.ts",
      // Test-side fixture waits must preserve the worker's IPC lifetime.
      "file-signals.test.ts",
      // Preserve unconfirmed outcomes and bounded, read-only repair evidence without a model query.
      "restart-reconcile.test.ts",
      "lifecycle-safety.test.ts",
      "journal-repair.test.ts",
      // Check zero idle timers, exact deadline recovery, and rearming on progress, results and Studio waits.
      "stall-watch.test.ts",
      // SDK UUID coverage, composite history controls and authoritative session transitions.
      "history-index.test.ts",
      "history-recovery.test.ts",
      "session-identity.test.ts",
      // Proven unread/cancelled transition inputs must release their guards.
      "steer.test.ts",
      "stop-receipts.test.ts",
      // Final stream accounting, deferred result totals and explicit unfinished-response gaps.
      "usage.test.ts",
      "usage-bridge.test.ts",
      // Portable image codecs, bounded compression and visible pre-input refusals.
      "images.test.ts",
      "images-bridge.test.ts",
    ],
  },
};
