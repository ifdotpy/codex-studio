import { subscriptionKey } from "./contracts";
import { sha256Hex } from "../../sha256";
export { subscriptionKey } from "./contracts";
import type { components } from "../../generated/api";

type ResourceRef = components["schemas"]["ResourceRef"];
export type SubscriptionCursor = components["schemas"]["SubscriptionCursor"];

/** Hash unbounded selector text to fit the projection's primary-key contract. */
export async function cursorDocumentId(resource: ResourceRef): Promise<string> {
  const digest = await sha256Hex(
    new TextEncoder().encode(subscriptionKey(resource)),
  );
  return `subscription:cursor:${digest}`;
}

export function parseAppliedCursor(
  payload: string | undefined,
  workspaceId: string,
): SubscriptionCursor | undefined {
  try {
    const cursor = JSON.parse(payload ?? "null") as SubscriptionCursor | null;
    if (
      cursor?.workspaceId === workspaceId &&
      Number.isSafeInteger(cursor.seq) &&
      cursor.seq >= 0
    )
      return { workspaceId, seq: cursor.seq };
  } catch {
    /* A damaged cache has no resumable projection. */
  }
  return undefined;
}
