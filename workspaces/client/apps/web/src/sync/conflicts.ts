import type { components } from "../generated/api.ts";
import { encodeDraftPayload } from "./draftPayload.ts";
import type { DraftVersion } from "../generated/rustOwned";
// Keep every concurrent text, including two tabs that share the same device branch.
type Document = components["schemas"]["SyncDocument"];
export async function resolveDraftConflict(
  realMasterState: Document,
  newDocumentState: Document,
): Promise<Document> {
  const master: DraftVersion = JSON.parse(realMasterState.payload),
    fork: DraftVersion = JSON.parse(newDocumentState.payload);
  const winner = (fork.updated || 0) > (master.updated || 0) ? fork : master;
  const other = winner === fork ? master : fork;
  const replaced = other.id && (winner.seen?.[other.id] ?? -1) >= other.updated;
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
}
