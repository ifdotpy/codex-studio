import type { Room, Snapshot } from "../../src/types.ts";

// This fixture serves and reads its own snapshot and room documents.
export function radioSnapshot(value: unknown): Snapshot {
  return value as Snapshot;
}
export function radioRoom(value: unknown): Room {
  return value as Room;
}
