import { useState, useRef } from "react";
import { createRoot } from "react-dom/client";
import { flushSync } from "react-dom";
import {
  useChatReadState,
  useVisibleChatResult,
} from "../../src/components/useChatReadState.ts";
import type { Agent, Message, Snapshot } from "../../src/types.ts";
import type {
  ChatReadState,
  ChatReadProof,
} from "../../src/components/useChatReadState.ts";
import {
  initialAgent as initial,
  readWrite,
  fixtureReadState,
} from "./chat-read-state.fixture.ts";
import type { ReadWrite } from "./chat-read-state.fixture.ts";
type Controls = ReturnType<typeof useChatReadState>;
type NativeRow = Message & { height: number; marginTop?: number };
interface ChatReadHarness {
  readWrite: typeof readWrite;
  foreground: boolean;
  visible: boolean;
  calls: (ReadWrite & { workspace: string | null })[];
  notifications: string[];
  refreshes: number;
  hold: boolean;
  fail: boolean;
  serverState: ChatReadState | null;
  release: () => void;
  releaseLost: (() => void) | null;
  transientFailure: boolean;
  lostWrites: ReadWrite[];
  lostReplies: number;
  controls: Controls;
  previousControls: Controls | null;
  agent: Agent;
  state: ChatReadState | null;
  marking: string[];
  markUnread: () => Promise<void>;
  observe: (proof: ChatReadProof) => void;
  unmount: () => void;
  setAgent: (value: Agent) => void;
  setOpened: (value: string) => void;
  setWorkspace: (value: string) => void;
  setLoaded: (value: boolean) => void;
  setMessage: (value: string) => void;
  setOffset: (value: number) => void;
  setStreaming: (value: boolean) => void;
  setPhase: (value: Message["phase"]) => void;
  replaceResult: () => void;
  setTurnStatus: (value: Message["turnStatus"]) => void;
  setLatestPage: (value: boolean) => void;
  setResultText: (value: string) => void;
  setGroupTurn: (value: string) => void;
  setGroupTurns: (value: string) => void;
  setNativeRows: (value: NativeRow[] | null) => void;
  setRowsOpen: (value: boolean) => void;
}
declare global {
  interface Window {
    chatReadFixture: ChatReadHarness;
  }
}
const notReady = () => {
  throw new Error("Fixture is not ready");
};
const initialHarness: Pick<
  ChatReadHarness,
  | "readWrite"
  | "foreground"
  | "visible"
  | "calls"
  | "notifications"
  | "refreshes"
  | "hold"
  | "fail"
  | "serverState"
  | "release"
  | "releaseLost"
  | "transientFailure"
  | "lostWrites"
  | "lostReplies"
  | "previousControls"
> = {
  readWrite,
  foreground: false,
  visible: true,
  calls: [],
  notifications: [],
  refreshes: 0,
  hold: false,
  fail: false,
  serverState: fixtureReadState(
    localStorage.getItem("server-read-state") || "null",
  ),
  release: notReady,
  releaseLost: null,
  transientFailure: false,
  lostWrites: [],
  lostReplies: 0,
  previousControls: null,
};
Object.defineProperty(document, "visibilityState", {
  configurable: true,
  get: () => (window.chatReadFixture.visible ? "visible" : "hidden"),
});
document.hasFocus = () => window.chatReadFixture.foreground;

