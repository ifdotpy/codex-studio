import { serverLocalStorage } from "../servers/storage";
import { createSessionCostCache } from "./sessionCostCacheCore";
export * from "./sessionCostCacheCore";

export const { peekSessionCost, storeSessionCost } =
  createSessionCostCache(serverLocalStorage);
