import {
  useServerApi,
  useServerPreferenceEvents,
} from "../servers/ServerServicesContext";
import { useEffect, useState } from "react";
/** Local cache supplies the first render; entity delivery updates later renders. */
export function useSyncedVisualState<T>(key: string, fallback: T) {
  const { saved } = useServerApi();
  const preferenceEvents = useServerPreferenceEvents();

  const [value, setValue] = useState<T>(() => saved(key, fallback));
  useEffect(() => {
    const update = (changed: string | null = null) => {
      if (changed !== null && changed !== key) return;
      setValue(saved(key, fallback));
    };
    update();
    return preferenceEvents.subscribe(update);
  }, [key, saved, preferenceEvents]);
  return [value, setValue] as const;
}
