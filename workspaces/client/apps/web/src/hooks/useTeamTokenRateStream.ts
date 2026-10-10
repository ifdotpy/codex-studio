import { useEffect } from "react";
import { watchTeamTokenRates } from "../usage/tokenRate";

// The existing workspace transport supplies rates to the visible Team panel.
export function useTeamTokenRateStream(
  teamId: string | undefined,
  enabled: boolean,
) {
  useEffect(() => {
    if (!teamId || !enabled) return;
    return watchTeamTokenRates(teamId);
  }, [teamId, enabled]);
}
