/** CSS pixel values supplied by the shared layout contract. */
export interface EdgeRailMetrics {
  minimumInset: number;
  statusReserve: number;
  targetSize: number;
}

export interface EdgeRailViewport {
  coarsePointer: boolean;
  viewportHeight: number;
  insets: { top: number; right: number; bottom: number; left: number };
}

export interface EdgeRailGeometry {
  side: "left" | "right";
  width: number;
  top: number;
  bottom: number;
  height: number;
  targetSize: number;
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
    metrics.targetSize <= 0 ||
    metrics.minimumInset < metrics.targetSize
  )
    return null;

  const side = insets.left > insets.right ? "left" : "right";
  const width = insets[side];
  const top = Math.max(insets.top, metrics.statusReserve);
  const height = viewportHeight - top - insets.bottom;
  if (width < metrics.minimumInset || height < metrics.targetSize) return null;
  return {
    side,
    width,
    top,
    bottom: insets.bottom,
    height,
    targetSize: metrics.targetSize,
  };
}
