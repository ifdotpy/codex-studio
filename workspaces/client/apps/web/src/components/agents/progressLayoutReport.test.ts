import { expect, test } from "vitest";
import type { components } from "../../generated/api";
import { layoutReportFor } from "./progressLayoutReport";

const layout: Pick<
  components["schemas"]["PanelLayoutBody"],
  | "revision"
  | "width"
  | "height"
  | "contentWidth"
  | "contentHeight"
  | "overflowX"
  | "overflowY"
  | "totalLines"
  | "visibleLines"
  | "lastVisibleLine"
  | "lastVisibleHeading"
  | "fits"
  | "reason"
> = {
  revision: "r1",
  width: 320,
  height: 150,
  contentWidth: 300,
  contentHeight: 120,
  overflowX: 0,
  overflowY: 0,
  totalLines: 2,
  visibleLines: 2,
  lastVisibleLine: "Ready.",
  lastVisibleHeading: null,
  fits: true,
  reason: null,
};

test("a retry keeps the same generated request body and sequence", () => {
  const first = layoutReportFor(undefined, "agent-1", "browser-1", layout, 1);
  const retry = layoutReportFor(first, "agent-1", "browser-1", layout, 2);

  expect(retry).toBe(first);
  expect(retry.body).toEqual({
    agent: "agent-1",
    client: "browser-1",
    sequence: 1,
    renderer: "progress-markdown-v2",
    ...layout,
  });
});

test("a changed measurement receives a new sequence identity", () => {
  const first = layoutReportFor(undefined, "agent-1", "browser-1", layout, 1);
  const changed = layoutReportFor(
    first,
    "agent-1",
    "browser-1",
    { ...layout, height: 160 },
    2,
  );

  expect(changed).not.toBe(first);
  expect(changed.body.sequence).toBe(2);
});
