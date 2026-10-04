import type { GetResult } from "../api";

export type SyncPullResponse = GetResult<"/api/sync/pull">;

export function isEntityResetResponse(
  response: SyncPullResponse,
  scope: string,
): boolean {
  if (response.reset !== true) return false;
  if (scope !== "state:entities:v1")
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
