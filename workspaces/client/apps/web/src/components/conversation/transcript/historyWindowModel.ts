import type { HistoryGroup } from "../../turnHistoryModel";
import { messageRenderKey } from "../../message-delivery/messageDelivery";

export const HISTORY_WINDOW_THRESHOLD = 80;
export const HISTORY_ROW_ITEMS = 16;
export const HISTORY_WINDOW_ROWS = 24;

/** Retain committed row boundaries while pages prepend and the last turn grows. */
export function windowHistoryRows(
  groups: HistoryGroup[],
  previous: HistoryGroup[] = [],
): HistoryGroup[] {
  if (
    !previous.length &&
    groups.every((group) => group.items.length <= HISTORY_ROW_ITEMS)
  )
    return groups;
  const present = new Set(
    groups.flatMap((group) => group.items.map(messageRenderKey)),
  );
  const boundaries = new Map<string, string>();
  for (const row of previous) {
    const survivor = row.items.find((item) =>
      present.has(messageRenderKey(item)),
    );
    if (survivor) boundaries.set(messageRenderKey(survivor), row.id);
  }
  return groups.flatMap((group) => {
    if (group.items.length <= HISTORY_ROW_ITEMS) {
      const id = boundaries.get(messageRenderKey(group.items[0])) || group.id;
      return [id === group.id ? group : { ...group, id }];
    }
    const rows: HistoryGroup[] = [];
    let start = 0;
    while (start < group.items.length) {
      let end = start + 1;
      while (
        end < group.items.length &&
        end - start < HISTORY_ROW_ITEMS &&
        !boundaries.has(messageRenderKey(group.items[end]))
      )
        end++;
      const items = group.items.slice(start, end);
      const last = end === group.items.length;
      rows.push({
        ...group,
        id: boundaries.get(messageRenderKey(items[0])) || items[0].id,
        items,
        result:
          group.result && items.includes(group.result)
            ? group.result
            : undefined,
        outcome: last ? group.outcome : group.outcome ? "completed" : undefined,
        turns: group.turns?.filter((turn) =>
          items.some((item) => item.turnId === turn.items[0].turnId),
        ),
      });
      start = end;
    }
    return rows;
  });
}

/** Saved row IDs remain a fallback for clients before message anchors. */
export function historyWindowAnchor(rows: HistoryGroup[], id: string) {
  for (const row of rows) {
    const item = row.items.find(
      (item) =>
        messageRenderKey(item) === id || item.id === id || item.sourceId === id,
    );
    if (item) return { row, item };
  }
  const row = rows.find((row) => row.id === id);
  return row ? { row, item: row.items[0] } : undefined;
}

export function historyOffsets(
  rows: HistoryGroup[],
  heights: ReadonlyMap<string, number>,
): number[] {
  const offsets = [0];
  for (const row of rows) {
    const measured = heights.get(row.id);
    const estimate = row.items.reduce(
      (height, item) =>
        height +
        (item.role === "tool" || item.role === "output"
          ? 40
          : Math.max(72, Math.ceil(item.text.length / 90) * 22 + 48)),
      0,
    );
    offsets.push(offsets[offsets.length - 1] + (measured ?? estimate));
  }
  return offsets;
}

export function historyWindowRange(
  offsets: number[],
  top: number,
  height: number,
) {
  const count = offsets.length - 1;
  if (count <= 0) return { start: 0, end: 0 };
  const find = (position: number) => {
    let low = 0;
    let high = count;
    while (low < high) {
      const middle = Math.floor((low + high) / 2);
      if (offsets[middle + 1] <= position) low = middle + 1;
      else high = middle;
    }
    return Math.min(count - 1, low);
  };
  const visible = find(Math.max(0, top));
  const start = Math.max(0, visible - 8);
  const end = Math.min(
    count,
    Math.max(visible + 9, find(top + height) + 9),
    start + HISTORY_WINDOW_ROWS,
  );
  return { start, end };
}

export function revealHistoryMessage(root: HTMLElement, id: string) {
  root.dispatchEvent(new CustomEvent("studio-history-reveal", { detail: id }));
}
