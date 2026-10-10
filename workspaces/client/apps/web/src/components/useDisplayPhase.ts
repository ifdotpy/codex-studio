import { useEffect, useState } from "react";

// Brief tool/reasoning transitions stay in the work log. Errors use live state.
export function useDisplayPhase(scope: string | null, phase: string) {
  const [shown, setShown] = useState({ scope, phase });
  useEffect(() => {
    if (shown.scope !== scope) {
      setShown({ scope, phase });
      return;
    }
    if (shown.phase === phase) return;
    const timer = setTimeout(() => setShown({ scope, phase }), 250);
    return () => clearTimeout(timer);
  }, [scope, phase, shown]);
  return shown.scope === scope ? shown.phase : phase;
}
