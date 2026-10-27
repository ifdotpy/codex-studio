import {
  createContext,
  useContext,
  useState,
  useCallback,
  type ReactNode,
} from "react";
import { createPortal } from "react-dom";
import { useMessages, useCommittedCallback } from "../hooks";
import type { Agent, Snapshot } from "../types";
import ConversationWarnings from "./ConversationWarnings";

type WarningTarget = {
  node: HTMLElement;
  utility: boolean;
  settings: (() => void) | undefined;
};
const Target = createContext<WarningTarget | null>(null);
const SetTarget = createContext<(target: WarningTarget | null) => void>(
  () => {},
);

/** The active server retains API ownership while the shared sidebar supplies the target. */
export function SidebarWarningsProvider({ children }: { children: ReactNode }) {
  const [target, setTarget] = useState<WarningTarget | null>(null);
  return (
    <SetTarget.Provider value={setTarget}>
      <Target.Provider value={target}>{children}</Target.Provider>
    </SetTarget.Provider>
  );
}

export function SidebarWarningsTarget(
  p: { utility?: boolean; settings?: (() => void) | undefined } = {},
) {
  const setTarget = useContext(SetTarget);
  const settings = useCommittedCallback(() => p.settings?.());
  const hasSettings = !!p.settings;
  const utility = !!p.utility;
  const ref = useCallback(
    (node: HTMLSpanElement | null) =>
      setTarget(
        node
          ? { node, utility, settings: hasSettings ? settings : undefined }
          : null,
      ),
    [setTarget, utility, hasSettings, settings],
  );
  return <span className="sidebar-warnings-target" ref={ref} />;
}

export default function SidebarWarnings({
  data,
  agent,
  opened,
  accountKey,
  syncWorkspaceId,
  open,
  visible = true,
}: {
  data: Snapshot;
  agent?: Agent;
  opened: string | null;
  accountKey: string;
  syncWorkspaceId: string;
  open: (id: string) => void;
  visible?: boolean;
}) {
  const target = useContext(Target);
  // Reuse the transcript projection so existing chat warnings remain available in the sidebar.
  const { items } = useMessages(
    agent ? opened : null,
    "agent",
    agent?.source === "managed",
    data.stateDir,
    syncWorkspaceId,
    visible,
  );
  return target
    ? createPortal(
        <ConversationWarnings
          key={`${data.stateDir}:${opened}:${accountKey}`}
          scope={`${data.stateDir}:${opened}:${accountKey}`}
          notices={data.runtime.nativeNotices ?? []}
          accountKey={accountKey}
          messages={items}
          openAgent={open}
          utility={target.utility}
          settings={target.settings}
        />,
        target.node,
      )
    : null;
}
