import { onResume } from "./sync/resume";
import { displayError } from "./errorPresentation";

const deadlines = new Map<
  AbortController,
  { at: number; expire: () => void }
>();
if (typeof window !== "undefined")
  onResume(() => {
    // iOS can suspend JavaScript timers while the request remains unresolved.
    const now = Date.now();
    for (const deadline of deadlines.values())
      if (now >= deadline.at) deadline.expire();
  });

let token = "";
let workspace = "";
export function setWorkspace(value: string) {
  workspace = value;
}
export class ApiError extends Error {
  constructor(
    message: unknown,
    public status: number,
    public readonly details: unknown = message,
  ) {
    super(displayError(message) || `Request failed (${status})`);
  }
}
export function setToken(value: string) {
  token = value;
}
export class NetworkTimeoutError extends TypeError {
  constructor() {
    super("The server did not respond in time.");
    this.name = "NetworkTimeoutError";
  }
}
export async function api<T = any>(
  path: string,
  body?: unknown,
  options: {
    timeoutMs?: number;
    workspaceId?: string;
    signal?: AbortSignal;
  } = {},
): Promise<T> {
  const timeoutMs =
    options.timeoutMs ?? (body === undefined ? 15000 : undefined);
  const controller =
    timeoutMs || options.signal ? new AbortController() : undefined;
  const cancel = () => controller?.abort(options.signal?.reason);
  if (options.signal?.aborted) cancel();
  else options.signal?.addEventListener("abort", cancel, { once: true });
  let timedOut = false;
  const expire = () => {
    if (!controller?.signal.aborted) {
      timedOut = true;
      controller?.abort();
    }
  };
  const timer = timeoutMs ? setTimeout(expire, timeoutMs) : undefined;
  if (controller && timeoutMs)
    deadlines.set(controller, { at: Date.now() + timeoutMs!, expire });
  try {
    const response = await fetch(path, {
      signal: controller?.signal,
      ...(body === undefined
        ? {}
        : {
            method: "POST",
            headers: {
              "Content-Type": "application/json",
              "X-Canvas-Token": token,
              ...((options.workspaceId ?? workspace)
                ? { "X-Canvas-Workspace": options.workspaceId ?? workspace }
                : {}),
            },
            body: JSON.stringify(body),
          }),
    });
    let data;
    try {
      data = await response.json();
    } catch (error) {
      if (!response.ok)
        throw new ApiError(
          `Request failed (${response.status})`,
          response.status,
        );
      throw error;
    }
    if (!response.ok)
      throw new ApiError(
        data?.error ||
          data?.message ||
          (typeof data === "string" ? data : null) ||
          `Request failed (${response.status})`,
        response.status,
        data,
      );
    return data;
  } catch (error) {
    if (timedOut) throw new NetworkTimeoutError();
    throw error;
  } finally {
    clearTimeout(timer);
    options.signal?.removeEventListener("abort", cancel);
    if (controller) deadlines.delete(controller);
  }
}
// Only reads and operations with a durable request identity use this deadline.
export const syncApi = <T = any>(
  path: string,
  body?: unknown,
  options: { workspaceId?: string } = {},
) => api<T>(path, body, { ...options, timeoutMs: 15000 });

export const errorText = displayError;
export function saved<T>(key: string, fallback: T): T {
  try {
    return JSON.parse(localStorage.getItem(key) || "null") ?? fallback;
  } catch {
    return fallback;
  }
}
export function save(key: string, value: unknown) {
  try {
    localStorage.setItem(key, JSON.stringify(value));
  } catch {
    /* Server data remains available if browser storage is full. */
  }
}
