import { roomLeadIds } from "../chatScope";
import type { Snapshot } from "../types";

export function retainArray<T>(previous: T[], next: T[]): T[] {
  return previous.length === next.length &&
    previous.every((value, index) => value === next[index])
    ? previous
    : next;
}

function sameFields<T extends object>(previous: T, next: T): boolean {
  const keys = Object.keys(next) as Array<keyof T>;
  return (
    Object.keys(previous).length === keys.length &&
    keys.every((key) => previous[key] === next[key])
  );
}

function arraySelection<T>() {
  let input: T[] | undefined;
  let scope: object | undefined;
  let selected: T[] = [];
  return (source: T[], key: object, include: (row: T) => boolean) => {
    if (input === source && scope === key) return selected;
    input = source;
    scope = key;
    selected = retainArray(selected, source.filter(include));
    return selected;
  };
}

// Keep each collection's identity until a member changes. Global projections
// can publish another tree without invalidating this conversation's consumers.
export function createChatSnapshotSelector(includeUnassignedRequests = false) {
  let previous: Snapshot | null = null;
  let root: string | undefined;
  let ids = new Set<string>();
  const members = arraySelection<Snapshot["threads"][number]>();
  const agents = arraySelection<Snapshot["threads"][number]>();
  const rooms = arraySelection<Snapshot["runtime"]["rooms"][number]>();
  const requests = arraySelection<Snapshot["runtime"]["requests"][number]>();
  const complaints =
    arraySelection<Snapshot["runtime"]["complaints"][number]>();
  const monitors = arraySelection<Snapshot["runtime"]["monitors"][number]>();
  const tasks = arraySelection<Snapshot["runtime"]["tasks"][number]>();
  const work = arraySelection<Snapshot["runtime"]["work"][number]>();
  const rules = arraySelection<Snapshot["runtime"]["rules"][number]>();
  const events = arraySelection<Snapshot["runtime"]["events"][number]>();
  const edges = arraySelection<Snapshot["edges"][number]>();
  const emptyChats: Snapshot["chats"] = [];
  let rootKey = {};
  return (data: Snapshot | null, rootId?: string): Snapshot | null => {
    if (!data) {
      previous = null;
      return null;
    }
    if (root !== rootId || previous?.stateDir !== data.stateDir) {
      root = rootId;
      rootKey = {};
      ids = new Set();
      previous = null;
    }
    const threads = members(
      data.threads,
      rootKey,
      (agent) => !!rootId && (agent.id === rootId || agent.rootId === rootId),
    );
    if (
      threads.length !== ids.size ||
      threads.some((agent) => !ids.has(agent.id))
    )
      ids = new Set(threads.map((agent) => agent.id));
    const owns = (id?: string | null) => !!id && ids.has(id);
    const source = data.runtime;
    const nextRuntime: Snapshot["runtime"] = {
      ...source,
      agents: agents(source.agents, ids, (agent) => owns(agent.id)),
      rooms: rooms(
        source.rooms,
        threads,
        (room) => !!rootId && roomLeadIds(room, data.threads).includes(rootId),
      ),
      requests: requests(
        source.requests,
        ids,
        (request) =>
          owns(request.agent) || (includeUnassignedRequests && !request.agent),
      ),
      complaints: complaints(
        source.complaints,
        ids,
        (complaint) => !!rootId && complaint.leadId === rootId,
      ),
      monitors: monitors(source.monitors, ids, (monitor) =>
        owns(monitor.agent),
      ),
      tasks: tasks(source.tasks, ids, (task) => owns(task.agent)),
      work: work(
        source.work,
        ids,
        (task) => !!rootId && task.rootId === rootId,
      ),
      rules: rules(source.rules, ids, (rule) => owns(rule.agent)),
      events: events(source.events, ids, (event) => owns(event.agent)),
    };
    const runtime =
      previous && sameFields(previous.runtime, nextRuntime)
        ? previous.runtime
        : nextRuntime;
    const next: Snapshot = {
      ...data,
      threads,
      chats: emptyChats,
      nodes: threads,
      edges: edges(
        data.edges,
        ids,
        (edge) => owns(edge.source) || owns(edge.target),
      ),
      runtime,
    };
    previous = previous && sameFields(previous, next) ? previous : next;
    return previous;
  };
}
