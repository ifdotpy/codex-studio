import { useCallback, useRef, useState } from "react";
import { watchVisualActivity } from "./visualActivity";

function useActivityRef<T extends HTMLElement>(
  listener: (active: boolean) => void,
) {
  const stop = useRef<(() => void) | undefined>(undefined);
  const ref = useCallback(
    (element: T | null) => {
      stop.current?.();
      stop.current = element
        ? watchVisualActivity(element, listener)
        : undefined;
    },
    [listener],
  );
  return ref;
}

const cssOnly = () => {};

export function useVisualActivityRef<T extends HTMLElement>() {
  return useActivityRef<T>(cssOnly);
}

export function useVisualActivity<T extends HTMLElement>() {
  const [active, setActive] = useState(true);
  const ref = useActivityRef<T>(setActive);
  return [ref, active] as const;
}
