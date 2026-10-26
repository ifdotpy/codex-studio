import { afterEach, expect, it, vi } from "vitest";
import type { ComponentProps, ReactElement } from "react";

const h = vi.hoisted(() => ({ effects: [] as Array<() => unknown> }));
vi.mock("react", async (original) => ({
  ...(await original<typeof import("react")>()),
  useState: (initial: unknown) => [
    typeof initial === "function" ? initial() : initial,
    vi.fn(),
  ],
  useRef: (current: unknown) => ({ current }),
  useMemo: (compute: () => unknown) => compute(),
  useCallback: (callback: unknown) => callback,
  useLayoutEffect: () => {},
  useEffect: (effect: () => unknown) => {
    h.effects.push(effect);
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
}));
vi.mock("../hooks", () => ({
  useMessages: () => ({ items: [], loaded: true }),
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
vi.mock("./useDisplayPhase", () => ({ useDisplayPhase: () => "error" }));
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
import Conversation from "./Conversation";
import { NativeError } from "./NativeNotice";

type Props = ComponentProps<typeof Conversation>;
function render(agent: Props["agent"], reloadLimits: () => void) {
  const element = Conversation({
    id: "lead",
    agent,
    data: { stateDir: "fixture", threads: [], runtime: {} },
    getDraft: () => "",
    reloadLimits,
  } as unknown as Props);
  // The compact-header observer needs only these browser boundaries.
  vi.stubGlobal("window", {
    matchMedia: () => ({
      addEventListener: vi.fn(),
      removeEventListener: vi.fn(),
      matches: false,
    }),
  });
  vi.stubGlobal("document", { querySelector: () => null, documentElement: {} });
  vi.stubGlobal("getComputedStyle", () => ({ getPropertyValue: () => "" }));
  vi.stubGlobal(
    "MutationObserver",
    class {
      observe() {}
      disconnect() {}
    },
  );
  const cleanups = h.effects.splice(0).map((effect) => effect());
  for (const cleanup of cleanups) if (typeof cleanup === "function") cleanup();
  vi.unstubAllGlobals();
  return element;
}
afterEach(() => {
  h.effects = [];
  vi.unstubAllGlobals();
});

it.each(["usageLimitExceeded", "rateLimitExceeded"])(
  "does not read limits after the streamed agent error changes to %s",
  (kind) => {
    const read = vi.fn();
    const reloadLimits = () => read("/api/limits");
    render({ id: "lead", turnId: "turn" } as Props["agent"], reloadLimits);
    render(
      {
        id: "lead",
        turnId: "turn",
        error: { codexErrorInfo: kind },
      } as Props["agent"],
      reloadLimits,
    );
    expect(read).not.toHaveBeenCalled();
  },
);

it("retains the explicit limits refresh action", () => {
  const reloadLimits = vi.fn();
  const element = render(
    {
      id: "lead",
      error: { codexErrorInfo: "usageLimitExceeded" },
    } as Props["agent"],
    reloadLimits,
  );
  const error = (element.props.children as ReactElement[]).find(
    (child) => child?.type === NativeError,
  ) as ReactElement<{ refreshLimits: () => void }>;
  reloadLimits.mockClear();
  error.props.refreshLimits();
  expect(reloadLimits).toHaveBeenCalledOnce();
});
