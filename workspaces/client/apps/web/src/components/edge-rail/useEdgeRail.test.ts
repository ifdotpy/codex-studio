import { expect, test } from "vitest";
import { layoutEdgeRailMetrics, readEdgeRailViewport } from "./useEdgeRail";
import { edgeRailGeometry } from "./edgeRailPolicy";

test("uses 48px controls only in side strips at least 64px wide", () => {
  const viewport = readEdgeRailViewport(
    {
      height: "669px",
      paddingTop: "0px",
      paddingRight: "84px",
      paddingBottom: "34px",
      paddingLeft: "0px",
    },
    true,
  );
  expect(edgeRailGeometry(viewport, layoutEdgeRailMetrics)?.targetSize).toBe(
    48,
  );
  expect(
    edgeRailGeometry(
      { ...viewport, insets: { ...viewport.insets, right: 59 } },
      layoutEdgeRailMetrics,
    ),
  ).toBeNull();
  expect(
    edgeRailGeometry(
      { ...viewport, insets: { ...viewport.insets, right: 64 } },
      layoutEdgeRailMetrics,
    )?.targetSize,
  ).toBe(48);
});

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
