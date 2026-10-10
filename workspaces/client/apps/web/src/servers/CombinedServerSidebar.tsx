import { useMemo, useRef, useState } from "react";
import Sidebar from "../components/Sidebar";
import { ApiError, errorText, type PostBody } from "../api";
import type {
  SidebarBackend,
  SidebarTarget,
} from "../components/sidebar/services";
import type { MergedSidebar } from "./mergedSidebar";
import { createMergedSidebarServices, sidebarOwner } from "./sidebarServices";
import type { createSidebarRpcClient } from "./sidebarRpc";
import type { ServerCommand, ServerNavigation } from "./navigation";
import { createSidebarVisualSession } from "./sidebarVisualSession";

export default function CombinedServerSidebar({
  model,
  backends,
  online,
  rpc,
  aliases,
  current,
  navigation,
  send,
  applyNavigation,
  mobile,
  hidden,
  close,
  settings,
  search,
  notify,
  notice,
  unreadCount,
}: {
  model: MergedSidebar;
  backends: ReadonlyMap<string, SidebarBackend>;
  online: (owner: string) => boolean;
  rpc: ReturnType<typeof createSidebarRpcClient>;
  aliases: Record<string, string>;
  current: string;
  navigation: Record<string, ServerNavigation>;
  send: (owner: string, command: ServerCommand) => void;
  applyNavigation: (owner: string, navigation: ServerNavigation) => void;
  mobile: boolean;
  hidden: boolean;
  close: () => void;
  settings: () => void;
  search: () => void;
  notify: (message: string) => void;
  notice: string;
  unreadCount: number;
}) {
  const [marking, setMarking] = useState(new Set<string>());
  const visualSession = useRef(createSidebarVisualSession());
  const services = useMemo(() => {
    const result = createMergedSidebarServices(
      model,
      backends,
      online,
      aliases,
    );
    result.cache = visualSession.current(model, result.cache, (failure) =>
      notify(errorText(failure)),
    );
    return result;
  }, [model, backends, online, aliases]);
  const action = (
    target: SidebarTarget,
    command: (owner: string) => ServerCommand,
  ) => {
    try {
      const owner = sidebarOwner(model, target);
      if (!online(owner))
        throw new Error("The sidebar server is offline or unavailable.");
      send(owner, command(owner));
    } catch (failure) {
      notify(errorText(failure));
    }
  };
  const refresh = async () => {
    await Promise.all(
      model.sources
        .filter((source) => online(source.id))
        .map(async (source) => {
          const value = (await rpc.request(source.id, {
            action: "refresh",
          })) as ServerNavigation;
          applyNavigation(source.id, value);
        }),
    );
  };
  const opened = navigation[current]?.opened;
  return (
    <Sidebar
      services={services}
      data={model.data}
      opened={opened ? model.chatKey(current, opened) : null}
      selectionIdentity={JSON.stringify([
        current,
        opened,
        model.sourceById
          .get(current)
          ?.sidebar.threads.find((row) => row.id === opened)?.projectFolder ||
          null,
      ])}
      indicators={model.indicators}
      markingRead={marking}
      creating={!!navigation[current]?.creating}
      mobile={mobile}
      collapsed={hidden}
      close={close}
      settings={settings}
      onSearch={search}
      notify={notify}
      notice={notice}
      unreadCount={unreadCount}
      refresh={refresh}
      open={(id) => {
        try {
          const reference = model.requireReference(model.chatReferences, id);
          send(reference.owner, { action: "open", id: reference.id });
        } catch (failure) {
          notify(errorText(failure));
        }
      }}
      prepareChat={(id) => {
        const reference = model.chatReferences.get(id);
        if (!reference || !online(reference.owner)) return;
        void rpc
          .request(reference.owner, {
            action: "command",
            command: { action: "prepare-chat", id: reference.id },
          })
          .catch(() => {});
      }}
      newChat={(path, folder) => {
        if (!path) {
          if (online(current)) send(current, { action: "new-chat" });
          else notify("The sidebar server is offline.");
          return;
        }
        action({ kind: "project", path, folder }, (owner) => {
          const group = model.groups.get(path);
          const member = group?.members.find(
            (item) => item.source.id === owner,
          );
          const nativeFolder = folder
            ? member?.project.folders?.find(
                (item) =>
                  model.folderKey(owner, member.project.path || "", item.id) ===
                  folder,
              )?.id
            : undefined;
          if (folder && !nativeFolder)
            throw new Error("The folder is unavailable on this server.");
          return {
            action: "new-chat",
            path: model.wireProject(owner, path),
            ...(nativeFolder
              ? {
                  folder: nativeFolder,
                  projectId: group?.id,
                  projectServerId:
                    group?.home.project.homeServerId || group?.owner,
                }
              : {}),
          };
        });
      }}
      newSharedChat={(path) =>
        path
          ? action({ kind: "project", path }, (owner) => ({
              action: "new-shared-chat",
              path: model.wireProject(owner, path),
            }))
          : online(current)
            ? send(current, { action: "new-shared-chat" })
            : notify("The sidebar server is offline.")
      }
      addProject={() =>
        online(current)
          ? send(current, { action: "add-project" })
          : notify("The sidebar server is offline.")
      }
      changeProject={(agent) =>
        action({ kind: "chat", id: agent.id }, () => ({
          action: "change-project",
          id: model.requireReference(model.chatReferences, agent.id).id,
        }))
      }
      projectFolders={(path) =>
        action({ kind: "project", path }, (owner) => ({
          action: "project-folders",
          projectId: model.wireProject(owner, path),
        }))
      }
      projectAccount={(path) =>
        action({ kind: "project", path }, (owner) => ({
          action: "project-account",
          path: model.wireProject(owner, path),
        }))
      }
      rename={async (id, name) => {
        const backend = services.owner({ kind: "chat", id });
        const reference = model.requireReference(model.chatReferences, id);
        const key = `studio-sidebar-rename:${reference.id}`;
        type Request = { body: PostBody<"/api/rename">; acknowledged: boolean };
        const request = backend.saved<Request | null>(key, null) || {
          body: { id, name, request_id: crypto.randomUUID() },
          acknowledged: false,
        };
        try {
          if (request.body.name !== name)
            throw new Error("Retry the saved name before changing it.");
          backend.storage.setItem(key, JSON.stringify(request));
          if (!request.acknowledged) {
            await backend.post("/api/rename", request.body);
            request.acknowledged = true;
            backend.storage.setItem(key, JSON.stringify(request));
          }
          await refresh();
          backend.storage.removeItem(key);
        } catch (failure) {
          if (
            !request.acknowledged &&
            failure instanceof ApiError &&
            failure.status >= 400 &&
            failure.status < 500 &&
            failure.status !== 408
          )
            backend.storage.removeItem(key);
          notify(errorText(failure));
          throw failure;
        }
      }}
      remove={(id, room) =>
        action({ kind: "chat", id }, () => ({
          action: "remove-chat",
          id: model.requireReference(model.chatReferences, id).id,
          room,
        }))
      }
      markUnread={(agent) => {
        const reference = model.chatReferences.get(agent.id);
        if (
          !reference ||
          !online(reference.owner) ||
          !agent.threadId ||
          !agent.lastCompletedTurn
        )
          return;
        setMarking((old) => new Set([...old, agent.id]));
        void rpc
          .request(reference.owner, {
            action: "command",
            command: {
              action: "mark-unread",
              id: reference.id,
              threadId: agent.threadId,
              turnId: agent.lastCompletedTurn,
            },
          })
          .then(refresh)
          .catch((failure) => notify(errorText(failure)))
          .finally(() =>
            setMarking((old) => {
              const next = new Set(old);
              next.delete(agent.id);
              return next;
            }),
          );
      }}
    />
  );
}
