import { useSyncExternalStore } from "react";
import type { components } from "./generated/api";

/** Maximum retained client log entries. Unit: entries. */
export const APP_LOG_MAX_ENTRIES = 200;
type Level = components["schemas"]["StudioErrorLevel"];
export type AppLogEntry = {
  id: string;
  level: Extract<Level, "log" | "error">;
  at: number; // Epoch milliseconds.
  title: string;
  reason: string;
  detail?: string;
};
let entries: AppLogEntry[] = [];
let nextId = 0;
const listeners = new Set<() => void>();
const snapshot = () => entries;
const subscribe = (listener: () => void) => {
  listeners.add(listener);
  return () => listeners.delete(listener);
};
function publish() {
  listeners.forEach((listener) => listener());
}
export function appendAppLog(
  entry: Omit<AppLogEntry, "id" | "level"> & { level?: AppLogEntry["level"] },
) {
  entries = [
    ...entries,
    { ...entry, id: `app-log:${++nextId}`, level: entry.level ?? "log" },
  ].slice(-APP_LOG_MAX_ENTRIES);
  publish();
}
export function clearAppLog() {
  entries = [];
  publish();
}
export function useAppLog() {
  return useSyncExternalStore(subscribe, snapshot, snapshot);
}
