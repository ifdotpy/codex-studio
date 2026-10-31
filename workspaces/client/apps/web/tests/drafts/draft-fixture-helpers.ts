import type { useSyncedDrafts } from "../../src/sync/drafts.ts";
import type { syncDatabase } from "../../src/sync/client.ts";
declare global {
  interface Window {
    draft: ReturnType<typeof useSyncedDrafts>;
    db: Awaited<ReturnType<typeof syncDatabase>>["db"];
  }
}
import type { TranscriptPageData } from "../../src/apiTypes.ts";
import type { DraftVersion } from "../../src/sync/drafts.ts";
import type { OutboxEntry } from "../../src/sync/send.ts";
// These records are written by this renderer and read back by its fixture.
export function readDraft(payload: string): DraftVersion {
  return JSON.parse(payload);
}
export function readOutbox(payload: string): Omit<OutboxEntry, "id"> {
  return JSON.parse(payload);
}

export function readTranscript(payload: string): TranscriptPageData {
  return JSON.parse(payload);
}

export function readRemovedKeys(payload: string): string[] {
  return JSON.parse(payload);
}
