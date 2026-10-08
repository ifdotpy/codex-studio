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
