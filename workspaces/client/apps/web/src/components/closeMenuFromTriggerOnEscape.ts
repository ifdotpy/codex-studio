import { useCallback, useEffect, useState } from "react";
import type { KeyboardEvent } from "react";

export function useMenuEscapeSuppression(opened: boolean) {
  const [suppressNextEscape, setSuppressNextEscape] = useState(false);
  useEffect(() => {
    if (!opened) return;
    setSuppressNextEscape(false);
    const rememberEscape = (event: globalThis.KeyboardEvent) => {
      if (event.key === "Escape") setSuppressNextEscape(true);
    };
    window.addEventListener("keydown", rememberEscape, true);
    return () => window.removeEventListener("keydown", rememberEscape, true);
  }, [opened]);
  const clearSuppression = useCallback(() => setSuppressNextEscape(false), []);
  return { suppressNextEscape, clearSuppression };
}

export function closeMenuFromTriggerOnEscape(
  event: KeyboardEvent<HTMLElement>,
  opened: boolean,
  close: () => void,
  suppressNextEscape: boolean,
  clearSuppression: () => void,
) {
  if (event.key !== "Escape") {
    clearSuppression();
    return;
  }
  if (!opened && suppressNextEscape) {
    clearSuppression();
    event.preventDefault();
    event.stopPropagation();
    return;
  }
  if (!opened) return;
  event.preventDefault();
  event.stopPropagation();
  const trigger = event.currentTarget;
  close();
  requestAnimationFrame(() => trigger.focus({ preventScroll: true }));
}

export function restoreMenuTriggerFocusAfterClose(trigger: HTMLElement | null) {
  requestAnimationFrame(() => trigger?.focus({ preventScroll: true }));
}
