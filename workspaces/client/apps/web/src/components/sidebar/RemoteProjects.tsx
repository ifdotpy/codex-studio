import { Folder } from "lucide-react";
import type { Snapshot } from "../../types";

export type RemoteProjectList = Snapshot["runtime"]["remoteProjects"];
type RemoteProject = NonNullable<RemoteProjectList>[number];
/** One paired server: its id, its name and its projects. */
export type RemoteProjectGroup = readonly [string, string, RemoteProject[]];

/** Group the projects that this server publishes for its paired servers, by server. */
export function remoteProjectGroups(
  projects: RemoteProjectList,
  query = "",
): RemoteProjectGroup[] {
  const found = query.trim().toLowerCase();
  const servers = new Map<string, [string, string, RemoteProject[]]>();
  for (const project of projects ?? []) {
    const name = project.name || project.path || project.id;
    if (found && !`${name} ${project.path ?? ""}`.toLowerCase().includes(found))
      continue;
    const server = project.homeServerId || project.serverLabel || "";
    const entry = servers.get(server) ?? [
      server,
      project.serverLabel || "Paired server",
      [],
    ];
    entry[2].push(project);
    servers.set(server, entry);
  }
  return [...servers.values()];
}

/**
 * Read-only rows of the projects on paired servers. A client without signed access to those
 * servers cannot open their chats, so the rows list the projects and name their server.
 */
export default function RemoteProjects({
  groups,
}: {
  groups: readonly RemoteProjectGroup[];
}) {
  return groups.map(([server, label, items]) => (
    <section
      className="sidebar-project sidebar-remote-projects"
      data-remote-server={server}
      key={server}
    >
      <div className="project-tree-heading">
        <span
          className="project-tree-name"
          title="These projects belong to a paired server. Open them there; this view only lists them."
        >
          {label}
        </span>
      </div>
      {items.map((project) => (
        <div
          className="sidebar-row remote-project-row"
          data-remote-project={project.id}
          title={project.path || project.id}
          key={project.id}
        >
          <Folder size={18} />
          <span className="project-tree-name">
            {project.name || project.path || project.id}
          </span>
        </div>
      ))}
    </section>
  ));
}
