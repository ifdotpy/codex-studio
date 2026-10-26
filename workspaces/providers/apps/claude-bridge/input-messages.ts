import type { SDKUserMessage } from "@anthropic-ai/claude-agent-sdk";

export type InputGroup = {
  origin: typeof HUMAN_INPUT | typeof STUDIO_EVENT_INPUT;
  blocks: Exclude<SDKUserMessage["message"]["content"], string>;
};
export type StudioInputMessage = QueuedInput<
  SDKUserMessage & {
    uuid: string;
    text_not_typed?: boolean;
    studioInputIdentity?: string;
  }
>;

import type { Item } from "./session-state.ts";
import type { QueuedInput } from "./features.ts";
import { HUMAN_INPUT, STUDIO_EVENT_INPUT } from "./input-origin.generated.ts";

// Validate every origin before conversion or submission. An omitted field is
// the legacy human-input contract; unknown provenance must not become human.
export async function inputGroups<T extends object, B>(
  input: T[] | null | undefined,
  convert: (input: T[]) => Promise<B>,
) {
  const groups: { origin: InputGroup["origin"]; input: T[] }[] = [];
  for (const item of input || []) {
    const suppliedOrigin = "inputOrigin" in item ? item.inputOrigin : undefined;
    const origin = suppliedOrigin === undefined ? HUMAN_INPUT : suppliedOrigin;
    if (origin !== HUMAN_INPUT && origin !== STUDIO_EVENT_INPUT)
      throw new Error("Unsupported Claude input origin: " + origin);
    if (groups.at(-1)?.origin !== origin) groups.push({ origin, input: [] });
    const group = groups.at(-1);
    if (group) group.input.push(item);
  }
  if (!groups.length) groups.push({ origin: HUMAN_INPUT, input: input || [] });
  return Promise.all(
    groups.map(async ({ origin, input }) => ({
      origin,
      blocks: await convert(input),
    })),
  );
}

// Claude 2.1.302 accepts task-notification (sdk.d.ts SDKMessageOrigin).
// Local synthetic ingress becomes unclassified + isMeta instead:
// readable/chunk-cgx41xfz.js:1783-1795 and chunk-1w3j266c.js:32892.
// CWe excludes that combination from the permission classifier as invisible
// meta (chunk-2jyw98bz.js:22214-22216, 22339-22363). It is not human framing.
// text_not_typed suppresses slash, mention, and skill expansion (:9650-9652
// in chunk-5c94ftfz.js); it does not supply authority by itself.
export function sdkInputMessages(
  sessionId: string,
  identity: string,
  groups: InputGroup[],
  nativeId: (id: string) => NonNullable<SDKUserMessage["uuid"]>,
): StudioInputMessage[] {
  return groups.map(({ origin, blocks }, index) => {
    const last = index === groups.length - 1;
    const message: StudioInputMessage = {
      type: "user",
      uuid: nativeId(last ? identity : `${identity}:part:${index}`),
      session_id: sessionId,
      parent_tool_use_id: null,
      message: { role: "user", content: blocks },
      ...(origin === STUDIO_EVENT_INPUT
        ? {
            origin: { kind: "task-notification" },
            isSynthetic: true,
            text_not_typed: true,
          }
        : {}),
      // Preserve separate provenance without starting one query per part.
      ...(!last ? { shouldQuery: false } : {}),
    };
    Object.defineProperty(message, "studioInputIdentity", { value: identity });
    return message;
  });
}

// Ordered native UUIDs of one logical input. The final UUID keeps the logical
// input identity; the first UUID is its history boundary, never a checkpoint.
export function nativeInputIds(
  item: Pick<Item, "nativeIds" | "nativeId"> | null | undefined,
) {
  const ids = item?.nativeIds ?? [item?.nativeId];
  if (
    !Array.isArray(ids) ||
    !ids.length ||
    !ids.every((id): id is string => typeof id === "string" && !!id) ||
    new Set(ids).size !== ids.length ||
    (item?.nativeId && ids.at(-1) !== item.nativeId)
  )
    return null;
  return ids;
}
