import { decodeSidebarRanks } from "./sidebarOrderRanks";
import type { Agent, PeerTeam, Project, Room, Snapshot } from "../types";
import type { ChatIndicator } from "../components/chat-status/chatStatusModel";
import { sidebarIdentity } from "../components/sidebar/services";
import type { ServerSidebarSnapshot } from "./sidebarSnapshot";

export type SidebarSource = {
  id: string;
  online: boolean;
  sidebar: ServerSidebarSnapshot;
};
export type SidebarReference = { owner: string; id: string; path?: string };
type ProjectMember = { source: SidebarSource; project: Project };
export type SidebarProjectGroup = {
  key: string;
  owner: string;
  id: string;
  home: ProjectMember;
  members: ProjectMember[];
};
export type SidebarOrder = Record<string, string[]>;

/** Render identities are separate from every server's stored and wire IDs. */
export function mergeSidebar(
  sources: readonly SidebarSource[],
  revision = 0,
  combined = sources.length > 1,
) {
  const identities = new Set(sources.map((source) => source.id));
  if (identities.size !== sources.length)
    throw new Error("Duplicate sidebar source identity.");
  const single = !combined && sources.length === 1;
  const sourceById = new Map(sources.map((source) => [source.id, source]));
  const chatReferences = new Map<string, SidebarReference>();
  const projectReferences = new Map<string, SidebarReference>();
  const folderReferences = new Map<string, SidebarReference>();
  const teamReferences = new Map<string, SidebarReference>();
  const projectKeys = new Map<string, string>();
  const folderKeys = new Map<string, string>();
  const groups = new Map<string, SidebarProjectGroup>();
  const uiServers = new Map<string, string>();
  for (const source of sources)
    for (const project of source.sidebar.projects)
      if (project.homeServerId && !project.projectAliases?.length)
        uiServers.set(project.homeServerId, source.id);
  const key = (owner: string, id: string) =>
    single ? id : JSON.stringify(["project", owner, id]);
  const chatKey = (owner: string, id: string) => {
    const result = single ? id : JSON.stringify(["chat", owner, id]);
    if (!chatReferences.has(result)) chatReferences.set(result, { owner, id });
    return result;
  };
  const teamKey = (owner: string, id: string) => {
    const result = single ? id : JSON.stringify(["team", owner, id]);
    teamReferences.set(result, { owner, id });
    return result;
  };
  // Resolve the home before projecting any alias, independent of arrival order.
  for (const source of sources)
    for (const project of source.sidebar.projects) {
      const alias = project.projectAliases?.[0];
      const owner = alias
        ? uiServers.get(alias.serverId) || alias.serverId
        : source.id;
      const id = alias?.projectId || project.id || project.path || "";
      const renderKey = single ? project.path || id : key(owner, id);
      projectKeys.set(
        sidebarIdentity(source.id, project.path || ""),
        renderKey,
      );
      projectKeys.set(sidebarIdentity(source.id, project.id || ""), renderKey);
      const member = { source, project };
      if (!groups.has(renderKey))
        groups.set(renderKey, {
          key: renderKey,
          owner,
          id,
          home: member,
          members: [],
        });
      const group = groups.get(renderKey)!;
      group.members.push(member);
      if (source.id === owner && !alias) group.home = member;
    }
  for (const group of groups.values())
    for (const member of group.members)
      for (const location of member.project.locations || []) {
        const owner = uiServers.get(location.serverId) || location.serverId;
        projectKeys.set(sidebarIdentity(owner, location.path), group.key);
        projectKeys.set(sidebarIdentity(owner, location.projectId), group.key);
      }
  const projectKey = (owner: string, path: string) => {
    const existing = projectKeys.get(sidebarIdentity(owner, path));
    if (existing !== undefined) return existing;
    const renderKey = key(owner, path);
    projectKeys.set(sidebarIdentity(owner, path), renderKey);
    projectReferences.set(renderKey, { owner, id: path, path });
    return renderKey;
  };
  const folderKey = (owner: string, path: string, id: string): string => {
    path =
      groups
        .get(projectKey(owner, path))
        ?.members.find((member) => member.source.id === owner)?.project.path ||
      path;
    const index = JSON.stringify([owner, path, id]);
    const existing = folderKeys.get(index);
    if (existing !== undefined) return existing;
    const renderKey = single ? id : JSON.stringify(["folder", owner, path, id]);
    folderKeys.set(index, renderKey);
    folderReferences.set(renderKey, { owner, id, path });
    return renderKey;
  };
  const projects: Project[] = [];
  for (const group of groups.values()) {
    const home = group.home;
    projectReferences.set(group.key, {
      owner: single ? home.source.id : group.owner,
      id: group.id,
      path:
        home.source.id === group.owner || single
          ? home.project.path || home.project.id || ""
          : group.id,
    });
    group.members.sort((a, b) => Number(b === home) - Number(a === home));
    const folders: NonNullable<Project["folders"]> = [];
    const homeFolders = home.project.folders || [];
    const compatibleFolder = (
      member: ProjectMember,
      id: string,
      seen = new Set<string>(),
    ): boolean => {
      if (member === home) return true;
      if (seen.has(id)) return false;
      seen.add(id);
      const folder = member.project.folders?.find((item) => item.id === id);
      const candidate = homeFolders.find((item) => item.id === id);
      return (
        !!folder &&
        !!candidate &&
        candidate.name === folder.name &&
        (candidate.parentId || null) === (folder.parentId || null) &&
        (!folder.parentId || compatibleFolder(member, folder.parentId, seen))
      );
    };
    for (const member of group.members)
      for (const folder of member.project.folders || []) {
        const compatible = compatibleFolder(member, folder.id);
        const origin = compatible ? home : member;
        const id = folderKey(
          origin.source.id,
          origin.project.path || "",
          folder.id,
        );
        folderKeys.set(
          JSON.stringify([
            member.source.id,
            member.project.path || "",
            folder.id,
          ]),
          id,
        );
        if (!folders.some((row) => row.id === id))
          folders.push({
            ...folder,
            id,
            parentId: folder.parentId
              ? folderKey(
                  origin.source.id,
                  origin.project.path || "",
                  folder.parentId,
                )
              : null,
          });
      }
    projects.push(
      single
        ? home.project
        : {
            ...home.project,
            id: group.key,
            path: group.key,
            name: home.project.name || home.project.path || "Other chats",
            folders,
            peerTeams: [],
          },
    );
  }
  const projectPaths = new Set(projects.map((project) => project.path));
  const boundProject = (source: SidebarSource, chat: Agent) => {
    if (chat.projectId && chat.projectServerId) {
      const home = uiServers.get(chat.projectServerId) || chat.projectServerId;
      const binding = key(home, chat.projectId);
      if (groups.has(binding)) return binding;
    }
    return projectKey(source.id, chat.cwd || "");
  };
  const threads: Agent[] = [];
  const peerTeams: PeerTeam[] = [];
  const rooms: Room[] = [];
  const indicators = new Map<string, ChatIndicator>();
  const unread = new Set<string>();
  for (const source of sources) {
    for (const chat of source.sidebar.threads) {
      const id = chatKey(source.id, chat.id);
      const path = boundProject(source, chat);
      chatReferences.set(id, {
        owner: source.id,
        id: chat.id,
        path: chat.cwd || "",
      });
      if (!single && !projectPaths.has(path)) {
        projectPaths.add(path);
        projects.push({
          id: path,
          path,
          name: chat.cwd || "Other chats",
          folders: [],
        });
      }
      threads.push(
        single
          ? chat
          : {
              ...chat,
              id,
              cwd: path,
              serverId: source.id,
              rootId: chat.rootId
                ? chatKey(source.id, chat.rootId)
                : chat.rootId,
              parentId: chat.parentId
                ? chatKey(source.id, chat.parentId)
                : chat.parentId,
              sharedRoomId: chat.sharedRoomId
                ? chatKey(source.id, chat.sharedRoomId)
                : chat.sharedRoomId,
              projectFolder: chat.projectFolder
                ? folderKey(source.id, chat.cwd || "", chat.projectFolder)
                : chat.projectFolder,
            },
      );
    }
    const teams = new Map<string, PeerTeam>();
    for (const team of source.sidebar.peerTeams) teams.set(team.id, team);
    for (const project of source.sidebar.projects)
      for (const team of project.peerTeams || [])
        if (!teams.has(team.id))
          teams.set(team.id, { ...team, projectPath: project.path });
    for (const team of teams.values())
      peerTeams.push(
        single
          ? team
          : {
              ...team,
              id: teamKey(source.id, team.id),
              projectPath: projectKey(source.id, team.projectPath || ""),
              members: team.members?.map((id) => chatKey(source.id, id)),
            },
      );
    for (const room of source.sidebar.rooms)
      rooms.push(
        single
          ? room
          : {
              ...room,
              id: chatKey(source.id, room.id),
              rootId: room.rootId
                ? chatKey(source.id, room.rootId)
                : room.rootId,
              members: room.members?.map((id) => chatKey(source.id, id)),
              projectPath: projectKey(source.id, room.projectPath || ""),
              peerTeamId: room.peerTeamId
                ? teamKey(source.id, room.peerTeamId)
                : room.peerTeamId,
              radio: room.radio
                ? {
                    ...room.radio,
                    teamId: room.radio.teamId
                      ? teamKey(source.id, room.radio.teamId)
                      : room.radio.teamId,
                  }
                : room.radio,
            },
      );
    for (const [id, indicator] of source.sidebar.indicators)
      indicators.set(chatKey(source.id, id), indicator);
    for (const id of source.sidebar.unread) unread.add(chatKey(source.id, id));
  }
  // Single-server mode preserves every identity, including rooms and teams.
  if (single) {
    for (const team of peerTeams) teamKey(sources[0].id, team.id);
    for (const room of rooms) chatKey(sources[0].id, room.id);
  } else {
    const teamsByPath = new Map<string, PeerTeam[]>();
    for (const team of peerTeams) {
      const path = team.projectPath || "";
      const rows = teamsByPath.get(path) || [];
      rows.push(team);
      teamsByPath.set(path, rows);
    }
    for (const project of projects)
      project.peerTeams = (teamsByPath.get(project.path || "") || []).map(
        (team) => ({
          id: team.id,
          name: team.name || "Team",
          members: team.members || [],
        }),
      );
  }
  const mapTreeKey = (owner: string, value: string) => {
    if (!value.startsWith("[")) return projectKey(owner, value);
    let parts: unknown;
    try {
      parts = JSON.parse(value);
    } catch {
      return projectKey(owner, value);
    }
    if (!Array.isArray(parts) || typeof parts[0] !== "string")
      return projectKey(owner, value);
    if (parts[1] === "folder" && typeof parts[2] === "string")
      return JSON.stringify([
        projectKey(owner, parts[0]),
        "folder",
        folderKey(owner, parts[0], parts[2]),
      ]);
    if (parts[1] === "team" && typeof parts[2] === "string")
      return JSON.stringify([
        projectKey(owner, parts[0]),
        "team",
        teamKey(owner, parts[2]),
      ]);
    return projectKey(owner, value);
  };
  const mapOrderGroup = (owner: string, value: string) => {
    if (value === "projects") return value;
    let parts: unknown;
    try {
      parts = JSON.parse(value);
    } catch {
      return key(owner, value);
    }
    if (!Array.isArray(parts) || typeof parts[1] !== "string")
      return key(owner, value);
    const result = [...parts];
    result[1] = projectKey(owner, parts[1]);
    if (parts[0] === "items" && typeof parts[2] === "string")
      result[2] = folderKey(owner, parts[1], parts[2]);
    if (parts[0] === "chats")
      for (let index = 4; index < parts.length; index++) {
        if (parts[index] === "team" && typeof parts[index + 1] === "string") {
          result[++index] = teamKey(owner, parts[index]);
        } else if (typeof parts[index] === "string")
          result[index] = folderKey(owner, parts[1], parts[index]);
      }
    return JSON.stringify(result);
  };
  const mapOrderItem = (owner: string, group: string, value: string) => {
    if (group === "projects") return projectKey(owner, value);
    if (!value.startsWith("[")) return chatKey(owner, value);
    try {
      const parts = JSON.parse(value);
      if (Array.isArray(parts) && ["folder", "team"].includes(parts[1]))
        return mapTreeKey(owner, value);
    } catch {
      /* A chat ID need not be JSON. */
    }
    return chatKey(owner, value);
  };
  const mergeOrder = (orders: ReadonlyMap<string, SidebarOrder>) => {
    const merged: SidebarOrder = {};
    const overlays = new Map<string, string[]>();
    // Home source fragments precede remote fragments for shared projects.
    for (const source of sources)
      for (const [group, items] of Object.entries(
        orders.get(source.id) || {},
      )) {
        try {
          const parts = JSON.parse(group);
          if (
            Array.isArray(parts) &&
            ["combined-sidebar", "combined-sidebar-ranks"].includes(parts[0]) &&
            typeof parts[1] === "string"
          ) {
            if (!overlays.has(parts[1]))
              overlays.set(
                parts[1],
                parts[0] === "combined-sidebar-ranks"
                  ? decodeSidebarRanks(items)
                  : items,
              );
            continue;
          }
        } catch {
          /* Original order keys may be plain strings. */
        }
        const destination = mapOrderGroup(source.id, group);
        const mapped = items.map((item) =>
          mapOrderItem(source.id, group, item),
        );
        let home = sourceById.has("local") ? "local" : sources[0]?.id;
        if (destination !== "projects") {
          try {
            const parts = JSON.parse(destination);
            home = projectReferences.get(parts[1])?.owner || home;
          } catch {
            /* Preserve unknown saved groups. */
          }
        }
        merged[destination] = [
          ...new Set(
            source.id === home
              ? [...mapped, ...(merged[destination] || [])]
              : [...(merged[destination] || []), ...mapped],
          ),
        ];
      }
    for (const [group, items] of overlays)
      merged[group] = [...new Set([...items, ...(merged[group] || [])])];
    return merged;
  };
  const order = mergeOrder(
    new Map(
      sources.map((source) => [
        source.id,
        source.sidebar.sidebarOrder?.groups || source.sidebar.savedOrder,
      ]),
    ),
  );
  const preferenceKeys = new Map<
    string,
    { compact: Set<string>; collapsed: Set<string> }
  >();
  for (const source of sources) {
    const compact = new Set(Object.keys(source.sidebar.compact));
    const collapsed = new Set(Object.keys(source.sidebar.collapsed));
    const addPath = (path: string) => {
      compact.add(path);
      collapsed.add(path);
    };
    for (const project of source.sidebar.projects) {
      const path = project.path || "";
      addPath(path);
      for (const folder of project.folders || [])
        collapsed.add(JSON.stringify([path, "folder", folder.id]));
      for (const team of project.peerTeams || [])
        collapsed.add(JSON.stringify([path, "team", team.id]));
    }
    for (const team of source.sidebar.peerTeams)
      collapsed.add(JSON.stringify([team.projectPath || "", "team", team.id]));
    for (const chat of source.sidebar.threads) addPath(chat.cwd || "");
    for (const room of source.sidebar.rooms) addPath(room.projectPath || "");
    preferenceKeys.set(source.id, { compact, collapsed });
  }
  const preferenceOwner = (kind: "compact" | "collapsed", field: string) => {
    if (kind === "collapsed") {
      try {
        const parts = JSON.parse(field);
        if (parts[1] === "folder") return folderReferences.get(parts[2])?.owner;
        if (parts[1] === "team") return teamReferences.get(parts[2])?.owner;
      } catch {
        /* Project paths need not be JSON. */
      }
    }
    return projectReferences.get(field)?.owner;
  };
  const mergeVisual = (
    kind: "compact" | "collapsed",
    values: ReadonlyMap<string, Record<string, boolean>>,
  ) => {
    const result: Record<string, boolean> = {};
    for (const source of sources)
      for (const [name, value] of Object.entries(values.get(source.id) || {})) {
        const mapped =
          kind === "compact"
            ? projectKey(source.id, name)
            : mapTreeKey(source.id, name);
        const headingOwner =
          kind === "collapsed"
            ? projectReferences.get(mapped)?.owner
            : undefined;
        if (headingOwner && headingOwner !== source.id) continue;
        result[mapped] = mapped in result ? result[mapped] && value : value;
      }
    if (kind === "collapsed")
      for (const source of sources)
        for (const name of preferenceKeys.get(source.id)!.collapsed)
          if (!values.get(source.id)?.[name]) {
            const mapped = mapTreeKey(source.id, name);
            const headingOwner = projectReferences.get(mapped)?.owner;
            if (!headingOwner || headingOwner === source.id)
              result[mapped] = false;
          }
    return result;
  };
  const compact = mergeVisual(
    "compact",
    new Map(sources.map((source) => [source.id, source.sidebar.compact])),
  );
  const collapsed = mergeVisual(
    "collapsed",
    new Map(sources.map((source) => [source.id, source.sidebar.collapsed])),
  );
  const scope = single
    ? sources[0].sidebar.stateDir
    : "studio-combined-sidebar";
  const requireReference = (
    values: Map<string, SidebarReference>,
    id: string,
  ) => {
    const reference = values.get(id);
    if (!reference) throw new Error("The sidebar item is unavailable.");
    return reference;
  };
  const wireProject = (owner: string, path: string) => {
    const member = groups
      .get(path)
      ?.members.find((item) => item.source.id === owner);
    if (member) return member.project.path || member.project.id || "";
    const reference = requireReference(projectReferences, path);
    if (reference.owner !== owner)
      throw new Error("The project belongs to another server.");
    return reference.path || reference.id;
  };
  const orderOwner = (group: string) => {
    if (group === "projects")
      return sourceById.has("local") ? "local" : sources[0]?.id || "";
    let parts: unknown;
    try {
      parts = JSON.parse(group);
    } catch {
      throw new Error("The sidebar order group is unavailable.");
    }
    if (!Array.isArray(parts) || typeof parts[1] !== "string")
      throw new Error("The sidebar order group is unavailable.");
    if (parts[0] === "items" && typeof parts[2] === "string")
      return requireReference(folderReferences, parts[2]).owner;
    if (parts[0] === "chats") {
      const team = parts.indexOf("team");
      if (team >= 0 && typeof parts[team + 1] === "string")
        return requireReference(teamReferences, parts[team + 1]).owner;
      const folder = parts[parts.length - 1];
      if (parts.length > 4 && team < 0 && typeof folder === "string")
        return requireReference(folderReferences, folder).owner;
    }
    return requireReference(projectReferences, parts[1]).owner;
  };
  const data: Snapshot = {
    token: "",
    stateDir: scope,
    threads,
    nodes: threads,
    chats: [],
    edges: [],
    runtime: {
      agents: threads,
      projects,
      rooms,
      peerTeams,
      tasks: [],
      monitors: [],
      complaints: [],
      requests: [],
      rules: [],
      events: [],
      work: [],
      peerTeamsVersion: sources.some(
        (source) => source.sidebar.peerTeamsVersion === 1,
      )
        ? 1
        : undefined,
      projectOrganizationVersion: sources.some(
        (source) => source.sidebar.projectOrganizationVersion,
      )
        ? 1
        : undefined,
      sidebarOrder: single
        ? sources[0].sidebar.sidebarOrder
        : { revision, groups: order },
    },
  };
  return {
    data,
    sources,
    sourceById,
    groups,
    order,
    compact,
    collapsed,
    indicators,
    unread,
    chatReferences,
    projectReferences,
    folderReferences,
    teamReferences,
    projectKey,
    chatKey,
    folderKey,
    teamKey,
    mapTreeKey,
    mapOrderGroup,
    mapOrderItem,
    mergeOrder,
    mergeVisual,
    preferenceKeys,
    preferenceOwner,
    wireProject,
    orderOwner,
    requireReference,
  };
}
export type MergedSidebar = ReturnType<typeof mergeSidebar>;

