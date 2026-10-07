import createClient from "openapi-fetch";
import type { FetchResponse } from "openapi-fetch";
import type { components, paths } from "./generated/api";
import {
  API_SCHEMA_HASH,
  API_SCHEMA_HASH_HEADER,
  API_SCHEMA_MISMATCH_HEADER,
} from "./generated/apiSchema";
import type {
  ApiGetOptions,
  ApiPathsFor,
  ApiPathsWithRequiredQuery,
  ApiQueryFor,
  ApiRequestBodyFor,
  ApiSuccessBodyFor,
} from "./apiContracts";
import { onResume } from "./sync/resume";
import { displayError } from "./errorPresentation";

type Method = "get" | "post";
type PathsFor<M extends Method> = ApiPathsFor<paths, M> & keyof paths;
type Operation<
  Path extends keyof paths,
  M extends Method,
> = Path extends keyof paths ? NonNullable<paths[Path][M]> : never;
type QueryOf<Op> = ApiQueryFor<Op>;
type QueryOfPath<Path extends PathsFor<"get">> = Path extends keyof paths
  ? ApiQueryFor<NonNullable<paths[Path]["get"]>>
  : never;
type InternalGetOptions<Path extends PathsFor<"get">> = ApiOptions & {
  query?: QueryOf<Operation<Path, "get">>;
  etag?: string;
  readMetadata?: ApiReadMetadata;
};
type PathsWithRequiredQuery = ApiPathsWithRequiredQuery<paths>;
type DownloadInit<Path extends PathsFor<"get">> = {
  parseAs: "blob";
  params?: { query?: ApiQueryFor<NonNullable<paths[Path]["get"]>> };
  headers?: Record<string, string>;
  [key: string]: unknown;
};
type JsonGetInit<Path extends PathsFor<"get">> = {
  parseAs: "json";
  params?: { query?: ApiQueryFor<NonNullable<paths[Path]["get"]>> };
  headers?: Record<string, string>;
  signal?: AbortSignal;
  cache?: RequestCache;
  [key: string]: unknown;
};
type JsonPostInit<Path extends PathsFor<"post">> = {
  parseAs: "json";
  body: PostBody<Path>;
  headers?: Record<string, string>;
  signal?: AbortSignal;
  [key: string]: unknown;
};
type GetFetchResponse<Path extends PathsFor<"get">> = FetchResponse<
  NonNullable<paths[Path]["get"]>,
  JsonGetInit<Path>,
  "application/json"
>;
type PostFetchResponse<Path extends PathsFor<"post">> = FetchResponse<
  NonNullable<paths[Path]["post"]>,
  JsonPostInit<Path>,
  "application/json"
>;

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
export type GetResult<Path extends PathsFor<"get">> = ApiSuccessBodyFor<
  Operation<Path, "get">
>;
export type PostResult<Path extends PathsFor<"post">> = ApiSuccessBodyFor<
  Operation<Path, "post">
>;
export type PostBody<Path extends PathsFor<"post">> = ApiRequestBodyFor<
  NonNullable<paths[Path]["post"]>
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
const schemaMismatchListeners = new Set<() => void>();
const matchingSchemaListeners = new Set<() => void>();
let schemaMismatch = false;
let matchingSchemaResponseGeneration = 0;
const SCHEMA_UPDATE_ATTEMPT_KEY = "studio-api-schema-update-attempted";
const SERVICE_WORKER_UPDATE_TIMEOUT_MS = 20_000;

async function withUpdateTimeout<T>(work: Promise<T>) {
  return new Promise<T>((resolve, reject) => {
    const timeout = setTimeout(
      () => reject(new Error("Studio update timed out.")),
      SERVICE_WORKER_UPDATE_TIMEOUT_MS,
    );
    work.then(
      (value) => {
        clearTimeout(timeout);
        resolve(value);
      },
      (error: unknown) => {
        clearTimeout(timeout);
        reject(error);
      },
    );
  });
}

export function isApiSchemaMismatch() {
  return schemaMismatch;
}

