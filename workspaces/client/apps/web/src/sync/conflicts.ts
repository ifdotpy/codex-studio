import type { RxConflictHandler, RxReplicationWriteToMasterRow } from "rxdb";
import type { SyncDocument } from "./client";
import { encodeDraftPayload } from "./draftPayload.ts";
import type { DraftVersion } from "../generated/rustOwned";
// Keep every concurrent text, including two tabs that share the same device branch.
export const draftConflictHandler: RxConflictHandler<SyncDocument> = {
  isEqual: (a, b) => a._deleted === b._deleted && a.payload === b.payload,
  resolve: async ({ realMasterState, newDocumentState }) => {
    const master: DraftVersion = JSON.parse(realMasterState.payload),
      fork: DraftVersion = JSON.parse(newDocumentState.payload);
    const winner = (fork.updated || 0) > (master.updated || 0) ? fork : master;
    const other = winner === fork ? master : fork;
    const replaced =
      other.id && (winner.seen?.[other.id] ?? -1) >= other.updated;
    const alternatives = [
      ...new Set([
        ...(master.alternatives || []),
        ...(fork.alternatives || []),
        ...(replaced ? [] : [other.text]),
      ]),
    ]
      .filter((text) => text && text !== winner.text)
      .sort();
    return {
      ...realMasterState,
      payload: encodeDraftPayload({ ...winner, alternatives }),
    };
  },
};

// One owner per open draft collection. Retain only the last two distinct own
// acknowledgements per branch; absent proof always uses exact payload equality.
export function createDraftConflictHandler() {
  type Version = Pick<SyncDocument, "payload" | "_deleted">;
  const acknowledged = new Map<
    string,
    { latest: Version; previous?: Version }
  >();
  const same = draftConflictHandler.isEqual;
  const conflictHandler: RxConflictHandler<SyncDocument> = {
    ...draftConflictHandler,
    isEqual: (a, b, context) => {
      if (same(a, b, context)) return true;
      if (context !== "downstream-check-if-equal-1") return false;
      const previous = acknowledged.get(a.id)?.previous;
      return (
        previous !== undefined &&
        a.payload === previous.payload &&
        a._deleted === previous._deleted
      );
    },
  };
  const acknowledge = (
    rows: readonly RxReplicationWriteToMasterRow<SyncDocument>[],
    conflicts: readonly SyncDocument[],
  ) => {
    const conflicted = new Set(conflicts.map((row) => row.id));
    for (const { newDocumentState: row } of rows) {
      if (conflicted.has(row.id)) continue;
      const current = acknowledged.get(row.id);
      if (
        current?.latest.payload === row.payload &&
        current.latest._deleted === row._deleted
      )
        continue;
      acknowledged.set(row.id, {
        latest: { payload: row.payload, _deleted: row._deleted },
        previous: current?.latest,
      });
    }
  };
  return { conflictHandler, acknowledge };
}
