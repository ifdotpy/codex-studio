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
      "live-permissions.test.ts",
      // Native tool plan fields retain their raw wire values.
      "protocol-output.test.ts",
      "provider-lifecycle.test.ts",
      "provider-lifecycle-boundary.test.ts",
      "sdk-drift.test.ts",
      "sdk-contract-gaps.test.ts",
      "question-input.test.ts",
      "rate-limit-input.test.ts",
      "features.test.ts",
      "command-background.test.ts",
      "file-restore.test.ts",
      "move.test.ts",
      "move-external.test.ts",
      // Test-side fixture waits must preserve the worker's IPC lifetime.
      "file-signals.test.ts",
      // Preserve unconfirmed outcomes and bounded, read-only repair evidence without a model query.
      "restart-reconcile.test.ts",
      "lifecycle-safety.test.ts",
      "journal-repair.test.ts",
      "workspace-fence.test.ts",
      // Recover an absent descriptor only with exact idle and unsent-input proof.
      "native-session-recovery.test.ts",
      "native-session-recovery-cli.test.ts",
      "steer.test.ts",
      // Check zero idle timers, exact deadline recovery, and rearming on progress, results and Studio waits.
      "stall-watch.test.ts",
      // Structured Claude failure causes keep bounded metadata and the native class.
      "failure-causes.test.ts",
      // MCP calls move to a retained task after the native wait; the opt-out keeps the foreground stall watch.
      "mcp-background.test.ts",
      // Quiet checks and pressure cleanup use fake clocks and never create memory load.
      "shell-policy.test.ts",
      // SDK UUID coverage, composite history controls and authoritative session transitions.
      "history-index.test.ts",
      "controls.test.ts",
      "history-recovery.test.ts",
      "session-identity.test.ts",
      // Proven unread/cancelled transition inputs must release their guards.
      "steer.test.ts",
      "stop-receipts.test.ts",
      // Final stream accounting, deferred result totals and explicit unfinished-response gaps.
      "usage.test.ts",
      "usage-bridge.test.ts",
      "usage-events.test.ts",
      // Portable image codecs, bounded compression and visible pre-input refusals.
      "images.test.ts",
      "images-bridge.test.ts",
    ],
  },
};
