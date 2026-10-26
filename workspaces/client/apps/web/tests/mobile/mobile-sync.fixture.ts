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
    values: { state?: Snapshot | null } & Record<string, unknown>;
    stops: (() => void)[];
    resumeCalls: number;
    stopResume: () => void;
    value: Snapshot | null;
    stopCompact: () => void;
    mobileSyncReadCachedEntity: typeof readCachedEntity;
    syncErrors: string[];
    stopState: () => void;
    stopDrafts: () => void;
    state: Snapshot | null;
  }
}
