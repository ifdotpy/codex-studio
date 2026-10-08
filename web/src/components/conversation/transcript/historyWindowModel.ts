import type { HistoryGroup } from "../../turnHistoryModel";

export const HISTORY_WINDOW_THRESHOLD = 80;
export const HISTORY_ROW_ITEMS = 32;
export const HISTORY_WINDOW_ROWS = 48;

/** Bound a single large native turn as well as a long list of turns. */
export function windowHistoryRows(groups: HistoryGroup[]): HistoryGroup[] {
  if (!groups.some((group) => group.items.length > HISTORY_ROW_ITEMS))
    return groups;
  return groups.flatMap((group) => {
    if (group.items.length <= HISTORY_ROW_ITEMS) return [group];
    const rows: HistoryGroup[] = [];
    for (
      let start = 0;
      start < group.items.length;
      start += HISTORY_ROW_ITEMS
    ) {
      const items = group.items.slice(start, start + HISTORY_ROW_ITEMS);
      const last = start + items.length === group.items.length;
      rows.push({
        ...group,
        id: items[0].id,
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
    }
    return rows;
  });
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
