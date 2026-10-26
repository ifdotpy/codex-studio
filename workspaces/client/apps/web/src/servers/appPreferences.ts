import type { createUiPreferenceBridge } from "../sync/uiPreferenceBridge";

/** App controls use one captured preference owner. The host owns native sync. */
export interface AppPreferences {
  editPreference<T extends Record<string, unknown>>(
    key: string,
    previous: T,
    next: T,
  ): T;
  projectionBridge: ReturnType<typeof createUiPreferenceBridge> | null;
  publishWorkspace(workspaceId: string): void;
  announceStudioPreferences?(): void;
}
