import { required } from "./componentTestFixtures";
import { afterEach, expect, it, vi } from "vitest";
import type { ComponentProps, ReactElement } from "react";
import type {
  ResourceConnectionDetail,
  ResourceConnectionState,
  ResourceStreamState,
} from "../sync/resourceConnection";

const h = vi.hoisted(() => ({
  health: "live" as ResourceConnectionState,
  activity: "open" as ResourceStreamState,
  detail: {} as ResourceConnectionDetail,
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
vi.mock("../hooks", async () => {
  const { selectMessageConnection } =
    await import("../hooks/messageConnection");
  return {
    useMessages: () => ({
      items: [],
      loaded: true,
      connection: selectMessageConnection(h.health, h.activity, h.detail),
    }),
  };
});
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
import { localTime } from "../local-time";
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
  expect(required(phases[0]).props.savedMessagePending).toBe(pending);
  expect(required(phases[0]).props.connectionDetails).toBe("Failed to fetch");
  return renderToStaticMarkup(
    createElement(LiveAgentPhase, required(phases[0]).props),
  );
}
afterEach(() => {
  h.health = "live";
  h.activity = "open";
  h.detail = {};
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
it("keeps actual retry activity beside the saved-message notice", () => {
  h.health = "degraded";
  h.activity = "connecting";
  h.detail = { failure: "stream-error" };
  const markup = renderActivity(feedback([saved], ["Failed to fetch"]).pending);
  expect(markup).toContain(
    "Connection failed. Reconnecting… Your message is saved and waiting for delivery.",
  );
  expect(markup).toContain("lucide-loader-circle");
  expect(markup).toContain("Failed to fetch");
});
it("retains the reconnecting notice without a saved message", () => {
  h.health = "degraded";
  h.activity = "connecting";
  h.detail = { failure: "stream-error" };
  const markup = renderActivity(false);
  expect(markup).toContain("Reconnecting…");
  expect(markup).toContain("lucide-loader-circle");
  expect(markup).not.toContain("<details>");
});
it.each([
  ["preparing", "Preparing connection…", "lucide-clock"],
  ["waiting-owner", "Waiting for the shared connection…", "lucide-clock"],
  ["retry-scheduled", "Reason unknown. Retry scheduled.", "lucide-clock"],
  ["connecting", "Reason unknown. Reconnecting…", "lucide-loader-circle"],
  ["open", "Reason unknown. Waiting for server updates…", "lucide-clock"],
  ["offline", "No network.", "lucide-wifi-off"],
  ["stopped", "Reason unknown. Connection interrupted.", "lucide-wifi-off"],
] as const)(
  "renders %s activity through the Conversation caller",
  (activity, copy, icon) => {
    h.health = "degraded";
    h.activity = activity;
    const markup = renderActivity(false);
    expect(markup).toContain(copy);
    expect(markup).toContain(icon);
    if (activity !== "connecting") expect(markup).not.toContain("Reconnecting");
    expect(markup).not.toContain("<details>");
    expect(markup).toContain('role="status"');
  },
);
it("shows no network even while the last validated server health is live", () => {
  h.health = "live";
  h.activity = "offline";
  const markup = renderActivity(false);
  expect(markup).toContain("No network.");
  expect(markup).not.toContain("Reconnecting");
  expect(markup).toContain("lucide-wifi-off");
});
it.each([
  ["stream-error", "Connection failed."],
  ["heartbeat-timeout", "Server updates timed out."],
  ["schema-handshake-timeout", "Server compatibility check timed out."],
  ["subscription-timeout", "Server updates did not start in time."],
  ["subscription-rejected", "Could not confirm live updates."],
  ["invalid-frame", "Received invalid server updates."],
  ["local-state-error", "Could not prepare or apply local sync data."],
  ["unknown", "Reason unknown."],
] as const)(
  "renders the known %s cause and scheduled retry time",
  (failure, cause) => {
    h.health = "degraded";
    h.activity = "retry-scheduled";
    h.detail = { failure, retryAt: 1_792_965_000_000 };
    const markup = renderActivity(false);
    expect(markup).toContain(cause);
    expect(markup).toContain(
      `Retry scheduled for ${localTime(new Date(h.detail.retryAt!), { hour: "2-digit", minute: "2-digit", second: "2-digit" })}.`,
    );
    expect(markup).not.toContain("Reconnecting");
    expect(markup).not.toContain("<details>");
  },
);
it.each([true, false])(
  "keeps intentional paused transport neutral (saved=%s)",
  (pending) => {
    h.health = "paused";
    h.activity = "paused";
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
  expect(required(entries()[0]).status).toBe("queued");
  expect(renderActivity(feedback(entries()).pending)).not.toContain(
    "Connection lost",
  );
  expect(feedback(entries()).pending).toBe(true);
  receipt({ id: saved.id, status: "delivered" });
  await Promise.all([send, repeat]);
  expect(post).toHaveBeenCalledTimes(1);
  expect(required(required(post.mock.calls[0])[1])).toEqual(saved.body);
  expect(required(entries()[0]).status).toBe("accepted");
  const completed = feedback(entries(), ["Failed to fetch"]);
  expect(completed.pending).toBe(false);
  expect(completed.owns("Failed to fetch")).toBe(true);
  expect(feedback(entries()).owns("Failed to fetch")).toBe(false);
  expect(renderActivity(completed.pending)).toContain("Thinking");
});
