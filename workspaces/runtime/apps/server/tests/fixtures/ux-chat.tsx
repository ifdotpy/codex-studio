import React, { useCallback, useRef, useState } from "react";
import { createRoot } from "react-dom/client";
import { MantineProvider } from "@mantine/core";
import "@mantine/core/styles.css";
import "../../web/src/style.css";
import Conversation from "../../web/src/components/Conversation";

function Fixture() {
  const [id, setId] = useState("lead");
  const [drafts, setDrafts] = useState<Record<string, string>>({});
  const currentDrafts = useRef(drafts);
  const draftListeners = useRef(new Map<string, Set<() => void>>());
  const [notice, setNotice] = useState("");
  const [jumpTarget, setJumpTarget] = useState<{
    messageId: string;
    requestId: string;
  }>();
  const [sending, setSending] = useState(false);
  const branchSelections = useRef<string[]>([]);
  const regularSelections = useRef<string[]>([]);
  const agent: any = {
    id,
    rootId: id,
    isLead: false,
    source: "managed",
    name: id,
    cwd: "/fixture",
    status: "idle",
    canSend: true,
    activity: { phase: "idle" },
  };
  const data: any = {
    stateDir: "ux-chat-fixture",
    token: "fixture",
    threads: [agent],
    runtime: {
      agents: [agent],
      requests: [],
      userTasks: [],
      tasks: [],
      monitors: [],
      nativeNotices: [],
    },
  };
  const getDraft = useCallback(
    (session: string) => currentDrafts.current[session] || "",
    [],
  );
  const subscribeDraft = useCallback(
    (session: string, listener: () => void) => {
      let listeners = draftListeners.current.get(session);
      if (!listeners)
        draftListeners.current.set(session, (listeners = new Set()));
      listeners.add(listener);
      return () => {
        listeners?.delete(listener);
        if (!listeners?.size) draftListeners.current.delete(session);
      };
    },
    [],
  );
  const setDraft = (
    value: string | ((text: string) => string),
    target = id,
  ) => {
    const previous = currentDrafts.current[target] || "";
    const text = typeof value === "function" ? value(previous) : value;
    currentDrafts.current = { ...currentDrafts.current, [target]: text };
    setDrafts(currentDrafts.current);
    if (text !== previous)
      draftListeners.current.get(target)?.forEach((listener) => listener());
  };
  (window as any).chatFixture = {
    select: setId,
    jump: (messageId: string) =>
      setJumpTarget({ messageId, requestId: crypto.randomUUID() }),
    drafts,
    id,
    branchSelections: branchSelections.current,
    regularSelections: regularSelections.current,
  };
  return (
    <MantineProvider defaultColorScheme="dark">
      <main
        style={{ height: "100dvh", display: "flex", flexDirection: "column" }}
      >
        {notice && (
          <div id="fixture-notice" role="status">
            {notice}
          </div>
        )}
        <Conversation
          id={id}
          syncWorkspaceId={"a".repeat(32)}
          agent={agent}
          data={data}
          getDraft={getDraft}
          subscribeDraft={subscribeDraft}
          setDraft={setDraft}
          sending={sending}
          send={async (options) => {
            setSending(true);
            try {
              await fetch("/api/fixture-send", {
                method: "POST",
                headers: { "Content-Type": "application/json" },
                body: JSON.stringify({ text: getDraft(id), ...options }),
              });
              setDraft("");
            } finally {
              setSending(false);
            }
          }}
          refresh={async () => {}}
          notify={setNotice}
          limits={null}
          reloadLimits={() => {}}
          onPhase={() => {}}
          onSelect={(target) => {
            regularSelections.current.push(target);
            // The normal selection path rejects a chat missing from its snapshot.
            if (data.threads.some((thread: any) => thread.id === target))
              setId(target);
          }}
          onBranchCreated={(target) => {
            branchSelections.current.push(target);
            setId(target);
          }}
          jumpTarget={jumpTarget}
          onJumpHandled={() => setJumpTarget(undefined)}
        />
      </main>
    </MantineProvider>
  );
}
createRoot(document.getElementById("root")!).render(<Fixture />);
