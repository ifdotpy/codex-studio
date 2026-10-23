import { describe, expect, it } from "vitest";
import { failedTurnsNeedingLookup } from "./useTurnErrors";

describe("failedTurnsNeedingLookup", () => {
  it("ignores failed transcript rows without a nonempty turn ID", () => {
    expect(
      failedTurnsNeedingLookup(
        [
          { turnStatus: "failed", turnId: "" },
          { turnStatus: "failed", turnId: null },
          { turnStatus: "failed", turnId: "turn-1" },
        ],
        new Set(),
      ),
    ).toEqual(["turn-1"]);
  });
});

it("requests reasons for unconfirmed turns without changing completed turns", () => {
  expect(
    failedTurnsNeedingLookup(
      [
        { turnId: "unknown", turnStatus: "uncertain" },
        { turnId: "done", turnStatus: "completed" },
        { turnId: "known", turnStatus: "uncertain" },
      ],
      new Set(["known"]),
    ),
  ).toEqual(["unknown"]);
});
