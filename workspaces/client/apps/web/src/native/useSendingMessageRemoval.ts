import { useMemo } from "react";
import { useServerServices } from "./servicesContext";
import { createSendingMessageRemovalWithPorts } from "../components/removeSendingMessagesCore";
import { isRemovedMessage } from "../components/removedMessageStorage";
import { createNativeRemovedMessageStore } from "./removedMessageStore";
import type { ServerApi } from "../api";
import type { createNativeLocalDrafts } from "./services/localDrafts";

type Services = {
  owner: { workspaceId: string };
  api: Pick<ServerApi, "get" | "syncPost">;
  outbox: { stopRemovedMessage(id: string): Promise<boolean> };
  storage: Storage;
  localDrafts: Pick<
    ReturnType<typeof createNativeLocalDrafts>,
    "updateLocalDraft"
  >;
  flush(): Promise<void>;
};
export function createNativeSendingMessageRemoval(services: Services) {
  const store = createNativeRemovedMessageStore(
    services.storage,
    services.localDrafts,
  );
  const removal = createSendingMessageRemovalWithPorts({
    get: services.api.get,
    syncPost: services.api.syncPost,
    stopRemovedMessage: services.outbox.stopRemovedMessage,
    localStorage: services.storage,
    saveRemovedMessage: store.saveRemovedMessage,
    removeMessageFromDevice: store.removeMessageFromDevice,
    isRemovedMessage,
    flush: services.flush,
  });
  const check = (scope: Parameters<typeof removal.removeSendingMessage>[0]) => {
    if (
      scope.workspaceId !== undefined &&
      scope.workspaceId !== services.owner.workspaceId
    )
      throw new Error(
        "The message removal belongs to another native workspace.",
      );
  };
  return {
    async removeSendingMessage(
      ...args: Parameters<typeof removal.removeSendingMessage>
    ) {
      check(args[0]);
      return removal.removeSendingMessage(...args);
    },
    async removeAllSendingMessages(
      ...args: Parameters<typeof removal.removeAllSendingMessages>
    ) {
      check(args[0]);
      return removal.removeAllSendingMessages(...args);
    },
  };
}
export function useSendingMessageRemoval() {
  const services = useServerServices();
  return useMemo(() => createNativeSendingMessageRemoval(services), [services]);
}
