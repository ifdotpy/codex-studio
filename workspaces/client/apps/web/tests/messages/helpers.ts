import type { Route } from "@playwright/test";
import type { components } from "../../src/generated/api.ts";

export { required } from "./browser-helpers.ts";

/** The fixture consumes Studio requests using the generated route body contract. */
export function requestBody<T>(route: Pick<Route, "request">): T {
  return route.request().postDataJSON() as T;
}

/** Studio HTTP responses use the generated contract selected by the caller. */
export async function responseJson<T>(
  response: Pick<Response, "json">,
): Promise<T> {
  return response.json() as Promise<T>;
}

export type MessageRequest =
  import("../../src/api.ts").PostBody<"/api/messages">;
export type ComplaintDetail = components["schemas"]["ComplaintDetailResponse"];
export type ComplaintBody = components["schemas"]["ComplaintBody"];

/** TCP port of an already listening disposable fixture server. */
export function listeningPort(
  server: Pick<import("node:net").Server, "address">,
): number {
  const address = server.address();
  if (!address || typeof address === "string")
    throw new Error("Fixture server has no TCP address");
  return address.port;
}
