import { useServerTokenRates } from "../servers/ServerServicesContext";
import { useEffect } from "react";
import "../usage/tokenRate";

// The existing workspace transport supplies rates to the visible Team panel.
export function useTeamTokenRateStream(
  teamId: string | undefined,
  enabled: boolean,
) {
  const { watchTeamTokenRates } = useServerTokenRates();

  useEffect(() => {
    if (!teamId || !enabled) return;
    return watchTeamTokenRates(teamId);
  }, [teamId, enabled]);
}
