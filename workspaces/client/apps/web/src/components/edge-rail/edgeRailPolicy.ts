/** CSS pixel values supplied by the shared layout contract. */
export interface EdgeRailMetrics {
  minimumInset: number;
  statusReserve: number;
  targetSize: number;
  /** Outer screen edge to the system status column center; strip center when absent. */
  statusColumnCenter?: number;
  /** Width kept clear at the right end of a top strip for the clock and Wi-Fi. */
  topStatusReserve?: number;
}

export interface EdgeRailViewport {
  coarsePointer: boolean;
  viewportHeight: number;
  insets: { top: number; right: number; bottom: number; left: number };
  /** Page width; needed for a top strip. */
  viewportWidth?: number;
  /** Height of a top status strip that the page does not reserve (Duo portrait). */
  topStrip?: number;
}

/**
 * Side rails are columns: width = strip width, height = column length.
 * A top rail is a row in the top strip: width = strip height, height = row
 * length from the left edge, ending before the status area.
 */
export interface EdgeRailGeometry {
  side: "left" | "right" | "top";
  width: number;
  top: number;
  bottom: number;
  height: number;
  targetSize: number;
  /** Outer screen edge to the button column center, inside the strip. */
  center: number;
}

export function edgeRailGeometry(
  viewport: EdgeRailViewport,
  metrics: EdgeRailMetrics,
): EdgeRailGeometry | null {
  const { insets, viewportHeight, coarsePointer } = viewport;
  if (
    !coarsePointer ||
    ![
      viewportHeight,
      ...Object.values(insets),
      ...Object.values(metrics),
    ].every((value) => Number.isFinite(value) && value >= 0) ||
    metrics.targetSize <= 0
  )
    return null;

  const side = insets.left > insets.right ? "left" : "right";
  if (insets[side] < metrics.minimumInset) return topRail(viewport, metrics);
  const width = insets[side];
  const targetSize = Math.min(width, metrics.targetSize);
  const top = Math.max(insets.top, metrics.statusReserve);
  const height = viewportHeight - top - insets.bottom;
  if (targetSize <= 0 || height < targetSize) return null;
  const half = targetSize / 2;
  const center = Math.min(
    Math.max(metrics.statusColumnCenter ?? width / 2, half),
    width - half,
  );
  return {
    side,
    width,
    top,
    bottom: insets.bottom,
    height,
    targetSize,
    center,
  };
}

/** A row of rail buttons in a top status strip, left of the status area. */
function topRail(
  { insets, viewportWidth, topStrip }: EdgeRailViewport,
  metrics: EdgeRailMetrics,
): EdgeRailGeometry | null {
  if (
    topStrip === undefined ||
    viewportWidth === undefined ||
    ![topStrip, viewportWidth].every((v) => Number.isFinite(v) && v >= 0) ||
    topStrip < metrics.minimumInset
  )
    return null;
  const targetSize = Math.min(topStrip, metrics.targetSize);
  const length = viewportWidth - (metrics.topStatusReserve ?? 0);
  if (length < targetSize) return null;
  const half = targetSize / 2;
  return {
    side: "top",
    width: topStrip,
    top: 0,
    bottom: insets.bottom,
    height: length,
    targetSize,
    center: Math.min(
      Math.max(metrics.statusColumnCenter ?? topStrip / 2, half),
      topStrip - half,
    ),
  };
}
