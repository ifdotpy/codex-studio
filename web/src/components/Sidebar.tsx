import { useEffect, useRef, useState, type ReactNode } from "react";
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
  Archive,
  Pin,
  PinOff,
  ArchiveRestore,
  Check,
  FolderOpen,
  MoreHorizontal,
  Pencil,
  Search,
  Folder,
  ChevronDown,
  Plus,
  Trash2,
  X,
} from "lucide-react";
import { api, errorText, save, saved } from "../api";
import "./sidebar-projects.css";
import { useSidebarOrder } from "./useSidebarOrder";
import {
  ProjectNameForm,
  MoveChatForm,
  folderLabel,
  type Project,
  type ProjectFolder,
} from "./ProjectOrganization";
import { busy, type Agent, type Snapshot } from "../types";
type Props = {
  data: Snapshot;
  opened: string | null;
  lead?: Agent;
  open: (id: string) => void;
  newChat: (path?: string, folder?: string) => void;
  addProject: () => void;
  changeProject: (agent: Agent) => void;
  projectAccount: (path: string) => void;
  creating: boolean;
  rename: (id: string, name: string) => Promise<void>;
  remove: (id: string, room: boolean) => void;
  mobile: boolean;
  collapsed?: boolean;
  onSearch: () => void;
  close: () => void;
  refresh?: () => Promise<void>;
  notify?: (message: string) => void;
};
export default function Sidebar(p: Props) {
  const compact = useMediaQuery("(max-width: 760px)");
  const [query, setQuery] = useState(""),
    [renaming, setRenaming] = useState<string | null>(null),
    [name, setName] = useState(""),
    [archive, setArchive] = useState(false),
    [projectsOpen, setProjectsOpen] = useState(true);
  const sorting = useSidebarOrder(
    `codex-sidebar-order:${p.data.stateDir}`,
    p.notify,
  );
  const [organizing, setOrganizing] = useState<string | null>(null);
  const organizationLock = useRef(false);
  const [dialog, setDialog] = useState<{
    title: string;
    path: string;
    folder?: string;
    parentId?: string;
    agentId?: string;
  } | null>(null);
  const canOrganizeProjects = !!p.data.runtime.projectOrganizationVersion;
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
    project: Project,
    folder?: ProjectFolder | "new",
    parentId?: string,
  ) => {
    if (!requireProjectSupport()) return;
    setDialog({
      title:
        folder === "new"
          ? "New folder"
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
  const organize = async (id: string, data: Record<string, unknown>) => {
    if (organizationLock.current) return false;
    organizationLock.current = true;
    setOrganizing(id);
    try {
      await api("/api/organization", { id, ...data });
      await p.refresh?.();
      return true;
    } catch (error) {
      p.notify?.(errorText(error));
      return false;
    } finally {
      organizationLock.current = false;
      setOrganizing(null);
    }
  };
  const agents = p.data.threads.filter(
    (a) => a.source === "managed" && a.isLead,
  );
  const selectedFolder = agents.find((a) => a.id === p.opened)?.projectFolder;
  useEffect(() => {
    if (agents.some((a) => a.id === p.opened)) {
      setQuery("");
      setProjectsOpen(true);
      const path = agents.find((a) => a.id === p.opened)?.cwd;
      if (path && collapsed[path]) {
        const next = { ...collapsed, [path]: false };
        setCollapsed(next);
        save(projectKey, next);
      }
      const folders =
        p.data.runtime.projects?.find((project) => project.path === path)
          ?.folders || [];
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
  const chatGroup = (a: Agent) =>
    JSON.stringify([
      "chats",
      a.cwd,
      !!a.pinned,
      !!a.archived,
      ...(a.projectFolder ? [a.projectFolder] : []),
    ]);
  const folderDrop = (project: Project, folder: ProjectFolder | null = null) =>
    sorting.dropBindings(
      folder ? folderKey(project.path, folder.id) : project.path,
      (source) => {
        const chat = agents.find((a) => a.id === source.id);
        return !!(
          canOrganizeProjects &&
          !organizationLock.current &&
          chat &&
          source.group === chatGroup(chat) &&
          chat.cwd === project.path &&
          (chat.projectFolder || null) !== (folder?.id || null)
        );
      },
      ({ id }) => {
        const chat = agents.find((a) => a.id === id)!;
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
  const orderedAgents = [...agents].sort(
    (a, b) =>
      Number(!!b.pinned) - Number(!!a.pinned) ||
      sorting.rank(chatGroup(a), a.id) - sorting.rank(chatGroup(b), b.id) ||
      (("updated" in b ? b.updated : b.created) || 0) -
        (("updated" in a ? a.updated : a.created) || 0),
  );
  const filtered = orderedAgents
    .filter((row) => !!row.archived === archive)
    .filter((a) => {
      const project = p.data.runtime.projects?.find(
        (item) => item.path === a.cwd,
      );
      return `${a.name} ${a.cwd || ""} ${project?.name || a.project || ""} ${folderLabel(project?.folders || [], a.projectFolder || "")} ${a.tail || ""}`
        .toLowerCase()
        .includes(query.toLowerCase());
    });
  const rename = async (e: React.FormEvent, id: string) => {
    e.preventDefault();
    try {
      await p.rename(id, name);
      setRenaming(null);
    } catch {
      /* Keep the draft when the server rejects the name. */
    }
  };
  const groupMap = new Map<
    string,
    Project & {
      chats: Agent[];
      registered: boolean;
      hasChats: boolean;
    }
  >();
  for (const project of p.data.runtime.projects || [])
    groupMap.set(project.path, {
      ...project,
      chats: [],
      registered: true,
      hasChats: agents.some((a) => a.cwd === project.path),
    });
  for (const a of agents) {
    const path = a.cwd || "";
    if (!groupMap.has(path))
      groupMap.set(path, {
        id: path,
        created: 0,
        path,
        name: path.split("/").filter(Boolean).at(-1) || "Other chats",
        chats: [],
        registered: false,
        hasChats: true,
      });
  }
  for (const a of filtered) groupMap.get(a.cwd || "")!.chats.push(a);
  const allProjectGroups = [...groupMap.values()].sort(
    (a, b) =>
      sorting.rank("projects", a.path) - sorting.rank("projects", b.path) ||
      a.name.localeCompare(b.name),
  );
  const projectGroups = allProjectGroups.filter(
    (group) =>
      !query ||
      group.chats.length ||
      `${group.path} ${group.name}`.toLowerCase().includes(query.toLowerCase()),
  );
  const renderRow = (row: Agent) => {
    const a = row;
    return (
      <div
        className={`sidebar-row lead-row ${p.opened === row.id ? "selected" : ""}`}
        key={row.id}
      >
        <UnstyledButton
          {...sorting.bindings(
            chatGroup(row),
            row.id,
            orderedAgents
              .filter((a) => chatGroup(a) === chatGroup(row))
              .map((a) => a.id),
          )}
          title={row.name}
          aria-description="Drag to reorder or move to a folder. Alt + Up or Down reorders this chat."
          className="chat-row"
          data-chat={row.id}
          onClick={() => p.open(row.id)}
          aria-current={p.opened === row.id}
        >
          <span className="row-copy">
            <strong>
              {a?.pinned && <Pin size={11} className="chat-pin" />}
              {row.name}
            </strong>
          </span>
          {busy.has(a.status) && (
            <span className="row-meta">
              <span className="dot running" />
            </span>
          )}
        </UnstyledButton>
        {renaming !== row.id && (
          <ActionIcon
            className="row-pin-action"
            aria-label={`${row.pinned ? "Unpin" : "Pin"} ${row.name}`}
            title={row.pinned ? "Unpin chat" : "Pin chat"}
            disabled={organizing === row.id}
            onClick={() => void organize(row.id, { pinned: !row.pinned })}
          >
            {row.pinned ? <PinOff size={14} /> : <Pin size={14} />}
          </ActionIcon>
        )}
        {renaming === row.id ? (
          <form
            className="inline-rename"
            onSubmit={(e) => void rename(e, row.id)}
          >
            <TextInput
              aria-label="Chat name"
              value={name}
              onChange={(e) => setName(e.target.value)}
              autoFocus
              maxLength={80}
              onKeyDown={(e) => {
                if (e.key === "Escape") setRenaming(null);
              }}
            />
            <ActionIcon type="submit" aria-label="Save name">
              <Check size={16} />
            </ActionIcon>
          </form>
        ) : (
          <Menu position="bottom-end" withinPortal shadow="lg" width={170}>
            <Menu.Target>
              <ActionIcon
                className="row-actions"
                aria-label={`Actions for ${row.name}`}
              >
                <MoreHorizontal size={16} />
              </ActionIcon>
            </Menu.Target>
            <Menu.Dropdown>
              {a.cwd && (
                <Menu.Item
                  leftSection={<Folder size={14} />}
                  onClick={() => {
                    if (!requireProjectSupport()) return;
                    const project = groupMap.get(a.cwd || "")!;
                    setDialog({
                      title: "Move chat",
                      path: project.path,
                      agentId: a.id,
                    });
                  }}
                >
                  Move to folder
                </Menu.Item>
              )}
              {a && (
                <>
                  <Menu.Item
                    leftSection={
                      a.pinned ? <PinOff size={14} /> : <Pin size={14} />
                    }
                    onClick={() => void organize(a.id, { pinned: !a.pinned })}
                  >
                    {a.pinned ? "Unpin" : "Pin"}
                  </Menu.Item>
                  {!compact && (
                    <Menu.Item
                      leftSection={<FolderOpen size={14} />}
                      disabled={!!a.threadId || !!a.inFlight}
                      onClick={() => p.changeProject(a)}
                    >
                      Change project folder
                    </Menu.Item>
                  )}
                  <Menu.Item
                    leftSection={
                      a.archived ? (
                        <ArchiveRestore size={14} />
                      ) : (
                        <Archive size={14} />
                      )
                    }
                    disabled={!!a.inFlight}
                    onClick={() =>
                      void organize(a.id, { archived: !a.archived })
                    }
                  >
                    {a.archived ? "Restore chat" : "Archive"}
                  </Menu.Item>
                  <Menu.Divider />
                </>
              )}
              <Menu.Item
                leftSection={<Pencil size={14} />}
                onClick={() => {
                  setName(row.name);
                  setRenaming(row.id);
                }}
              >
                Rename
              </Menu.Item>
              <Menu.Item
                color="red"
                leftSection={<Trash2 size={14} />}
                onClick={() => p.remove(row.id, false)}
              >
                Delete
              </Menu.Item>
            </Menu.Dropdown>
          </Menu>
        )}
      </div>
    );
  };
  const renderProjectChats = (
    group: Project & { chats: Agent[] },
    parent: ProjectFolder | null = null,
  ): ReactNode => {
    const folders = group.folders || [];
    const key = parent ? folderKey(group.path, parent.id) : group.path;
    const assigned = group.chats.filter((chat) =>
      parent
        ? chat.projectFolder === parent.id
        : !folders.some((folder) => folder.id === chat.projectFolder),
    );
    const shown = projectLimits[key] || 5;
    const chats = assigned.slice(0, query ? assigned.length : shown);
    const selected = assigned.find((chat) => chat.id === p.opened);
    if (selected && !chats.includes(selected)) chats.push(selected);
    const children = folders
      .filter((folder) => (folder.parentId || null) === (parent?.id || null))
      .sort((a, b) => a.name.localeCompare(b.name));
    return (
      <>
        {children.map((folder) => {
          const id = folderKey(group.path, folder.id);
          const closed = collapsed[id] && !query;
          const occupied =
            folders.some((item) => item.parentId === folder.id) ||
            agents.some(
              (chat) =>
                chat.cwd === group.path && chat.projectFolder === folder.id,
            );
          return (
            <section
              className="project-folder"
              data-folder-id={folder.id}
              key={folder.id}
            >
              <div
                className="project-tree-heading"
                {...folderDrop(group, folder)}
              >
                <UnstyledButton
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
                          await api("/api/projects", {
                            action: "remove_folder",
                            path: group.path,
                            folder_id: folder.id,
                            expected_revision: group.organizationRevision || 0,
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
          );
        })}
        {chats.map(renderRow)}
        {!chats.length && !children.length && (
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
      <Button
        className="sidebar-search"
        leftSection={<Search size={15} />}
        onClick={p.onSearch}
      >
        Search chats
      </Button>
      <Button
        leftSection={<Plus size={15} />}
        disabled={p.creating}
        onClick={() => p.newChat()}
      >
        New chat
      </Button>
      <TextInput
        id="chat-search"
        type="search"
        placeholder="Filter projects and chats"
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
                    aria-description="Drag to reorder. Alt + Up or Down moves this project."
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
                    <span>{group.name}</span>
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
                        <Menu.Item onClick={() => editProject(group)}>
                          Rename project
                        </Menu.Item>
                        <Menu.Item onClick={() => editProject(group, "new")}>
                          New folder
                        </Menu.Item>
                        <Menu.Item onClick={() => p.projectAccount(group.path)}>
                          Project account
                        </Menu.Item>
                        {group.registered && !group.hasChats && (
                          <Menu.Item
                            onClick={async () => {
                              try {
                                await api("/api/projects", {
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
                {!isCollapsed && (
                  <div className="project-chats">
                    {renderProjectChats(group)}
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