/** Summary-only or offline updates cannot erase the last complete sidebar. */
export function createSidebarSourceStore() {
  const snapshots = new Map<string, ServerSidebarSnapshot>();
  return {
    update(id: string, sidebar: ServerSidebarSnapshot | undefined) {
      if (sidebar) snapshots.set(id, sidebar);
    },
    forget(id: string) {
      snapshots.delete(id);
    },
    sources(
      ids: readonly string[],
      online: ReadonlySet<string>,
    ): SidebarSource[] {
      return ids.flatMap((id) => {
        const sidebar = snapshots.get(id);
        return sidebar ? [{ id, sidebar, online: online.has(id) }] : [];
      });
    },
  };
}

export function createMergedSidebarSelector(combined = true) {
  let previous: MergedSidebar | undefined;
  let revision = 0;
  return (sources: readonly SidebarSource[]) => {
    if (
      previous &&
      sources.length === previous.sources.length &&
      sources.every(
        (source, index) =>
          source.id === previous!.sources[index].id &&
          source.online === previous!.sources[index].online &&
          source.sidebar === previous!.sources[index].sidebar,
      )
    )
      return previous;
    const next = mergeSidebar(sources, ++revision, combined);
    if (previous) {
      const retain = <T extends { id: string }>(rows: T[], old: T[]) => {
        const prior = new Map(old.map((row) => [row.id, row]));
        return rows.map((row) => {
          const existing = prior.get(row.id);
          return existing && JSON.stringify(existing) === JSON.stringify(row)
            ? existing
            : row;
        });
      };
      next.data.threads = retain(next.data.threads, previous.data.threads);
      next.data.nodes = next.data.threads;
      next.data.runtime.agents = next.data.threads;
      next.data.runtime.projects = retain(
        next.data.runtime.projects,
        previous.data.runtime.projects,
      );
      next.data.runtime.rooms = retain(
        next.data.runtime.rooms,
        previous.data.runtime.rooms,
      );
      next.data.runtime.peerTeams = retain(
        next.data.runtime.peerTeams,
        previous.data.runtime.peerTeams,
      );
    }
    previous = next;
    return next;
  };
}

/** Classic mode does not construct or traverse a merged model. */
export function selectCombinedSidebar(
  enabled: boolean,
  select: ReturnType<typeof createMergedSidebarSelector>,
  sources: () => SidebarSource[],
) {
  return enabled ? select(sources()) : undefined;
}