export function matchingApiSchemaResponseGeneration() {
  return matchingSchemaResponseGeneration;
}

export function onMatchingApiSchemaResponse(listener: () => void) {
  matchingSchemaListeners.add(listener);
  if (matchingSchemaResponseGeneration > 0 && !schemaMismatch) listener();
  return () => matchingSchemaListeners.delete(listener);
}

export function onApiSchemaMismatch(listener: () => void) {
  schemaMismatchListeners.add(listener);
  if (schemaMismatch) listener();
  return () => {
    schemaMismatchListeners.delete(listener);
  };
}

export function markApiSchemaMismatch() {
  if (schemaMismatch) return;
  schemaMismatch = true;
  for (const listener of schemaMismatchListeners) listener();
}

export function schemaUpdateFailedAfterReload() {
  try {
    return sessionStorage.getItem(SCHEMA_UPDATE_ATTEMPT_KEY) === "1";
  } catch {
    return false;
  }
}

export function clearSchemaUpdateAttemptAfterMatch() {
  try {
    sessionStorage.removeItem(SCHEMA_UPDATE_ATTEMPT_KEY);
  } catch {
    // Storage may be disabled; the mismatch gate still works in memory.
  }
}

export async function updateRendererAndReload() {
  try {
    if (
      (window as Window & { codexDesktop?: unknown }).codexDesktop ||
      !navigator.serviceWorker
    ) {
      sessionStorage.setItem(SCHEMA_UPDATE_ATTEMPT_KEY, "1");
      return window.location.reload();
    }
    const registration = await navigator.serviceWorker.getRegistration("/");
    if (!registration)
      throw new Error(
        "Studio's service worker is not registered. Reload Studio and try again.",
      );
    await withUpdateTimeout(registration.update());
    const worker = registration.waiting ?? registration.installing;
    if (worker) {
      await new Promise<void>((resolve, reject) => {
        const timeout = setTimeout(
          () => finish(new Error("Studio update timed out.")),
          SERVICE_WORKER_UPDATE_TIMEOUT_MS,
        );
        const finish = (error?: Error) => {
          clearTimeout(timeout);
          worker.removeEventListener("statechange", changed);
          if (error) reject(error);
          else resolve();
        };
        let requestedActivation = false;
        const changed = () => {
          if (worker.state === "installed" && !requestedActivation) {
            requestedActivation = true;
            worker.postMessage({ type: "STUDIO_SKIP_WAITING" });
          }
          if (worker.state === "activated") finish();
          else if (worker.state === "redundant")
            finish(new Error("Studio update could not be installed."));
        };
        worker.addEventListener("statechange", changed);
        changed();
      });
    }
    sessionStorage.setItem(SCHEMA_UPDATE_ATTEMPT_KEY, "1");
    const url = new URL(window.location.href);
    url.searchParams.set("studio-update", String(Date.now()));
    window.location.assign(url.href);
  } catch (error) {
    try {
      sessionStorage.removeItem(SCHEMA_UPDATE_ATTEMPT_KEY);
    } catch {
      // Storage may be unavailable; a failed update remains retryable in memory.
    }
    throw error;
  }
}

export const client = createClient<paths, "application/json">({
  baseUrl: globalThis.location?.origin ?? "http://localhost",
  fetch: async (request) => {
    if (schemaMismatch) throw new ApiSchemaMismatchError();
    const headers = new Headers(request.headers);
    headers.set(API_SCHEMA_HASH_HEADER, API_SCHEMA_HASH);
    const response = await globalThis.fetch(new Request(request, { headers }));
    const serverHash = response.headers.get(API_SCHEMA_HASH_HEADER);
    if (
      response.headers.get(API_SCHEMA_MISMATCH_HEADER) === "1" ||
      (serverHash && serverHash !== API_SCHEMA_HASH) ||
      (response.ok &&
        new URL(request.url).pathname === "/api/sync/identity" &&
        !serverHash)
    ) {
      markApiSchemaMismatch();
      // Do not let a response from another contract reach a projection or
      // mutation persister. The update gate is still raised before rejection.
      throw new ApiSchemaMismatchError(true);
    } else if (serverHash === API_SCHEMA_HASH) {
      matchingSchemaResponseGeneration++;
      clearSchemaUpdateAttemptAfterMatch();
      for (const listener of matchingSchemaListeners) {
        try {
          listener();
        } catch {
          // Schema confirmation observers must not break a successful request.
        }
      }
    }
    return response;
  },
});

