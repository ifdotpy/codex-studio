import { outboxRequests } from "../browserPersistence";
import { randomUUID } from "../uuid";
import {
  localSidebarServices,
  type SidebarBackend,
  type SidebarServices,
} from "./sidebar/services";
import {
  useEffect,
  useRef,
  useState,
  type DragEvent,
  type KeyboardEvent,
} from "react";
import { ApiError, errorText, untilAnswered, type PostBody } from "../api";

import type { components } from "../generated/api";

type Order = Record<string, string[]>;
type Drag = { group: string; id: string };
type ServerOrder = components["schemas"]["SidebarOrderDto"];
type SidebarOrderResponse = components["schemas"]["SidebarOrderResponse"];
type Pending = {
  ownerId?: string;
  body: Extract<PostBody<"/api/projects">, { action: "reorder" }>;
};

function isSidebarOrderResponse(value: unknown): value is SidebarOrderResponse {
  if (typeof value !== "object" || value === null || Array.isArray(value))
    return false;
  const record = value as Record<string, unknown>;
  if (typeof record.revision !== "number" || !Number.isFinite(record.revision))
    return false;
  if (record.groups === null) return true;
  if (typeof record.groups !== "object" || Array.isArray(record.groups))
    return false;
  return Object.values(record.groups).every(
    (items) =>
      Array.isArray(items) && items.every((item) => typeof item === "string"),
  );
}

export function sidebarOrderCanWrite(
  sessionToken: string | undefined,
  available: boolean | undefined,
): boolean {
  return available !== false && (!!sessionToken || available === true);
}

export function useSidebarOrder(
  key: string,
  server: ServerOrder | undefined,
  refresh?: () => Promise<void>,
  notify?: (text: string) => void,
  services: SidebarServices = localSidebarServices,
  sessionToken?: string,
) {
  const outbox = outboxRequests(services.cache.storage);
  const prefix = outbox.key("sidebarOrder", "");
  if (!key.startsWith(prefix)) throw new Error("Invalid sidebar order scope.");
  const pendingSuffix = `${key.slice(prefix.length)}:pending`;
  const readPending = (): Pending | null => {
    try {
      return JSON.parse(outbox.get("sidebarOrder", pendingSuffix) || "null");
    } catch {
      return null;
    }
  };
  const requestBackend = useRef<SidebarBackend | null>(null);
  const [order, setOrder] = useState<Order>(() => server?.groups || {});
  const revision = useRef(-1);
  const busy = useRef(false);
  // The newest server order. A move keeps its own order on screen until its request ends.
  const latestServer = useRef(server);
  latestServer.current = server;
  const adoptServerOrder = (value: ServerOrder | undefined) => {
    if (!value || value.revision <= revision.current) return;
    revision.current = value.revision;
    if (value.groups !== null) {
      setOrder(value.groups);
    }
  };
  const drag = useRef<Drag | null>(null);
  const [target, setTarget] = useState<(Drag & { after: boolean }) | null>(
    null,
  );
  const [announcement, announce] = useState("");
  const [destination, setDestination] = useState<string | null>(null);
  useEffect(() => {
    revision.current = -1;
    setOrder(server?.groups || {});
  }, [key]);
  useEffect(() => {
    const migrationTarget = { kind: "order" as const, group: "projects" };
    if (
      !server ||
      !sidebarOrderCanWrite(sessionToken, services.available?.(migrationTarget))
    )
      return;
    if (busy.current) return;
    adoptServerOrder(server);
    const pending = readPending();
    if (pending?.body) void send(pending);
    // The server revision controls this effect. The request keeps its exact body.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [key, server?.revision, server?.groups, sessionToken, services]);
  const send = async (request: Pending, optimistic?: Order) => {
    if (busy.current) return false;
    busy.current = true;
    let clearPending = false;
    try {
      outbox.set("sidebarOrder", pendingSuffix, JSON.stringify(request));
      if (optimistic) setOrder(optimistic);
      const backend =
        requestBackend.current ||
        services.byOwner(request.ownerId || services.cache.ownerId);
      requestBackend.current = backend;
      // The request ID makes a repeated reorder return the first receipt.
      const result = await untilAnswered(() =>
        backend.post("/api/projects", request.body, { timeoutMs: 30000 }),
      );
      if (!isSidebarOrderResponse(result))
        throw new Error("Invalid sidebar order response");
      if (result.revision >= revision.current) {
        revision.current = result.revision;
        setOrder(result.groups || {});
      }
      adoptServerOrder(latestServer.current);
      try {
        await refresh?.();
      } catch (error) {
        notify?.(`Could not refresh sidebar order: ${errorText(error)}`);
      }
      requestBackend.current = null;
      clearPending = true;
      return true;
    } catch (error) {
      if (
        error instanceof ApiError &&
        error.status >= 400 &&
        error.status < 500 &&
        error.status !== 408
      ) {
        requestBackend.current = null;
        clearPending = true;
        setOrder(latestServer.current?.groups || {});
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
      if (clearPending) outbox.remove("sidebarOrder", pendingSuffix);
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
    const target = { kind: "order" as const, group };
    if (
      !sidebarOrderCanWrite(sessionToken, services.available?.(target)) ||
      !server ||
      busy.current
    )
      return;
    if (readPending()) {
      notify?.(
        "Retry the saved sidebar order request before moving another item.",
      );
      return;
    }
    const value = { ...order, [group]: next };
    if (services.available && !services.available(target)) {
      notify?.("The sidebar server is offline or unavailable.");
      return;
    }
    const backend = services.owner({ kind: "order", group });
    requestBackend.current = backend;
    void send(
      {
        ...(backend.ownerId === services.cache.ownerId
          ? {}
          : { ownerId: backend.ownerId }),
        body: {
          action: "reorder",
          request_id: randomUUID(),
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
