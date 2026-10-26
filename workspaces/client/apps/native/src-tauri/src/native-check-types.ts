import type { Host } from "../../../web/src/native/host.ts";

import type { Response } from "../../../web/src/generated/rustOwned.ts";

export type CheckResponse<T = Record<string, unknown>> = Omit<
  Response,
  "result"
> & { result: T };

declare global {
  interface Window {
    __studioCheckStarted?: boolean;
    __studioCheckMode: string;
    __TAURI__: Host;
  }
}
