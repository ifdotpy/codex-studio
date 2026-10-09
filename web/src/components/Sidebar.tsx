import { serverLocalStorage as localStorage } from "../servers/storage";
import {
  Fragment,
  useEffect,
  useMemo,
  useRef,
  useState,
  type ReactNode,
} from "react";
import {
  ActionIcon,
  Button,
  Drawer,
  Menu,
  Modal,
  TextInput,
  UnstyledButton,
} from "@mantine/core";
import { useMediaQuery } from "@mantine/hooks";
import {
  Users,
  FolderOpen,
  MoreHorizontal,
  SquarePen,
  Search,
  Folder,
  ChevronDown,
  Plus,
  X,
} from "lucide-react";
import { post, errorText, save, saved } from "../api";
import type { paths } from "../generated/api";
import "./sidebar-projects.css";
import { useSidebarOrder } from "./useSidebarOrder";
import {
  ProjectNameForm,
  MoveChatForm,
  ConvertChatForm,
  folderLabel,
  type Project,
  type ProjectFolder,
} from "./ProjectOrganization";
import { type Agent, type Snapshot } from "../types";
import {
  PeerTeamForm,
  PeerTeamGroup,
  usePeerTeamMove,
} from "./shell/messages/PeerTeams";

import SidebarRow from "./SidebarRow";
import {
  createSidebarCatalogSelector,
  createSidebarSearchSelector,
  itemGroup,
  chatGroup as catalogChatGroup,
} from "./sidebar/catalog";
export { isVisibleSidebarAgent } from "./sidebar/catalog";
import type { ChatIndicator } from "./chat-status/chatStatusModel";
import { reportPromptComposerRender } from "./prompt-composer/renderProbe";

type OrganizationRequest =
  paths["/api/organization"]["post"]["requestBody"]["content"]["application/json"];
type WithoutOrganizationId<Request> = Request extends { id: string }
  ? Omit<Request, "id">
  : never;
