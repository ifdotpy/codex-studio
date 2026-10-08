import type {
  AgentEntityDto,
  ChatEntityDto,
  ComplaintEntityDto,
  EdgeEntityDto,
  EventEntityDto,
  MonitorEntityDto,
  PeerTeamEntityDto,
  ProjectEntityDto,
  RequestEntityDto,
  RoomEntityDto,
  RuleEntityDto,
  SyncEntityPayload,
  TaskEntityDto,
  WorkEntityDto,
  WorkspaceEntityDto,
} from "../generated/api";
import type { Snapshot } from "../types";

export type EntityRow = {
  id: string;
  payload: string;
  seq: number;
  _deleted?: boolean;
};

type EntityCollection = SyncEntityPayload["collection"];
const ENTITY_COLLECTIONS = new Set<string>([
  "agent",
  "room",
  "task",
  "monitor",
  "complaint",
  "request",
  "rule",
  "project",
  "peerTeam",
  "chat",
  "edge",
  "event",
  "work",
  "workspace",
]);
type EntityFor<C extends EntityCollection> = Extract<
  SyncEntityPayload,
  { collection: C }
>["value"];
type EntityValues = { [C in EntityCollection]: Map<string, EntityFor<C>> };
type EntityRowState = {
  seq: number;
  collection?: EntityCollection;
  entityId?: string;
  deleted?: boolean;
};

export type EntityProjection = {
  rows: Map<string, EntityRowState>;
  invalidReportedSeq: Map<string, number>;
  tombstones: Set<string>;
  changed: Set<EntityCollection>;
  values: EntityValues;
  snapshot: Snapshot | null;
};

function emptyEntityValues(): EntityValues {
  return {
    agent: new Map<string, AgentEntityDto>(),
    room: new Map<string, RoomEntityDto>(),
    task: new Map<string, TaskEntityDto>(),
    monitor: new Map<string, MonitorEntityDto>(),
    complaint: new Map<string, ComplaintEntityDto>(),
    request: new Map<string, RequestEntityDto>(),
    rule: new Map<string, RuleEntityDto>(),
    project: new Map<string, ProjectEntityDto>(),
    peerTeam: new Map<string, PeerTeamEntityDto>(),
    chat: new Map<string, ChatEntityDto>(),
    edge: new Map<string, EdgeEntityDto>(),
    event: new Map<string, EventEntityDto>(),
    work: new Map<string, WorkEntityDto>(),
    workspace: new Map<string, WorkspaceEntityDto>(),
  };
}

/**
 * The schema-versioned cache contains payloads from this renderer contract.
 * Shape validation remains necessary because a mismatched server response or
 * storage corruption can still damage a row before it reaches the cache.
 */
function parseEntityPayload(payload: string): SyncEntityPayload | null {
  let parsed: unknown;
  try {
    parsed = JSON.parse(payload);
  } catch {
    return null;
  }
  if (parsed === null || typeof parsed !== "object" || Array.isArray(parsed))
    return null;
  const candidate = parsed as Record<string, unknown>;
  if (
    typeof candidate.collection !== "string" ||
    !ENTITY_COLLECTIONS.has(candidate.collection) ||
    typeof candidate.id !== "string" ||
    candidate.value === null ||
    typeof candidate.value !== "object" ||
    Array.isArray(candidate.value)
  )
    return null;
  return candidate as unknown as SyncEntityPayload;
}

function entityValues<C extends EntityCollection>(
  values: EntityValues,
  collection: C,
): EntityFor<C>[] {
  return [...values[collection].values()];
}

function projectSnapshotCollections(
  prior: Snapshot | null,
  changed: ReadonlySet<EntityCollection>,
  values: EntityValues,
): Snapshot {
  const agents =
    prior && !changed.has("agent")
      ? prior.runtime.agents
      : entityValues(values, "agent");
  const chats =
    prior && !changed.has("chat") ? prior.chats : entityValues(values, "chat");
  const edges =
    prior && !changed.has("edge") ? prior.edges : entityValues(values, "edge");
  const rooms =
    prior && !changed.has("room")
      ? prior.runtime.rooms
      : entityValues(values, "room");
  const tasks =
    prior && !changed.has("task")
      ? prior.runtime.tasks
      : entityValues(values, "task");
  const monitors =
    prior && !changed.has("monitor")
      ? prior.runtime.monitors
      : entityValues(values, "monitor");
  const complaints =
    prior && !changed.has("complaint")
      ? prior.runtime.complaints
      : entityValues(values, "complaint");
  const requests =
    prior && !changed.has("request")
      ? prior.runtime.requests
      : entityValues(values, "request");
  const rules =
    prior && !changed.has("rule")
      ? prior.runtime.rules
      : entityValues(values, "rule");
  const projects =
    prior && !changed.has("project")
      ? prior.runtime.projects
      : entityValues(values, "project");
  const peerTeams =
    prior && !changed.has("peerTeam")
      ? prior.runtime.peerTeams
      : entityValues(values, "peerTeam");
  const events =
    prior && !changed.has("event")
      ? prior.runtime.events
      : entityValues(values, "event");
  const work =
    prior && !changed.has("work")
      ? prior.runtime.work
      : entityValues(values, "work");
  const workspace = values.workspace.get("current");
  const runtime = {
    ...workspace,
    agents,
    rooms,
    tasks,
    monitors,
    complaints,
    requests,
    rules,
    projects,
    peerTeams,
    events,
    work,
  };

  return {
    ...prior,
    token: "",
    stateDir: workspace?.stateDir ?? prior?.stateDir ?? "",
    threads: agents,
    chats,
    nodes:
      prior && !changed.has("agent") && !changed.has("chat")
        ? prior.nodes
        : [...agents, ...chats],
    edges,
    runtime,
  };
}

