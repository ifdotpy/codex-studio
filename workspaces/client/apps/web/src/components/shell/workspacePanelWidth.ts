import { useCallback, useState } from "react";

export function readPanelWidth(value: string | null): number | undefined {
  if (!value?.trim()) return undefined;
  const width = Number(value);
  return Number.isFinite(width) && width > 0 ? width : undefined;
}

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

// Preferences use rem. A temporary host clamp never changes this stored value.
export function useWorkspacePanelWidth(scope: string) {
  const key = `studio-workspace-panel-width:${scope}`;
  const [saved, setSaved] = useState<{
    key: string;
    width: number | undefined;
  }>(() => ({ key, width: readStored(key) }));
  if (saved.key !== key) setSaved({ key, width: readStored(key) });
  const setPreferredWidthRem = useCallback(
    (width: number | undefined) => {
      if (width !== undefined && (!Number.isFinite(width) || width <= 0))
        return;
      try {
        if (width === undefined) localStorage.removeItem(key);
        else localStorage.setItem(key, String(width));
      } catch {
        // The preference remains available for this session if storage is unavailable.
      }
      setSaved({ key, width });
    },
    [key],
  );
  return {
    preferredWidthRem: saved.key === key ? saved.width : undefined,
    setPreferredWidthRem,
  };
}

function readStored(key: string) {
  try {
    return readPanelWidth(localStorage.getItem(key));
  } catch {
    return undefined;
  }
}
