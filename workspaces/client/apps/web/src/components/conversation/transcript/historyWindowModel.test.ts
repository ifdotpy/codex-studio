import { describe, expect, it } from "vitest";
import {
  historyOffsets,
  historyWindowRange,
  HISTORY_ROW_ITEMS,
  HISTORY_WINDOW_ROWS,
  windowHistoryRows,
} from "./historyWindowModel";
import type { HistoryGroup } from "../../turnHistoryModel";

describe("history windows", () => {
  it("keeps small native groups unchanged", () => {
    const rows: HistoryGroup[] = [
      { id: "a", items: [{ id: "a", role: "assistant", text: "Answer" }] },
    ];
    expect(windowHistoryRows(rows)).toBe(rows);
  });
  it("bounds one large turn and retains each message and final failure once", () => {
    const items = Array.from({ length: 1001 }, (_, index) => ({
      id: `m${index}`,
      role: "assistant",
      text: "Answer",
      turnId: "turn",
    }));
    const group: HistoryGroup = {
      id: "m0",
      items,
      outcome: "failed",
      result: items.at(-1),
    };
    const rows = windowHistoryRows([group]);
    expect(rows.every((row) => row.items.length <= HISTORY_ROW_ITEMS)).toBe(
      true,
    );
    expect(rows.flatMap((row) => row.items)).toEqual(items);
    expect(rows.filter((row) => row.outcome === "failed")).toEqual([
      rows.at(-1),
    ]);
    expect(rows.filter((row) => row.result)).toEqual([rows.at(-1)]);
  });
  it("retains prior boundaries when the same turn prepends, appends and removes items", () => {
    const items = Array.from({ length: 3000 }, (_, index) => ({
      id: `m${index}`,
      role: "tool",
      text: "Saved output",
      turnId: "turn",
    }));
    const group: HistoryGroup = {
      id: "m0",
      items,
      outcome: "failed",
      result: items.at(-1),
    };
    const initial = windowHistoryRows([group]);
    const prefix = Array.from({ length: 50 }, (_, index) => ({
      ...items[0],
      id: `prefix${index}`,
    }));
    const prepended = windowHistoryRows(
      [{ ...group, id: "prefix0", items: [...prefix, ...items] }],
      initial,
    );
    for (const old of initial)
      expect(prepended.find((row) => row.id === old.id)?.items).toEqual(
        old.items,
      );
    expect(prepended.flatMap((row) => row.items)).toEqual([
      ...prefix,
      ...items,
    ]);
    const suffix = Array.from({ length: 50 }, (_, index) => ({
      ...items[0],
      id: `suffix${index}`,
    }));
    const appended = windowHistoryRows(
      [
        {
          ...group,
          items: [...prefix, ...items, ...suffix],
          result: suffix.at(-1),
        },
      ],
      prepended,
    );
    for (const old of prepended)
      expect(appended.some((row) => row.id === old.id)).toBe(true);
    expect(appended.every((row) => row.items.length <= HISTORY_ROW_ITEMS)).toBe(
      true,
    );
    expect(appended.filter((row) => row.outcome === "failed")).toEqual([
      appended.at(-1),
    ]);
    expect(appended.filter((row) => row.result)).toEqual([appended.at(-1)]);
    const removed = items.filter(
      (item) => item.id !== "m0" && !initial[1].items.includes(item),
    );
    const pruned = windowHistoryRows(
      [{ ...group, id: removed[0].id, items: removed }],
      initial,
    );
    expect(pruned[0].id).toBe("m0");
    expect(pruned.some((row) => row.id === initial[1].id)).toBe(false);
    expect(pruned.flatMap((row) => row.items)).toEqual(removed);
  });
  it("keeps the viewport range bounded, including zero-height rows", () => {
    const offsets = Array.from({ length: 10001 }, (_, index) => index * 100);
    const range = historyWindowRange(offsets, 540000, 600);
    expect(range.start).toBeLessThanOrEqual(5400);
    expect(range.end).toBeGreaterThan(5406);
    expect(range.end - range.start).toBeLessThanOrEqual(HISTORY_WINDOW_ROWS);
    const empty = historyWindowRange(
      Array.from({ length: 10001 }, () => 0),
      0,
      600,
    );
    expect(empty.end - empty.start).toBeLessThanOrEqual(HISTORY_WINDOW_ROWS);
  });
  it("uses measured heights, including a hidden row", () => {
    const rows: HistoryGroup[] = [0, 1, 2].map((id) => ({
      id: `${id}`,
      items: [{ id: `${id}`, text: "Answer", role: "assistant" }],
    }));
    expect(
      historyOffsets(
        rows,
        new Map([
          ["0", 0],
          ["1", 120],
        ]),
      ),
    ).toEqual([0, 0, 120, 192]);
  });
});
