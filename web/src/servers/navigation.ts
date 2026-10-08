import {
  isStudioSettingsTab,
  type StudioSettingsTab,
} from "../components/StudioSettingsTabs";
import type { DesktopAlert } from "../desktop/desktopAlerts";
import type { Snapshot } from "../types";
export type ServerAccount = {
  provider: "codex" | "claude";
  email: string | null;
  plan: string | null;
  status: string;
  label: string;
};
export type ServerNavigation = {
  projects: { path: string; name: string }[];
  chats: {
    id: string;
    name: string;
    path: string;
    archived: boolean;
    status: string;
    unread: boolean;
  }[];
  ready: boolean;
  busy?: boolean;
  system?: string;
  agentsRunning?: number;
  alerts: DesktopAlert[];
  opened: string | null;
  error: string;
};
export type ServerCommand =
  | { action: "open"; id: string; messageId?: string }
  | { action: "new-chat"; path?: string }
  | { action: "projects" }
  | { action: "settings"; tab?: StudioSettingsTab }
  | { action: "search"; query: string; requestId: string }
  | { action: "notifications"; agentId: string; itemId?: string }
  | {
      action: "account-sign-in";
      provider: "codex" | "claude";
      email: string | null;
      label: string;
      serverLabel: string;
      forceAdd?: boolean;
    }
  | {
      action: "account-action";
      operation: "rename" | "default" | "disconnect" | "remove";
      provider: "codex" | "claude";
      email: string | null;
      label: string;
      nextLabel?: string;
    }
  | { action: "focus" };
export function navigationSnapshot(
  data: Snapshot | null,
  opened: string | null,
  error: string,
  unread = new Set<string>(),
  alerts: DesktopAlert[] = [],
): ServerNavigation {
  const projects = new Map<string, { path: string; name: string }>();
  for (const project of data?.runtime?.projects || []) {
    if (project.path)
      projects.set(project.path, {
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
    if (!projects.has(path))
      projects.set(path, { path, name: path || "Other chats" });
    chats.push({
      id: chat.id,
      name: chat.name || "Chat",
      path,
      archived: !!chat.archived,
      status: chat.status || "",
      unread: unread.has(chat.id),
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
    agentsRunning: data?.threads.filter((agent) => agent.inFlight).length || 0,
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
  if (command.action === "search")
    return (
      typeof command.query === "string" && typeof command.requestId === "string"
    );
  if (command.action === "notifications")
    return typeof command.agentId === "string";
  if (command.action === "account-sign-in")
    return (
      (command.provider === "codex" || command.provider === "claude") &&
      (command.email === null || typeof command.email === "string") &&
      typeof command.label === "string" &&
      typeof command.serverLabel === "string" &&
      (command.forceAdd === undefined || typeof command.forceAdd === "boolean")
    );
  if (command.action === "account-action")
    return (
      ["rename", "default", "disconnect", "remove"].includes(
        String(command.operation),
      ) &&
      (command.provider === "codex" || command.provider === "claude") &&
      (command.email === null || typeof command.email === "string") &&
      typeof command.label === "string" &&
      (command.nextLabel === undefined || typeof command.nextLabel === "string")
    );
  return command.action === "open"
    ? typeof command.id === "string" && !!command.id
    : command.action === "new-chat"
      ? command.path === undefined || typeof command.path === "string"
      : (command.action === "settings" &&
          (command.tab === undefined || isStudioSettingsTab(command.tab))) ||
        command.action === "projects" ||
        command.action === "focus";
}
