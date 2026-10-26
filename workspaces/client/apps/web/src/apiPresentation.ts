import { displayError } from "./errorPresentation";
import {
  ApiError,
  ApiSchemaMismatchError,
  NetworkTimeoutError,
} from "./apiErrors";

/** Send one exact write until the server answers it. A timeout means a slow server, not
 * a failure: each attempt has the same request identity, so the server applies it once. */
export async function untilAnswered<T>(send: () => Promise<T>): Promise<T> {
  for (;;) {
    try {
      return await send();
    } catch (error) {
      if (!(error instanceof NetworkTimeoutError)) throw error;
    }
  }
}

export function errorText(error: unknown) {
  if (error instanceof ApiSchemaMismatchError) return "";
  if (
    error instanceof ApiError &&
    error.message === "Invalid request" &&
    error.location?.length
  )
    return `Invalid request (${error.location.join(".")}).`;
  return displayError(error);
}
