import { onResume } from "./sync/resume";

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
    message: string,
    public status: number,
  ) {
    super(message);
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
  options: { timeoutMs?: number } = {},
): Promise<T> {
  const timeoutMs =
    options.timeoutMs ?? (body === undefined ? 15000 : undefined);
  const controller = timeoutMs ? new AbortController() : undefined;
  let timedOut = false;
  const expire = () => {
    timedOut = true;
    controller?.abort();
  };
  const timer = controller ? setTimeout(expire, timeoutMs) : undefined;
  if (controller)
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
              ...(workspace ? { "X-Canvas-Workspace": workspace } : {}),
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
        data.error || `Request failed (${response.status})`,
        response.status,
      );
    return data;
  } catch (error) {
    if (timedOut) throw new NetworkTimeoutError();
    throw error;
  } finally {
    clearTimeout(timer);
    if (controller) deadlines.delete(controller);
  }
}
// Only reads and operations with a durable request identity use this deadline.
export const syncApi = <T = any>(path: string, body?: unknown) =>
  api<T>(path, body, { timeoutMs: 15000 });

export const errorText = (e: unknown) =>
  e instanceof Error ? e.message : String(e);
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
