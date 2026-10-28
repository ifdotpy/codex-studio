import { hasNativeClient, nativeRequest } from "../native/client";

/** Sends one usage.* command to the native client core. */
export type UsageRequest = <T>(method: string, params: unknown) => Promise<T>;

/** Returns the core request, or undefined when the cache must stay in memory. */
export type UsagePersistence = () => UsageRequest | undefined;

export const nativeUsage: UsagePersistence = () =>
  hasNativeClient() ? nativeRequest : undefined;
