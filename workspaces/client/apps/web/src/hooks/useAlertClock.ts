import { useEffect, useState } from "react";
import { nextAlertChange } from "../desktop/desktopAlerts";
import type { Snapshot } from "../types";

/**
 * The time, in seconds, for `desktopAlerts`. It moves when a sign-in alert or its reminder
 * becomes due (`nextAlertChange`), because no record changes at that moment.
 */
export function useAlertClock(data: Snapshot | null) {
  const [now, setNow] = useState(() => Date.now() / 1000);
  useEffect(() => {
    if (!data) return;
    const next = nextAlertChange(data, Date.now() / 1000);
    if (next === null) return;
    const timer = setTimeout(
      () => setNow(Date.now() / 1000),
      Math.max(0, next * 1000 - Date.now()) + 50,
    );
    return () => clearTimeout(timer);
  }, [data, now]);
  return now;
}
