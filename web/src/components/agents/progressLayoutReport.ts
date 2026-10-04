import type { components } from "../../generated/api";

type PanelLayoutBody = components["schemas"]["PanelLayoutBody"];
type ProgressLayout = Pick<
  PanelLayoutBody,
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
>;

export interface LayoutMeasurement {
  fingerprint: string;
  body: PanelLayoutBody;
}

export function layoutReportFor(
  previous: LayoutMeasurement | undefined,
  agent: string,
  client: string,
  layout: ProgressLayout,
  sequence: number,
): LayoutMeasurement {
  const fingerprint = JSON.stringify(layout);
  if (previous?.fingerprint === fingerprint) return previous;
  return {
    fingerprint,
    body: {
      agent,
      client,
      sequence,
      renderer: "progress-markdown-v2",
      ...layout,
    },
  };
}
