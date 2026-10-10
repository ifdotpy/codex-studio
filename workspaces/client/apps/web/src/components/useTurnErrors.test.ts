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
