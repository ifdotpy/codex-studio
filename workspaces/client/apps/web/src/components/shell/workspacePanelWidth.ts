import { useCallback, useState } from "react";

export function panelWidthAfterResize(
  width: number,
  startX: number,
  currentX: number,
  min: number,
  max: number,
): number | undefined {
  if (
    ![width, startX, currentX, min, max].every(Number.isFinite) ||
    min <= 0 ||
    max < min
  )
    return undefined;
  return Math.min(max, Math.max(min, width + startX - currentX));
}

// Widths use rem. A temporary host clamp never changes the preferred value.
export function useWorkspacePanelWidth(scope: string) {
  const [saved, setSaved] = useState<{
    scope: string;
    width: number | undefined;
  }>(() => ({ scope, width: undefined }));
  if (saved.scope !== scope) setSaved({ scope, width: undefined });
  const setPreferredWidthRem = useCallback(
    (width: number | undefined) => {
      if (width !== undefined && (!Number.isFinite(width) || width <= 0))
        return;
      setSaved({ scope, width });
    },
    [scope],
  );
  return {
    preferredWidthRem: saved.scope === scope ? saved.width : undefined,
    setPreferredWidthRem,
  };
}
