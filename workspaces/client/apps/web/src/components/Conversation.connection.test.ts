import { afterEach, expect, it, vi } from "vitest";
import type { ComponentProps, ReactElement } from "react";

const h = vi.hoisted(() => ({
  connection: "live",
  online: false,
  feedback: false,
  cursor: 0,
  state: [] as unknown[],
  effects: [] as Array<() => unknown>,
  dirty: false,
}));
vi.mock("react", async (original) => ({
  ...(await original<typeof import("react")>()),
  useState: (initial: unknown) => {
    const initialValue = () =>
      typeof initial === "function" ? initial() : initial;
    if (!h.feedback) return [initialValue(), vi.fn()];
    const index = h.cursor++;
    if (!(index in h.state)) h.state[index] = initialValue();
    return [
      h.state[index],
      (next: unknown) => {
        const value = typeof next === "function" ? next(h.state[index]) : next;
        if (!Object.is(h.state[index], value)) h.dirty = true;
        h.state[index] = value;
      },
    ];
  },
  useRef: (current: unknown) => ({ current }),
  useMemo: (compute: () => unknown) => compute(),
  useCallback: (callback: unknown) => callback,
  useLayoutEffect: () => {},
  useEffect: (effect: () => unknown) => {
    if (h.feedback) h.effects.push(effect);
  },
}));
vi.mock("../servers/ServerServicesContext", async (original) => ({
  ...(await original<typeof import("../servers/ServerServicesContext")>()),
  useServerApi: () => ({ get: vi.fn(), post: vi.fn() }),
  useServerOutbox: () => ({}),
  useServerUploads: () => ({}),
}));
vi.mock("../servers/activity", async (original) => ({
  ...(await original<typeof import("../servers/activity")>()),
  useServerActivity: () => {},
}));
vi.mock("@mantine/hooks", async (original) => ({
  ...(await original<typeof import("@mantine/hooks")>()),
  useMediaQuery: () => false,
  useNetwork: () => ({ online: h.online }),
}));
vi.mock("../hooks", () => ({
  useMessages: () => ({ items: [], loaded: true, connection: h.connection }),
}));
vi.mock("./useMessageReceipts", () => ({
  useMessageReceipts: () => new Map(),
}));
vi.mock("./removedMessages", () => ({
  useRemovedMessages: () => ({ restored: [], hidden: () => false }),
}));
vi.mock("./usePromptRecall", () => ({ usePromptRecall: () => ({}) }));
vi.mock("./useConversationScroll", () => ({
  useConversationScroll: () => ({
    scroll: { current: null },
    content: { current: null },
  }),
}));
vi.mock("./useAttachmentDrafts", () => ({
  useAttachmentDrafts: () => [{}, vi.fn()],
}));
vi.mock("./useUploadRecovery", () => ({
  useUploadRecovery: () => ({ pending: [] }),
}));
vi.mock("./useChatReadState", () => ({ useVisibleChatResult: () => {} }));
vi.mock("./useDisplayPhase", () => ({ useDisplayPhase: () => "thinking" }));
vi.mock("./agents/WorkerModelPicker", () => ({
  useWorkerModels: () => ({ models: [] }),
}));
vi.mock("./useMessageQueue", () => ({
  useMessageQueue: () => ({ items: [] }),
}));
vi.mock("./useSendingMessageRemoval", () => ({
  useSendingMessageRemoval: () => ({}),
}));
vi.mock("../wasm/client", () => ({}));
vi.mock("../hooks/conversationData", () => ({ useChatWait: () => undefined }));
import { createElement, isValidElement, type ReactNode } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import Conversation from "./Conversation";
import { LiveAgentPhase } from "./agents/AgentPhase";
import { useOfflineSendFeedback } from "./message-delivery/useOfflineSendFeedback";
import { createOutbox, type OutgoingMessage } from "../sync/send";

type Props = ComponentProps<typeof Conversation>;
const agent = {
  id: "lead",
  source: "managed",
  status: "running",
  activity: { phase: "thinking" },
} as Props["agent"];
const saved: OutgoingMessage = {
  id: "message-1",
  body: { id: "message-1", room: "lead", text: "Hello" },
  status: "queued",
  attempted: false,
  created: 1,
};

// Run the real feedback hook across offline/online renders. Effects only update
// hook state; they cannot send or acknowledge the stored message.
function feedback(entries: OutgoingMessage[], errors: string[] = []) {
  h.feedback = true;
  let result: ReturnType<typeof useOfflineSendFeedback>;
  for (let render = 0; render < 10; render++) {
    h.cursor = 0;
    h.dirty = false;
    result = useOfflineSendFeedback(entries, "lead", errors);
    h.effects.splice(0).forEach((effect) => effect());
    if (!h.dirty) break;
  }
  expect(h.dirty).toBe(false);
  h.feedback = false;
  return result!;
}
function phaseElements(
  node: ReactNode,
): ReactElement<ComponentProps<typeof LiveAgentPhase>>[] {
  if (Array.isArray(node)) return node.flatMap(phaseElements);
  if (!isValidElement<{ children?: ReactNode }>(node)) return [];
  if (node.type === LiveAgentPhase)
    return [node as ReactElement<ComponentProps<typeof LiveAgentPhase>>];
  return phaseElements(node.props.children);
}
function renderActivity(pending: boolean, currentAgent = agent) {
  const tree = Conversation({
    id: "lead",
    agent: currentAgent,
    data: { stateDir: "fixture", threads: [], runtime: {} },
    getDraft: () => "",
    offlineSendPending: pending,
    offlineSendDetails: "Failed to fetch",
  } as unknown as Props);
  const phases = phaseElements(tree);
  expect(phases).toHaveLength(1);
  expect(phases[0].props.savedMessagePending).toBe(pending);
  expect(phases[0].props.connectionDetails).toBe("Failed to fetch");
  return renderToStaticMarkup(createElement(LiveAgentPhase, phases[0].props));
}
afterEach(() => {
  h.connection = "live";
  h.online = false;
  h.feedback = false;
  h.state = [];
  h.effects = [];
  vi.unstubAllGlobals();
});

