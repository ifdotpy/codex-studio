import { createRoot } from "react-dom/client";
import type { Root } from "react-dom/client";
import { useMessageQueue } from "../../src/components/useMessageQueue.ts";
import type { removeSendingMessage } from "../../src/components/removeSendingMessages.ts";

declare global {
  interface Window {
    queueDeadlineQueue: ReturnType<typeof useMessageQueue>;
    queueDeadlineObserved: string[][];
    queueDeadlineEdited: Array<{ id: string; text: string }>;
    queueDeadlineAdvance: number;
    queueDeadlineReactRoot: Root;
    queueDeadlineOutcome: string;
    queueDeadlineOperation: Promise<string>;
    queueDeadlineMount: typeof mount;
    queueDeadlineRemoval: typeof import("../../src/components/removeSendingMessages.ts");
    queueDeadlineRemoved: typeof import("../../src/components/removedMessages.ts");
    queueDeadlineScope: Parameters<typeof removeSendingMessage>[0];
    queueDeadlineTarget: Parameters<typeof removeSendingMessage>[1];
  }
}

function Harness({ workspaceId }: { workspaceId: string }) {
  window.queueDeadlineQueue = useMessageQueue({
    id: "chat",
    enabled: true,
    scope: "deadline-fixture",
    workspaceId,
    refresh: async () => {},
    observed: (ids) => window.queueDeadlineObserved.push(ids),
    edited: (id, text) => window.queueDeadlineEdited.push({ id, text }),
  });
  return null;
}

function mount(workspaceId: string) {
  const root = createRoot(document.body);
  root.render(<Harness workspaceId={workspaceId} />);
  return root;
}
window.queueDeadlineMount = mount;
