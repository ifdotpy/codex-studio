import { required } from "./performance-fixture.ts";
import type { Message } from "../../src/types.ts";
import React, { useState, useMemo } from "react";
import { createRoot } from "react-dom/client";
import { flushSync, createPortal } from "react-dom";
import { MantineProvider } from "@mantine/core";
import TurnHistory from "../../src/components/conversation/transcript/TurnHistory.tsx";
import StreamingText from "../../src/components/conversation/transcript/StreamingText.tsx";
import PromptComposer from "../../src/components/prompt-composer/PromptComposer.tsx";
import { useConversationScroll } from "../../src/components/useConversationScroll.ts";
import { revealHistoryMessage } from "../../src/components/conversation/transcript/historyWindowModel.ts";
import "../../src/components/conversation/transcript/turn-history.css";
import "../../src/visual-activity.css";
const make = (start: number, count: number): Message[] =>
  Array.from({ length: count }, (_, n) => {
    const i = start + n;
    return [
      {
        id: "u" + i,
        role: "user",
        text: "Question " + i,
        turnId: "t" + i,
        turnStatus: "completed",
      },
      ...[0, 1, 2].map((j) => ({
        id: "tool" + i + "-" + j,
        sourceId:
          (i === 500 || i === 0) && j === 1
            ? "original-tool" + i + "-1"
            : undefined,
        role: "tool",
        text: JSON.stringify({
          type: j === 1 ? "commandExecution" : "mcpToolCall",
          command: j === 1 ? "echo saved" : undefined,
          status: "completed",
          tool: "read",
          arguments: { path: "file-" + i },
          result: "Saved output " + i,
        }),
        turnId: "t" + i,
        turnStatus: "completed",
      })),
      {
        id: "a" + i,
        role: "assistant",
        text:
          "Answer " +
          i +
          "\n\n" +
          "A paragraph with **formatted text** for stable viewport anchors. ".repeat(
            10,
          ),
        turnId: "t" + i,
        turnStatus: "completed",
        phase: "final_answer",
      },
    ];
  }).flat();
