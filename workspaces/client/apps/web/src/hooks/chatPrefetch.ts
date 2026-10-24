import { useMemo } from "react";
import type { Snapshot } from "../types";

/** Hover preparation no longer owns transcript subscriptions. The mounted chat
 * owns its resource through useSubscription; existing local history stays cached.
 * Keep the UI's preparation/delete callbacks stable without warming hidden chats. */
export function useChatPrefetch(
  _data: Snapshot | null,
  _opened: string | null,
  _workspaceId: string,
  _enabled = true,
) {
  return useMemo(
    () =>
      Object.assign((_id: string) => {}, {
        suspend: (_ids: ReadonlySet<string>) => {},
        resume: (_ids: ReadonlySet<string>) => {},
      }),
    [],
  );
}