export function emptyEntityProjection(): EntityProjection {
  return {
    rows: new Map(),
    invalidReportedSeq: new Map(),
    tombstones: new Set(),
    changed: new Set(),
    values: emptyEntityValues(),
    snapshot: null,
  };
}

function retainTombstone(
  state: EntityProjection,
  rowId: string,
  row: EntityRowState,
) {
  state.rows.delete(rowId);
  state.rows.set(rowId, row);
  state.tombstones.delete(rowId);
  state.tombstones.add(rowId);
  // Match full-query retention without scanning the live rows after each delete.
  if (state.tombstones.size > 4096) {
    state.tombstones.delete(rowId);
    state.rows.delete(rowId);
  }
}

/** Apply explicit changes by ID. Absent rows remain present until a tombstone
 * arrives. Passing ready=false retains changes for one later publication. */
export function applyEntityChanges(
  state: EntityProjection,
  rows: Iterable<EntityRow>,
  ready: boolean,
): Snapshot | null {
  const changed = state.changed;
  for (const row of rows) {
    if (!row.id.startsWith("entity:")) continue;
    const previous = state.rows.get(row.id);
    if (
      previous &&
      (previous.seq > row.seq ||
        (previous.seq === row.seq && (!row._deleted || previous.deleted)))
    )
      continue;
    const oldCollection = previous?.collection;
    const oldEntityId = previous?.entityId;
    if (row._deleted) {
      state.invalidReportedSeq.delete(row.id);
      if (oldCollection && oldEntityId !== undefined) {
        state.values[oldCollection].delete(oldEntityId);
        changed.add(oldCollection);
      }
      retainTombstone(state, row.id, {
        seq: row.seq,
        ...(oldCollection ? { collection: oldCollection } : {}),
        ...(oldEntityId === undefined ? {} : { entityId: oldEntityId }),
        deleted: true,
      });
      continue;
    }
    const payload = parseEntityPayload(row.payload);
    if (!payload) {
      if (state.invalidReportedSeq.get(row.id) !== row.seq) {
        state.invalidReportedSeq.set(row.id, row.seq);
        console.error(
          `Skipping sync entity row with invalid payload for document "${row.id}"`,
          { id: row.id, seq: row.seq },
        );
      }
      continue;
    }
    state.invalidReportedSeq.delete(row.id);
    if (
      oldCollection &&
      oldEntityId !== undefined &&
      (oldCollection !== payload.collection || oldEntityId !== payload.id)
    ) {
      state.values[oldCollection].delete(oldEntityId);
      changed.add(oldCollection);
    }
    switch (payload.collection) {
      case "agent":
        state.values.agent.set(payload.id, payload.value);
        break;
      case "room":
        state.values.room.set(payload.id, payload.value);
        break;
      case "task":
        state.values.task.set(payload.id, payload.value);
        break;
      case "monitor":
        state.values.monitor.set(payload.id, payload.value);
        break;
      case "complaint":
        state.values.complaint.set(payload.id, payload.value);
        break;
      case "request":
        state.values.request.set(payload.id, payload.value);
        break;
      case "rule":
        state.values.rule.set(payload.id, payload.value);
        break;
      case "project":
        state.values.project.set(payload.id, payload.value);
        break;
      case "peerTeam":
        state.values.peerTeam.set(payload.id, payload.value);
        break;
      case "chat":
        state.values.chat.set(payload.id, payload.value);
        break;
      case "edge":
        state.values.edge.set(payload.id, payload.value);
        break;
      case "event":
        state.values.event.set(payload.id, payload.value);
        break;
      case "work":
        state.values.work.set(payload.id, payload.value);
        break;
      case "workspace":
        state.values.workspace.set(payload.id, payload.value);
        break;
      default: {
        const exhaustive: never = payload;
        void exhaustive;
        continue;
      }
    }
    state.rows.set(row.id, {
      seq: row.seq,
      collection: payload.collection,
      entityId: payload.id,
    });
    state.tombstones.delete(row.id);
    changed.add(payload.collection);
  }
  if (!ready) return null;
  const prior = state.snapshot;
  if (prior && !changed.size) return prior;
  const next = projectSnapshotCollections(prior, changed, state.values);
  changed.clear();
  state.snapshot = next;
  return next;
}

/** Reconcile a complete RxDB query, including rows omitted after a delete. */
export function applyEntityRows(
  state: EntityProjection,
  rows: EntityRow[],
  ready: boolean,
): Snapshot | null {
  applyEntityChanges(state, rows, false);
  const seen = new Set(rows.map((row) => row.id));
  for (const rowId of state.invalidReportedSeq.keys()) {
    if (!seen.has(rowId)) state.invalidReportedSeq.delete(rowId);
  }
  // RxDB find queries omit tombstones. Missing IDs therefore represent deletes.
  const missing: Array<[string, EntityRowState]> = [];
  for (const [rowId, previous] of state.rows) {
    if (seen.has(rowId)) continue;
    if (previous.deleted) continue;
    missing.push([rowId, previous]);
  }
  for (const [rowId, previous] of missing) {
    state.invalidReportedSeq.delete(rowId);
    if (previous.collection && previous.entityId !== undefined) {
      state.values[previous.collection].delete(previous.entityId);
      state.changed.add(previous.collection);
    }
    retainTombstone(state, rowId, { ...previous, deleted: true });
  }
  return applyEntityChanges(state, [], ready);
}
