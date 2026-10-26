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
      "workspace-fence.test.mjs",
      // Recover an absent descriptor only with exact idle and unsent-input proof.
      "native-session-recovery.test.mjs",
      "native-session-recovery-cli.test.mjs",
      "steer.test.mjs",
      // Check zero idle timers, exact deadline recovery, and rearming on progress, results and Studio waits.
      "stall-watch.test.mjs",
      // Final stream accounting, deferred result totals and explicit unfinished-response gaps.
      "usage.test.mjs",
      "usage-bridge.test.mjs",
      // Portable image codecs, bounded compression and visible pre-input refusals.
      "images.test.mjs",
      "images-bridge.test.mjs",
    ],
  },
};
