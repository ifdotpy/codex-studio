import type { TranscriptPageData } from "../../src/apiTypes.ts";
import type { ViteDevServer } from "vite";
export function fixtureOrigin(server: ViteDevServer): string {
  const origin = server.resolvedUrls?.local[0];
  if (!origin) throw new Error("Fixture server has no local URL");
  return origin;
}
import type { DraftVersion } from "../../src/sync/drafts.ts";
import type { LocalDraftRecord } from "../../src/sync/draftStorage.ts";
import type { OutboxEntry } from "../../src/sync/send.ts";
// These records are written by this renderer and read back by its fixture.
export function readDraft(payload: string): DraftVersion {
  return JSON.parse(payload);
}
export function readLocalDraft(payload: string): LocalDraftRecord {
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
