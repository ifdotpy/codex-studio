import type { DraftVersion } from "./drafts";

/** Remove draft versions already observed and replaced by a later writer. */
export function activeDraftVersions(versions: DraftVersion[]) {
  return versions
    .filter(
      (version) =>
        !versions.some(
          (other) =>
            other.id !== version.id &&
            (other.seen?.[version.id] ?? -1) >= version.updated,
        ),
    )
    .sort((a, b) => b.updated - a.updated || a.id.localeCompare(b.id));
}
