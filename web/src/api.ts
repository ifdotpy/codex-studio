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
let sessionGeneration = 0;
let manualTokenGeneration = 0;
const pendingSessions = new Set<number>();
let successfulSession: { generation: number; token: string } | undefined;
let confirmedSession: { generation: number; token: string } | undefined;
let workspace = "";
export type ApiReadMetadata = { etag?: string; notModified?: boolean };
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
  manualTokenGeneration = ++sessionGeneration;
  successfulSession = undefined;
  confirmedSession = undefined;
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
    sessionToken?: string;
    signal?: AbortSignal;
    etag?: string;
    readMetadata?: ApiReadMetadata;
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
        ? options.etag
          ? { headers: { "If-None-Match": options.etag } }
          : {}
        : {
            method: "POST",
            headers: {
              "Content-Type": "application/json",
              "X-Canvas-Token": options.sessionToken ?? token,
              ...((options.workspaceId ?? workspace)
                ? { "X-Canvas-Workspace": options.workspaceId ?? workspace }
                : {}),
            },
            body: JSON.stringify(body),
          }),
    });
    if (response.status === 304 && options.readMetadata) {
      options.readMetadata.etag = response.headers.get("ETag") || options.etag;
      options.readMetadata.notModified = true;
      return undefined as T;
    }
    if (options.readMetadata)
      options.readMetadata.etag = response.headers.get("ETag") || undefined;
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
    if (
      body !== undefined &&
      Array.isArray(data?._syncEntities) &&
      data._syncEntities.length &&
      typeof window !== "undefined"
    )
      window.dispatchEvent(
        new CustomEvent("codex-sync-entities", {
          detail: {
            workspaceId: options.workspaceId ?? workspace,
            documents: data._syncEntities,
          },
        }),
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

export async function apiDownload(path: string): Promise<{
  blob: Blob;
  name: string;
  truncated: boolean;
}> {
  const response = await fetch(path, {
    headers: {
      "X-Canvas-Token": token,
      ...(workspace ? { "X-Canvas-Workspace": workspace } : {}),
    },
  });
  if (!response.ok) {
    let details: any;
    try {
      details = await response.json();
    } catch {
      details = `Request failed (${response.status})`;
    }
    throw new ApiError(
      details?.error || details?.message || details,
      response.status,
      details,
    );
  }
  const disposition = response.headers.get("Content-Disposition") || "";
  const name = disposition.match(/filename="([^"]+)"/)?.[1] || "download.log";
  return {
    blob: await response.blob(),
    name,
    truncated: response.headers.get("X-Log-Truncated") === "true",
  };
}
// Only reads and operations with a durable request identity use this deadline.
export const syncApi = <T = any>(
  path: string,
  body?: unknown,
  options: { workspaceId?: string; sessionToken?: string } = {},
) => api<T>(path, body, { ...options, timeoutMs: 15000 });

export async function refreshSession(): Promise<{ token: string }> {
  const generation = ++sessionGeneration;
  pendingSessions.add(generation);
  let session: { token: string };
  try {
    session = await syncApi<{ token: string }>("/api/session");
    if (
      generation > manualTokenGeneration &&
      (!successfulSession || successfulSession.generation < generation)
    )
      successfulSession = { generation, token: session.token };
  } finally {
    pendingSessions.delete(generation);
    // A failed newer read releases the last successful credential. Pending
    // newer reads and explicit setToken owners keep their precedence.
    const latest = successfulSession;
    if (
      latest &&
      ![...pendingSessions].some((pending) => pending > latest.generation)
    ) {
      token = latest.token;
      confirmedSession = latest;
    }
  }
  // A delayed response cannot replace a newer confirmed server credential.
  if (confirmedSession && confirmedSession.generation > generation)
    return { ...session, token: confirmedSession.token };
  return session;
}

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
