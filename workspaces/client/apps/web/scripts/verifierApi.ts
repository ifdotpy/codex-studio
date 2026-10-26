import type { Request, Response } from "playwright";

// Studio's Rust routes own these generated response/request contracts. Keep
// parsing in these boundaries and retain each verifier's existing checks and
// choice to catch or propagate malformed JSON.
export function parseVerifierApi<T>(text: string): T {
  return JSON.parse(text);
}

export function readVerifierResponse<T>(
  response: Pick<Response, "json">,
): Promise<T> {
  return response.json();
}

export function readVerifierRequest<T>(
  request: Pick<Request, "postDataJSON">,
): T {
  return request.postDataJSON();
}
