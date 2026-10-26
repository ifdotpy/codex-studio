import { createRoot } from "react-dom/client";
import type { Root } from "react-dom/client";
import { useMessageQueue } from "../../src/components/useMessageQueue.ts";
import type { removeSendingMessage } from "../../src/components/removeSendingMessages.ts";

declare global {
  interface Window {
    queue: ReturnType<typeof useMessageQueue>;
    observed: string[][];
    edited: Array<{ id: string; text: string }>;
    advance: number;
    reactRoot: Root;
    outcome: string;
    operation: Promise<string>;
    removal: typeof import("../../src/components/removeSendingMessages.ts");
    removed: typeof import("../../src/components/removedMessages.ts");
    scope: Parameters<typeof removeSendingMessage>[0];
    target: Parameters<typeof removeSendingMessage>[1];
  }
}

function Harness({ workspaceId }: { workspaceId: string }) {
  window.queue = useMessageQueue({
    id: "chat",
    enabled: true,
    scope: "deadline-fixture",
    workspaceId,
    refresh: async () => {},
    observed: (ids) => window.observed.push(ids),
    edited: (id, text) => window.edited.push({ id, text }),
  });
  return null;
}

export function mount(workspaceId: string) {
  const root = createRoot(document.body);
  root.render(<Harness workspaceId={workspaceId} />);
  return root;
}
