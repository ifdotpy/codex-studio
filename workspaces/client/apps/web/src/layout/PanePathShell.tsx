import { createContext, useContext, useLayoutEffect } from "react";
import type { PanePathLayout } from "./deriveWorkspaceLayout";

/** The active screen owns navigation state; the combined shell renders its sidebar rect. */
export type PanePathShellPresentation = {
  layout: PanePathLayout;
  toggleSidebar: () => void;
  closeSidebar: () => void;
};
export const PanePathShellContext = createContext<
  | ((owner: string, presentation: PanePathShellPresentation | null) => void)
  | null
>(null);

export function usePublishPanePath(
  owner: string | null | undefined,
  presentation: PanePathShellPresentation | null,
) {
  const publish = useContext(PanePathShellContext);
  useLayoutEffect(() => {
    if (owner && publish) publish(owner, presentation);
  }, [owner, publish, presentation]);
  useLayoutEffect(
    () => () => {
      if (owner && publish) publish(owner, null);
    },
    [owner, publish],
  );
}
