import React, { useCallback, useRef, useState } from "react";
import { createRoot } from "react-dom/client";
import { MantineProvider } from "@mantine/core";
import "@mantine/core/styles.css";
import "../../src/style.css";
import Conversation from "../../src/components/Conversation";
import { ChatWaitSource } from "../../src/hooks/conversationData";

import type { Agent } from "../../src/types";
import type { components } from "../../src/generated/api";
import type { OutgoingMessage } from "../../src/sync/send";
import { fixtureSnapshot } from "./snapshot.ts";

declare global {
  interface Window {
    __uxChatSyncIdentity?: components["schemas"]["SyncIdentityResponse"];
    __uxChatSendFailure?: string;
    chatFixture: {
      select: (id: string) => void;
      jump: (messageId: string) => void;
      drafts: Record<string, string>;
      id: string;
      branchSelections: string[];
      regularSelections: string[];
    };
  }
}

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
  const [failedOutgoing, setFailedOutgoing] = useState<OutgoingMessage[]>([]);
  const branchSelections = useRef<string[]>([]);
  const regularSelections = useRef<string[]>([]);
  const agent = {
    id,
    rootId: id,
    isLead: false,
    source: "managed",
    name: id,
    cwd: "/fixture",
    status: "idle",
    canSend: true,
    activity: { phase: "idle" },
  } satisfies Agent;
  const accountsState: React.ComponentProps<
    typeof Conversation
  >["accountsState"] = {
    data: {
      accounts: [],
      archivedAccounts: [],
      defaultAccountKey: "default",
      logins: [],
      defaultAccountGroups: [],
      supportsDelete: true,
      supportsDisconnect: true,
    },
    setData: () => {},
    error: "",
    refresh: async () => null,
    scope: "ux-chat-fixture",
  };
  const data = fixtureSnapshot([agent]);
  const [waitSource] = useState(() => new ChatWaitSource());
  waitSource.set(data);
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
  window.chatFixture = {
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
          {...(window.__uxChatSyncIdentity
            ? { syncWorkspaceId: window.__uxChatSyncIdentity.workspaceId }
            : {})}
          agent={agent}
          data={{
            stateDir: data.stateDir,
            threads: data.threads,
            runtime: {
              agents: data.runtime.agents,
              requests: data.runtime.requests,
              projects: data.runtime.projects,
              nativeNotices: data.runtime.nativeNotices ?? [],
            },
          }}
          waitSource={waitSource}
          accountsState={accountsState}
          getDraft={getDraft}
          subscribeDraft={subscribeDraft}
          setDraft={setDraft}
          sending={sending}
          outgoing={failedOutgoing}
          send={async (options) => {
            setSending(true);
            try {
              const failure = window.__uxChatSendFailure;
              if (failure !== undefined) {
                const messageId = "image-refusal";
                setFailedOutgoing([
                  {
                    id: messageId,
                    body: {
                      id: messageId,
                      room: id,
                      text: getDraft(id),
                      assets: [],
                      delivery: "queue",
                    },
                    status: "failed",
                    created: Date.now(),
                    error: failure,
                  },
                ]);
                throw new Error(failure);
              }
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
          onSelect={(target) => {
            regularSelections.current.push(target);
            // The normal selection path rejects a chat missing from its snapshot.
            if (data.threads.some((thread) => thread.id === target))
              setId(target);
          }}
          onBranchCreated={(target) => {
            branchSelections.current.push(target);
            setId(target);
          }}
          {...(jumpTarget ? { jumpTarget } : {})}
          onJumpHandled={() => setJumpTarget(undefined)}
        />
      </main>
    </MantineProvider>
  );
}
const root = document.getElementById("root");
if (!root) throw new Error("Missing chat fixture root");
createRoot(root).render(<Fixture />);