const listeners = new Set<() => void>();
let draft = "";
const subscribe = (_id: string, listener: () => void) => {
  listeners.add(listener);
  return () => listeners.delete(listener);
};
const getDraft = () => draft;
const setDraft = (value: string) => {
  draft = value;
  listeners.forEach((fn) => fn());
};
window.historyWindowRenderedMessages = 0;
function PreviewMessage({ m }: { m: Message }) {
  const [open, setOpen] = useState(false);
  return (
    <article data-message={m.id} className="message">
      <StreamingText text={m.text} />
      <img
        alt="Saved attachment"
        width="60"
        height="30"
        src="data:image/svg+xml,%3Csvg xmlns='http://www.w3.org/2000/svg' width='60' height='30'%3E%3Crect width='60' height='30' fill='red'/%3E%3C/svg%3E"
      />
      <button onClick={() => setOpen(true)}>Preview attachment</button>
      {open &&
        createPortal(
          <div role="dialog">
            <p>Saved attachment preview</p>
            <button autoFocus onClick={() => setOpen(false)}>
              Close preview
            </button>
          </div>,
          document.body,
        )}
    </article>
  );
}
function Harness() {
  const [items, setItems] = useState<Message[]>(() => readSavedHistory());
  const [sent, setSent] = useState("");
  const [enabled, setEnabled] = useState(true);
  const sc = useConversationScroll("window-browser-fixture", true);
  window.historyWindowPrepend = () =>
    flushSync(() => setItems((old) => [...make(-50, 50), ...old]));
  window.historyWindowLatest = () => sc.setFollow(true);
  window.historyWindowOptimistic = (many: boolean) =>
    flushSync(() => {
      setEnabled(true);
      setItems([
        ...make(0, many ? 1000 : 0),
        {
          id: "optimistic:user",
          clientMessageId: "request",
          role: "user",
          text: "Optimistic preview",
        },
      ]);
    });
  window.historyWindowLegacy = (many) =>
    flushSync(() => {
      setEnabled(false);
      setItems(make(0, many ? 1000 : 1));
    });
  window.historyWindowAck = () =>
    flushSync(() =>
      setItems((old) =>
        old.map((item) =>
          item.clientMessageId === "request"
            ? { ...item, id: "durable-user" }
            : item,
        ),
      ),
    );
  window.historyWindowPrependSameTurn = () =>
    flushSync(() =>
      setItems((old) => {
        const next: Message[] = [
          ...Array.from({ length: 50 }, (_, i) => ({
            id: "prefix-" + i,
            role: "tool",
            text: JSON.stringify({
              type: "mcpToolCall",
              status: "completed",
              tool: "read",
              result: "Older output",
            }),
            turnId: "large",
            turnStatus: "completed",
          })),
          ...old,
        ];
        localStorage.setItem("fixture-large-history", JSON.stringify(next));
        return next;
      }),
    );
  window.historyWindowSmall = () => flushSync(() => setItems(make(0, 1)));
  window.historyWindowLargeTurn = () =>
    flushSync(() =>
      setItems(
        Array.from({ length: 3000 }, (_, i) => ({
          id: "large-" + i,
          role: "tool",
          text: JSON.stringify({
            type: "mcpToolCall",
            status: "completed",
            tool: "read",
            result: "Saved output",
          }),
          turnId: "large",
          turnStatus: "completed",
        })),
      ),
    );
  window.historyWindowAppendStream = () =>
    flushSync(() =>
      setItems((old) => [
        ...old,
        {
          id: "stream",
          role: "assistant",
          text: "Newest streamed reply",
          turnId: "live",
          streaming: true,
        },
      ]),
    );
  window.historyWindowJump = async (id: string) => {
    sc.setFollow(false);
    if (!sc.scroll.current) throw new Error("Missing scroll container");
    for (let i = 0; i < 30; i++) {
      revealHistoryMessage(sc.scroll.current, id);
      const target: HTMLElement | undefined = Array.from(
        sc.scroll.current?.querySelectorAll<HTMLElement>("[data-message]") ??
          [],
      ).find(
        (node) =>
          node.dataset.message === id || node.dataset.sourceMessage === id,
      );
      if (target && !target.hasAttribute("data-lazy-message")) {
        for (
          let parent: HTMLElement | null = target.parentElement;
          parent && parent !== sc.scroll.current;
          parent = parent.parentElement
        )
          if (parent instanceof HTMLDetailsElement) parent.open = true;
        sc.scroll.current.scrollTop +=
          target.getBoundingClientRect().top -
          sc.scroll.current.getBoundingClientRect().top -
          16;
        sc.remember();
        return;
      }
      if (target)
        for (
          let parent: HTMLElement | null = target.parentElement;
          parent && parent !== sc.scroll.current;
          parent = parent.parentElement
        )
          if (parent instanceof HTMLDetailsElement) parent.open = true;
      await new Promise((resolve) => requestAnimationFrame(resolve));
    }
    throw Error("Jump did not find " + id);
  };
  const transcript = useMemo(
    () => (
      <TurnHistory
        items={items}
        enabled={enabled}
        storageKey="history-window-browser"
        scrollContainer={sc.scroll}
        rememberScroll={sc.remember}
        currentTurn="live"
        onJump={window.historyWindowJump}
        renderMessage={(m) => {
          window.historyWindowRenderedMessages++;
          if (m.id === "a500" || m.clientMessageId === "request")
            return <PreviewMessage m={m} />;
          return (
            <article data-message={m.id} className="message">
              <StreamingText text={m.text} streaming={!!m.streaming} />
              {m.id === "a500" && (
                <img
                  alt="Saved attachment"
                  width="60"
                  height="30"
                  src="data:image/svg+xml,%3Csvg xmlns='http://www.w3.org/2000/svg' width='60' height='30'%3E%3Crect width='60' height='30' fill='red'/%3E%3C/svg%3E"
                />
              )}
            </article>
          );
        }}
      />
    ),
    [items],
  );
  return (
    <MantineProvider>
      <style>
        {
          "#messages {height:520px; width:720px; overflow:auto; overflow-anchor:none;} .message {padding:16px; margin:0 0 16px;} p {line-height:24px;} .message-content {min-width:0;}"
        }
      </style>
      <div id="messages" ref={sc.scroll} onScroll={sc.onScroll}>
        <div ref={sc.content} className="message-content">
          {transcript}
        </div>
      </div>
      <PromptComposer
        session="fixture"
        getDraft={getDraft}
        subscribeDraft={subscribe}
        onSubmit={(e) => {
          e.preventDefault();
          setSent(draft);
          setDraft("");
        }}
      >
        {(text) => (
          <>
            <textarea
              id="message"
              value={text}
              onChange={(e) => setDraft(e.target.value)}
            />
            <button>Send</button>
          </>
        )}
      </PromptComposer>
      <output>{sent}</output>
    </MantineProvider>
  );
}
createRoot(required(document.getElementById("root"))).render(<Harness />);
function readSavedHistory(): Message[] {
  // The fixture is the only writer of this saved history document.
  const saved: Message[] | null = JSON.parse(
    localStorage.getItem("fixture-large-history") || "null",
  );
  return saved || make(0, 1000);
}
declare global {
  interface Window {
    historyWindowRenderedMessages: number;
    historyWindowPrepend: () => void;
    historyWindowLatest: () => void;
    historyWindowOptimistic: (many: boolean) => void;
    historyWindowLegacy: (many: boolean) => void;
    historyWindowAck: () => void;
    historyWindowPrependSameTurn: () => void;
    historyWindowSmall: () => void;
    historyWindowLargeTurn: () => void;
    historyWindowAppendStream: () => void;
    historyWindowJump: (id: string) => Promise<void>;
    historyWindowSameTurnNode: Element;
    historyWindowOptimisticNode: Element;
  }
}