it("renders a saved message with neutral text and icon through the conversation while transport is live", () => {
  expect(feedback([saved]).pending).toBe(true);
  h.online = true;
  const retained = feedback([saved]);
  expect(retained.pending).toBe(true);
  const markup = renderActivity(retained.pending);
  expect(markup).toContain("Your message is saved and waiting for delivery.");
  expect(markup).toContain("lucide-clock");
  expect(markup).not.toContain("Connection lost");
  expect(markup).not.toContain("lucide-wifi-off");
  expect(saved.status).toBe("queued");
});
it("keeps loss wording and icon when transport is reconnecting with a saved message", () => {
  h.connection = "reconnecting";
  const markup = renderActivity(feedback([saved], ["Failed to fetch"]).pending);
  expect(markup).toContain(
    "Connection lost. Your message is saved and will send when connected.",
  );
  expect(markup).toContain("lucide-wifi-off");
  expect(markup).toContain("Failed to fetch");
});
it("retains the reconnecting notice without a saved message", () => {
  h.connection = "reconnecting";
  const markup = renderActivity(false);
  expect(markup).toContain("Connection lost. Reconnecting…");
  expect(markup).toContain("lucide-wifi-off");
});
it.each([true, false])(
  "keeps intentional paused transport neutral (saved=%s)",
  (pending) => {
    h.connection = "paused";
    const markup = renderActivity(pending);
    expect(markup).not.toContain("Connection lost");
    expect(markup).not.toContain("lucide-wifi-off");
    expect(markup).toContain(
      pending ? "Your message is saved and waiting for delivery." : "Thinking",
    );
  },
);
it("retains normal compact activity when there is no delivery notice", () => {
  const markup = renderActivity(false);
  expect(markup).toContain("Thinking");
  expect(markup).toContain("lucide-brain");
  expect(markup).not.toContain("Your message is saved");
});
it("retains terminal transcript errors after delivery instead of duplicating the phase banner", () => {
  const failed = {
    ...agent,
    status: "failed",
    error: "Provider failed",
  } as Props["agent"];
  expect(renderActivity(false, failed)).toBe("");
});
it("keeps queued feedback through a real in-flight send and clears it only after its receipt", async () => {
  let record = { id: saved.id, payload: JSON.stringify(saved) };
  const doc = {
    id: record.id,
    get payload() {
      return record.payload;
    },
    getLatest: () => record,
    incrementalModify: async (modify: (row: typeof record) => typeof record) =>
      (record = modify(record)),
  };
  const entries = (): OutgoingMessage[] => [
    { ...JSON.parse(record.payload), id: record.id },
  ];
  const db = {
    outbox: {
      findOne: () => ({ exec: async () => doc }),
      find: () => ({ exec: async () => [doc] }),
    },
  };
  let receipt!: (value: { id: string; status: "delivered" }) => void;
  let started!: () => void;
  const requestStarted = new Promise<void>((resolve) => {
    started = resolve;
  });
  const post = vi.fn((_path: string, _body: unknown) => {
    started();
    return new Promise<{ id: string; status: "delivered" }>((resolve) => {
      receipt = resolve;
    });
  });
  const outbox = createOutbox({
    api: {
      syncPost: post,
      syncGet: vi.fn(async () => ({ workspaceId: "workspace" })),
      refreshSession: vi.fn(async () => ({ token: "token" })),
      isApiSchemaMismatch: () => false,
      onApiSchemaMismatch: () => () => {},
    } as unknown as Parameters<typeof createOutbox>[0]["api"],
    syncDatabase: vi.fn(async () => ({
      db,
      workspaceId: "workspace",
    })) as unknown as Parameters<typeof createOutbox>[0]["syncDatabase"],
    storage: { getItem: () => null } as unknown as Storage,
  });
  expect(feedback(entries(), ["Failed to fetch"]).pending).toBe(true);
  h.online = true;
  vi.stubGlobal("navigator", { onLine: true });
  const send = outbox.durableSend(saved.body);
  await requestStarted;
  const repeat = outbox.durableSend(saved.body);
  expect(entries()[0].status).toBe("queued");
  expect(renderActivity(feedback(entries()).pending)).not.toContain(
    "Connection lost",
  );
  expect(feedback(entries()).pending).toBe(true);
  receipt({ id: saved.id, status: "delivered" });
  await Promise.all([send, repeat]);
  expect(post).toHaveBeenCalledTimes(1);
  expect(post.mock.calls[0][1]).toEqual(saved.body);
  expect(entries()[0].status).toBe("accepted");
  const completed = feedback(entries(), ["Failed to fetch"]);
  expect(completed.pending).toBe(false);
  expect(completed.owns("Failed to fetch")).toBe(true);
  expect(feedback(entries()).owns("Failed to fetch")).toBe(false);
  expect(renderActivity(completed.pending)).toContain("Thinking");
});
