import { useMemo } from "react";
import { useServerServices } from "./servicesContext";
import { createNativeRemovedMessageStore } from "./removedMessageStore";
import { useRemovedMessagesView } from "../components/removedMessagesView";
import type { Message } from "../types";
export {
  messageRemovalKeys,
  removedMessagesStorageKey,
  isRemovedMessage,
} from "../components/removedMessageStorage";

export function useRemovedMessages(
  workspace: string,
  kind: string,
  chat: string | null,
) {
  const services = useServerServices();
  const store = useMemo(
    () =>
      createNativeRemovedMessageStore(services.storage, services.localDrafts),
    [services],
  );
  const view = useRemovedMessagesView(
    workspace,
    kind,
    chat,
    services.storage,
    services.preferenceEvents,
  );
  return {
    ...view,
    restore: async () => {
      await store.restore(workspace, kind, chat || "");
      await services.flush();
    },
    remove: async (message: Message) => {
      await store.removeMessageFromDevice(workspace, kind, chat || "", message);
      await services.flush();
    },
  };
}
