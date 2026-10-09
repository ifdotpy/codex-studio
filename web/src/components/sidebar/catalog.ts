import type { Agent, PeerTeam } from "../../types";
import { retainArray } from "../../hooks/snapshotSelection";

export type SidebarOverrides = Record<
  string,
  { pinned?: boolean; archived?: boolean }
>;
export type SidebarOrder = Record<string, string[]>;
export type SidebarModelWork =
  | "catalog-row"
  | "order-compare"
  | "search-row"
  | "app-index";
declare global {
  interface Window {
    /** Optional counter for isolated browser performance checks. */
    __studioSidebarModelProbe?: (work: SidebarModelWork, count: number) => void;
  }
}
export function reportSidebarModelWork(work: SidebarModelWork, count = 1) {
  if (typeof window !== "undefined")
    window.__studioSidebarModelProbe?.(work, count);
}
export function isVisibleSidebarAgent(
  agent: Pick<Agent, "source" | "isLead" | "deletedAt" | "sharedRoomId">,
) {
  return (
    agent.source === "managed" &&
    agent.isLead &&
    !agent.deletedAt &&
    !agent.sharedRoomId
  );
}

/** Prefer latest explicit update; without update timestamps, prefer the oldest lead. */
export function defaultLead(leads: Agent[]) {
  const hasUpdates = leads.some((lead) => lead.updated != null);
  let selected: Agent | undefined;
  for (const lead of leads) {
    if (!selected) {
      selected = lead;
      continue;
    }
    const updated = hasUpdates ? (lead.updated ?? 0) : -(lead.created ?? 0);
    const selectedUpdated = hasUpdates
      ? (selected.updated ?? 0)
      : -(selected.created ?? 0);
    if (
      updated > selectedUpdated ||
      (updated === selectedUpdated && lead.id < selected.id)
    )
      selected = lead;
  }
  return selected;
}

export function countProjectChats(
  agents: Pick<Agent, "cwd" | "archived">[],
  archived: boolean,
) {
  const counts = new Map<string, number>();
  for (const agent of agents)
    if (!!agent.archived === archived) {
      const path = agent.cwd || "";
      counts.set(path, (counts.get(path) || 0) + 1);
    }
  return counts;
}
export function itemGroup(path: string, parent: string | null = null) {
  return JSON.stringify(["items", path, parent]);
}
function teamMap(teams: PeerTeam[]) {
  const result = new Map<string, PeerTeam>();
  for (const team of teams)
    for (const id of team.members ?? [])
      if (!result.has(id)) result.set(id, team);
  return result;
}
export function legacyChatGroup(agent: Agent, team?: PeerTeam) {
  return JSON.stringify([
    "chats",
    agent.cwd,
    !!agent.pinned,
    !!agent.archived,
    ...(team ? ["team", team.id] : []),
    ...(agent.projectFolder ? [agent.projectFolder] : []),
  ]);
}
export function chatGroup(agent: Agent, team?: PeerTeam) {
  return !team && !agent.pinned
    ? itemGroup(agent.cwd || "", agent.projectFolder || null)
    : legacyChatGroup(agent, team);
}
function orderingChanged(a: Agent, b: Agent) {
  return (
    a.pinned !== b.pinned ||
    a.archived !== b.archived ||
    a.cwd !== b.cwd ||
    a.projectFolder !== b.projectFolder ||
    "updated" in a !== "updated" in b ||
    a.updated !== b.updated ||
    a.created !== b.created
  );
}

