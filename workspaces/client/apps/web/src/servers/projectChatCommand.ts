import type { ServerCommand, ServerNavigation } from "./navigation";

// The shell accepts a cross-server choice only from the owning frame's project list.
export function projectChatCommand(
  view: ServerNavigation | undefined,
  value: Record<string, unknown>,
): ServerCommand | null {
  if (
    typeof value.projectId !== "string" ||
    typeof value.projectServerId !== "string" ||
    typeof value.path !== "string" ||
    typeof value.target !== "string"
  )
    return null;
  const project = view?.projects.find(
    (row) =>
      row.id === value.projectId && row.homeServerId === value.projectServerId,
  );
  if (
    !project?.locations?.some(
      (row) => row.serverId === value.target && row.path === value.path,
    )
  )
    return null;
  return {
    action: "new-chat",
    path: value.path,
    projectId: value.projectId,
    projectServerId: value.projectServerId,
  };
}
