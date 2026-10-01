import { useEffect } from "react";
import { clearTeamTokenRates, receiveTeamTokenRates } from "../tokenRate";
import { onResume } from "../sync/resume";

// One connection for the visible team. Cards subscribe to the memory cache.
export function useTeamTokenRateStream(
  teamId: string | undefined,
  enabled: boolean,
) {
  useEffect(() => {
    if (!teamId || !enabled) return;
    let source: EventSource | undefined;
    let stopped = false;
    const available = () => !document.hidden && navigator.onLine !== false;
    const close = () => {
      source?.close();
      source = undefined;
      clearTeamTokenRates(teamId);
    };
    const connect = () => {
      if (stopped || !available() || source) return;
      source = new EventSource(
        `/api/token-rates/stream?team=${encodeURIComponent(teamId)}`,
      );
      source.addEventListener("token-rates", (event) => {
        if (!stopped) receiveTeamTokenRates(teamId, event as MessageEvent);
      });
      source.onerror = () => clearTeamTokenRates(teamId);
    };
    const suspend = () => {
      if (!available()) close();
    };
    const resume = () => {
      close();
      connect();
    };
    window.addEventListener("offline", suspend);
    document.addEventListener("visibilitychange", suspend);
    const stopResume = onResume(resume);
    connect();
    return () => {
      stopped = true;
      close();
      stopResume();
      window.removeEventListener("offline", suspend);
      document.removeEventListener("visibilitychange", suspend);
    };
  }, [teamId, enabled]);
}
