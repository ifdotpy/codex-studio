import { serverLocalStorage } from "./servers/storage";
import { createChatCreationStore } from "./chatCreationStore";
export * from "./chatCreationStore";

export const {
  pendingChatCreations,
  saveChatCreation,
  confirmChatCreation,
  confirmChatCreationResponse,
  rejectChatCreation,
  settleFailedChatCreation,
  dismissChatCreation,
} = createChatCreationStore(serverLocalStorage);
