import { ActionIcon, Button, Menu, Modal, UnstyledButton } from "@mantine/core";
import { FolderOpen, MoreHorizontal, Plus } from "lucide-react";
import { useMemo, useState } from "react";
import { saved, save } from "../api";
import ProjectServerCards from "../components/ProjectServerCards";
import ProjectChatRows from "./ProjectChatRows";
import { logicalProjects, type LogicalProject } from "./logicalProjects";
import { projectServerChoices } from "./projectLocations";
import type { ServerCommand, ServerNavigation } from "./navigation";
import type { StudioServer } from "./registry";
import type { ResourceConnectionState } from "../sync/resourceEvents";

export default function ProjectGroupsSidebar({
  navigation,
  servers,
  statuses,
  current,
  query,
  send,
}: {
  navigation: Record<string, ServerNavigation>;
  servers: StudioServer[];
  statuses: Record<string, ResourceConnectionState>;
  current: string;
  query: string;
  send: (server: string, command: ServerCommand) => void;
}) {
  const groups = useMemo(() => logicalProjects(navigation), [navigation]);
  const [collapsed, setCollapsed] = useState<Record<string, boolean>>(() =>
    saved("studio-logical-project-collapsed", {}),
  );
  const [choosing, setChoosing] = useState<LogicalProject | null>(null);
  const [compact, setCompact] = useState<Record<string, boolean>>(() =>
    saved("studio-logical-project-compact", {}),
  );
  const choices = projectServerChoices(servers, statuses);
  const aliases = Object.fromEntries(
    servers.map((server) => [
      server.id,
      (server as StudioServer & { alias?: string }).alias ||
        (server.id === "local"
          ? "MAC"
          : server.label
              .replace(/[^a-z]/gi, "")
              .slice(0, 3)
              .toUpperCase()),
    ]),
  );
  const needle = query.trim().toLocaleLowerCase();
  const lastServer = (group: LogicalProject) =>
    saved<string>(`project-last-server:${group.key}`, "") ||
    [...group.chats].sort(
      (a, b) => (b.updated || b.created || 0) - (a.updated || a.created || 0),
    )[0]?.serverId;
  return (
    <>
      {groups.map((project) => {
        const chats = project.chats.filter(
          (chat) =>
            !chat.archived &&
            (!needle ||
              `${project.name} ${chat.name}`
                .toLocaleLowerCase()
                .includes(needle)),
        );
        if (
          needle &&
          !chats.length &&
          !project.name.toLocaleLowerCase().includes(needle)
        )
          return null;
        return (
          <section
            className="sidebar-project"
            key={project.key}
            data-project-path={project.path}
          >
            <div className="project-tree-heading">
              <UnstyledButton
                className="project-tree-toggle"
                title={project.name}
                aria-expanded={!collapsed[project.key]}
                onClick={() => {
                  const next = {
                    ...collapsed,
                    [project.key]: !collapsed[project.key],
                  };
                  setCollapsed(next);
                  save("studio-logical-project-collapsed", next);
                }}
              >
                <FolderOpen size={18} />
                <span>{project.name}</span>
              </UnstyledButton>
              <ActionIcon
                className="project-tree-action"
                aria-label={`New chat in ${project.name}`}
                onClick={() => setChoosing(project)}
              >
                <Plus size={15} />
              </ActionIcon>
              <Menu withinPortal position="bottom-end">
                <Menu.Target>
                  <ActionIcon
                    className="project-tree-action"
                    aria-label={`Options for project ${project.name}`}
                  >
                    <MoreHorizontal size={15} />
                  </ActionIcon>
                </Menu.Target>
                <Menu.Dropdown>
                  <Menu.Label>Project settings</Menu.Label>
                  <Menu.Item
                    onClick={() =>
                      send(project.owner, {
                        action: "project-folders",
                        projectId: project.id,
                      })
                    }
                  >
                    Folders
                  </Menu.Item>
                </Menu.Dropdown>
              </Menu>
            </div>
            {!collapsed[project.key] && (
              <ProjectChatRows
                chats={chats.map((chat) => ({
                  ...chat,
                  provider: chat.provider || undefined,
                  updated: chat.updated || undefined,
                  created: chat.created || undefined,
                }))}
                aliases={aliases}
                selected={{
                  id: navigation[current]?.opened || "",
                  serverId: current,
                }}
                compact={compact[project.key] ?? project.compact ?? true}
                query={query}
                setCompact={(value) => {
                  const next = { ...compact, [project.key]: value };
                  setCompact(next);
                  save("studio-logical-project-compact", next);
                }}
                open={(chat) =>
                  send(chat.serverId || project.owner, {
                    action: "open",
                    id: chat.id,
                  })
                }
              />
            )}
            {!collapsed[project.key] && !chats.length && (
              <Button
                size="xs"
                variant="subtle"
                onClick={() => setChoosing(project)}
              >
                New chat
              </Button>
            )}
          </section>
        );
      })}
      <Modal
        opened={!!choosing}
        onClose={() => setChoosing(null)}
        title="New chat"
        aria-label="New chat"
        size="lg"
      >
        {choosing && (
          <ProjectServerCards
            project={choosing}
            servers={choices}
            lastServer={lastServer(choosing)}
            onChoose={(location) => {
              save(`project-last-server:${choosing.key}`, location.serverId);
              send(location.serverId, {
                action: "new-chat",
                path: location.path,
                projectId: choosing.id,
                projectServerId: choosing.homeServerId || choosing.owner,
              });
              setChoosing(null);
            }}
            onAdd={(server) => {
              send(choosing.owner, {
                action: "project-folders",
                projectId: choosing.id,
                server,
              });
              setChoosing(null);
            }}
          />
        )}
      </Modal>
    </>
  );
}
