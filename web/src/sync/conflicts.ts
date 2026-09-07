import type { RxConflictHandler } from "rxdb";
import type { SyncDocument } from "./client";
import { encodeDraftPayload } from "./draftPayload";
// Keep every concurrent text, including two tabs that share the same device branch.
export const draftConflictHandler: RxConflictHandler<SyncDocument> = {
  isEqual: (a, b) => a.payload === b.payload && a._deleted === b._deleted,
  resolve: async ({ realMasterState, newDocumentState }) => {
    const master = JSON.parse(realMasterState.payload),
      fork = JSON.parse(newDocumentState.payload);
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
