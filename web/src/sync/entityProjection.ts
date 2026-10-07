import type { Snapshot } from "../types";

export type EntityRow = {
  id: string;
  payload: string;
  seq: number;
  _deleted?: boolean;
};

type EntityValue = { collection: string; id: string; value: any };
export type EntityProjection = {
  rows: Map<
    string,
    { seq: number; collection: string; entityId: string; deleted?: boolean }
  >;
  values: Map<string, Map<string, any>>;
  snapshot: Snapshot | null;
};

export function emptyEntityProjection(): EntityProjection {
  return { rows: new Map(), values: new Map(), snapshot: null };
}

/** Apply only rows whose server sequence changed. Unchanged entity values and
 * collection arrays keep their references for memoized React consumers. */
export function applyEntityRows(
  state: EntityProjection,
  rows: EntityRow[],
  ready: boolean,
): Snapshot | null {
  const changed = new Set<string>();
  const seen = new Set<string>();
  // Stryker disable next-line BooleanLiteral: when no tombstone is added, prior applies preserve the 4096-tombstone cap, so a cleanup scan cannot prune a row.
  let addedTombstone = false;
  for (const row of rows) {
    if (!row.id.startsWith("entity:")) continue;
    seen.add(row.id);
    const previous = state.rows.get(row.id);
    if (previous && previous.seq >= row.seq) continue;
    const oldCollection = previous?.collection;
    const oldEntityId = previous?.entityId;
    if (row._deleted) {
      if (oldCollection) {
        state.values.get(oldCollection)!.delete(oldEntityId!);
        changed.add(oldCollection);
      }
      state.rows.delete(row.id);
      state.rows.set(row.id, {
        seq: row.seq,
        collection: oldCollection || "",
        entityId: oldEntityId || "",
        deleted: true,
      });
      addedTombstone = true;
      continue;
    }
    let parsed: EntityValue;
    try {
      parsed = JSON.parse(row.payload);
    } catch {
      continue;
    }
    if (typeof parsed.collection !== "string" || typeof parsed.id !== "string")
      continue;
    if (
      oldCollection &&
      (oldCollection !== parsed.collection || oldEntityId !== parsed.id)
    ) {
      state.values.get(oldCollection)!.delete(oldEntityId!);
      changed.add(oldCollection);
    }
    let collection = state.values.get(parsed.collection);
    if (!collection)
      state.values.set(parsed.collection, (collection = new Map()));
    collection.set(parsed.id, parsed.value);
    state.rows.set(row.id, {
      seq: row.seq,
      collection: parsed.collection,
      entityId: parsed.id,
    });
    changed.add(parsed.collection);
  }
  // RxDB find queries omit tombstones. Missing IDs therefore represent deletes.
  const missing: Array<
    [
      string,
      { seq: number; collection: string; entityId: string; deleted?: boolean },
    ]
  > = [];
  for (const [rowId, previous] of state.rows) {
    if (seen.has(rowId)) continue;
    if (previous.deleted) continue;
    missing.push([rowId, previous]);
  }
  for (const [rowId, previous] of missing) {
    state.values.get(previous.collection)!.delete(previous.entityId);
    changed.add(previous.collection);
    state.rows.delete(rowId);
    state.rows.set(rowId, { ...previous, deleted: true });
    addedTombstone = true;
  }
  // Stryker disable next-line ConditionalExpression: without a new tombstone, the API invariant makes this cleanup scan a no-op.
  if (addedTombstone) {
    let tombstones = 0;
    for (const [rowId, row] of state.rows) {
      if (!row.deleted) continue;
      tombstones++;
      if (tombstones > 4096) state.rows.delete(rowId);
    }
  }
  if (!ready) return null;
  const prior = state.snapshot;
  const list = (name: string) =>
    prior && !changed.has(name)
      ? (prior.runtime as any)[`${name}s`] || []
      : [...(state.values.get(name)?.values() || [])];
  const agents = list("agent");
  const chats = prior && !changed.has("chat") ? prior.chats : list("chat");
  const runtime: Record<string, any> = prior ? { ...prior.runtime } : {};
  const keys: Record<string, string> = {
    agent: "agents",
    room: "rooms",
    task: "tasks",
    monitor: "monitors",
    complaint: "complaints",
    request: "requests",
    rule: "rules",
    project: "projects",
    peerTeam: "peerTeams",
    event: "events",
    work: "work",
  };
  for (const [collection, key] of Object.entries(keys)) {
    if (changed.has(collection) || !prior) {
      runtime[key] = collection === "agent" ? agents : list(collection);
    }
  }
  const workspace = state.values.get("workspace")?.get("current") || {};
  if (changed.has("workspace") || !prior) {
    Object.assign(runtime, workspace);
  }
  const next: Snapshot =
    prior && !changed.size
      ? prior
      : ({
          token: "",
          stateDir: workspace.stateDir || "",
          threads: agents,
          chats,
          nodes:
            prior && !changed.has("agent") && !changed.has("chat")
              ? prior.nodes
              : [...agents, ...chats],
          edges:
            changed.has("edge") || !prior
              ? [...(state.values.get("edge")?.values() || [])]
              : (prior as any).edges,
          runtime,
        } as unknown as Snapshot);
  state.snapshot = next;
  return next;
}
