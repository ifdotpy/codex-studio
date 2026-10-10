import { saved } from "../api";
import {
  chatIndicators,
  type ChatIndicator,
} from "../components/chat-status/chatStatusModel";
import type { Agent, PeerTeam, Project, Room, Snapshot } from "../types";

export type ServerSidebarSnapshot = {
  stateDir: string;
  threads: Agent[];
  projects: Project[];
  rooms: Room[];
  peerTeams: PeerTeam[];
  peerTeamsVersion: Snapshot["runtime"]["peerTeamsVersion"];
  projectOrganizationVersion: Snapshot["runtime"]["projectOrganizationVersion"];
  sidebarOrder: Snapshot["runtime"]["sidebarOrder"];
  savedOrder: Record<string, string[]>;
  compact: Record<string, boolean>;
  collapsed: Record<string, boolean>;
  indicators: [string, ChatIndicator][];
  unread: string[];
};

function pick<T extends object, K extends keyof T>(
  value: T,
  keys: readonly K[],
): Pick<T, K> {
  const result = {} as Pick<T, K>;
  for (const key of keys)
    if (Object.hasOwn(value, key)) result[key] = value[key];
  return result;
}

export function sidebarAgent(agent: Agent): Agent {
  return pick(agent, [
    "id",
    "name",
    "tail",
    "source",
    "isLead",
    "parentId",
    "rootId",
    "cwd",
    "project",
    "projectId",
    "projectServerId",
    "projectFolder",
    "projectFolderRevision",
    "serverId",
    "provider",
    "model",
    "created",
    "updated",
    "pinned",
    "archived",
    "deletedAt",
    "sharedRoomId",
    "remoteAnchor",
    "movedTo",
    "movedFrom",
    "moveImportPending",
    "status",
    "inFlight",
    "autoWake",
    "empty",
    "threadId",
    "lastCompletedTurn",
    "lastCompletedTurnStatus",
    "readState",
    "readStateSupported",
  ]);
}

/** Preserve sidebar fields and source identities without transcripts or runtime collections. */
export function sidebarSnapshot(
  data: Snapshot,
  unread = new Set<string>(),
  indicators = chatIndicators(data),
): ServerSidebarSnapshot {
  const runtime = data.runtime;
  return {
    stateDir: data.stateDir,
    threads: data.threads.map(sidebarAgent),
    projects: (runtime?.projects || []).map((project) =>
      pick(project, [
        "id",
        "path",
        "name",
        "folders",
        "organizationRevision",
        "peerTeams",
        "peerTeamsRevision",
        "homeServerId",
        "locations",
        "locationsRevision",
        "projectAliases",
        "accountKey",
        "accountKeys",
        "accountRevision",
        "created",
        "updated",
      ]),
    ),
    rooms: (runtime?.rooms || []).map((room) =>
      pick(room, [
        "id",
        "kind",
        "name",
        "projectPath",
        "members",
        "localMembers",
        "rootId",
        "peerTeamId",
        "peerTeamName",
        "peerLabel",
        "userHidden",
        "radio",
        "updated",
      ]),
    ),
    peerTeams: (runtime?.peerTeams || []).map((team) =>
      pick(team, ["id", "name", "projectPath", "members", "revision"]),
    ),
    peerTeamsVersion: runtime?.peerTeamsVersion,
    projectOrganizationVersion: runtime?.projectOrganizationVersion,
    sidebarOrder: runtime?.sidebarOrder,
    savedOrder: saved<Record<string, string[]>>(
      `codex-sidebar-order:${data.stateDir}`,
      {},
    ),
    compact: saved<Record<string, boolean>>(
      `codex-project-compact:${data.stateDir}`,
      {},
    ),
    collapsed: saved<Record<string, boolean>>(
      `codex-project-tree:${data.stateDir}`,
      {},
    ),
    indicators: [...indicators],
    unread: [...unread],
  };
}
