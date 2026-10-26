import { subscriptionKey } from "./contracts";
export { subscriptionKey } from "./contracts";
import type { components } from "../../generated/api";

type ResourceRef = components["schemas"]["ResourceRef"];
export type SubscriptionCursor = components["schemas"]["SubscriptionCursor"];

/** Hash unbounded selector text to fit the projection's primary-key contract. */
export async function cursorDocumentId(resource: ResourceRef): Promise<string> {
  const digest = await crypto.subtle.digest(
    "SHA-256",
    new TextEncoder().encode(subscriptionKey(resource)),
  );
  return `subscription:cursor:${Array.from(new Uint8Array(digest), (byte) => byte.toString(16).padStart(2, "0")).join("")}`;
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
      return {
        workspaceId,
        seq: cursor.seq,
        ...(Number.isSafeInteger(cursor.terminalGeneration) &&
        cursor.terminalGeneration! >= 0
          ? { terminalGeneration: cursor.terminalGeneration }
          : {}),
      };
  } catch {
    /* A damaged cache has no resumable projection. */
  }
  return undefined;
}