// Each snapshot still needs one identity pass. Hidden worker updates do not
// rebuild the visible catalog. Changed chats keep the existing sorted ID list.
export function createSidebarCatalogSelector() {
  let source: Agent[] | undefined;
  let overrides: SidebarOverrides | undefined;
  let teams: PeerTeam[] | undefined;
  let order: SidebarOrder | undefined;
  let byTeam = new Map<string, PeerTeam>();
  let byId = new Map<string, Agent>();
  let sourceById = new Map<string, Agent>();
  let ranks = new Map<string, Map<string, number>>();
  let agents: Agent[] = [];
  let ordered: Agent[] = [];
  let ids: string[] = [];
  let byProject = new Map<string, Agent[]>();
  let grouped = new Set<string>();
  return (
    next: Agent[],
    optimistic: SidebarOverrides,
    nextTeams: PeerTeam[],
    nextOrder: SidebarOrder,
  ) => {
    if (
      source === next &&
      overrides === optimistic &&
      teams === nextTeams &&
      order === nextOrder
    )
      return { agents, ordered, byId, byProject, byTeam, grouped };
    const fullSort = teams !== nextTeams || order !== nextOrder;
    if (order !== nextOrder) {
      ranks = new Map(
        Object.entries(nextOrder).map(([group, values]) => {
          const positions = new Map<string, number>();
          values.forEach((id, index) => {
            if (!positions.has(id)) positions.set(id, index);
          });
          return [group, positions];
        }),
      );
    }
    if (teams !== nextTeams) byTeam = teamMap(nextTeams);
    const nextById = new Map<string, Agent>();
    const nextSourceById = new Map<string, Agent>();
    const changedOrder = new Set<string>();
    if (source !== next || overrides !== optimistic) {
      reportSidebarModelWork("catalog-row", next.length);
      for (const row of next) {
        if (!isVisibleSidebarAgent(row)) continue;
        nextSourceById.set(row.id, row);
        const old = byId.get(row.id);
        const patch = optimistic[row.id];
        const agent = patch ? { ...row, ...patch } : row;
        // Reuse an optimistic row while the same server row and override stay active.
        const value =
          patch &&
          old &&
          sourceById.get(row.id) === row &&
          overrides?.[row.id] === patch
            ? old
            : agent;
        nextById.set(row.id, value);
        if (!old || orderingChanged(old, value)) changedOrder.add(row.id);
      }
    } else
      for (const [id, row] of byId) {
        nextById.set(id, row);
        nextSourceById.set(id, sourceById.get(id)!);
      }
    const nextAgents = retainArray(agents, [...nextById.values()]);
    if (!fullSort && nextAgents === agents) {
      source = next;
      sourceById = nextSourceById;
      overrides = optimistic;
      return { agents, ordered, byId, byProject, byTeam, grouped };
    }
    const rank = (group: string, id: string) =>
      ranks.get(group)?.get(id) ?? Number.MAX_SAFE_INTEGER;
    const chatRank = (agent: Agent) => {
      const team = byTeam.get(agent.id);
      const value = rank(chatGroup(agent, team), agent.id);
      return value === Number.MAX_SAFE_INTEGER
        ? rank(legacyChatGroup(agent, team), agent.id)
        : value;
    };
    const positions = new Map(
      [...nextById.keys()].map((id, index) => [id, index]),
    );
    const compare = (left: string, right: string) => {
      reportSidebarModelWork("order-compare");
      const a = nextById.get(left)!,
        b = nextById.get(right)!;
      return (
        Number(!!b.pinned) - Number(!!a.pinned) ||
        chatRank(a) - chatRank(b) ||
        (("updated" in b ? b.updated : b.created) || 0) -
          (("updated" in a ? a.updated : a.created) || 0) ||
        positions.get(left)! - positions.get(right)!
      );
    };
    // The source order breaks equal-rank/time ties. Reorder equal keys if the
    // projection changes its source order, even when all row objects survive.
    const surviving = [...nextById.keys()];
    const oldSurviving = agents
      .filter((row) => nextById.has(row.id))
      .map((row) => row.id);
    const retainedSource = surviving.filter((value) => byId.has(value));
    const sourceReordered = oldSurviving.some(
      (id, index) => id !== retainedSource[index],
    );
    // Bound bulk inserts and timestamp changes. Repeated array insertion is
    // useful for small deltas, but can copy a quadratic number of references.
    const bulkChange = changedOrder.size > Math.max(32, nextById.size / 4);
    if (fullSort || sourceReordered || bulkChange)
      ids = [...nextById.keys()].sort(compare);
    else {
      ids = ids.filter((id) => nextById.has(id) && !changedOrder.has(id));
      for (const id of changedOrder) {
        let low = 0,
          high = ids.length;
        while (low < high) {
          const mid = (low + high) >>> 1;
          if (compare(ids[mid], id) <= 0) low = mid + 1;
          else high = mid;
        }
        ids.splice(low, 0, id);
      }
    }
    agents = nextAgents;
    ordered = retainArray(
      ordered,
      ids.map((id) => nextById.get(id)!),
    );
    const projects = new Map<string, Agent[]>();
    for (const agent of ordered) {
      const path = agent.cwd || "";
      const rows = projects.get(path);
      if (rows) rows.push(agent);
      else projects.set(path, [agent]);
    }
    for (const [path, rows] of projects)
      projects.set(path, retainArray(byProject.get(path) || [], rows));
    byProject = projects;
    const groupedIds = new Set<string>();
    for (const agent of agents)
      if (byTeam.get(agent.id)?.projectPath === agent.cwd)
        groupedIds.add(agent.id);
    if (
      groupedIds.size !== grouped.size ||
      [...groupedIds].some((id) => !grouped.has(id))
    )
      grouped = groupedIds;
    sourceById = nextSourceById;
    source = next;
    overrides = optimistic;
    teams = nextTeams;
    order = nextOrder;
    byId = nextById;
    return { agents, ordered, byId, byProject, byTeam, grouped };
  };
}

export function createAppCatalogSelector() {
  let source: Agent[] | undefined;
  let byId = new Map<string, Agent>();
  let leads: Agent[] = [];
  let version = 0;
  const selected = new Map<
    string,
    { version: number; team: Agent[]; workers: Agent[] }
  >();
  const teamFor = (root: string) => {
    let cached = selected.get(root);
    if (cached?.version !== version) {
      const team = retainArray(
        cached?.team || [],
        (source || []).filter((row) => row.id === root || row.rootId === root),
      );
      const workers =
        team === cached?.team
          ? cached.workers
          : retainArray(
              cached?.workers || [],
              team.filter((row) => !row.isLead),
            );
      cached = { version, team, workers };
      selected.set(root, cached);
    }
    return cached!;
  };
  return (next: Agent[]) => {
    if (source !== next) {
      version++;
      reportSidebarModelWork("app-index", next.length);
      const nextById = new Map<string, Agent>();
      const nextLeads: Agent[] = [];
      for (const row of next) {
        nextById.set(row.id, row);
        if (row.source === "managed" && row.isLead && !row.sharedRoomId)
          nextLeads.push(row);
      }
      byId = nextById;
      leads = retainArray(leads, nextLeads);
      source = next;
      for (const id of selected.keys()) if (!byId.has(id)) selected.delete(id);
    }
    return {
      byId,
      leads,
      team: (root: string) => teamFor(root).team,
      workers: (root: string) => teamFor(root).workers,
    };
  };
}

export function createSidebarSearchSelector() {
  const labels = new WeakMap<Agent, { context: object; label: string }>();
  return (
    rows: Agent[],
    context: object,
    query: string,
    labelFor: (row: Agent) => string,
  ) => {
    if (!query) return rows;
    const needle = query.toLowerCase();
    return rows.filter((row) => {
      let cached = labels.get(row);
      if (!cached || cached.context !== context) {
        reportSidebarModelWork("search-row");
        cached = { context, label: labelFor(row).toLowerCase() };
        labels.set(row, cached);
      }
      return cached.label.includes(needle);
    });
  };
}
