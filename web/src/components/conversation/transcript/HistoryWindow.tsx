import {
  Fragment,
  useEffect,
  useLayoutEffect,
  useMemo,
  useRef,
  useState,
  type ReactNode,
  type RefObject,
} from "react";
import { save, saved } from "../../../api";
import { messageRenderKey } from "../../message-delivery/messageDelivery";
import type { HistoryGroup } from "../../turnHistoryModel";
import {
  HISTORY_WINDOW_THRESHOLD,
  historyOffsets,
  historyWindowAnchor,
  historyWindowRange,
} from "./historyWindowModel";

/** A viewport owns only mounted rows. The conversation scroll hook owns anchors. */
export default function HistoryWindow({
  rows,
  scrollContainer,
  render,
  storageKey,
  rememberScroll,
}: {
  rows: HistoryGroup[];
  scrollContainer?: RefObject<HTMLDivElement | null>;
  render: (row: HistoryGroup, revealedMessage?: string) => ReactNode;
  storageKey: string;
  rememberScroll?: () => void;
}) {
  const body = useRef<HTMLDivElement>(null);
  const scheduleUpdate = useRef<() => void>(() => {});
  const heights = useRef(new Map<string, number>());
  const details = useRef(new Map<string, boolean>());
  const [revision, setRevision] = useState(0);
  const offsets = useMemo(
    () => historyOffsets(rows, heights.current),
    [rows, revision],
  );
  const [visible, setVisible] = useState<string[]>(() =>
    rows.slice(-12).map((row) => row.id),
  );
  const [initialAnchor] = useState(() =>
    saved<{ id: string; offset: number; kind?: "message" | "row" } | null>(
      `${storageKey}:window-anchor`,
      null,
    ),
  );
  const storedAnchor = useRef(initialAnchor);
  const [requested, setRequested] = useState<string | null>(
    () => storedAnchor.current?.id || null,
  );
  const rememberLatest = useRef(rememberScroll);
  rememberLatest.current = rememberScroll;
  const [revealedMessage, setRevealedMessage] = useState<string>();
  const [pinned, setPinned] = useState<string[]>([]);
  const active =
    !!scrollContainer &&
    (rows.length > HISTORY_WINDOW_THRESHOLD ||
      rows.reduce((count, row) => count + row.items.length, 0) > 256);
  const indices = useMemo(() => {
    const requestedRow = requested
      ? historyWindowAnchor(rows, requested)?.row.id
      : undefined;
    const keep = new Set([
      ...visible,
      ...pinned,
      ...(requestedRow ? [requestedRow] : []),
    ]);
    return rows.flatMap((row, index) => (keep.has(row.id) ? [index] : []));
  }, [rows, visible, pinned, requested]);
  const rowsById = useMemo(
    () => new Map(rows.map((row) => [row.id, row])),
    [rows],
  );
  const latest = useRef({ rows, offsets, rowsById });
  latest.current = { rows, offsets, rowsById };
  useEffect(() => {
    const root = scrollContainer?.current;
    const node = body.current;
    if (!root || !node) return;
    const reveal = (event: Event) => {
      const id: unknown = (event as CustomEvent).detail;
      if (typeof id !== "string") return;
      const match = historyWindowAnchor(latest.current.rows, id);
      if (match) {
        setRequested(match.row.id);
        setRevealedMessage(match.item.id);
      }
    };
    root.addEventListener("studio-history-reveal", reveal);
    if (!active)
      return () => root.removeEventListener("studio-history-reveal", reveal);
    let frame = 0;
    let restoreFrame = 0;
    const persistAnchor = () => {
      const bounds = root.getBoundingClientRect();
      const row = Array.from(
        node.querySelectorAll<HTMLElement>("[data-history-row]"),
      ).find((row) => {
        const box = row.getBoundingClientRect();
        return box.bottom > bounds.top && box.top < bounds.bottom;
      });
      if (root.scrollHeight - root.scrollTop - root.clientHeight < 32) {
        save(`${storageKey}:window-anchor`, null);
      } else if (row) {
        const metadata = latest.current.rowsById.get(row.dataset.historyRow!);
        if (!metadata) return;
        const visible = Array.from(
          row.querySelectorAll<HTMLElement>(
            "[data-message]:not([data-lazy-message])",
          ),
        ).find((message) => {
          const box = message.getBoundingClientRect();
          return (
            box.height > 0 && box.bottom > bounds.top && box.top < bounds.bottom
          );
        });
        const item = visible
          ? metadata.items.find((item) => item.id === visible.dataset.message)
          : metadata.items[0];
        if (item)
          save(`${storageKey}:window-anchor`, {
            id: messageRenderKey(item),
            kind: visible ? "message" : "row",
            offset: (visible || row).getBoundingClientRect().top - bounds.top,
          });
      }
    };
    const update = () => {
      frame = 0;
      const { rows: current, offsets: positions } = latest.current;
      const top =
        root.scrollTop -
        (node.getBoundingClientRect().top -
          root.getBoundingClientRect().top +
          root.scrollTop);
      const range = historyWindowRange(positions, top, root.clientHeight);
      const ids = current.slice(range.start, range.end).map((row) => row.id);
      setVisible((old) =>
        old.length === ids.length && old.every((id, index) => id === ids[index])
          ? old
          : ids,
      );
      setRequested((id) =>
        id && ids.includes(historyWindowAnchor(current, id)?.row.id || id)
          ? null
          : id,
      );
    };
    const schedule = () => {
      if (!frame) frame = requestAnimationFrame(update);
    };
    scheduleUpdate.current = schedule;
    const scrolled = () => {
      schedule();
      persistAnchor();
    };
    let focusOwner: string | undefined;
    const pin = () => {
      const selection = document.getSelection();
      const selected =
        selection &&
        !selection.isCollapsed &&
        node.contains(selection.anchorNode);
      const focused =
        node.contains(document.activeElement) &&
        document.activeElement !== document.body;
      const focusedRow = focused
        ? document.activeElement?.closest<HTMLElement>("[data-history-row]")
            ?.dataset.historyRow
        : undefined;
      if (focusedRow) focusOwner = focusedRow;
      const modal =
        !focused &&
        !!document.activeElement?.closest('[role="dialog"]') &&
        focusOwner;
      if (!focused && !modal) focusOwner = undefined;
      setPinned((old) => {
        if (!selected && !focused && !modal) return old.length ? [] : old;
        const ids = Array.from(
          node.querySelectorAll<HTMLElement>("[data-history-row]"),
        )
          .filter((row) =>
            selected
              ? selection.containsNode(row, true)
              : row.contains(document.activeElement),
          )
          .map((row) => row.dataset.historyRow!);
        if (modal && !ids.includes(modal)) ids.push(modal);
        return old.length === ids.length &&
          old.every((id, index) => id === ids[index])
          ? old
          : ids;
      });
    };
    root.addEventListener("scroll", scrolled, { passive: true });
    document.addEventListener("selectionchange", pin);
    const focusChanged = () => queueMicrotask(pin);
    document.addEventListener("focusin", focusChanged);
    document.addEventListener("focusout", focusChanged);
    const observer = new ResizeObserver(schedule);
    observer.observe(root);
    const anchor = storedAnchor.current;
    storedAnchor.current = null;
    if (
      anchor &&
      typeof anchor.id === "string" &&
      Number.isFinite(anchor.offset)
    ) {
      const match = historyWindowAnchor(latest.current.rows, anchor.id);
      if (anchor.kind === "message" && match) setRevealedMessage(match.item.id);
      let attempts = 0;
      const restoreAnchor = () => {
        const resolved = historyWindowAnchor(latest.current.rows, anchor.id);
        const row =
          resolved &&
          Array.from(
            node.querySelectorAll<HTMLElement>("[data-history-row]"),
          ).find((row) => row.dataset.historyRow === resolved.row.id);
        const message =
          row &&
          resolved &&
          Array.from(row.querySelectorAll<HTMLElement>("[data-message]")).find(
            (message) =>
              message.dataset.message === resolved.item.id ||
              (!!resolved.item.sourceId &&
                message.dataset.sourceMessage === resolved.item.sourceId),
          );
        if (anchor.kind === "message" && message) {
          for (
            let parent = message.parentElement;
            parent && parent !== node;
            parent = parent.parentElement
          )
            if (parent instanceof HTMLDetailsElement) parent.open = true;
        }
        const visibleMessage =
          message &&
          !message.hasAttribute("data-lazy-message") &&
          message.getBoundingClientRect().height > 0;
        if (
          anchor.kind === "message" &&
          resolved &&
          !visibleMessage &&
          attempts++ < 20
        ) {
          restoreFrame = requestAnimationFrame(restoreAnchor);
          return;
        }
        const target =
          anchor.kind === "message" && visibleMessage ? message : row;
        if (target) {
          root.scrollTop +=
            target.getBoundingClientRect().top -
            root.getBoundingClientRect().top -
            anchor.offset;
          rememberLatest.current?.();
        }
        update();
      };
      restoreFrame = requestAnimationFrame(restoreAnchor);
    } else update();
    return () => {
      scheduleUpdate.current = () => {};
      if (frame) cancelAnimationFrame(frame);
      if (restoreFrame) cancelAnimationFrame(restoreFrame);
      observer.disconnect();
      root.removeEventListener("scroll", scrolled);
      root.removeEventListener("studio-history-reveal", reveal);
      document.removeEventListener("selectionchange", pin);
      document.removeEventListener("focusin", focusChanged);
      document.removeEventListener("focusout", focusChanged);
    };
  }, [active, scrollContainer, storageKey]);
  useLayoutEffect(() => scheduleUpdate.current(), [rows, offsets]);
  useLayoutEffect(() => {
    const node = body.current;
    if (!node) return;
    const restored = new WeakSet<HTMLDetailsElement>();
    const key = (detail: HTMLDetailsElement) => {
      const row = detail.closest<HTMLElement>("[data-history-row]");
      const message = detail.closest<HTMLElement>("[data-message]");
      if (detail.classList.contains("turn-work")) return null;
      if (!row && !message) return null;
      const parent = message || row!;
      return `${parent.dataset.message || row?.dataset.historyRow}:${Array.from(parent.querySelectorAll("details")).indexOf(detail)}:${detail.className}`;
    };
    const restore = () => {
      for (const detail of node.querySelectorAll<HTMLDetailsElement>(
        "details",
      )) {
        if (restored.has(detail)) continue;
        restored.add(detail);
        const id = key(detail);
        const open = id && details.current.get(id);
        if (typeof open === "boolean" && detail.open !== open)
          detail.open = open;
      }
    };
    const toggled = (event: Event) => {
      if (!(event.target instanceof HTMLDetailsElement)) return;
      const id = key(event.target);
      if (id) details.current.set(id, event.target.open);
    };
    node.addEventListener("toggle", toggled, true);
    restore();
    const changes = new MutationObserver(restore);
    changes.observe(node, { childList: true, subtree: true });
    const observer = new ResizeObserver((entries) => {
      let changed = false;
      for (const entry of entries) {
        const row = entry.target as HTMLElement;
        const id = row.dataset.historyRow!;
        const height = row.getBoundingClientRect().height;
        if (heights.current.get(id) !== height) {
          heights.current.set(id, height);
          changed = true;
        }
      }
      if (changed) setRevision((value) => value + 1);
    });
    if (active)
      node
        .querySelectorAll<HTMLElement>("[data-history-row]")
        .forEach((row) => observer.observe(row));
    return () => {
      observer.disconnect();
      changes.disconnect();
      node.removeEventListener("toggle", toggled, true);
    };
  }, [active, indices]);
  if (!active)
    return (
      <div ref={body} className="history-window-static">
        {rows.map((row) => (
          <Fragment key={row.id}>{render(row, revealedMessage)}</Fragment>
        ))}
      </div>
    );
  let previous = 0;
  return (
    <div ref={body} className="history-window" data-history-window>
      {indices.map((index) => {
        const gap = offsets[index] - offsets[previous];
        previous = index + 1;
        return (
          <Fragment key={rows[index].id}>
            {gap > 0 && (
              <div
                className="history-window-spacer"
                style={{ height: gap }}
                aria-hidden="true"
              />
            )}
            <div
              className="history-window-row"
              data-history-row={rows[index].id}
            >
              {render(rows[index], revealedMessage)}
            </div>
          </Fragment>
        );
      })}
      <div
        className="history-window-spacer"
        style={{ height: offsets[rows.length] - offsets[previous] }}
        aria-hidden="true"
      />
    </div>
  );
}
