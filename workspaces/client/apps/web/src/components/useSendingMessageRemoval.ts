import { useMemo } from "react";
import {
  useServerApi,
  useServerOutbox,
  useServerStorage,
} from "../servers/ServerServicesContext";
import { createSendingMessageRemoval } from "./removeSendingMessages";

export function useSendingMessageRemoval() {
  const { get, syncPost } = useServerApi();
  const { stopRemovedMessage } = useServerOutbox();
  const localStorage = useServerStorage();
  return useMemo(
    () =>
      createSendingMessageRemoval({
        get,
        syncPost,
        stopRemovedMessage,
        localStorage,
      }),
    [get, syncPost, stopRemovedMessage, localStorage],
  );
}
