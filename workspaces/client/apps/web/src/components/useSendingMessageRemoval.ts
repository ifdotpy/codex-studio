import { useMemo } from "react";
import {
  useServerApi,
  useServerOutbox,
  useServerStorage,
} from "../servers/ServerServicesContext";
import { createSendingMessageRemoval } from "./removeSendingMessages";

export function useSendingMessageRemoval() {
  const { get, command } = useServerApi();
  const { stopRemovedMessage } = useServerOutbox();
  const localStorage = useServerStorage();
  return useMemo(
    () =>
      createSendingMessageRemoval({
        get,
        command,
        stopRemovedMessage,
        localStorage,
      }),
    [get, command, stopRemovedMessage, localStorage],
  );
}
