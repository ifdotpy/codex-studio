import type { ServerNavigation } from "./navigation";
import type { LocationProject, ProjectLocation } from "./projectLocations";

export type LogicalProject = LocationProject & {
  key: string;
  owner: string;
  chats: (ServerNavigation["chats"][number] & { serverId: string })[];
};
const keyFor = (server: string, id: string) => JSON.stringify([server, id]);
export function logicalProjects(
  navigation: Record<string, ServerNavigation>,
): LogicalProject[] {
  const groups = new Map<string, LogicalProject>();
  const folders = new Map<string, string>();
  const serverIds = new Map<string, string>();
  for (const [source, view] of Object.entries(navigation))
    for (const project of view.projects)
      if (project.homeServerId) serverIds.set(project.homeServerId, source);
  const ensure = (
    home: string,
    id: string,
    name: string,
    owner: string,
    path: string,
  ) => {
    const key = keyFor(home, id);
    if (!groups.has(key))
      groups.set(key, {
        key,
        id,
        path,
        name,
        owner,
        homeServerId: home,
        locations: [],
        chats: [],
      });
    return groups.get(key)!;
  };
  for (const [source, view] of Object.entries(navigation)) {
    for (const project of view.projects) {
      const id = project.id || project.path;
      const home = project.homeServerId || source;
      const aliases = project.projectAliases || [];
      const canonical = aliases[0];
      const owner = canonical
        ? serverIds.get(canonical.serverId) || canonical.serverId
        : source;
      const group = ensure(
        canonical?.serverId || home,
        canonical?.projectId || id,
        canonical?.name || project.name,
        owner,
        canonical?.projectId || project.path,
      );
      if (!canonical) {
        group.name = project.name;
        group.path = project.path;
        group.owner = source;
        group.compact = project.compact;
      }
      const rows = project.locations || [
        { serverId: source, path: project.path, projectId: id },
      ];
      for (const location of rows) {
        const uiServer = serverIds.get(location.serverId) || location.serverId;
        const next: ProjectLocation = { ...location, serverId: uiServer };
        if (!group.locations?.some((row) => row.serverId === uiServer))
          group.locations!.push(next);
        folders.set(keyFor(uiServer, location.path), group.key);
      }
      if (canonical) folders.set(keyFor(source, project.path), group.key);
    }
  }
  for (const [source, view] of Object.entries(navigation)) {
    for (const chat of view.chats) {
      const binding =
        chat.projectId && chat.projectServerId
          ? keyFor(chat.projectServerId, chat.projectId)
          : undefined;
      const group =
        (binding && groups.get(binding)) ||
        groups.get(folders.get(keyFor(source, chat.path)) || "") ||
        ensure(
          source,
          chat.path,
          chat.path || "Other chats",
          source,
          chat.path,
        );
      group.chats.push({ ...chat, serverId: source });
    }
  }
  return [...groups.values()];
}
