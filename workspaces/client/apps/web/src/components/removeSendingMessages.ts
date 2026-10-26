import { serverLocalStorage as localStorage } from "../servers/storage";
import { get, syncPost } from "../api";
import { stopRemovedMessage } from "../sync/send";
import {
  isRemovedMessage,
  removeMessageFromDevice,
  saveRemovedMessage,
} from "./removedMessages";
import { createSendingMessageRemovalWithPorts } from "./removeSendingMessagesCore";
export { isSendingMessage } from "./removeSendingMessagesCore";
export function createSendingMessageRemoval(
  dependencies = { get, syncPost, stopRemovedMessage, localStorage },
) {
  return createSendingMessageRemovalWithPorts({
    ...dependencies,
    isRemovedMessage,
    removeMessageFromDevice,
    saveRemovedMessage,
  });
}
export const { removeSendingMessage, removeAllSendingMessages } =
  createSendingMessageRemoval();
