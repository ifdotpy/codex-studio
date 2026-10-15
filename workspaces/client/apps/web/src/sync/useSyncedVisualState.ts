import { useServerApi } from "../servers/ServerServicesContext";
import { useEffect, useState } from "react";
import "../api";
import { preferenceEvent } from "./uiPreferenceMerge";
/** Local cache supplies the first render; entity delivery updates later renders. */
export function useSyncedVisualState<T>(key: string, fallback: T) {
  const { saved } = useServerApi();

  const [value, setValue] = useState<T>(() => saved(key, fallback));
  useEffect(() => {
    const update = (event?: Event) => {
      if (event instanceof CustomEvent && event.detail !== key) return;
      if (event instanceof StorageEvent && event.key !== key) return;
      setValue(saved(key, fallback));
    };
    update();
    window.addEventListener(preferenceEvent, update);
    window.addEventListener("storage", update);
    return () => {
      window.removeEventListener(preferenceEvent, update);
      window.removeEventListener("storage", update);
    };
  }, [key]);
  return [value, setValue] as const;
}
