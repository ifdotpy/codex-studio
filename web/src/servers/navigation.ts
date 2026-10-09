import {
  isStudioSettingsTab,
  type StudioSettingsTab,
} from "../components/StudioSettingsTabs";
import type { DesktopAlert } from "../desktop/desktopAlerts";
import type { Snapshot } from "../types";
import { saved } from "../api";
import type { LocationProject } from "./projectLocations";
import { chatIndicators } from "../components/chat-status/chatStatusModel";
import type { ChatIndicator } from "../components/chat-status/chatStatusModel";
export type ServerNavigation = {
  projects: (Omit<LocationProject, "id"> & { id?: string })[];
  chats: {
    id: string;
    name: string;
    path: string;
    archived: boolean;
    status: string;
    unread: boolean;
    serverId?: string | null;
    projectId?: string | null;
    projectServerId?: string | null;
    provider?: string | null;
    updated?: number | null;
    created?: number | null;
    indicator?: ChatIndicator;
    inFlight?: boolean;
    pinned?: boolean;
    team?: boolean;
  }[];
  ready: boolean;
  busy?: boolean;
  alerts: DesktopAlert[];
  opened: string | null;
  error: string;
};
export type ServerCommand =
  | { action: "open"; id: string; messageId?: string }
  | {
      action: "new-chat";
      path?: string;
      projectId?: string;
      projectServerId?: string;
    }
  | { action: "add-project" }
  | { action: "project-folders"; projectId: string; server?: string }
  | { action: "projects" }
  | { action: "settings"; tab?: StudioSettingsTab }
  | { action: "search"; query: string; requestId: string }
  | { action: "notifications"; agentId: string; itemId?: string }
  | { action: "focus" };
export function navigationSnapshot(
  data: Snapshot | null,
  opened: string | null,
  error: string,
  unread = new Set<string>(),
  alerts: DesktopAlert[] = [],
): ServerNavigation {
  const projects = new Map<string, ServerNavigation["projects"][number]>();
  const indicators = data
    ? chatIndicators(data)
    : new Map<string, ChatIndicator>();
  for (const project of data?.runtime?.projects || []) {
    if (project.path)
      projects.set(project.path, {
        id: project.id,
        compact: saved<Record<string, boolean>>(
          `codex-project-compact:${data?.stateDir}`,
          {},
        )[project.path],
        homeServerId: project.homeServerId,
        locations: project.locations,
        projectAliases: project.projectAliases,
        path: project.path,
        name: project.name || project.path,
      });
  }
  const chats: ServerNavigation["chats"] = [];
  for (const chat of data?.threads || []) {
    if (
      chat.source !== "managed" ||
      !chat.isLead ||
      chat.deletedAt ||
      chat.sharedRoomId ||
      (chat as typeof chat & { remoteAnchor?: unknown }).remoteAnchor
    )
      continue;
    const path = chat.cwd || "";
    if (
      !projects.has(path) &&
      ![...projects.values()].some(
        (project) =>
          (project.id === chat.projectId &&
            project.homeServerId === chat.projectServerId) ||
          project.locations?.some(
            (location) =>
              location.path === path && location.serverId === chat.serverId,
          ),
      )
    )
      projects.set(path, { path, name: path || "Other chats" });
    chats.push({
      id: chat.id,
      name: chat.name || "Chat",
      path,
      archived: !!chat.archived,
      status: chat.status || "",
      unread: unread.has(chat.id),
      serverId: chat.serverId,
      projectId: chat.projectId,
      projectServerId: chat.projectServerId,
      provider: chat.provider,
      updated: chat.updated,
      created: chat.created,
      inFlight: !!chat.inFlight,
      pinned: !!chat.pinned,
      team: !!data?.runtime?.projects?.some((project) =>
        project.peerTeams?.some((team) => team.members.includes(chat.id)),
      ),
      indicator: indicators.get(chat.id),
    });
  }
  for (const room of data?.runtime?.rooms || []) {
    if (!room.radio || room.userHidden) continue;
    const path = room.projectPath || "";
    if (!projects.has(path))
      projects.set(path, { path, name: path || "Other chats" });
    chats.push({
      id: room.id,
      name: room.name || "Shared chat",
      path,
      archived: false,
      status: "",
      unread: (room.members || []).some((id) => unread.has(id)),
    });
  }
  return {
    ready: !!data,
    busy: !!data?.threads.some((agent) => agent.inFlight),
    alerts,
    projects: [...projects.values()],
    chats,
    opened,
    error,
  };
}
export function isServerCommand(value: unknown): value is ServerCommand {
  if (!value || typeof value !== "object") return false;
  const command = value as Record<string, unknown>;
  if (command.action === "project-folders")
    return (
      typeof command.projectId === "string" &&
      (command.server === undefined || typeof command.server === "string")
    );
  if (command.action === "search")
    return (
      typeof command.query === "string" && typeof command.requestId === "string"
    );
  if (command.action === "notifications")
    return typeof command.agentId === "string";
  return command.action === "open"
    ? typeof command.id === "string" && !!command.id
    : command.action === "new-chat"
      ? (command.path === undefined || typeof command.path === "string") &&
        (command.projectId === undefined ||
          typeof command.projectId === "string") &&
        (command.projectServerId === undefined ||
          typeof command.projectServerId === "string")
      : (command.action === "settings" &&
          (command.tab === undefined || isStudioSettingsTab(command.tab))) ||
        command.action === "projects" ||
        command.action === "add-project" ||
        command.action === "focus";
}
