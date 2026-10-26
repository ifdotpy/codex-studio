/** CSS pixel values supplied by the shared layout contract. */
export interface EdgeRailMetrics {
  minimumInset: number;
  statusReserve: number;
  targetSize: number;
  /** Outer screen edge to the system status column center; strip center when absent. */
  statusColumnCenter?: number;
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
  const width = insets[side];
  const targetSize = Math.min(width, metrics.targetSize);
  const top = Math.max(insets.top, metrics.statusReserve);
  const height = viewportHeight - top - insets.bottom;
  if (targetSize <= 0 || width < metrics.minimumInset || height < targetSize)
    return null;
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
