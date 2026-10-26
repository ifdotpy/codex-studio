import {
  readSavedDraftJournal,
  readSavedLocalDraftRecord,
  type DraftValueSource,
} from "../sync/draftRead";
import type { DraftVersion } from "../sync/drafts";

/** Read saved values through the existing draft owners, without browser IO. */
export function readLegacyDraftValues(
  workspace: string,
  storage: DraftValueSource,
  savedVersions: readonly DraftVersion[] = [],
): DraftVersion[] {
  const scope = `codex-drafts:${workspace}`;
  const versions = [...savedVersions];
  for (const entry of readSavedDraftJournal(storage, scope))
    versions.push(entry.version);
  const prefix = `codex-chat-draft:${workspace}:`;
  const sessions = Array.from({ length: storage.length }, (_, index) =>
    storage.key(index)!,
  )
    .filter((key) => key.startsWith(prefix))
    .map((key) => decodeURIComponent(key.slice(prefix.length)))
    .sort();
  for (const session of sessions) {
    const record = readSavedLocalDraftRecord(storage, scope, session)!;
    const text = record.deleted ? "" : record.text;
    // A local recovery copy is an independent branch. Do not overwrite another
    // device's branch or discard divergent text during the migration.
    if (
      !versions.some(
        (version) => version.session === session && version.text === text,
      )
    ) {
      versions.push({
        id: `browser-import:${session}`,
        device: "browser-import",
        session,
        text,
        updated:
          record.updated ??
          (record.source === "local"
            ? Math.max(
                0,
                ...versions
                  .filter((version) => version.session === session)
                  .map((version) => version.updated + 1),
              )
            : 0),
      });
    }
  }
  // Legacy scope-wide snapshots predate per-session records.
  const raw = storage.getItem(scope);
  if (raw !== null) {
    const legacy: unknown = JSON.parse(raw);
    if (!legacy || typeof legacy !== "object" || Array.isArray(legacy))
      throw new Error("Invalid saved draft snapshot.");
    for (const [session, text] of Object.entries(legacy).sort()) {
      if (typeof text !== "string")
        throw new Error("Invalid saved draft text.");
      if (
        text &&
        !readSavedLocalDraftRecord(storage, scope, session) &&
        !versions.some(
          (version) => version.session === session && version.text === text,
        )
      )
        versions.push({
          id: `browser-snapshot:${session}`,
          device: "browser-snapshot",
          session,
          text,
          updated: 0,
        });
    }
  }
  return versions;
}
