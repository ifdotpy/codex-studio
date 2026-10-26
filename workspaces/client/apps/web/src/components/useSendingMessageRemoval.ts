import { useMemo } from "react";
import {
  useServerApi,
  useServerOutbox,
  useServerStorage,
} from "../servers/ServerServicesContext";
import { createSendingMessageRemovalWithPorts } from "./removeSendingMessagesCore";
import {
  isRemovedMessage,
  removeMessageFromDevice,
  saveRemovedMessage,
} from "./removedMessages";

export function useSendingMessageRemoval() {
  const { get, syncPost } = useServerApi();
  const { stopRemovedMessage } = useServerOutbox();
  const localStorage = useServerStorage();
  return useMemo(
    () =>
      createSendingMessageRemovalWithPorts({
        get,
        syncPost,
        stopRemovedMessage,
        localStorage,
        isRemovedMessage,
        removeMessageFromDevice,
        saveRemovedMessage,
      }),
    [get, syncPost, stopRemovedMessage, localStorage],
  );
}
