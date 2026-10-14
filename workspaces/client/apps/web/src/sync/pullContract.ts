import type { GetResult } from "../api";
import { SYNC_ENTITY_SCOPE } from "../generated/api";

export type SyncPullResponse = GetResult<"/api/sync/pull">;

export function isEntityResetResponse(
  response: SyncPullResponse,
  scope: string,
): response is Extract<SyncPullResponse, { reset: true }> {
  if (response.reset !== true) return false;
  if (scope !== SYNC_ENTITY_SCOPE)
    throw new Error("The server reset an unsupported sync scope.");
  return true;
}

export function requiredSyncNumber(
  value: number | null | undefined,
  field: string,
): number {
  if (typeof value !== "number" || !Number.isSafeInteger(value) || value < 0)
    throw new Error(`The server returned an invalid ${field} sync value.`);
  return value;
}
