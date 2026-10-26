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

export type MessageRequest = components["schemas"]["MessageRequest"];
export type ComplaintDetail = components["schemas"]["ComplaintDetailResponse"];
export type ComplaintBody = components["schemas"]["ComplaintBody"];
