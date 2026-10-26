import type { Snapshot } from "../../src/types.ts";
import type { components } from "../../src/generated/api.ts";
/** These cache envelopes were written by this fixture's renderer. */
function readCachedEntity(
  payload: string,
): components["schemas"]["SyncAgentEntityPayload"] {
  return JSON.parse(payload);
}
window.mobileSyncReadCachedEntity = readCachedEntity;
declare global {
  interface Window {
    mobileSyncValues: { state?: Snapshot | null } & Record<string, unknown>;
    mobileSyncStops: (() => void)[];
    mobileSyncResumeCalls: number;
    mobileSyncStopResume: () => void;
    mobileSyncValue: Snapshot | null;
    mobileSyncStopCompact: () => void;
    mobileSyncReadCachedEntity: typeof readCachedEntity;
    mobileCachedSyncErrors: string[];
    mobileCachedStopState: () => void;
    mobileCachedStopDrafts: () => void;
    mobileCachedState: Snapshot | null;
  }
}
