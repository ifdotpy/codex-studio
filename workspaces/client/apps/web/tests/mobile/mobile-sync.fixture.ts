import type { Snapshot } from "../../src/types.ts";
import type { components } from "../../src/generated/api.ts";
declare global {
  interface Window {
    mobileSync: {
      values: { state?: Snapshot | null } & Record<string, unknown>;
      stops: (() => void)[];
      resumeCalls: number;
      stopResume: () => void;
      value: Snapshot | null;
      stopCompact: () => void;
      revision: (scope: string) => unknown;
      readCachedEntity: typeof readCachedEntity;
    };
  }
}
declare global {
  interface Window {
    mobileCached: {
      syncErrors: string[];
      stopState: () => void;
      stopDrafts: () => void;
      state: Snapshot | null;
    };
  }
}
/** These cache envelopes were written by this fixture's renderer. */
function readCachedEntity(
  payload: string,
): components["schemas"]["SyncAgentEntityPayload"] {
  return JSON.parse(payload);
}
window.mobileSync = {
  values: {},
  stops: [],
  resumeCalls: 0,
  stopResume: () => {},
  value: null,
  stopCompact: () => {},
  readCachedEntity,
  revision(scope) {
    const value = window.mobileSync.values[scope];
    return value && typeof value === "object" && "revision" in value
      ? value.revision
      : undefined;
  },
};
window.mobileCached = {
  syncErrors: [],
  stopState: () => {},
  stopDrafts: () => {},
  state: null,
};
