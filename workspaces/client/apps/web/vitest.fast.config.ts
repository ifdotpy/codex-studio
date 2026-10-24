import base from "./vitest.unit.config";

// Single owner of this package's short-tier witnesses; preserve file isolation.
export default {
  ...base,
  test: {
    ...base.test,
    name: "fast",
    include: [
      // Preserve newly landed safety witnesses, including the real Rust draft-route fixture.
      "src/sync/entityProjection.test.mjs",
      "src/sync/resourceEvents.live.test.ts",
      "src/sync/progressSubscription.test.ts",
      "src/hooks/chatPrefetch.test.ts",
      "src/sync/drafts.publication.test.ts",
      "src/sync/transcriptCache.test.ts",
      "src/sync/entityProjectionObserver.test.ts",
      "src/sync/client.draftServer.test.ts",

      // Token snapshots carry each agent once and derive team membership without entity projections.
      "src/usage/tokenRate.records.test.ts",
      "src/usage/tokenRate.test.mjs",
      "src/components/StudioErrors.test.ts",
      "src/components/SidebarWarnings.test.ts",
      "src/components/ConversationWarnings.test.ts",
      // web unit safety witnesses
      "src/api.test.ts",
      "src/startWait.test.ts",
      "src/apiSchema.test.ts",
      "src/chatScope.test.ts",
      "src/hooks/conversationData.test.ts",
      "src/scaleVerifierHelpers.test.mjs",
      "src/uiTeamScaleHelpers.test.mjs",
      "src/uiTeamScaleAudit.test.mjs",
      "src/verifyRustChatLifecycle.test.mjs",
      "src/components/agents/AgentProgress.test.ts",
      "src/components/models/modelOption.test.ts",
      "src/components/chat-status/chatStatusModel.test.mjs",
      "src/sync/outboxWriteFailure.test.ts",
      "src/sync/useOutbox.test.ts",
      "src/pythonBackendRemoval.test.mjs",
      "src/sync/draftPayload.test.mjs",
      "src/scrollFollowFixture.test.mjs",
      "src/receiptSummaryFixture.test.mjs",
      "src/transcriptPageBounds.test.ts",
      "src/components/message-delivery/messageDelivery.test.mjs",
      "src/components/claudeRepairModel.test.ts",
      "src/components/turnHistoryModel.test.mjs",
      "src/components/useTurnErrors.test.ts",
      "src/components/conversation/transcript/TurnHistory.uncertain.test.ts",
      "src/components/shell/WorkerPermissions.test.ts",
      "src/components/agents/RetainedWorktrees.test.ts",
      "src/components/liveRequestConsumers.test.ts",
      "src/components/readState.noRefetch.test.ts",
      "src/components/sidebarOrder.migration.test.ts",
      "src/components/ProjectServerCards.test.ts",
      "src/components/models/modelWhitelist.test.ts",
      // Read a stable baseline once and retain the full-disk notice across failed reads.
      "src/components/SupervisorRecoveryNotice.test.ts",
      "src/components/desktopNotices.test.ts",
      "src/components/watchResourceReads.test.ts",
      // Bounded account, queue and cost consumers apply stream values without reads.
      "src/components/resourceReadRecovery.test.ts",
      "src/components/RealtimeVoice.test.ts",
      // Normal draft changes use RxDB's master stream; recovery reads its durable checkpoint.
      "src/sync/client.draftStream.test.ts",
      // A complete state batch advances the durable projection with no new pull; storage failures, gaps, reconnects and epochs still reconcile.
      "src/sync/client.dispatchGate.test.ts",
      "src/sync/client.entityReset.test.ts",
      "src/sync/resourceEntityDelivery.test.ts",
      "src/sync/resourceEvents.test.ts",
      "src/servers/eventSource.test.ts",
      // Prevent duplicate entity pulls and preserve server storage and API ownership.
      "src/servers/useUiPreferenceSync.test.ts",
      "src/servers/useServerDiscovery.test.ts",
      "src/servers/sidebarSnapshot.test.ts",
      "src/servers/mergedSidebar.test.ts",
      "src/sync/uiPreferenceBridge.test.ts",
      // Prevent idle polling and lost draft notifications during a refresh.
      "src/native/useNativeDrafts.test.ts",
      // An authoritative delete readback must settle even when another refresh owns the current render generation.
      "src/hooks/snapshotDeleteReadback.test.ts",
      "src/chatDeleteScope.test.ts",
    ],
  },
};
