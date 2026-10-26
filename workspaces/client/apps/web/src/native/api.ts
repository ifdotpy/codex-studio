import type {
  ApiGetPath,
  ApiPostPath,
  ApiOptions,
  ApiReadMetadata,
  PostBody,
  PostOptions,
  PostResult,
  ServerApi,
  NativeDownloadResult,
} from "../api";
import {
  ApiError,
  ApiSchemaMismatchError,
  NetworkTimeoutError,
} from "../apiErrors";
import { READ_DEADLINE_MS, MUTATION_DEADLINE_MS } from "../apiPolicy";
import { API_SCHEMA_HASH } from "../generated/apiSchema";
import type { NativeServerContext } from "./client";
import { createNativeMutationJournal } from "./mutationJournal";
import { NativeCommandError } from "./commandError";
export {
  ApiError,
  ApiSchemaMismatchError,
  NetworkTimeoutError,
} from "../apiErrors";
export { errorText, untilAnswered } from "../apiPresentation";

type Preferences = Pick<Storage, "getItem" | "setItem">;
type Reply = { status: number; headers: Record<string, string>; body: unknown };
type ReadOptions = ApiOptions & {
  query?: unknown;
  etag?: string;
  readMetadata?: ApiReadMetadata;
};

function apiError(body: unknown, status: number) {
  const value =
    body && typeof body === "object"
      ? (body as Record<string, unknown>)
      : undefined;
  const message = value?.error ?? value?.message ?? body;
  const location = Array.isArray(value?.location)
    ? value.location.filter((part): part is string => typeof part === "string")
    : undefined;
  return new ApiError(message, status, body, location);
}

// An abort ends this UI wait, not an already submitted native command.
function waitFor<T>(work: Promise<T>, signal?: AbortSignal): Promise<T> {
  work = work.catch((error) => {
    if (error instanceof NativeCommandError && error.code === "timeout")
      throw new NetworkTimeoutError();
    throw error;
  });
  if (!signal) return work;
  return new Promise((resolve, reject) => {
    const abort = () => reject(signal.reason);
    signal.addEventListener("abort", abort, { once: true });
    if (signal.aborted) abort();
    work
      .then(resolve, reject)
      .finally(() => signal.removeEventListener("abort", abort));
  });
}

