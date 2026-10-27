// Owns the pinned source and extraction paths for consumed Codex wire contracts.
// Claude types come directly from the version pinned by the bridge manifest.
export const codexContractSource = {
  version: "0.164.0",
  directory: "workspaces/runtime/packages/runtime-schema/src/schemas/codex",
  lifecycle: {
    TurnStatus: "v2/TurnCompletedNotification.json",
    ThreadStatus: "v2/ThreadStatusChangedNotification.json",
    ThreadActiveFlag: "v2/ThreadStatusChangedNotification.json",
    TurnItemsView: "v2/TurnCompletedNotification.json",
  },
  usage: "v2/ThreadTokenUsageUpdatedNotification.json",
  errors: "v2/ErrorNotification.json",
} as const;
