import {
  useEffect,
  useRef,
  useState,
  type DragEvent,
  type KeyboardEvent,
} from "react";
import { saved } from "../api";

type Order = Record<string, string[]>;
type Drag = { group: string; id: string };
export function useSidebarOrder(key: string, notify?: (text: string) => void) {
  const [order, setOrder] = useState<Order>(() => saved(key, {}));
  const drag = useRef<Drag | null>(null);
  const [target, setTarget] = useState<(Drag & { after: boolean }) | null>(
    null,
  );
  const [announcement, announce] = useState("");
  useEffect(() => {
    setOrder(saved(key, {}));
  }, [key]);
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
    const value = { ...order, [group]: next };
    try {
      localStorage.setItem(key, JSON.stringify(value));
      setOrder(value);
      announce(`Moved to position ${next.indexOf(from) + 1} of ${next.length}`);
    } catch (error) {
      notify?.(`Could not save sidebar order: ${String(error)}`);
    }
  };
  const finish = () => {
    drag.current = null;
    setTarget(null);
  };
  const bindings = (group: string, id: string, ids: string[]) => ({
    draggable: true,
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
      const to = ids[ids.indexOf(id) + (after ? 1 : -1)];
      if (to) move(group, ids, id, to, after);
    },
  });
  return { rank, bindings, announcement };
}
