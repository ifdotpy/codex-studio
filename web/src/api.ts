import createClient from "openapi-fetch";
import type { paths } from "./generated/api";
import type {
  ApiGetOptions,
  ApiOperationFor,
  ApiPathsFor,
  ApiPathsWithRequiredQuery,
  ApiQueryFor,
  ApiRequestBodyFor,
  ApiSuccessBodyFor,
} from "./apiContracts";
import { onResume } from "./sync/resume";
import { displayError } from "./errorPresentation";

type Method = "get" | "post";
type PathsFor<M extends Method> = ApiPathsFor<paths, M>;
type Operation<Path extends keyof paths, M extends Method> = ApiOperationFor<
  paths,
  Path,
  M
>;
type QueryOf<Op> = ApiQueryFor<Op>;
type SuccessBody<Op> = ApiSuccessBodyFor<Op>;
type BodyOf<Op> = ApiRequestBodyFor<Op>;
type PathsWithRequiredQuery = ApiPathsWithRequiredQuery<paths>;

export type ApiReadMetadata = { etag?: string; notModified?: boolean };
export type ApiGetPath = PathsFor<"get">;
export type ApiPostPath = PathsFor<"post">;
export type ApiOptions = {
  timeoutMs?: number;
  workspaceId?: string;
  sessionToken?: string;
  signal?: AbortSignal;
  cache?: RequestCache;
};
export type GetOptions<Path extends PathsFor<"get">> = ApiGetOptions<
  Operation<Path, "get">,
  ApiOptions,
  ApiReadMetadata
>;
export type PostOptions = ApiOptions;
export type GetResult<Path extends PathsFor<"get">> = SuccessBody<
  Operation<Path, "get">
>;
export type PostResult<Path extends PathsFor<"post">> = SuccessBody<
  Operation<Path, "post">
>;
export type PostBody<Path extends PathsFor<"post">> = BodyOf<
  Operation<Path, "post">
>;

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

export const client = createClient<paths>({
  baseUrl: globalThis.location?.origin ?? "http://localhost",
  fetch: (request) => globalThis.fetch(request),
});

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

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function errorPayload(value: unknown, status: number): ApiError {
  const message = isRecord(value)
    ? value.error || value.message || `Request failed (${status})`
    : typeof value === "string"
      ? value
      : `Request failed (${status})`;
  return new ApiError(message, status, value);
}

function requestController(options: ApiOptions, timeoutMs: number | undefined) {
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
    deadlines.set(controller, { at: Date.now() + timeoutMs, expire });
  return {
    signal: controller?.signal,
    timedOut: () => timedOut,
    finish: () => {
      clearTimeout(timer);
      options.signal?.removeEventListener("abort", cancel);
      if (controller) deadlines.delete(controller);
    },
  };
}

function syncDocuments(value: unknown, workspaceId: string | undefined) {
  if (
    !isRecord(value) ||
    !Array.isArray(value._syncEntities) ||
    value._syncEntities.length === 0 ||
    typeof window === "undefined"
  )
    return;
  window.dispatchEvent(
    new CustomEvent("codex-sync-entities", {
      detail: {
        workspaceId: workspaceId ?? workspace,
        documents: value._syncEntities,
      },
    }),
  );
}

export function get<Path extends PathsFor<"get">>(
  path: Path,
  options: GetOptions<Path> & { readMetadata: ApiReadMetadata },
): Promise<GetResult<Path> | undefined>;
export function get<Path extends PathsWithRequiredQuery>(
  path: Path,
  options: GetOptions<Path> & { readMetadata?: undefined },
): Promise<GetResult<Path>>;
export function get<
  Path extends Exclude<PathsFor<"get">, PathsWithRequiredQuery>,
>(
  path: Path,
  options?: GetOptions<Path> & { readMetadata?: undefined },
): Promise<GetResult<Path>>;
export async function get<Path extends PathsFor<"get">>(
  path: Path,
  options?: GetOptions<Path>,
): Promise<GetResult<Path> | undefined> {
  const requestOptions = options ?? {};
  const timeoutMs = requestOptions.timeoutMs ?? 15000;
  const controller = requestController(requestOptions, timeoutMs);
  try {
    const result = await client.GET(path, {
      ...(requestOptions.query === undefined
        ? {}
        : { params: { query: requestOptions.query } }),
      ...(controller.signal ? { signal: controller.signal } : {}),
      ...(requestOptions.cache ? { cache: requestOptions.cache } : {}),
      ...(requestOptions.etag
        ? { headers: { "If-None-Match": requestOptions.etag } }
        : {}),
    });
    const response = result.response;
    if (response.status === 304 && requestOptions.readMetadata) {
      requestOptions.readMetadata.etag =
        response.headers.get("ETag") || requestOptions.etag;
      requestOptions.readMetadata.notModified = true;
      return undefined;
    }
    if (requestOptions.readMetadata)
      requestOptions.readMetadata.etag =
        response.headers.get("ETag") || undefined;
    if (!response.ok) throw errorPayload(result.error, response.status);
    return result.data;
  } catch (error) {
    if (controller.timedOut()) throw new NetworkTimeoutError();
    throw error;
  } finally {
    controller.finish();
  }
}

