import type { BridgeResult } from "./bridge-protocol.ts";

const invalidThreadPage = {
  data: [],
  // @ts-expect-error nextCursor is a string or null in the thread/list result.
  nextCursor: 17,
} satisfies BridgeResult<"thread/list">;
void invalidThreadPage;