type OrganizationData = WithoutOrganizationId<OrganizationRequest>;
type Props = {
  data: Snapshot;
  opened: string | null;
  lead?: Agent;
  open: (id: string) => void;
  prepareChat?: (id: string) => void;
  newChat: (path?: string, folder?: string) => void;
  newSharedChat?: (path?: string) => void;
  addProject: () => void;
  changeProject: (agent: Agent) => void;
  projectAccount: (path: string) => void;
  projectFolders?: (path: string) => void;
  creating: boolean;
  rename: (id: string, name: string) => Promise<void>;
  remove: (id: string, room: boolean) => void;
  mobile: boolean;
  collapsed?: boolean;
  onSearch: () => void;
  close: () => void;
  refresh?: () => Promise<void>;
  notify?: (message: string) => void;
  indicators: Map<string, ChatIndicator>;
  markUnread: (agent: Agent) => void;
  markingRead: Set<string>;
};
type NavigableProject = Project & { path: string; name: string };
export function projectDisplayName(
  project: Pick<Project, "path" | "name">,
): string {
  return typeof project.name === "string" && project.name
    ? project.name
    : project.path || "Project";
}
export function projectSearchLabel(
  projectName: string | null | undefined,
  legacyProjectName: unknown,
): string {
  return (
    projectName ||
    (typeof legacyProjectName === "string" ? legacyProjectName : "")
  );
}
export default function Sidebar(p: Props) {
  reportPromptComposerRender("sidebar");
  const runtime = p.data.runtime;
  const compact = useMediaQuery("(max-width: 760px)");
  const [query, setQuery] = useState(""),
    [renaming, setRenaming] = useState<string | null>(null),
    [name, setName] = useState(""),
    [archive, setArchive] = useState(false),
    [projectsOpen, setProjectsOpen] = useState(true);
  const sorting = useSidebarOrder(
    `codex-sidebar-order:${p.data.stateDir}`,
    runtime?.sidebarOrder ?? undefined,
    p.refresh,
    p.notify,
  );
  const [organizing, setOrganizing] = useState<string | null>(null);
  const organizationLock = useRef(false);
  type Conversion = { source: Agent; target: Agent; project: NavigableProject };
  const conversionKey = `studio-peer-conversion-dialog:${p.data.stateDir}`;
  const [conversion, updateConversion] = useState<Conversion | null>(() =>
    saved(conversionKey, null),
  );
  const setConversion = (value: Conversion | null) => {
    try {
      localStorage.setItem(conversionKey, JSON.stringify(value));
      updateConversion(value);
    } catch (error) {
      p.notify?.(`Could not save the conversion request: ${String(error)}`);
    }
  };
  const closeConversion = () => {
    if (
      conversion &&
      localStorage.getItem(
        `studio-peer-convert:${p.data.stateDir}:${conversion.source.id}:${conversion.target.id}`,
      )
    ) {
      p.notify?.(
        "Retry the saved request to confirm its result before closing.",
      );
      return;
    }
    setConversion(null);
  };
  const compactKey = `codex-project-compact:${p.data.stateDir}`;
  const [compactProjects, setCompactProjects] = useState<
    Record<string, boolean>
  >(() => saved(compactKey, {}));
  const setCompactProject = (path: string, value: boolean) => {
    const next = { ...compactProjects, [path]: value };
    save(compactKey, next);
    setCompactProjects(next);
  };
  const [teamDialog, setTeamDialog] = useState<{
    path: string;
    id?: string;
    action: "save" | "delete";
  } | null>(null);
  const peerTeams = useMemo(
    () => (runtime?.peerTeamsVersion === 1 ? runtime.peerTeams || [] : []),
    [runtime?.peerTeamsVersion, runtime?.peerTeams],
  );
  const projects = useMemo(
    () =>
      (runtime?.projects ?? [])
        .filter(
          (project): project is Project & { path: string } =>
            typeof project.path === "string",
        )
        .map((project): NavigableProject => ({
          ...project,
          name: projectDisplayName(project),
        })),
    [runtime?.projects],
  );
  const teamMove = usePeerTeamMove(p.refresh, p.notify);
  const [dialog, setDialog] = useState<{
    title: string;
    path: string;
    folder?: string;
    parentId?: string;
    agentId?: string;
  } | null>(null);
  const canOrganizeProjects = !!runtime?.projectOrganizationVersion;
  const requireProjectSupport = () => {
    if (canOrganizeProjects) return true;
    p.notify?.(
      "Restart Studio after active work finishes to edit project names and folders.",
    );
    return false;
  };
  const savedProject = async () => {
    await p.refresh?.();
    setDialog(null);
  };
  const editProject = (
    project: NavigableProject,
    folder?: ProjectFolder | "new",
    parentId?: string,
  ) => {
    if (!requireProjectSupport()) return;
    setDialog({
      title:
        folder === "new"
          ? "New chat folder"
          : folder
            ? "Rename folder"
            : "Rename project",
      path: project.path,
      folder: folder === "new" ? "new" : folder?.id,
      parentId,
    });
  };
  const folderKey = (path: string, id: string) =>
    JSON.stringify([path, "folder", id]);
  const projectKey = `codex-project-tree:${p.data.stateDir}`;
  const [collapsed, setCollapsed] = useState<Record<string, boolean>>(() =>
    saved(projectKey, {}),
  );
  const [projectLimits, setProjectLimits] = useState<Record<string, number>>(
    {},
  );
  const toggleProject = (path: string) => {
    const next = { ...collapsed, [path]: !collapsed[path] };
    setCollapsed(next);
    save(projectKey, next);
  };
  // Pin and archive show at once; the next snapshot confirms them.
  const [overrides, setOverrides] = useState<
    Record<string, { pinned?: boolean; archived?: boolean }>
  >({});
  const organize = async (id: string, data: OrganizationData) => {
    if (organizationLock.current) return false;
    organizationLock.current = true;
    setOrganizing(id);
    const optimistic = Object.fromEntries(
      Object.entries(data).filter(
        ([key, value]) =>
          (key === "pinned" || key === "archived") &&
          typeof value === "boolean",
      ),
    );
    const hasOptimistic = Object.keys(optimistic).length > 0;
    if (hasOptimistic)
      setOverrides((old) => ({ ...old, [id]: { ...old[id], ...optimistic } }));
    try {
      await post("/api/organization", { id, ...data });
      if (hasOptimistic) void p.refresh?.();
      else await p.refresh?.();
      return true;
    } catch (error) {
      if (hasOptimistic)
        setOverrides((old) => {
          const next = { ...old };
          delete next[id];
          return next;
        });
      p.notify?.(errorText(error));
      return false;
    } finally {
      organizationLock.current = false;
      setOrganizing(null);
    }
  };
  useEffect(() => {
    // Drop an override once the snapshot shows the same value.
    setOverrides((old) => {
      const next = { ...old };
      let changed = false;
      for (const [id, values] of Object.entries(old)) {
        const thread = p.data.threads.find((a) => a.id === id);
        if (
          !thread ||
          ((values.pinned === undefined || thread.pinned === values.pinned) &&
            (values.archived === undefined ||
              thread.archived === values.archived))
        ) {
          delete next[id];
          changed = true;
        }
      }
      return changed ? next : old;
    });
  }, [p.data.threads]);
  const catalogSelector = useMemo(createSidebarCatalogSelector, []);
  const catalog = catalogSelector(
    p.data.threads,
    overrides,
    peerTeams,
    sorting.order,
  );
  const { agents, byProject, grouped } = catalog;
  const teamFor = (id: string) => catalog.byTeam.get(id);
  const sharedRooms = useMemo(
    () =>
      (runtime?.rooms || []).filter((r) => r.radio?.direct && !r.userHidden),
    [runtime?.rooms],
  );
  const visibleSharedRooms = useMemo(
    () =>
      archive
        ? []
        : sharedRooms.filter((r) =>
            `${r.name} ${r.projectPath || ""} ${projects.find((project) => project.path === r.projectPath)?.name || ""}`
              .toLowerCase()
              .includes(query.toLowerCase()),
          ),
    [archive, sharedRooms, projects, query],
  );
  const selectedFolder = catalog.byId.get(p.opened || "")?.projectFolder;
  useEffect(() => {
    if (
      agents.some((a) => a.id === p.opened) ||
      sharedRooms.some((r) => r.id === p.opened)
    ) {
      setQuery("");
      setProjectsOpen(true);
      const path =
        agents.find((a) => a.id === p.opened)?.cwd ||
        sharedRooms.find((r) => r.id === p.opened)?.projectPath;
      if (path && collapsed[path]) {
        const next = { ...collapsed, [path]: false };
        setCollapsed(next);
        save(projectKey, next);
      }
      const folders =
        projects.find((project) => project.path === path)?.folders || [];
      let folder = folders.find((item) => item.id === selectedFolder);
      const ancestors: Record<string, boolean> = {};
      while (folder && !(folderKey(path || "", folder.id) in ancestors)) {
        ancestors[folderKey(path || "", folder.id)] = false;
        folder = folders.find((item) => item.id === folder!.parentId);
      }
      if (path && Object.keys(ancestors).length) {
        const next = { ...collapsed, [path]: false, ...ancestors };
        setCollapsed(next);
        save(projectKey, next);
      }
    }
  }, [p.opened, selectedFolder]);
  const teamKey = (path: string, id: string) =>
    JSON.stringify([path, "team", id]);
  const chatGroup = (agent: Agent) =>
    catalogChatGroup(agent, teamFor(agent.id));
  const folderDrop = (
    project: NavigableProject,
    folder: ProjectFolder | null = null,
  ) =>
    sorting.dropBindings(
      folder ? folderKey(project.path, folder.id) : project.path,
      (source) => {
        const chat = catalog.byId.get(source.id);
        return !!(
          (canOrganizeProjects || (!folder && teamFor(source.id))) &&
          !teamMove.blocked() &&
          !organizationLock.current &&
          chat &&
          source.group === chatGroup(chat) &&
          chat.cwd === project.path &&
          ((!folder && !!teamFor(chat.id)) ||
            (chat.projectFolder || null) !== (folder?.id || null))
        );
      },
      ({ id }) => {
        const chat = catalog.byId.get(id)!;
        if (!folder && teamFor(id)) {
          teamMove.move(project, id, null);
          return;
        }
        void organize(id, {
          project_path: project.path,
          project_folder: folder?.id || null,
          expected_folder: chat.projectFolder || null,
          expected_revision: chat.projectFolderRevision || 0,
        }).then((moved) => {
          if (!moved) return;
          setCollapsed((previous) => {
            const next = { ...previous, [project.path]: false };
            let parent = folder;
            const seen = new Set<string>();
            while (parent && !seen.has(parent.id)) {
              seen.add(parent.id);
              next[folderKey(project.path, parent.id)] = false;
              parent =
                project.folders?.find((f) => f.id === parent?.parentId) || null;
            }
            save(projectKey, next);
            return next;
          });
          const key = folder
            ? folderKey(project.path, folder.id)
            : project.path;
          setProjectLimits((previous) => ({
            ...previous,
            [key]: agents.length,
          }));
          sorting.announce(
            `Moved ${chat.name} to ${folder?.name || project.name}`,
          );
        });
      },
    );
  const orderedAgents = catalog.ordered;
  const searchSelector = useMemo(createSidebarSearchSelector, []);
  const searchContext = useMemo(
    () => ({ projects, peerTeams }),
    [projects, peerTeams],
  );
  const projectsByPath = useMemo(
    () => new Map(projects.map((project) => [project.path, project])),
    [projects],
  );
  const filtered = useMemo(() => {
    const archived = orderedAgents.filter((row) => !!row.archived === archive);
    const matching = searchSelector(archived, searchContext, query, (a) => {
      const project = projectsByPath.get(a.cwd || "");
      const legacyProjectName = "project" in a ? a.project : undefined;
      const team = catalog.byTeam.get(a.id);
      const teamName = team && team.projectPath === a.cwd ? team.name : "";
      return `${teamName || ""} ${a.name || ""} ${a.cwd || ""} ${projectSearchLabel(project?.name, legacyProjectName)} ${folderLabel(project?.folders || [], a.projectFolder || "")} ${a.tail || ""}`;
    });
    // Keep the selected team member visible under a search, as before.
    const selected = catalog.byId.get(p.opened || "");
    if (
      query &&
      selected &&
      !!selected.archived === archive &&
      grouped.has(selected.id) &&
      !matching.includes(selected)
    ) {
      const matches = new Set(matching);
      return archived.filter((row) => row === selected || matches.has(row));
    }
    return matching;
  }, [
    orderedAgents,
    archive,
    query,
    searchContext,
    projectsByPath,
    p.opened,
    grouped,
  ]);
  const rename = async (e: React.FormEvent, id: string) => {
    e.preventDefault();
    try {
      await p.rename(id, name);
      setRenaming(null);
    } catch {
      /* Keep the draft when the server rejects the name. */
    }
  };
  const sharedProjectPaths = useMemo(
    () => new Set(sharedRooms.map((room) => room.projectPath)),
    [sharedRooms],
  );
  const keptInCompact = (a: Agent) =>
    !!(
      teamFor(a.id) ||
      a.pinned ||
      a.inFlight ||
      ["running", "starting", "approval"].includes(a.status || "") ||
      a.hasUnread ||
      a.unreadCount ||
      p.indicators.get(a.id)?.kind === "unread" ||
      (a.updated || a.created || 0) >= Date.now() / 1000 - 86400
    );
  const compactIndicatorKey = agents
    .filter(
      (agent) =>
        compactProjects[agent.cwd || ""] &&
        p.indicators.get(agent.id)?.kind === "unread",
    )
    .map((agent) => agent.id)
    .join("\n");
  const groupMap = useMemo(() => {
    const groups = new Map<
      string,
      NavigableProject & {
        chats: Agent[];
        registered: boolean;
        hasChats: boolean;
      }
    >();
    for (const project of projects)
      groups.set(project.path, {
        ...project,
        chats: [],
        registered: true,
        hasChats:
          byProject.has(project.path) || sharedProjectPaths.has(project.path),
      });
    for (const a of agents) {
      const path = a.cwd || "";
      if (!groups.has(path))
        groups.set(path, {
          id: path,
          created: 0,
          path,
          name: path.split("/").filter(Boolean).at(-1) || "Other chats",
          chats: [],
          registered: false,
          hasChats: true,
        });
    }
    for (const room of sharedRooms) {
      const path = room.projectPath || "";
      if (!groups.has(path))
        groups.set(path, {
          id: path,
          created: 0,
          path,
          name: path.split("/").filter(Boolean).at(-1) || "Other chats",
          chats: [],
          registered: false,
          hasChats: true,
        });
    }
    for (const a of filtered) {
      if (query || !compactProjects[a.cwd || ""] || keptInCompact(a))
        groups.get(a.cwd || "")!.chats.push(a);
    }
    return groups;
  }, [
    projects,
    agents,
    sharedRooms,
    sharedProjectPaths,
    filtered,
    query,
    compactProjects,
    compactIndicatorKey,
  ]);
  const itemIds = (group: NavigableProject, parent: string | null = null) =>
    [
      ...(!parent
        ? peerTeams
            .filter((t) => t.projectPath === group.path)
            .map((t) => teamKey(group.path, t.id))
        : []),
      ...(group.folders || [])
        .filter((f) => (f.parentId || null) === parent)
        .sort((a, b) => a.name.localeCompare(b.name))
        .map((f) => folderKey(group.path, f.id)),
      ...(byProject.get(group.path) || [])
        .filter(
          (a) =>
            !!a.archived === archive &&
            !grouped.has(a.id) &&
            !a.pinned &&
            (a.projectFolder || null) === parent,
        )
        .map((a) => a.id),
    ].sort(
      (a, b) =>
        sorting.rank(itemGroup(group.path, parent), a) -
        sorting.rank(itemGroup(group.path, parent), b),
    );
  const projectOrderKey = JSON.stringify(
    [...groupMap.values()].map((group) => [group.path, group.name]),
  );
  const projectPaths = useMemo(
    () =>
      [...groupMap.values()]
        .sort(
          (a, b) =>
            sorting.rank("projects", a.path) -
              sorting.rank("projects", b.path) || a.name.localeCompare(b.name),
        )
        .map((group) => group.path),
    [projectOrderKey, sorting.order],
  );
  const allProjectGroups = projectPaths.map((path) => groupMap.get(path)!);
  const projectGroups = allProjectGroups.filter(
    (group) =>
      !query ||
      group.chats.length ||
      visibleSharedRooms.some((r) => r.projectPath === group.path) ||
      `${group.path} ${group.name}`.toLowerCase().includes(query.toLowerCase()),
  );
  const renderRow = (row: Agent) => (
    <SidebarRow
      key={row.id}
      row={row}
      selected={p.opened === row.id}
      indicator={p.indicators.get(row.id)}
      renaming={renaming === row.id}
      name={renaming === row.id ? name : ""}
      organizing={organizing === row.id}
      compact={compact}
      markingRead={p.markingRead.has(row.id)}
      bindings={sorting.dropBindings(
        `convert:${row.id}`,
        (source, event) => {
          const chat = catalog.byId.get(source.id);
          const box = event.currentTarget.getBoundingClientRect();
          return !!(
            chat &&
            chat.id !== row.id &&
            teamFor(chat.id) &&
            chat.cwd === row.cwd &&
            source.group === chatGroup(chat) &&
            !conversion &&
            !organizationLock.current &&
            !teamMove.blocked() &&
            event.clientY > box.top + box.height * 0.25 &&
            event.clientY < box.top + box.height * 0.75
          );
        },
        ({ id }) =>
          setConversion({
            source: catalog.byId.get(id)!,
            target: row,
            project: groupMap.get(row.cwd || "")!,
          }),
        sorting.bindings(
          chatGroup(row),
          row.id,
          teamFor(row.id) || row.pinned
            ? orderedAgents
                .filter((a) => chatGroup(a) === chatGroup(row))
                .map((a) => a.id)
            : itemIds(groupMap.get(row.cwd || "")!, row.projectFolder || null),
        ),
      )}
      actions={{
        prepare: () => p.prepareChat?.(row.id),
        open: () => p.open(row.id),
        pin: () => {
          void organize(row.id, { pinned: !row.pinned });
        },
        rename: (event) => {
          void rename(event, row.id);
        },
        name: setName,
        cancelRename: () => setRenaming(null),
        beginRename: () => {
          setName(row.name ?? "");
          setRenaming(row.id);
        },
        unread: () => p.markUnread(row),
        move: () => {
          if (!requireProjectSupport()) return;
          const project = groupMap.get(row.cwd || "")!;
          setDialog({
            title: "Move chat",
            path: project.path,
            agentId: row.id,
          });
        },
        changeProject: () => p.changeProject(row),
        archive: () => {
          void organize(row.id, { archived: !row.archived });
        },
        remove: () => p.remove(row.id, false),
      }}
    />
  );
  const renderPeerTeam = (
    group: NavigableProject & { chats: Agent[] },
    team: (typeof peerTeams)[number],
  ): ReactNode => {
    const teamMembers = team.members ?? [];
    const members = group.chats.filter((a) => teamMembers.includes(a.id));
    const selected = agents.find(
      (a) => a.id === p.opened && teamMembers.includes(a.id),
    );
    if (selected && !members.includes(selected)) members.push(selected);
    if (!members.length && (query || archive)) return null;
    const key = JSON.stringify([group.path, "team", team.id]);
    const closed = !!collapsed[key] && !query;
    return (
      <PeerTeamGroup
        key={team.id}
        itemId={teamKey(group.path, team.id)}
        team={team}
        roomId={runtime?.rooms?.find((r) => r.radio?.teamId === team.id)?.id}
        scope={p.data.stateDir}
        openRoom={p.open}
        refresh={p.refresh}
        reorder={sorting.bindings(
          itemGroup(group.path),
          teamKey(group.path, team.id),
          itemIds(group),
        )}
        drop={sorting.dropBindings(
          `peer-team:${team.id}`,
          (source) => {
            const chat = catalog.byId.get(source.id);
            return !!(
              chat &&
              source.group === chatGroup(chat) &&
              chat.cwd === group.path &&
              !teamMembers.includes(chat.id) &&
              !teamMove.blocked() &&
              !organizationLock.current
            );
          },
          ({ id }) => teamMove.move(group, id, team.id),
        )}
        closed={closed}
        toggle={() => toggleProject(key)}
        edit={() =>
          setTeamDialog({
            path: group.path,
            id: team.id,
            action: "save",
          })
        }
        dissolve={() =>
          setTeamDialog({
            path: group.path,
            id: team.id,
            action: "delete",
          })
        }
      >
        <div className="peer-team-chats">
          {(runtime?.rooms || [])
            .filter(
              (r) =>
                r.radio?.teamId === team.id && (!closed || r.id === p.opened),
            )
            .map((r) => (
              <UnstyledButton
                key={r.id}
                className="peer-team-shared-chat"
                aria-current={r.id === p.opened ? "page" : undefined}
                onClick={() => p.open(r.id)}
              >
                Shared chat
              </UnstyledButton>
            ))}
          {(closed ? members.filter((a) => a.id === p.opened) : members).map(
            renderRow,
          )}
        </div>
      </PeerTeamGroup>
    );
  };
  const renderProjectChats = (
    group: NavigableProject & { chats: Agent[] },
    parent: ProjectFolder | null = null,
  ): ReactNode => {
    const folders = group.folders || [];
    const key = parent ? folderKey(group.path, parent.id) : group.path;
    const assigned = group.chats
      .filter((chat) => !grouped.has(chat.id))
      .filter((chat) =>
        parent
          ? chat.projectFolder === parent.id
          : !folders.some((folder) => folder.id === chat.projectFolder),
      );
    const shown =
      compactProjects[group.path] !== undefined
        ? assigned.length
        : projectLimits[key] || 5;
    const chats = assigned.slice(0, query ? assigned.length : shown);
    const selected = assigned.find((chat) => chat.id === p.opened);
    if (selected && !chats.includes(selected)) chats.push(selected);
    const children = folders
      .filter((folder) => (folder.parentId || null) === (parent?.id || null))
      .filter((folder) => {
        if (query || !compactProjects[group.path]) return true;
        const contains = (id: string, seen = new Set<string>()): boolean => {
          if (seen.has(id)) return false;
          seen.add(id);
          return (
            group.chats.some(
              (a) => a.projectFolder === id && !grouped.has(a.id),
            ) || folders.some((f) => f.parentId === id && contains(f.id, seen))
          );
        };
        return contains(folder.id);
      })
      .sort((a, b) => a.name.localeCompare(b.name));
    const siblingIds = itemIds(group, parent?.id || null);
    return (
      <>
        {[
          ...(!parent
            ? peerTeams
                .filter((t) => t.projectPath === group.path)
                .map((t) => ({
                  id: teamKey(group.path, t.id),
                  pinned: false,
                  node: renderPeerTeam(group, t),
                }))
            : []),
          ...children.map((folder) => {
            const id = folderKey(group.path, folder.id);
            const closed = collapsed[id] && !query;
            const occupied =
              folders.some((item) => item.parentId === folder.id) ||
              (byProject.get(group.path) || []).some(
                (chat) => chat.projectFolder === folder.id,
              );
            return {
              id,
              pinned: false,
              node: (
                <section
                  className="project-folder"
                  data-folder-id={folder.id}
                  data-sidebar-item={id}
                  key={folder.id}
                >
                  <div
                    className="project-tree-heading"
                    {...folderDrop(group, folder)}
                  >
                    <UnstyledButton
                      {...sorting.bindings(
                        itemGroup(group.path, parent?.id || null),
                        id,
                        siblingIds,
                      )}
                      aria-description="Drag to reorder. Alt + Up or Down also works."
                      className="project-tree-toggle"
                      aria-expanded={!closed}
                      onClick={() => toggleProject(id)}
                    >
                      {closed ? <Folder size={16} /> : <FolderOpen size={16} />}
                      <span>{folder.name}</span>
                    </UnstyledButton>
                    {!archive && (
                      <ActionIcon
                        className="project-tree-action"
                        aria-label={`New chat in folder ${folder.name}`}
                        disabled={p.creating}
                        onClick={() => {
                          if (requireProjectSupport())
                            p.newChat(group.path, folder.id);
                        }}
                      >
                        <Plus size={15} />
                      </ActionIcon>
                    )}
                    <Menu withinPortal position="bottom-end">
                      <Menu.Target>
                        <ActionIcon
                          className="project-tree-action"
                          aria-label={`Options for folder ${folder.name}`}
                        >
                          <MoreHorizontal size={15} />
                        </ActionIcon>
                      </Menu.Target>
                      <Menu.Dropdown>
                        <Menu.Item
                          onClick={() => editProject(group, "new", folder.id)}
                        >
                          New subfolder
                        </Menu.Item>
                        <Menu.Item onClick={() => editProject(group, folder)}>
                          Rename folder
                        </Menu.Item>
                        <Menu.Item
                          disabled={occupied}
                          onClick={async () => {
                            if (!requireProjectSupport()) return;
                            try {
                              await post("/api/projects", {
                                action: "remove_folder",
                                path: group.path,
                                folder_id: folder.id,
                                expected_revision:
                                  group.organizationRevision || 0,
                              });
                              await p.refresh?.();
                            } catch (error) {
                              p.notify?.(errorText(error));
                            }
                          }}
                        >
                          Remove empty folder
                        </Menu.Item>
                      </Menu.Dropdown>
                    </Menu>
                  </div>
                  {!closed && (
                    <div className="project-folder-chats">
                      {renderProjectChats(group, folder)}
                    </div>
                  )}
                </section>
              ),
            };
          }),
          ...chats.map((chat) => ({
            id: chat.id,
            pinned: !!chat.pinned,
            node: renderRow(chat),
          })),
        ]
          .sort(
            (a, b) =>
              Number(b.pinned) - Number(a.pinned) ||
              sorting.rank(
                a.pinned
                  ? chatGroup(chats.find((c) => c.id === a.id)!)
                  : itemGroup(group.path, parent?.id || null),
                a.id,
              ) -
                sorting.rank(
                  b.pinned
                    ? chatGroup(chats.find((c) => c.id === b.id)!)
                    : itemGroup(group.path, parent?.id || null),
                  b.id,
                ),
          )
          .map((item) => (
            <Fragment key={item.id}>{item.node}</Fragment>
          ))}
        {!chats.length &&
          !children.length &&
          (parent ||
            !visibleSharedRooms.some((r) => r.projectPath === group.path)) &&
          (parent ||
            !peerTeams.some((team) => team.projectPath === group.path)) && (
            <p className="project-empty">No chats</p>
          )}
        {!query && assigned.length > shown && (
          <UnstyledButton
            className="project-show-more"
            onClick={() =>
              setProjectLimits({ ...projectLimits, [key]: shown + 10 })
            }
          >
            Show more
          </UnstyledButton>
        )}
      </>
    );
  };
  const content = (
    <aside id="sidebar" aria-label="Conversations">
      <span className="sr-only" role="status">
        {sorting.announcement}
      </span>
      <div className="sidebar-header">
        <a className="brand" href="/">
          <span className="brand-name">
            <strong>Codex</strong> <span>Studio</span>
          </span>
        </a>
        {
          <ActionIcon aria-label="Close conversations" onClick={p.close}>
            <X size={20} />
          </ActionIcon>
        }
      </div>
      <div className="sidebar-nav">
        <div className="sidebar-primary-actions">
          <Button
            className="sidebar-nav-button sidebar-search"
            aria-label="Search chats"
            leftSection={<Search size={15} />}
            onClick={p.onSearch}
          >
            Search{" "}
            <kbd>
              {navigator.platform.toLowerCase().includes("mac")
                ? "⌘K"
                : "Ctrl+K"}
            </kbd>
          </Button>
          <ActionIcon
            className="sidebar-new-chat"
            aria-label="New chat"
            title="New chat"
            disabled={p.creating}
            onClick={() => p.newChat()}
          >
            <SquarePen size={16} />
          </ActionIcon>
        </div>
        {p.newSharedChat && (
          <Button
            className="sidebar-nav-button sidebar-shared-chat"
            title="New shared chat"
            leftSection={<Users size={15} />}
            onClick={() => p.newSharedChat?.()}
          >
            New shared chat
          </Button>
        )}
      </div>
      <TextInput
        id="chat-search"
        type="search"
        placeholder="Filter sidebar"
        aria-label="Filter projects and chats"
        leftSection={<Search size={15} />}
        value={query}
        onChange={(e) => {
          setQuery(e.target.value);
        }}
      />
      <div className="projects-heading">
        <UnstyledButton
          className="projects-heading-toggle"
          onClick={() => setProjectsOpen(!projectsOpen)}
          aria-expanded={projectsOpen}
        >
          {archive ? "Archived projects" : "Projects"}
          <ChevronDown size={14} />
        </UnstyledButton>
        <Menu position="bottom-end" withinPortal>
          <Menu.Target>
            <ActionIcon aria-label="Project list options">
              <MoreHorizontal size={16} />
            </ActionIcon>
          </Menu.Target>
          <Menu.Dropdown>
            <Menu.Item onClick={() => setArchive(!archive)}>
              {archive ? "Show active chats" : "Show archived chats"}
            </Menu.Item>
            <Menu.Item
              onClick={() => {
                setCollapsed({});
                save(projectKey, {});
                setProjectsOpen(true);
              }}
            >
              Expand all projects
            </Menu.Item>
          </Menu.Dropdown>
        </Menu>
        {
          <ActionIcon aria-label="Add project" onClick={p.addProject}>
            <Plus size={18} />
          </ActionIcon>
        }
      </div>
      {teamMove.feedback}
      <nav id="chat-list" className="chat-scroll" aria-label="Chats">
        {projectsOpen &&
          projectGroups.map((group) => {
            const isCollapsed = collapsed[group.path] && !query;
            return (
              <section
                className="sidebar-project"
                data-project-path={group.path}
                key={group.path}
              >
                <div className="project-tree-heading" {...folderDrop(group)}>
                  <UnstyledButton
                    {...sorting.bindings(
                      "projects",
                      group.path,
                      allProjectGroups.map((item) => item.path),
                    )}
                    aria-description="Drag to reorder. Alt + Up or Down also works."
                    className="project-tree-toggle"
                    title={group.path}
                    aria-expanded={!isCollapsed}
                    onClick={() => toggleProject(group.path)}
                  >
                    {isCollapsed ? (
                      <Folder size={18} />
                    ) : (
                      <FolderOpen size={18} />
                    )}
                    <span title={group.name}>{group.name}</span>
                  </UnstyledButton>
                  {!archive && (!compact || !!group.path) && (
                    <ActionIcon
                      className="project-tree-action"
                      aria-label={`New chat in ${group.name}`}
                      onClick={() => p.newChat(group.path || undefined)}
                      disabled={p.creating}
                    >
                      <Plus size={15} />
                    </ActionIcon>
                  )}
                  {!!group.path && (
                    <Menu withinPortal position="bottom-end">
                      <Menu.Target>
                        <ActionIcon
                          className="project-tree-action"
                          aria-label={`Options for project ${group.name}`}
                        >
                          <MoreHorizontal size={15} />
                        </ActionIcon>
                      </Menu.Target>
                      <Menu.Dropdown>
                        <Menu.Label>Create</Menu.Label>
                        <Menu.Item onClick={() => editProject(group, "new")}>
                          New chat folder
                        </Menu.Item>
                        {p.newSharedChat && (
                          <Menu.Item
                            onClick={() => p.newSharedChat?.(group.path)}
                          >
                            New shared chat
                          </Menu.Item>
                        )}
                        {runtime?.peerTeamsVersion === 1 && (
                          <Menu.Item
                            onClick={() =>
                              setTeamDialog({
                                path: group.path,
                                action: "save",
                              })
                            }
                          >
                            New team
                          </Menu.Item>
                        )}
                        <Menu.Divider />
                        <Menu.Label>View</Menu.Label>
                        <Menu.Item
                          title="Keep peer team chats, pinned chats, running chats, unread chats, and chats active in the last 24 hours. Search finds all chats."
                          aria-label={
                            compactProjects[group.path]
                              ? "Show all chats"
                              : "Compact project"
                          }
                          onClick={() =>
                            setCompactProject(
                              group.path,
                              !compactProjects[group.path],
                            )
                          }
                        >
                          {compactProjects[group.path]
                            ? "Show all chats"
                            : "Compact project"}
                          <small className="menu-action-help">
                            Show recent and active chats. Keep pinned, unread,
                            and team chats.
                          </small>
                        </Menu.Item>
                        <Menu.Divider />
                        <Menu.Label>Project settings</Menu.Label>
                        <Menu.Item onClick={() => editProject(group)}>
                          Rename project
                        </Menu.Item>
                        {p.projectFolders && (
                          <Menu.Item
                            onClick={() => p.projectFolders?.(group.path)}
                          >
                            Folders
                          </Menu.Item>
                        )}
                        <Menu.Item onClick={() => p.projectAccount(group.path)}>
                          Project account
                        </Menu.Item>
                        {group.registered && !group.hasChats && (
                          <Menu.Item
                            onClick={async () => {
                              try {
                                await post("/api/projects", {
                                  action: "remove",
                                  path: group.path,
                                });
                                await p.refresh?.();
                              } catch (error) {
                                p.notify?.(errorText(error));
                              }
                            }}
                          >
                            Remove from sidebar
                          </Menu.Item>
                        )}
                      </Menu.Dropdown>
                    </Menu>
                  )}
                </div>
                {teamDialog?.path === group.path &&
                  (teamDialog.id &&
                  !peerTeams.some((t) => t.id === teamDialog.id) ? (
                    <p role="alert">
                      This team is no longer available.{" "}
                      <UnstyledButton onClick={() => setTeamDialog(null)}>
                        Close
                      </UnstyledButton>
                    </p>
                  ) : (
                    <PeerTeamForm
                      key={`${teamDialog.path}:${teamDialog.id || "new"}:${teamDialog.action}`}
                      project={group}
                      team={peerTeams.find((t) => t.id === teamDialog.id)}
                      teams={peerTeams}
                      agents={agents}
                      action={teamDialog.action}
                      refresh={p.refresh}
                      close={() => setTeamDialog(null)}
                    />
                  ))}
                {!isCollapsed && (
                  <div className="project-chats">
                    {visibleSharedRooms
                      .filter((r) => r.projectPath === group.path)
                      .map((room) => (
                        <div
                          key={room.id}
                          className={`sidebar-row lead-row ${p.opened === room.id ? "selected" : ""}`}
                        >
                          <UnstyledButton
                            className="chat-row"
                            aria-current={
                              p.opened === room.id ? "page" : undefined
                            }
                            onClick={() => p.open(room.id)}
                          >
                            <span className="row-copy">
                              <strong>{room.name}</strong>
                            </span>
                          </UnstyledButton>
                        </div>
                      ))}
                    {renderProjectChats(group)}
                    {!query &&
                      compactProjects[group.path] &&
                      filtered.filter((a) => a.cwd === group.path).length >
                        group.chats.length && (
                        <UnstyledButton
                          className="project-show-all"
                          title="Keep peer team chats, pinned chats, running chats, unread chats, and chats active in the last 24 hours. Search finds all chats."
                          onClick={() => setCompactProject(group.path, false)}
                        >
                          Show all{" "}
                          {filtered.filter((a) => a.cwd === group.path).length}
                        </UnstyledButton>
                      )}
                  </div>
                )}
              </section>
            );
          })}
        {!filtered.length && !projectGroups.length && (
          <p className="notice">
            {query ? "No matching chats." : "No chats yet."}
          </p>
        )}
      </nav>
    </aside>
  );
  const dialogProject = dialog ? groupMap.get(dialog.path) : undefined;
  const dialogAgent = dialog?.agentId
    ? agents.find((item) => item.id === dialog.agentId)
    : undefined;
  const dialogFolder = dialogProject?.folders?.find(
    (item) => item.id === dialog?.folder,
  );
  const dialogAvailable =
    dialogProject &&
    (!dialog?.agentId || dialogAgent) &&
    (!dialog?.folder || dialog.folder === "new" || dialogFolder);
  return (
    <>
      {compact ? (
        <Drawer
          opened={p.mobile}
          onClose={p.close}
          size="min(360px, 100vw)"
          padding={0}
          withCloseButton={false}
          title="Conversations"
          classNames={{ header: "sr-only" }}
        >
          {content}
        </Drawer>
      ) : p.collapsed ? null : (
        content
      )}
      <Modal
        opened={!!conversion}
        onClose={closeConversion}
        title="Make chat a subagent"
        closeOnClickOutside={false}
      >
        {conversion && (
          <ConvertChatForm
            key={`${conversion.source.id}:${conversion.target.id}`}
            project={conversion.project}
            source={conversion.source}
            target={conversion.target}
            scope={p.data.stateDir}
            saved={async () => {
              await p.refresh?.();
              p.open(conversion.target.id);
              setConversion(null);
            }}
          />
        )}
      </Modal>
      <Modal
        opened={!!dialog}
        onClose={() => setDialog(null)}
        title={dialog?.title}
      >
        {dialog &&
          (dialogAvailable && dialogProject ? (
            dialogAgent ? (
              <MoveChatForm
                project={dialogProject}
                agent={dialogAgent}
                saved={savedProject}
              />
            ) : (
              <ProjectNameForm
                project={dialogProject}
                folder={dialog.folder === "new" ? "new" : dialogFolder}
                parentId={dialog.parentId}
                saved={savedProject}
              />
            )
          ) : (
            <p role="alert">
              This project, chat, or folder is no longer available.
            </p>
          ))}
      </Modal>
    </>
  );
}
