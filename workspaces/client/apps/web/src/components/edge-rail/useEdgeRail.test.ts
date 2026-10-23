import { expect, test } from "vitest";
import { readEdgeRailViewport } from "./useEdgeRail";

test("reads the safe rectangle from resolved root padding and viewport height", () => {
  const viewport = readEdgeRailViewport(
    {
      paddingTop: "0px",
      paddingRight: "84px",
      paddingBottom: "34px",
      paddingLeft: "0px",
      height: "669px",
    },
    true,
  );
  expect(viewport).toEqual({
    coarsePointer: true,
    viewportHeight: 669,
    insets: { top: 0, right: 84, bottom: 34, left: 0 },
  });
});

test("uses existing viewport bottom compensation instead of a keyboard guess", () => {
  expect(
    readEdgeRailViewport(
      {
        paddingTop: "0px",
        paddingRight: "84px",
        paddingBottom: "0px",
        paddingLeft: "0px",
        height: "320px",
      },
      true,
    ).insets.bottom,
  ).toBe(0);
});
