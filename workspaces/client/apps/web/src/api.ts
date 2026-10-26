import { hasNativeClient } from "./native/client";

import { beginServerActivity } from "./servers/activity";

import { beginServerMutation } from "./servers/mutationIdentity";

import {
  serverLocalStorage as localStorage,
  serverSessionStorage as sessionStorage,
} from "./servers/storage";

import { apiOrigin, serverFetch } from "./servers/transport";

import createClient from "openapi-fetch";

import type { FetchResponse } from "openapi-fetch";

import type {
  FilterKeys,
  OperationRequestBodyContent,
  PathsWithMethod,
} from "openapi-typescript-helpers";

import type { paths } from "./generated/api";

import {
  API_SCHEMA_HASH,
  API_SCHEMA_HASH_HEADER,
  API_SCHEMA_MISMATCH_FIELD,
  API_SCHEMA_MISMATCH_HEADER,
} from "./generated/apiSchema";

import type {
  ApiGetOptions,
  ApiSuccessBodyFor,
  ApiPathsWithRequiredQuery,
  ApiQueryFor,
} from "./apiContracts";

import { onResume } from "./sync/resume";

import { displayError } from "./errorPresentation";

type Method = "get" | "post";

type PathsFor<M extends Method> = PathsWithMethod<paths, M> & keyof paths;

type Operation<Path extends keyof paths, M extends Method> = FilterKeys<
  paths[Path],
  M
>;

type QueryOf<Op> = ApiQueryFor<Op>;

type QueryOfPath<Path extends PathsFor<"get">> = Path extends keyof paths
  ? ApiQueryFor<NonNullable<paths[Path]["get"]>>
  : never;

