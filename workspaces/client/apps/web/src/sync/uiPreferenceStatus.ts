import type { PreferenceScope } from "./uiPreferenceMerge";

export type PreferenceWriteStatus =
  | { state: "saved" }
  | { state: "saving" }
  | { state: "error"; error: string };
const saved: PreferenceWriteStatus = { state: "saved" };
const statuses = new WeakMap<
  Storage,
  Partial<Record<PreferenceScope, PreferenceWriteStatus>>
>();
const retries = new WeakMap<Storage, () => void>();
const listeners = new Set<() => void>();
export const readPreferenceWriteStatus = (
  storage: Storage,
  scope: PreferenceScope,
) => statuses.get(storage)?.[scope] ?? saved;
export function setPreferenceWriteStatus(
  storage: Storage,
  scope: PreferenceScope,
  status: PreferenceWriteStatus,
) {
  statuses.set(storage, { ...statuses.get(storage), [scope]: status });
  listeners.forEach((listener) => listener());
}
export function subscribePreferenceWriteStatus(listener: () => void) {
  listeners.add(listener);
  return () => {
    listeners.delete(listener);
  };
}
export function registerPreferenceRetry(storage: Storage, retry: () => void) {
  retries.set(storage, retry);
  return () => {
    if (retries.get(storage) === retry) retries.delete(storage);
  };
}
export function retryPreferenceWrites(storage: Storage) {
  retries.get(storage)?.();
}