/** Typed commands and UI metadata only. Rust owns HTTP, credentials and deadlines. */
export function createNativeApi(
  context: Pick<NativeServerContext, "request" | "workspaceId">,
  preferences: Preferences,
) {
  const journal = createNativeMutationJournal(context);
  let schemaMismatch = false;
  let matchingGeneration = 0;
  const mismatches = new Set<() => void>();
  const matches = new Set<() => void>();
  const stable = new Map<string, Promise<string>>();
  function markApiSchemaMismatch() {
    if (schemaMismatch) return;
    schemaMismatch = true;
    for (const listener of mismatches) listener();
  }
  function checked(reply: Reply, path: string) {
    if (
      !reply ||
      !Number.isInteger(reply.status) ||
      !reply.headers ||
      typeof reply.headers !== "object"
    )
      throw new Error("The native API response is invalid.");
    const hash = reply.headers["x-studio-api-schema"];
    if (
      reply.headers["x-studio-api-schema-mismatch"] === "1" ||
      (hash && hash !== API_SCHEMA_HASH) ||
      (path === "/api/sync/identity" &&
        reply.status >= 200 &&
        reply.status < 300 &&
        !hash)
    ) {
      markApiSchemaMismatch();
      throw new ApiSchemaMismatchError(true);
    }
    if (hash === API_SCHEMA_HASH) {
      matchingGeneration++;
      for (const listener of matches) listener();
    }
  }
  async function read(path: ApiGetPath, options: ReadOptions = {}) {
    if (schemaMismatch) throw new ApiSchemaMismatchError();
    options.signal?.throwIfAborted();
    const reply = await waitFor(
      context.request<Reply>("api.request", {
        path,
        method: "GET",
        ...(options.query === undefined ? {} : { query: options.query }),
        ...(options.etag ? { etag: options.etag } : {}),
        ...(options.workspaceId ? { workspaceId: options.workspaceId } : {}),
        apiSchema: API_SCHEMA_HASH,
        jsonResponse: true,
        timeoutMs: options.timeoutMs ?? READ_DEADLINE_MS,
      }),
      options.signal,
    );
    checked(reply, path);
    if (options.readMetadata) {
      options.readMetadata.etag = reply.headers.etag ?? options.etag;
      options.readMetadata.notModified = reply.status === 304;
      if (reply.status === 304) return undefined;
    }
    if (reply.status < 200 || reply.status >= 300)
      throw apiError(reply.body, reply.status);
    if (reply.body === null && reply.status !== 204 && reply.status !== 205)
      throw new Error("Successful response did not contain a body.");
    return reply.body === null ? undefined : reply.body;
  }
  const get = read as ServerApi["get"];
  async function post<Path extends ApiPostPath>(
    path: Path,
    body: PostBody<Path>,
    options: PostOptions = {},
  ): Promise<PostResult<Path>> {
    if (schemaMismatch) throw new ApiSchemaMismatchError();
    options.signal?.throwIfAborted();
    const serialized = JSON.stringify(body);
    if (serialized === undefined)
      throw new Error("A JSON mutation payload is required.");
    const frozenBody = JSON.parse(serialized) as PostBody<Path>;
    const mutation = await journal(path, frozenBody, options.requestId);
    let responseRejected = false;
    try {
      options.signal?.throwIfAborted();
      const reply = await waitFor(
        context.request<Reply>(
          "api.request",
          {
            path,
            method: "POST",
            body: frozenBody,
            apiSchema: API_SCHEMA_HASH,
            jsonResponse: true,
            timeoutMs: options.timeoutMs ?? MUTATION_DEADLINE_MS,
            ...(options.workspaceId
              ? { workspaceId: options.workspaceId }
              : {}),
            ...(mutation ? { requestId: mutation.requestId } : {}),
          },
          mutation?.requestId,
        ),
        options.signal,
      );
      if (reply.status < 200 || reply.status >= 300) {
        responseRejected = true;
        await mutation?.reject(reply.status, reply.body);
      }
      checked(reply, path);
      if (reply.status < 200 || reply.status >= 300)
        throw apiError(reply.body, reply.status);
      if (reply.body === null && reply.status !== 204 && reply.status !== 205)
        throw new Error("Successful response did not contain a body.");
      await mutation?.finish();
      return (reply.body === null ? undefined : reply.body) as PostResult<Path>;
    } catch (error) {
      if (!responseRejected) await mutation?.unknown();
      throw error;
    }
  }
  async function pendingMonitorInput(id: string) {
    const entry = await journal.nativeMonitorInput.pending(id);
    return entry ? { requestId: entry.requestId } : null;
  }
  async function checkMonitorInputReceipt(
    id: string,
    owner: { agent: string; accountKey: string; epoch: number },
  ): Promise<"none" | "unknown" | "acknowledged" | "rejected"> {
    const pending = await pendingMonitorInput(id);
    if (!pending) return "none";
    const receipt = await get("/api/processes/action", {
      query: { requestId: pending.requestId, ...owner },
    });
    const result = receipt.result;
    const response = result?.response;
    if (
      !receipt.found ||
      result?.requestId !== pending.requestId ||
      result.status !== "completed" ||
      response?.kind !== "monitor_input" ||
      response.monitorId !== id ||
      (response.status !== "acknowledged" && response.status !== "rejected")
    )
      return "unknown";
    return (await journal.nativeMonitorInput.settle(id, pending.requestId))
      ? response.status
      : "unknown";
  }
  function saved<T>(key: string, fallback: T): T {
    try {
      return JSON.parse(preferences.getItem(key) ?? "null") ?? fallback;
    } catch {
      return fallback;
    }
  }
  function save(key: string, value: unknown) {
    const raw = JSON.stringify(value);
    if (raw === undefined)
      throw new Error("A JSON preference value is required.");
    if (preferences.getItem(key) !== raw) preferences.setItem(key, raw);
  }
  function stableRequestId(key: string): Promise<string> {
    let pending = stable.get(key);
    if (!pending) {
      pending = context
        .request<string>("mutation.stable_id", { key })
        .catch((error) => {
          stable.delete(key);
          throw error;
        });
      stable.set(key, pending);
    }
    return pending;
  }
  async function clearStableRequestId(key: string) {
    const known = stable.get(key);
    if (!known) return;
    const requestId = await known;
    await context.request("mutation.clear_stable_id", { key, requestId });
    if (stable.get(key) === known) stable.delete(key);
  }
  return {
    get,
    post,
    syncGet: read as ServerApi["syncGet"],
    syncPost: post,
    saved,
    save,
    stableRequestId,
    clearStableRequestId,
    prepareSession: () =>
      context.request<{ ready: true }>("api.prepareSession", {}),
    apiDownload: ((path: ApiGetPath, query?: unknown) =>
      context.request<NativeDownloadResult>("desktop.downloadAndSave", {
        value: { path, ...(query === undefined ? {} : { query }) },
      })) as ServerApi["apiDownload"],
    pendingMonitorInput,
    checkMonitorInputReceipt,
    isApiSchemaMismatch: () => schemaMismatch,
    markApiSchemaMismatch,
    matchingApiSchemaResponseGeneration: () => matchingGeneration,
    onApiSchemaMismatch(listener: () => void) {
      mismatches.add(listener);
      if (schemaMismatch) listener();
      return () => {
        mismatches.delete(listener);
      };
    },
    onMatchingApiSchemaResponse(listener: () => void) {
      matches.add(listener);
      if (matchingGeneration && !schemaMismatch) listener();
      return () => {
        matches.delete(listener);
      };
    },
  };
}
