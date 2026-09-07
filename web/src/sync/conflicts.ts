import type { RxConflictHandler } from "rxdb";
import type { SyncDocument } from "./client";
// Keep every concurrent text, including two tabs that share the same device branch.
export const draftConflictHandler: RxConflictHandler<SyncDocument> = {
  isEqual: (a, b) => a.payload === b.payload && a._deleted === b._deleted,
  resolve: async ({ realMasterState, newDocumentState }) => {
    const master = JSON.parse(realMasterState.payload),
      fork = JSON.parse(newDocumentState.payload);
    const alternatives = [
      ...new Set([
        ...(master.alternatives || []),
        ...(fork.alternatives || []),
        fork.text,
      ]),
    ]
      .filter((text) => text && text !== master.text)
      .sort();
    return {
      ...realMasterState,
      payload: JSON.stringify({ ...master, alternatives }),
    };
  },
};