export async function post<Path extends PathsFor<"post">>(
  path: Path,
  body: PostBody<Path>,
  options: PostOptions = {},
): Promise<PostResult<Path>> {
  const timeoutMs = options.timeoutMs;
  const controller = requestController(options, timeoutMs);
  try {
    const result = await client.POST(path, {
      body,
      ...(controller.signal ? { signal: controller.signal } : {}),
      headers: {
        "Content-Type": "application/json",
        "X-Canvas-Token": options.sessionToken ?? token,
        ...((options.workspaceId ?? workspace)
          ? { "X-Canvas-Workspace": options.workspaceId ?? workspace }
          : {}),
      },
    });
    if (!result.response.ok)
      throw errorPayload(result.error, result.response.status);
    syncDocuments(result.data, options.workspaceId);
    return result.data;
  } catch (error) {
    if (controller.timedOut()) throw new NetworkTimeoutError();
    throw error;
  } finally {
    controller.finish();
  }
}

export function apiDownload<Path extends PathsWithRequiredQuery>(
  path: Path,
  query: QueryOf<Operation<Path, "get">>,
): Promise<{ blob: Blob; name: string; truncated: boolean }>;
export function apiDownload<
  Path extends Exclude<PathsFor<"get">, PathsWithRequiredQuery>,
>(
  path: Path,
  query?: QueryOf<Operation<Path, "get">>,
): Promise<{
  blob: Blob;
  name: string;
  truncated: boolean;
}>;
export async function apiDownload<Path extends PathsFor<"get">>(
  path: Path,
  query?: QueryOf<Operation<Path, "get">>,
): Promise<{
  blob: Blob;
  name: string;
  truncated: boolean;
}> {
  const result = await client.GET(path, {
    ...(query === undefined ? {} : { params: { query } }),
    parseAs: "blob",
    headers: {
      "X-Canvas-Token": token,
      ...(workspace ? { "X-Canvas-Workspace": workspace } : {}),
    },
  });
  const response = result.response;
  if (!response.ok) throw errorPayload(result.error, response.status);
  const disposition = response.headers.get("Content-Disposition") || "";
  const name = disposition.match(/filename="([^"]+)"/)?.[1] || "download.log";
  return {
    blob: result.data,
    name,
    truncated: response.headers.get("X-Log-Truncated") === "true",
  };
}

// Only reads and operations with a durable request identity use this deadline.
type SyncGetOptions<Path extends PathsFor<"get">> = Omit<
  GetOptions<Path>,
  "timeoutMs"
>;
export function syncGet<Path extends PathsFor<"get">>(
  path: Path,
  options: SyncGetOptions<Path> & { readMetadata: ApiReadMetadata },
): Promise<GetResult<Path> | undefined>;
export function syncGet<Path extends PathsWithRequiredQuery>(
  path: Path,
  options: SyncGetOptions<Path> & { readMetadata?: undefined },
): Promise<GetResult<Path>>;
export function syncGet<
  Path extends Exclude<PathsFor<"get">, PathsWithRequiredQuery>,
>(
  path: Path,
  options?: SyncGetOptions<Path> & { readMetadata?: undefined },
): Promise<GetResult<Path>>;
export function syncGet<Path extends PathsFor<"get">>(
  path: Path,
  options?: SyncGetOptions<Path>,
): Promise<GetResult<Path> | undefined> {
  return get(path, { ...options, timeoutMs: 15000 });
}
export const syncPost = <Path extends PathsFor<"post">>(
  path: Path,
  body: PostBody<Path>,
  options?: Omit<PostOptions, "timeoutMs">,
) => post(path, body, { ...options, timeoutMs: 15000 });

export type QueueView = GetResult<"/api/queue">;
export type QueueItemDto = QueueView["items"][number];

export async function refreshSession(): Promise<GetResult<"/api/session">> {
  const generation = ++sessionGeneration;
  pendingSessions.add(generation);
  let session: GetResult<"/api/session">;
  try {
    session = await get("/api/session");
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
