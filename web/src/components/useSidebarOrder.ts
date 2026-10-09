import { serverLocalStorage as localStorage } from "../servers/storage";
import {
  useEffect,
  useRef,
  useState,
  type DragEvent,
  type KeyboardEvent,
} from "react";
import { post, ApiError, errorText, saved, save, type PostBody } from "../api";

import type { components } from "../generated/api";

type Order = Record<string, string[]>;
type Drag = { group: string; id: string };
type ServerOrder = components["schemas"]["SidebarOrderDto"];
type Pending = {
  body: Extract<PostBody<"/api/projects">, { action: "reorder" }>;
};
export function useSidebarOrder(
  key: string,
  server: ServerOrder | undefined,
  refresh?: () => Promise<void>,
  notify?: (text: string) => void,
  sessionToken?: string,
) {
  const [order, setOrder] = useState<Order>(
    () => server?.groups || saved(key, {}),
  );
  const revision = useRef(-1);
  const busy = useRef(false);
  const pendingKey = `${key}:pending`;
  const drag = useRef<Drag | null>(null);
  const [target, setTarget] = useState<(Drag & { after: boolean }) | null>(
    null,
  );
  const [announcement, announce] = useState("");
  const [destination, setDestination] = useState<string | null>(null);
  useEffect(() => {
    revision.current = -1;
    setOrder(saved(key, {}));
  }, [key]);
  useEffect(() => {
    if (!server || !sessionToken) return;
    if (server.revision > revision.current) {
      revision.current = server.revision;
      if (server.groups !== null) {
        setOrder(server.groups);
        save(key, server.groups);
      }
    }
    if (busy.current) return;
    const pending = saved<Pending | null>(pendingKey, null);
    if (pending?.body) {
      void send(pending);
    } else if (server.groups === null) {
      void send({
        body: {
          action: "reorder",
          request_id: crypto.randomUUID(),
          expected_revision: server.revision,
          groups: saved(key, {}),
          migration: true,
        },
      });
    }
    // The server revision controls this effect. The request keeps its exact body.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [key, server?.revision, server?.groups, sessionToken]);
  const send = async (request: Pending, optimistic?: Order) => {
    if (busy.current) return false;
    busy.current = true;
    try {
      localStorage.setItem(pendingKey, JSON.stringify(request));
      if (optimistic) setOrder(optimistic);
      const result = await post("/api/projects", request.body, {
        timeoutMs: 15000,
        ...(sessionToken ? { sessionToken } : {}),
      });
      if (!("revision" in result))
        throw new Error("Invalid sidebar order response");
      localStorage.removeItem(pendingKey);
      if (result.revision >= revision.current) {
        revision.current = result.revision;
        setOrder(result.groups || {});
        save(key, result.groups || {});
      }
      void refresh?.().catch((error) =>
        notify?.(`Could not refresh sidebar order: ${errorText(error)}`),
      );
      return true;
    } catch (error) {
      if (
        error instanceof ApiError &&
        error.status >= 400 &&
        error.status < 500 &&
        error.status !== 408
      ) {
        localStorage.removeItem(pendingKey);
        setOrder(server?.groups || {});
        notify?.(
          error.status === 409
            ? "Sidebar order changed in another client. Refreshing from the server."
            : `Could not save sidebar order: ${errorText(error)}`,
        );
        try {
          await refresh?.();
        } catch (refreshError) {
          notify?.(
            `Could not refresh sidebar order: ${errorText(refreshError)}`,
          );
        }
      } else {
        notify?.(`Could not save sidebar order: ${errorText(error)}`);
      }
      return false;
    } finally {
      busy.current = false;
    }
  };
  const rank = (group: string, id: string) => {
    const index = order[group]?.indexOf(id) ?? -1;
    return index < 0 ? Number.MAX_SAFE_INTEGER : index;
  };
  const move = (
    group: string,
    ids: string[],
    from: string,
    to: string,
    after: boolean,
  ) => {
    if (from === to || !ids.includes(from) || !ids.includes(to)) return;
    const next = ids.filter((id) => id !== from);
    next.splice(next.indexOf(to) + Number(after), 0, from);
    if (!sessionToken || !server || server.groups === null || busy.current)
      return;
    if (saved<Pending | null>(pendingKey, null)) {
      notify?.(
        "Retry the saved sidebar order request before moving another item.",
      );
      return;
    }
    const value = { ...order, [group]: next };
    void send(
      {
        body: {
          action: "reorder",
          request_id: crypto.randomUUID(),
          expected_revision: revision.current,
          groups: value,
        },
      },
      value,
    ).then((saved) => {
      if (saved)
        announce(
          `Moved to position ${next.indexOf(from) + 1} of ${next.length}`,
        );
    });
  };
  const finish = () => {
    drag.current = null;
    setTarget(null);
    setDestination(null);
  };
  const dropBindings = (
    key: string,
    accepts: (source: Drag, event: DragEvent<HTMLElement>) => boolean,
    dropped: (source: Drag) => void,
    fallback: ReturnType<typeof bindings> | Record<string, never> = {},
  ) => ({
    ...fallback,
    "data-folder-drop": destination === key ? "true" : undefined,
    onDragOver: (event: DragEvent<HTMLElement>) => {
      if (!drag.current || !accepts(drag.current, event)) {
        fallback.onDragOver?.(event);
        return;
      }
      event.preventDefault();
      event.stopPropagation();
      event.dataTransfer.dropEffect = "move";
      setTarget(null);
      setDestination(key);
    },
    onDragLeave: (event: DragEvent<HTMLElement>) => {
      if (!event.currentTarget.contains(event.relatedTarget as Node | null))
        setDestination(null);
      fallback.onDragLeave?.(event);
    },
    onDrop: (event: DragEvent<HTMLElement>) => {
      if (!drag.current || !accepts(drag.current, event)) {
        fallback.onDrop?.(event);
        return;
      }
      event.preventDefault();
      event.stopPropagation();
      const source = drag.current;
      finish();
      dropped(source);
    },
  });
  const bindings = (group: string, id: string, ids: string[]) => ({
    draggable: true,
    "data-sidebar-group": group,
    "data-sidebar-id": id,
    "data-drop-edge":
      target?.group === group && target.id === id
        ? target.after
          ? "after"
          : "before"
        : undefined,
    onDragStart: (event: DragEvent<HTMLElement>) => {
      event.stopPropagation();
      drag.current = { group, id };
      event.dataTransfer.effectAllowed = "move";
      event.dataTransfer.setData("application/x-studio-sidebar", id);
    },
    onDragOver: (event: DragEvent<HTMLElement>) => {
      if (drag.current?.group !== group || drag.current.id === id) return;
      event.preventDefault();
      event.stopPropagation();
      event.dataTransfer.dropEffect = "move";
      setDestination(null);
      const box = event.currentTarget.getBoundingClientRect();
      const after = event.clientY >= box.top + box.height / 2;
      setTarget((old) =>
        old?.group === group && old.id === id && old.after === after
          ? old
          : { group, id, after },
      );
    },
    onDragLeave: (event: DragEvent<HTMLElement>) => {
      if (!event.currentTarget.contains(event.relatedTarget as Node | null))
        setTarget(null);
    },
    onDrop: (event: DragEvent<HTMLElement>) => {
      if (drag.current?.group !== group) return;
      event.preventDefault();
      event.stopPropagation();
      const box = event.currentTarget.getBoundingClientRect();
      move(
        group,
        ids,
        drag.current.id,
        id,
        event.clientY >= box.top + box.height / 2,
      );
      finish();
    },
    onDragEnd: finish,
    onKeyDown: (event: KeyboardEvent<HTMLElement>) => {
      if (
        !event.altKey ||
        event.ctrlKey ||
        event.metaKey ||
        event.shiftKey ||
        !["ArrowUp", "ArrowDown"].includes(event.key)
      )
        return;
      event.preventDefault();
      const after = event.key === "ArrowDown";
      const visible = Array.from(
        event.currentTarget
          .closest("#chat-list")
          ?.querySelectorAll<HTMLElement>("[data-sidebar-group]") || [],
      )
        .filter(
          (element) =>
            element.dataset.sidebarGroup === group &&
            element.getClientRects().length,
        )
        .map((element) => element.dataset.sidebarId!);
      const neighbors = visible.length ? visible : ids;
      const to = neighbors[neighbors.indexOf(id) + (after ? 1 : -1)];
      if (to) move(group, ids, id, to, after);
    },
  });
  return { order, rank, bindings, dropBindings, announcement, announce };
}
