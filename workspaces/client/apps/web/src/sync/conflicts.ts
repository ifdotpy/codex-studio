import type { RxConflictHandler } from "rxdb";
import type { SyncDocument } from "./client";
import { encodeDraftPayload } from "./draftPayload.ts";
import type { DraftVersion } from "../generated/rustOwned";
// Keep every concurrent text, including two tabs that share the same device branch.
export const draftConflictHandler: RxConflictHandler<SyncDocument> = {
  isEqual: (a, b, context) => {
    if (a._deleted !== b._deleted) return false;
    if (a.payload === b.payload) return true;
    if (context !== "downstream-check-if-equal-1") return false;
    const master: DraftVersion = JSON.parse(a.payload);
    const fork: DraftVersion = JSON.parse(b.payload);
    // An echo can trail a newer successful push. Keep both the fork and its
    // assumed master when the fork has already observed and replaced this edit.
    return (
      master.updated < fork.updated &&
      (fork.seen?.[a.id] ?? -1) >= master.updated
    );
  },
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