const nativeFetch = window.fetch;
window.fetch = async (input, options) => {
  const request =
    input instanceof Request ? input : new Request(input, options);
  const url = new URL(request.url);
  if (url.pathname != "/api/organization") return nativeFetch(input, options);
  const body = readWrite(JSON.parse(await request.clone().text()));
  window.chatReadFixture.calls.push({
    ...body,
    workspace: request.headers.get("X-Canvas-Workspace"),
  });
  if (window.chatReadFixture.hold)
    await new Promise<void>(
      (resolve) => (window.chatReadFixture.release = resolve),
    );
  if (window.chatReadFixture.fail)
    return new Response(JSON.stringify({ error: "Read state changed" }), {
      status: 409,
    });
  const value = body.read_state;
  window.chatReadFixture.serverState = {
    threadId: value.thread_id,
    turnId: value.turn_id,
    read: value.read,
    revision: value.expected_revision + 1,
  };
  localStorage.setItem(
    "server-read-state",
    JSON.stringify(window.chatReadFixture.serverState),
  );
  await nativeFetch("/api/test/read-state", {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(window.chatReadFixture.serverState),
  });
  return new Response(
    JSON.stringify({
      ...initial,
      readState: window.chatReadFixture.serverState,
    }),
    { headers: { "Content-Type": "application/json" } },
  );
};
function Fixture() {
  const [agent, setAgent] = useState<Agent>({
      ...initial,
      readState: initialHarness.serverState,
    }),
    [opened, setOpened] = useState("one"),
    [workspace, setWorkspace] = useState("first"),
    [loaded, setLoaded] = useState(true),
    [message, setMessage] = useState("turn"),
    [offset, setOffset] = useState(900),
    [streaming, setStreaming] = useState(false),
    [phase, setPhase] = useState<Message["phase"]>("final_answer"),
    [nodeVersion, setNodeVersion] = useState(0),
    [turnStatus, setTurnStatus] = useState<Message["turnStatus"]>(undefined),
    [latestPage, setLatestPage] = useState(true),
    [resultText, setResultText] = useState("Completed result"),
    [groupTurn, setGroupTurn] = useState("turn"),
    [groupTurns, setGroupTurns] = useState("turn"),
    [nativeRows, setNativeRows] = useState<NativeRow[] | null>(null),
    [rowsOpen, setRowsOpen] = useState(true);
  const scroll = useRef<HTMLDivElement>(null);
  const data = {
    stateDir: "/state",
    threads: [agent],
    runtime: {
      agents: [agent],
      studioErrors: [],
      rooms: [],
      tasks: [],
      monitors: [],
      complaints: [],
      requests: [],
      rules: [],
      projects: [],
      peerTeams: [],
      events: [],
      work: [],
    },
    token: "fixture",
    nodes: [agent],
    chats: [],
    edges: [],
  } satisfies Snapshot;
  const controls = useChatReadState(
    data,
    opened,
    (text) => window.chatReadFixture.notifications.push(text),
    async () => {
      window.chatReadFixture.refreshes++;
    },
    workspace,
  );
  window.chatReadFixture = Object.assign(
    window.chatReadFixture ?? initialHarness,
    {
      controls: controls,
      agent: agent,
      state: controls.readStateFor(agent),
      marking: [...controls.marking],
      markUnread: () => controls.markUnread(agent),
      setAgent: (value) => flushSync(() => setAgent(value)),
      setOpened: (value) => flushSync(() => setOpened(value)),
      setWorkspace: (value) => flushSync(() => setWorkspace(value)),
      setLoaded: (value) => flushSync(() => setLoaded(value)),
      setMessage: (value) => flushSync(() => setMessage(value)),
      setOffset: (value) => flushSync(() => setOffset(value)),
      setStreaming: (value) => flushSync(() => setStreaming(value)),
      setPhase: (value) => flushSync(() => setPhase(value)),
      replaceResult: () => flushSync(() => setNodeVersion((v) => v + 1)),
      setTurnStatus: (value) => flushSync(() => setTurnStatus(value)),
      setLatestPage: (value) => flushSync(() => setLatestPage(value)),
      setResultText: (value) => flushSync(() => setResultText(value)),
      setGroupTurn: (value) => flushSync(() => setGroupTurn(value)),
      setGroupTurns: (value) => flushSync(() => setGroupTurns(value)),
      setNativeRows: (value) => flushSync(() => setNativeRows(value)),
      setRowsOpen: (value) => flushSync(() => setRowsOpen(value)),
      observe: (proof) => controls.observeRead(proof),
      unmount: () => appRoot.unmount(),
    } satisfies Omit<ChatReadHarness, keyof typeof initialHarness>,
  );
  useVisibleChatResult(
    scroll,
    agent,
    nativeRows || [
      {
        id: "result",
        role: "assistant",
        turnId: message,
        phase,
        streaming,
        turnStatus,
        text: resultText,
      },
    ],
    loaded,
    controls.observeRead,
    workspace,
    latestPage,
  );
  return (
    <>
      <div id="messages" ref={scroll} style={{ height: 180, overflow: "auto" }}>
        <div style={{ height: offset }} />
        <section
          key={nodeVersion}
          data-turn={groupTurn}
          data-turns={groupTurns}
          data-outcome={turnStatus}
        >
          <>
            {nativeRows ? (
              <details open={rowsOpen}>
                <summary>Joined work</summary>
                {nativeRows.map((row) => (
                  <div
                    key={row.id}
                    data-message={row.id}
                    style={{
                      height: row.height,
                      marginTop: row.marginTop || 0,
                    }}
                  >
                    {row.text}
                  </div>
                ))}
              </details>
            ) : (
              <article data-message="result" style={{ height: 80 }}>
                Completed result
              </article>
            )}
          </>
        </section>
      </div>
      <button onClick={() => void controls.markUnread(agent)}>Unread</button>
    </>
  );
}
const root = document.getElementById("root");
if (!root) throw new Error("Missing fixture root");
const appRoot = createRoot(root);
appRoot.render(<Fixture />);
