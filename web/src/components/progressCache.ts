import { ApiError, NetworkTimeoutError } from "../api";
import { onResume } from "../sync/resume";

export type CachedProgress = {
  scope: string;
  markdown: string;
  path: string;
  revision: string | null;
};
type Pending = {
  controller: AbortController;
  promise: Promise<CachedProgress>;
  readers: number;
  started: number;
  expire: () => void;
};
const entries = new Map<string, CachedProgress>();
const pending = new Map<string, Pending>();
const prefix = "codex-progress-cache:";
const maxEntries = 32;
const maxStoredBytes = 256 * 1024;
const maxMemoryBytes = 1024 * 1024;
onResume(() => {
  for (const request of pending.values())
    if (Date.now() - request.started >= 8000) request.expire();
});
export const progressScope = (stateDir: string, agentId: string) =>
  JSON.stringify([stateDir, agentId]);

const valid = (value: any, scope: string): value is CachedProgress =>
  value?.scope === scope &&
  typeof value.markdown === "string" &&
  typeof value.path === "string" &&
  (value.revision === null || typeof value.revision === "string");

function remember(value: CachedProgress) {
  entries.delete(value.scope);
  entries.set(value.scope, value);
  while (
    entries.size > maxEntries ||
    [...entries.values()].reduce(
      (bytes, entry) => bytes + (entry.markdown.length + entry.path.length) * 2,
      0,
    ) > maxMemoryBytes
  )
    entries.delete(entries.keys().next().value!);
  return value;
}

export function peekProgress(stateDir: string, agentId: string) {
  const scope = progressScope(stateDir, agentId);
  const memory = entries.get(scope);
  if (memory) return remember(memory);
  try {
    const value = JSON.parse(localStorage.getItem(prefix + scope) || "null");
    if (valid(value, scope)) return remember(value);
  } catch {
    /* This disposable cache must not block a live read. */
  }
  return null;
}

function storeProgress(value: CachedProgress) {
  const previous = entries.get(value.scope);
  if (
    previous &&
    previous.markdown === value.markdown &&
    previous.path === value.path &&
    previous.revision === value.revision
  )
    return remember(previous);
  remember(value);
  try {
    const key = prefix + value.scope;
    const serialized = JSON.stringify({ ...value, storedAt: Date.now() });
    if (serialized.length * 2 > maxStoredBytes) {
      localStorage.removeItem(key);
      return value;
    }
    const stored: Array<{ key: string; bytes: number; at: number }> = [];
    for (let index = 0; index < localStorage.length; index++) {
      const candidate = localStorage.key(index);
      if (!candidate?.startsWith(prefix) || candidate === key) continue;
      const raw = localStorage.getItem(candidate) || "";
      let at = 0;
      try {
        at = Number(JSON.parse(raw).storedAt) || 0;
      } catch {
        /* Evict invalid cache records first. */
      }
      stored.push({ key: candidate, bytes: raw.length * 2, at });
    }
    stored.sort((a, b) => a.at - b.at);
    let bytes =
      serialized.length * 2 +
      stored.reduce((total, item) => total + item.bytes, 0);
    while (stored.length >= maxEntries || bytes > maxStoredBytes) {
      const oldest = stored.shift();
      if (!oldest) break;
      localStorage.removeItem(oldest.key);
      bytes -= oldest.bytes;
    }
    localStorage.setItem(key, serialized);
  } catch {
    // Free only disposable progress copies if browser storage is full. Drafts
    // and attachments keep their storage; this read remains available in memory.
    try {
      for (const key of Object.keys(localStorage))
        if (key.startsWith(prefix)) localStorage.removeItem(key);
    } catch {
      /* Storage can also be unavailable. */
    }
  }
  return value;
}

function startRead(stateDir: string, agentId: string): Pending {
  const scope = progressScope(stateDir, agentId);
  const controller = new AbortController();
  let expire: ReturnType<typeof setTimeout>;
  let timedOut = false;
  const request: Pending = {
    controller,
    readers: 0,
    promise: undefined!,
    started: Date.now(),
    expire: () => {},
  };
  const deadline = new Promise<never>((_resolve, reject) => {
    request.expire = () => {
      timedOut = true;
      reject(new NetworkTimeoutError());
      controller.abort();
    };
    expire = setTimeout(request.expire, 8000);
  });
  const read = async () => {
    const response = await fetch(
      `/api/panel?agent=${encodeURIComponent(agentId)}`,
      {
        signal: controller.signal,
        cache: "no-store",
      },
    );
    const value = await response.json();
    if (!response.ok || value?.error)
      throw new ApiError(
        value?.error || `Request failed (${response.status})`,
        response.status,
        value,
      );
    if (
      value?.agent !== agentId ||
      value.format !== "markdown" ||
      !valid({ ...value, scope }, scope)
    )
      throw new Error("The PROGRESS.md response is invalid.");
    if (controller.signal.aborted)
      throw new DOMException("Aborted", "AbortError");
    return {
      scope,
      markdown: value.markdown,
      path: value.path,
      revision: value.revision,
    };
  };
  request.promise = Promise.race([read(), deadline])
    .then((value) => {
      if (controller.signal.aborted)
        throw new DOMException("Aborted", "AbortError");
      return storeProgress(value);
    })
    .catch((error) => {
      throw timedOut ? new NetworkTimeoutError() : error;
    })
    .finally(() => {
      clearTimeout(expire);
      if (pending.get(scope) === request) pending.delete(scope);
    });
  pending.set(scope, request);
  return request;
}

/** Share a read without letting one departing view cancel another reader. */
export function readProgress(
  stateDir: string,
  agentId: string,
  signal?: AbortSignal,
): Promise<CachedProgress> {
  if (signal?.aborted)
    return Promise.reject(new DOMException("Aborted", "AbortError"));
  const scope = progressScope(stateDir, agentId);
  let request = pending.get(scope);
  if (request && Date.now() - request.started >= 8000) {
    request.expire();
    request = undefined;
  }
  if (!request || request.controller.signal.aborted)
    request = startRead(stateDir, agentId);
  const retained = request;
  retained.readers++;
  return new Promise((resolve, reject) => {
    let settled = false;
    const finish = (value?: CachedProgress, error?: unknown) => {
      if (settled) return;
      settled = true;
      signal?.removeEventListener("abort", abort);
      retained.readers--;
      if (!retained.readers) retained.controller.abort();
      if (error) reject(error);
      else resolve(value!);
    };
    const abort = () =>
      finish(undefined, new DOMException("Aborted", "AbortError"));
    signal?.addEventListener("abort", abort, { once: true });
    retained.promise.then(
      (value) => finish(value),
      (error) => finish(undefined, error),
    );
  });
}

export async function prefetchProgress(stateDir: string, agentId: string) {
  if (document.hidden || navigator.onLine === false) return false;
  await readProgress(stateDir, agentId);
  return true;
}
