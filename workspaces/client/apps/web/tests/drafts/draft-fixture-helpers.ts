import type { ViteDevServer } from "vite";
export function fixtureOrigin(server: ViteDevServer): string {
  const origin = server.resolvedUrls?.local[0];
  if (!origin) throw new Error("Fixture server has no local URL");
  return origin;
}
import type {
  ApiPostPath,
  PostBody,
  ApiGetPath,
  GetResult,
} from "../../src/apiTypes.ts";
import type { DraftVersion } from "../../src/sync/drafts.ts";
import type { LocalDraftRecord } from "../../src/sync/draftStorage.ts";
import type { OutboxEntry } from "../../src/sync/send.ts";
// Fixture requests come from the typed renderer under test.
export function fixturePostBody<Path extends ApiPostPath>(
  body: unknown,
): PostBody<Path> {
  return body as PostBody<Path>;
}
// The fixture reads the generated contract served by its backend.
export async function fixtureGet<Path extends ApiGetPath>(
  response: { json(): Promise<unknown> },
  _path: Path,
): Promise<GetResult<Path>> {
  return (await response.json()) as GetResult<Path>;
}
// These records are written by this renderer and read back by its fixture.
export function readDraft(payload: string): DraftVersion {
  return JSON.parse(payload);
}
export function readLocalDraft(payload: string): LocalDraftRecord {
  return JSON.parse(payload);
}
export function readOutbox(payload: string): OutboxEntry {
  return JSON.parse(payload);
}

export function readTranscript(payload: string): GetResult<"/api/transcript"> {
  return JSON.parse(payload);
}

export function readRemovedKeys(payload: string): string[] {
  return JSON.parse(payload);
}
