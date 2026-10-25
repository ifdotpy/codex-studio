import { createContext, useContext, useState, type ReactNode } from "react";
import { createPortal } from "react-dom";
import { useMessages } from "../hooks";
import type { Agent, Snapshot } from "../types";
import ConversationWarnings from "./ConversationWarnings";

const Target = createContext<HTMLElement | null>(null);
const SetTarget = createContext<(target: HTMLElement | null) => void>(() => {});

/** The active server retains API ownership while the shared sidebar supplies the target. */
export function SidebarWarningsProvider({ children }: { children: ReactNode }) {
  const [target, setTarget] = useState<HTMLElement | null>(null);
  return (
    <SetTarget.Provider value={setTarget}>
      <Target.Provider value={target}>{children}</Target.Provider>
    </SetTarget.Provider>
  );
}

export function SidebarWarningsTarget() {
  const setTarget = useContext(SetTarget);
  return <span className="sidebar-warnings-target" ref={setTarget} />;
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
        />,
        target,
      )
    : null;
}