type InternalGetOptions<Path extends PathsFor<"get">> = ApiOptions & {
  query?: QueryOf<Operation<Path, "get">> | undefined;
  etag?: string | undefined;
  readMetadata?: ApiReadMetadata | undefined;
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

export type ApiReadMetadata = {
  etag?: string | undefined;
  notModified?: boolean;
};

export type ApiGetPath = PathsFor<"get">;

export type ApiPostPath = PathsFor<"post">;

export type ApiOptions = {
  timeoutMs?: number | undefined;
  workspaceId?: string | undefined;
  sessionToken?: string | undefined;
  signal?: AbortSignal | undefined;
  cache?: RequestCache | undefined;
};

export type GetOptions<Path extends PathsFor<"get">> = ApiGetOptions<
  Operation<Path, "get">,
  ApiOptions,
  ApiReadMetadata
>;

export type PostOptions = ApiOptions & { requestId?: string };

export type GetResult<Path extends PathsFor<"get">> = ApiSuccessBodyFor<
  Operation<Path, "get">
>;

export type PostResult<Path extends PathsFor<"post">> = ApiSuccessBodyFor<
  Operation<Path, "post">
>;

export type PostBody<Path extends PathsFor<"post">> = NonNullable<
  OperationRequestBodyContent<Operation<Path, "post">>
>;

/** A read waits this long for a slow server. Only a lost connection should end it. */
export const READ_DEADLINE_MS = 120_000;

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

export function isApiSchemaHashMismatch(serverHash: unknown) {
  return serverHash !== API_SCHEMA_HASH;
}

export function isApiSchemaMismatchHandshake(payload: unknown) {
  if (!payload || typeof payload !== "object") return true;
  const handshake = payload as Record<string, unknown>;
  return (
    isApiSchemaHashMismatch(handshake.hash) ||
    handshake[API_SCHEMA_MISMATCH_FIELD] === true
  );
}

export function isApiSchemaMismatchResponse(
  response: Response,
  request: Request,
) {
  const serverHash = response.headers.get(API_SCHEMA_HASH_HEADER);
  return (
    response.headers.get(API_SCHEMA_MISMATCH_HEADER) === "1" ||
    (!!serverHash && isApiSchemaHashMismatch(serverHash)) ||
    (response.ok &&
      new URL(request.url).pathname === "/api/sync/identity" &&
      !serverHash)
  );
}

export class ApiError extends Error {
  public readonly outcome?: string;
  public readonly catalogPending?: boolean;

  public status: number;
  public readonly response: unknown;
  public readonly location: string[] | undefined;
  constructor(
    message: unknown,
    status: number,
    response: unknown = message,
    location?: string[],
  ) {
    super(displayError(message) || `Request failed (${status})`);
    this.status = status;
    this.response = response;
    this.location = location;

    if (isRecord(response)) {
      if (typeof response.outcome === "string") this.outcome = response.outcome;
      if (typeof response.catalogPending === "boolean")
        this.catalogPending = response.catalogPending;
    }
  }
}

export class ApiSchemaMismatchError extends ApiError {
  public readonly markedResponse: boolean;
  constructor(markedResponse = false) {
    super("Studio must be updated before using this tab.", 426);
    this.markedResponse = markedResponse;

    this.name = "ApiSchemaMismatchError";
  }
}

export class NetworkTimeoutError extends TypeError {
  constructor() {
    super("The server did not respond in time.");
    this.name = "NetworkTimeoutError";
  }
}

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

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function errorPayload(value: unknown, status: number): ApiError {
  const message = isRecord(value)
    ? value.error || value.message || `Request failed (${status})`
    : typeof value === "string"
      ? value
      : `Request failed (${status})`;
  const location =
    isRecord(value) && Array.isArray(value.location)
      ? value.location.filter(
          (part): part is string => typeof part === "string",
        )
      : undefined;
  return new ApiError(message, status, value, location);
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

// Only reads and operations with a durable request identity use this deadline.
type SyncGetOptions<Path extends PathsFor<"get">> = Omit<
  GetOptions<Path>,
  "timeoutMs"
>;

export type QueueView = GetResult<"/api/queue">;

export type QueueItemDto = QueueView["items"][number];

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
/** Each instance owns one server's session, schema gate, workspace and persistence. */
export type ApiScopeOptions = {
  origin: string;
  fetch: (request: Request) => Promise<Response>;
  localStorage: Storage;
  sessionStorage: Storage;
  beginMutation: typeof beginServerMutation;
  beginActivity: typeof beginServerActivity;
};
export function createApiScope(options: ApiScopeOptions) {
  const { localStorage, sessionStorage } = options;
  const apiOrigin = () => options.origin;
  const serverFetch = options.fetch;
  const beginServerMutation = options.beginMutation;
  const beginServerActivity = options.beginActivity;

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

  let mismatchedServerSchema: string | undefined;

  let matchingSchemaResponseGeneration = 0;

  function isApiSchemaMismatch() {
    return schemaMismatch;
  }

  function matchingApiSchemaResponseGeneration() {
    return matchingSchemaResponseGeneration;
  }

  function onMatchingApiSchemaResponse(listener: () => void) {
    matchingSchemaListeners.add(listener);
    if (matchingSchemaResponseGeneration > 0 && !schemaMismatch) listener();
    return () => matchingSchemaListeners.delete(listener);
  }

  function onApiSchemaMismatch(listener: () => void) {
    schemaMismatchListeners.add(listener);
    if (schemaMismatch) listener();
    return () => {
      schemaMismatchListeners.delete(listener);
    };
  }

  function markApiSchemaMismatch(serverHash?: unknown) {
    if (schemaMismatch) return;
    if (typeof serverHash === "string" && /^[a-f0-9]{64}$/.test(serverHash))
      mismatchedServerSchema = serverHash;
    schemaMismatch = true;
    for (const listener of schemaMismatchListeners) listener();
  }

  function schemaUpdateFailedAfterReload() {
    try {
      const attempt = sessionStorage.getItem(SCHEMA_UPDATE_ATTEMPT_KEY);
      return attempt === "1" || !!attempt?.startsWith("auto:");
    } catch {
      return false;
    }
  }

  function clearSchemaUpdateAttemptAfterMatch() {
    try {
      sessionStorage.removeItem(SCHEMA_UPDATE_ATTEMPT_KEY);
    } catch {
      // Storage may be disabled; the mismatch gate still works in memory.
    }
  }

  async function updateRendererAndReload(
    attempt = "1",
    canReload = () => true,
  ) {
    try {
      if (
        (window as Window & { codexDesktop?: unknown }).codexDesktop ||
        hasNativeClient() ||
        !navigator.serviceWorker
      ) {
        if (!canReload()) return false;
        sessionStorage.setItem(SCHEMA_UPDATE_ATTEMPT_KEY, attempt);
        window.location.reload();
        return true;
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
      if (!canReload()) return false;
      sessionStorage.setItem(SCHEMA_UPDATE_ATTEMPT_KEY, attempt);
      const url = new URL(window.location.href);
      url.searchParams.set("studio-update", String(Date.now()));
      window.location.assign(url.href);
      return true;
    } catch (error) {
      try {
        sessionStorage.removeItem(SCHEMA_UPDATE_ATTEMPT_KEY);
      } catch {
        // Storage may be unavailable; a failed update remains retryable in memory.
      }
      throw error;
    }
  }

  async function automaticallyUpdateRenderer(
    canReload: () => boolean,
    signal: AbortSignal,
  ) {
    if (!schemaMismatch || !mismatchedServerSchema || !canReload())
      return false;
    // Read the renderer origin, which can differ from the selected API server.
    // This file is generated with the assets and never enters the offline cache.
    const response = await fetch(
      new URL("./studio-renderer.json", window.location.href),
      {
        cache: "no-store",
        redirect: "error",
        signal: AbortSignal.any([signal, AbortSignal.timeout(5000)]),
      },
    );
    if (!response.ok) return false;
    const manifest: unknown = await response.json();
    if (!manifest || typeof manifest !== "object") return false;
    const { version, build, apiSchema } = manifest as Record<string, unknown>;
    if (
      version !== 1 ||
      typeof build !== "string" ||
      !/^[a-f0-9]{16}$/.test(build) ||
      apiSchema !== mismatchedServerSchema ||
      apiSchema === API_SCHEMA_HASH ||
      build ===
        document
          .querySelector('meta[name="studio-build"]')
          ?.getAttribute("content")
    )
      return false;
    const attempt = `auto:${apiSchema}:${build}`;
    // One attempt per target build. A broken deployment must not reload forever.
    if (sessionStorage.getItem(SCHEMA_UPDATE_ATTEMPT_KEY) === attempt)
      return false;
    return updateRendererAndReload(
      attempt,
      () => !signal.aborted && canReload(),
    );
  }

  const fetchApiRequest = async (request: Request) => {
    if (schemaMismatch) throw new ApiSchemaMismatchError();
    const headers = new Headers(request.headers);
    headers.set(API_SCHEMA_HASH_HEADER, API_SCHEMA_HASH);
    const response = await serverFetch(new Request(request, { headers }));
    const serverHash = response.headers.get(API_SCHEMA_HASH_HEADER);
    if (isApiSchemaMismatchResponse(response, request)) {
      markApiSchemaMismatch(serverHash);
      await response.body?.cancel().catch(() => {});
      // Do not let a response from another contract reach a projection or
      // caller. The update gate is still raised before rejection.
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
  };

  const client = createClient<paths, "application/json">({
    baseUrl: apiOrigin(),
    fetch: fetchApiRequest,
  });

  // Pairing's versioned wire contract is independent of the generated workspace API.
  const accessClient = createClient<
    import("./servers/accessContract").ServerAccessPaths
  >({ baseUrl: apiOrigin(), fetch: fetchApiRequest });

  async function serverAccess(
    method: "GET" | "POST",
    body?: import("./servers/accessContract").ServerAccessRequest,
    timeoutMs = 15000,
    sessionToken?: string,
  ) {
    const controller = requestController({}, timeoutMs);
    try {
      const result =
        method === "GET"
          ? await accessClient.GET(
              "/api/multi-server",
              controller.signal ? { signal: controller.signal } : {},
            )
          : await accessClient.POST("/api/multi-server", {
              body: body!,
              ...(controller.signal ? { signal: controller.signal } : {}),
              headers: {
                "X-Canvas-Token": sessionToken ?? token,
                "X-Canvas-Workspace": workspace,
              },
            });
      if (!result.response.ok)
        throw errorPayload(result.error, result.response.status);
      if (!result.data) throw new Error("The server access response is empty.");
      return result.data;
    } catch (error) {
      if (controller.timedOut()) throw new NetworkTimeoutError();
      throw error;
    } finally {
      controller.finish();
    }
  }

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

  function setWorkspace(value: string) {
    workspace = value;
  }

  function setToken(value: string) {
    manualTokenGeneration = ++sessionGeneration;
    successfulSession = undefined;
    confirmedSession = undefined;
    token = value;
  }

  async function performGet<Path extends PathsFor<"get">>(
    path: Path,
    options?: InternalGetOptions<Path>,
  ): Promise<GetResult<Path> | undefined> {
    const requestOptions = options ?? {};
    const timeoutMs = requestOptions.timeoutMs ?? READ_DEADLINE_MS;
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

  function get<Path extends PathsFor<"get">>(
    path: Path,
    options: GetOptions<Path> & { readMetadata: ApiReadMetadata },
  ): Promise<GetResult<Path> | undefined>;

  function get<Path extends PathsWithRequiredQuery>(
    path: Path,
    options: GetOptions<Path> & { readMetadata?: undefined },
  ): Promise<GetResult<Path>>;

  function get<Path extends Exclude<PathsFor<"get">, PathsWithRequiredQuery>>(
    path: Path,
    options?: GetOptions<Path> & { readMetadata?: undefined },
  ): Promise<GetResult<Path>>;

  function get<Path extends PathsFor<"get">>(
    path: Path,
    options?: InternalGetOptions<Path>,
  ): Promise<GetResult<Path> | undefined> {
    return performGet(path, options);
  }

  async function post<Path extends PathsFor<"post">>(
    path: Path,
    body: PostBody<Path>,
    options: PostOptions = {},
  ): Promise<PostResult<Path>> {
    if (schemaMismatch) throw new ApiSchemaMismatchError();
    const targetWorkspaceId = options.workspaceId ?? workspace;
    const sessionToken = options.sessionToken ?? token;
    const mutation = await beginServerMutation(path, body, options.requestId);
    const timeoutMs = options.timeoutMs ?? (mutation ? 15000 : undefined);
    const controller = requestController(options, timeoutMs);
    let responseRejected = false;
    const stopActivity = beginServerActivity();
    try {
      const fetchOptions = {
        parseAs: "json" as const,
        body,
        ...(controller.signal ? { signal: controller.signal } : {}),
        headers: {
          "Content-Type": "application/json",
          ...(mutation ? { "X-Studio-Request-Id": mutation.requestId } : {}),
          "X-Canvas-Token": sessionToken,
          ...(targetWorkspaceId
            ? { "X-Canvas-Workspace": targetWorkspaceId }
            : {}),
        },
      };
      const result = await requestPost(
        path,
        fetchOptions as JsonPostInit<Path>,
      );
      if (!result.response.ok) {
        responseRejected = true;
        mutation?.reject(result.response.status, result.error);
      }
      if (
        !result.response.ok &&
        result.response.headers.get(API_SCHEMA_MISMATCH_HEADER) === "1"
      )
        throw new ApiSchemaMismatchError(true);
      if (!result.response.ok)
        throw errorPayload(result.error, result.response.status);
      if (!("data" in result)) {
        mutation?.finish();
        return undefined as PostResult<Path>;
      }
      if (result.data === null)
        throw new Error("Successful response did not contain a body.");
      const data = result.data as PostResult<Path>;
      mutation?.finish();
      return data;
    } catch (error) {
      if (!responseRejected) mutation?.unknown();
      if (controller.timedOut()) throw new NetworkTimeoutError();
      throw error;
    } finally {
      stopActivity();
      controller.finish();
    }
  }

  function apiDownload<Path extends PathsWithRequiredQuery>(
    path: Path,
    query: QueryOfPath<Path>,
  ): Promise<{ blob: Blob; name: string; truncated: boolean }>;

  function apiDownload<
    Path extends Exclude<PathsFor<"get">, PathsWithRequiredQuery>,
  >(
    path: Path,
    query?: QueryOfPath<Path>,
  ): Promise<{
    blob: Blob;
    name: string;
    truncated: boolean;
  }>;

  async function apiDownload<Path extends PathsFor<"get">>(
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

  function syncGet<Path extends PathsFor<"get">>(
    path: Path,
    options: SyncGetOptions<Path> & { readMetadata: ApiReadMetadata },
  ): Promise<GetResult<Path> | undefined>;

  function syncGet<Path extends PathsWithRequiredQuery>(
    path: Path,
    options: SyncGetOptions<Path> & { readMetadata?: undefined },
  ): Promise<GetResult<Path>>;

  function syncGet<
    Path extends Exclude<PathsFor<"get">, PathsWithRequiredQuery>,
  >(
    path: Path,
    options?: SyncGetOptions<Path> & { readMetadata?: undefined },
  ): Promise<GetResult<Path>>;

  function syncGet<Path extends PathsFor<"get">>(
    path: Path,
    options?: SyncGetOptions<Path>,
  ): Promise<GetResult<Path> | undefined> {
    if (schemaMismatch) return Promise.reject(new ApiSchemaMismatchError());
    return performGet(path, { ...options, timeoutMs: READ_DEADLINE_MS });
  }

  const syncPost = <Path extends PathsFor<"post">>(
    path: Path,
    body: PostBody<Path>,
    options?: Omit<PostOptions, "timeoutMs">,
  ) => post(path, body, { ...options, timeoutMs: 15000 });

  async function refreshSession(): Promise<GetResult<"/api/session">> {
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

  function saved<T>(key: string, fallback: T): T {
    try {
      return JSON.parse(localStorage.getItem(key) || "null") ?? fallback;
    } catch {
      return fallback;
    }
  }

  function save(key: string, value: unknown) {
    try {
      const raw = JSON.stringify(value);
      // Keep the storage adapter's pending journal intact when the UI repeats a value.
      if (localStorage.getItem(key) !== raw) localStorage.setItem(key, raw);
    } catch {
      /* Server data remains available if browser storage is full. */
    }
  }

  function stableRequestId(key: string): string {
    const savedId = saved<string>(`request-id:${key}`, "");
    if (savedId) return savedId;
    const requestId = crypto.randomUUID();
    save(`request-id:${key}`, requestId);
    return requestId;
  }

  function clearStableRequestId(key: string) {
    save(`request-id:${key}`, null);
  }
  return {
    isApiSchemaMismatch,
    matchingApiSchemaResponseGeneration,
    onMatchingApiSchemaResponse,
    onApiSchemaMismatch,
    markApiSchemaMismatch,
    schemaUpdateFailedAfterReload,
    clearSchemaUpdateAttemptAfterMatch,
    updateRendererAndReload,
    automaticallyUpdateRenderer,
    fetchApiRequest,
    client,
    serverAccess,
    setWorkspace,
    setToken,
    get,
    post,
    apiDownload,
    syncGet,
    syncPost,
    refreshSession,
    saved,
    save,
    stableRequestId,
    clearStableRequestId,
  };
}

const defaultApi = createApiScope({
  origin: apiOrigin(),
  fetch: serverFetch,
  localStorage,
  sessionStorage,
  beginMutation: beginServerMutation,
  beginActivity: beginServerActivity,
});
export const {
  isApiSchemaMismatch,
  matchingApiSchemaResponseGeneration,
  onMatchingApiSchemaResponse,
  onApiSchemaMismatch,
  markApiSchemaMismatch,
  schemaUpdateFailedAfterReload,
  clearSchemaUpdateAttemptAfterMatch,
  updateRendererAndReload,
  automaticallyUpdateRenderer,
  fetchApiRequest,
  client,
  serverAccess,
  setWorkspace,
  setToken,
  get,
  post,
  apiDownload,
  syncGet,
  syncPost,
  refreshSession,
  saved,
  save,
  stableRequestId,
  clearStableRequestId,
} = defaultApi;
export type ServerApi = ReturnType<typeof createApiScope>;