// The openapi-fetch generic accepts a correlated path/init pair. TypeScript
// cannot preserve that correlation inside the generic transport functions;
// these adapters keep the generated path, params, body, and response types.
const requestGet = client.GET as <Path extends PathsFor<"get">>(
  path: Path,
  init: JsonGetInit<Path>,
) => Promise<GetFetchResponse<Path>>;
const requestPost = client.POST as <Path extends PathsFor<"post">>(
  path: Path,
  init: JsonPostInit<Path>,
) => Promise<PostFetchResponse<Path>>;
const requestDownload = client.GET as <Path extends PathsFor<"get">>(
  path: Path,
  init: DownloadInit<Path>,
) => Promise<
  FetchResponse<
    NonNullable<paths[Path]["get"]>,
    DownloadInit<Path>,
    "application/json"
  >
>;

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

export class ApiSchemaMismatchError extends ApiError {
  constructor(public readonly markedResponse = false) {
    super("Studio must be updated before using this tab.", 426);
    this.name = "ApiSchemaMismatchError";
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

type SyncEnvelope = {
  _syncEntities?: components["schemas"]["SyncEntity"][] | null;
};

type SyncEntityDocument = components["schemas"]["SyncEntity"];
type SyncEntityPersister = (
  workspaceId: string,
  documents: SyncEntityDocument[],
) => Promise<void>;
let syncEntityPersister: SyncEntityPersister | undefined;
const SYNC_ENTITY_PERSIST_TIMEOUT_MS = 250;

export function registerSyncEntityPersister(
  persist: SyncEntityPersister,
): () => void {
  syncEntityPersister = persist;
  return () => {
    if (syncEntityPersister === persist) syncEntityPersister = undefined;
  };
}

async function syncDocuments(
  value: SyncEnvelope | null | undefined,
  workspaceId: string | undefined,
): Promise<void> {
  if (!value?._syncEntities?.length || typeof window === "undefined") return;
  const targetWorkspaceId = workspaceId ?? workspace;
  if (syncEntityPersister) {
    let timer: ReturnType<typeof setTimeout> | undefined;
    try {
      await Promise.race([
        syncEntityPersister(targetWorkspaceId, value._syncEntities),
        new Promise<never>((_, reject) => {
          timer = setTimeout(
            () => reject(new Error("IndexedDB persistence timed out")),
            SYNC_ENTITY_PERSIST_TIMEOUT_MS,
          );
        }),
      ]);
    } catch (error) {
      console.error("Mutation entity persistence failed", {
        workspaceId: targetWorkspaceId,
        error: error instanceof Error ? error.message : String(error),
      });
    } finally {
      clearTimeout(timer);
    }
  }
}

async function performGet<Path extends PathsFor<"get">>(
  path: Path,
  options?: InternalGetOptions<Path>,
): Promise<GetResult<Path> | undefined> {
  const requestOptions = options ?? {};
  const timeoutMs = requestOptions.timeoutMs ?? 15000;
  const controller = requestController(requestOptions, timeoutMs);
  try {
    const fetchOptions = {
      parseAs: "json" as const,
      ...(requestOptions.query === undefined
        ? {}
        : { params: { query: requestOptions.query } }),
      ...(controller.signal ? { signal: controller.signal } : {}),
      ...(requestOptions.cache ? { cache: requestOptions.cache } : {}),
      ...(requestOptions.etag
        ? { headers: { "If-None-Match": requestOptions.etag } }
        : {}),
    };
    // openapi-fetch's path generic loses path/query correlation in this
    // generic implementation. Public overloads validate that correlation;
    // this assertion only maps `query` into the library's `params.query`.
    const result = await requestGet(path, fetchOptions as JsonGetInit<Path>);
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
    if (!("data" in result)) return undefined;
    if (result.data === null)
      throw new Error("Successful response did not contain a body.");
    return result.data as GetResult<Path>;
  } catch (error) {
    if (controller.timedOut()) throw new NetworkTimeoutError();
    throw error;
  } finally {
    controller.finish();
  }
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
export function get<Path extends PathsFor<"get">>(
  path: Path,
  options?: InternalGetOptions<Path>,
): Promise<GetResult<Path> | undefined> {
  return performGet(path, options);
}

export async function post<Path extends PathsFor<"post">>(
  path: Path,
  body: PostBody<Path>,
  options: PostOptions = {},
): Promise<PostResult<Path>> {
  if (schemaMismatch) throw new ApiSchemaMismatchError();
  const timeoutMs = options.timeoutMs;
  const controller = requestController(options, timeoutMs);
  try {
    const fetchOptions = {
      parseAs: "json" as const,
      body,
      ...(controller.signal ? { signal: controller.signal } : {}),
      headers: {
        "Content-Type": "application/json",
        "X-Canvas-Token": options.sessionToken ?? token,
        ...((options.workspaceId ?? workspace)
          ? { "X-Canvas-Workspace": options.workspaceId ?? workspace }
          : {}),
      },
    };
    const result = await requestPost(path, fetchOptions as JsonPostInit<Path>);
    if (
      !result.response.ok &&
      result.response.headers.get(API_SCHEMA_MISMATCH_HEADER) === "1"
    )
      throw new ApiSchemaMismatchError(true);
    if (!result.response.ok)
      throw errorPayload(result.error, result.response.status);
    if (!("data" in result)) return undefined as PostResult<Path>;
    if (result.data === null)
      throw new Error("Successful response did not contain a body.");
    await syncDocuments(result.data as PostResult<Path>, options.workspaceId);
    return result.data as PostResult<Path>;
  } catch (error) {
    if (controller.timedOut()) throw new NetworkTimeoutError();
    throw error;
  } finally {
    controller.finish();
  }
}

export function apiDownload<Path extends PathsWithRequiredQuery>(
  path: Path,
  query: QueryOfPath<Path>,
): Promise<{ blob: Blob; name: string; truncated: boolean }>;
export function apiDownload<
  Path extends Exclude<PathsFor<"get">, PathsWithRequiredQuery>,
>(
  path: Path,
  query?: QueryOfPath<Path>,
): Promise<{
  blob: Blob;
  name: string;
  truncated: boolean;
}>;
export async function apiDownload<Path extends PathsFor<"get">>(
  path: Path,
  query?: QueryOfPath<Path>,
): Promise<{
  blob: Blob;
  name: string;
  truncated: boolean;
}> {
  const fetchOptions = {
    ...(query === undefined ? {} : { params: { query } }),
    parseAs: "blob" as const,
    headers: {
      "X-Canvas-Token": token,
      ...(workspace ? { "X-Canvas-Workspace": workspace } : {}),
    },
  };
  const result = await requestDownload(
    path,
    fetchOptions as DownloadInit<Path>,
  );
  const response = result.response;
  if (!response.ok) throw errorPayload(result.error, response.status);
  const disposition = response.headers.get("Content-Disposition") || "";
  const name = disposition.match(/filename="([^"]+)"/)?.[1] || "download.log";
  if (!("data" in result) || result.data === undefined)
    throw new Error("Download response did not contain a file.");
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
  if (schemaMismatch) return Promise.reject(new ApiSchemaMismatchError());
  return performGet(path, { ...options, timeoutMs: 15000 });
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

export function errorText(error: unknown) {
  if (error instanceof ApiSchemaMismatchError) return "";
  return displayError(error);
}
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
