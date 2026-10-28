import { randomUUID } from "../uuid";
import type { nativeRequest } from "./client";

// A lost reply leaves the core command applied or not applied. Each attempt
// sends the same request identity, so the core replays the first reply.
const ATTEMPTS = 3;

export async function retriedRequest<T>(
  request: typeof nativeRequest,
  method: string,
  params: unknown,
  id: string = randomUUID(),
): Promise<T> {
  let failure: unknown;
  for (let attempt = 0; attempt < ATTEMPTS; attempt += 1) {
    try {
      return await request<T>(method, params, id);
    } catch (error) {
      failure = error;
    }
  }
  throw failure;
}
