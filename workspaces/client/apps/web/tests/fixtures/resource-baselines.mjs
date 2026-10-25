import { subscriptionKey } from "../../src/sync/subscriptions/contracts.ts";
import { SYNC_PROTOCOL_VERSION } from "../../src/generated/api.ts";

/** Maximum fixture baseline wait, in milliseconds. */
const RESOURCE_BASELINE_TIMEOUT_MS = 15_000;

/**
 * Read one baseline per requested view, then release the real fixture stream.
 * @param {string} origin
 * @param {import("../../src/generated/api").components["schemas"]["ResourceRef"][]} resources
 * @returns {Promise<import("../../src/generated/api").components["schemas"]["ResourceRevisionEntry"][]>}
 */
export async function readResourceBaselines(origin, resources) {
  if (!resources.length) return [];
  const url = new URL("/api/sync/stream", origin);
  url.searchParams.set("protocol", String(SYNC_PROTOCOL_VERSION));
  url.searchParams.set("resources", JSON.stringify(resources));
  const response = await fetch(url, {
    signal: AbortSignal.timeout(RESOURCE_BASELINE_TIMEOUT_MS),
  });
  if (!response.ok || !response.body)
    throw new Error(`Fixture resource stream failed (${response.status})`);
  const reader = response.body.getReader();
  const decoder = new TextDecoder();
  const pending = new Set(resources.map(subscriptionKey));
  const entries = [];
  let buffer = "";
  try {
    for (;;) {
      const { done, value } = await reader.read();
      if (done)
        throw new Error(
          `Fixture stream ended before baselines: ${[...pending]}`,
        );
      buffer += decoder.decode(value, { stream: true });
      let boundary;
      while ((boundary = buffer.indexOf("\n\n")) >= 0) {
        const frame = buffer.slice(0, boundary);
        buffer = buffer.slice(boundary + 2);
        if (!frame.startsWith("event: resources\n")) continue;
        const event = JSON.parse(
          frame
            .split("\n")
            .filter((line) => line.startsWith("data: "))
            .map((line) => line.slice(6))
            .join("\n"),
        );
        for (const [index, entry] of (event.resourceVersions || []).entries()) {
          const resource = entry.resource || event.resources[index];
          const key = subscriptionKey(resource);
          if (pending.has(key)) {
            if (!entry.cursor || !entry.replyKind)
              throw new Error(
                `Fixture resource baseline is incomplete: ${key}`,
              );
            entries.push({ ...entry, resource });
            pending.delete(key);
          }
        }
        if (!pending.size) return entries;
      }
    }
  } finally {
    await reader.cancel();
  }
}
