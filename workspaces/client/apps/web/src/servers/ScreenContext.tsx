import * as legacy from "./environment";
import type { ScreenMessage } from "./screenChannel";
import { createContext, useContext, useCallback } from "react";
import type { ScreenChannel } from "./screenChannel";

export const ScreenContext = createContext<ScreenChannel | null>(null);
export const useScreenChannel = () => useContext(ScreenContext);

export function useScreenEnvironment() {
  const screen = useScreenChannel();
  const postToShell = useCallback(
    (message: ScreenMessage, _legacyOrigin?: string) => {
      if (screen) screen.publish(message);
      else if (window.parent !== window)
        window.parent.postMessage(message, legacy.serverParentOrigin);
    },
    [screen],
  );
  const owner = screen?.owner ?? legacy.serverViewId;
  return {
    screen,
    postToShell,
    hasShell: !!screen || window.parent !== window,
    serverViewId: owner,
    isServerView: !!owner,
    isClassicServerView: !screen && legacy.isClassicServerView,
    isRemoteServerView: !!owner && owner !== "local",
  };
}
