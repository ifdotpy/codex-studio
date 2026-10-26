import { serverLocalStorage } from "../servers/storage";
import { createAccountLimitsCache } from "./accountUsageCache";
export * from "./accountUsageCache";

export const {
  configureLimitsCache,
  cachedAccountLimits,
  subscribeLimitsCache,
  cacheAccountLimits,
  cachedLimitsByAccount,
} = createAccountLimitsCache(serverLocalStorage);
