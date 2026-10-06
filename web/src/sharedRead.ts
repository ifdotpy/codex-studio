import { get, getWorkspaceReadScope } from "./api";
import type { GetOptions, GetResult } from "./api";
import type { components } from "./generated/api";
import { observeResourceEvents } from "./sync/resourceEvents";

type GetPath =
  | "/api/accounts"
  | "/api/models"
  | "/api/limits"
  | "/api/costs"
  | "/api/desktop"
  | "/api/worktree-disk";
type ResourceRef = components["schemas"]["ResourceRef"];
type Entry = {
  promise: Promise<unknown>;
  value?: unknown;
  settled: boolean;
  invalidated: boolean;
};

const entries = new Map<string, Entry>();
const entryPaths = new Map<string, GetPath>();
const requestSharedGet = get as unknown as <Path extends GetPath>(
  path: Path,
  options?: GetOptions<Path>,
) => Promise<GetResult<Path>>;

function canonical(value: unknown): unknown {
  if (Array.isArray(value)) return value.map(canonical);
  if (value && typeof value === "object")
    return Object.fromEntries(
      Object.entries(value)
        .filter(([, item]) => item !== undefined)
        .sort(([left], [right]) => left.localeCompare(right))
        .map(([key, item]) => [key, canonical(item)]),
    );
  return value;
}

function keyMatchesResource(key: string, resource: ResourceRef) {
  const entry = entryResources.get(key);
  return entry?.some((candidate) => {
    if (candidate.kind !== resource.kind) return false;
    if ("accountKey" in candidate && "accountKey" in resource)
      return candidate.accountKey === resource.accountKey;
    if ("agentId" in candidate && "agentId" in resource)
      return !candidate.agentId || candidate.agentId === resource.agentId;
    return true;
  });
}

const entryResources = new Map<string, ResourceRef[]>();
let activeWorkspaceScope = getWorkspaceReadScope();

export function clearSharedReads() {
  entries.clear();
  entryResources.clear();
  entryPaths.clear();
}

function currentWorkspaceScope() {
  const scope = getWorkspaceReadScope();
  if (scope !== activeWorkspaceScope) {
    clearSharedReads();
    activeWorkspaceScope = scope;
  }
  return scope;
}

function resourceRefs<Path extends GetPath>(
  path: Path,
  query: GetOptions<Path>["query"],
): ResourceRef[] {
  const values = (query || {}) as Record<string, unknown>;
  switch (path) {
    case "/api/accounts":
      return [{ kind: "accounts" }];
    case "/api/models":
      return [{ kind: "models" }];
    case "/api/limits":
      return [
        { kind: "limits", accountKey: String(values.account_key || "default") },
      ];
    case "/api/costs":
      return [{ kind: "costs" }];
    case "/api/desktop":
      return [{ kind: "desktop" }];
    case "/api/worktree-disk": {
      const workers = typeof values.workers === "string" ? values.workers : "";
      return workers
        ? workers
            .split(",")
            .map((agentId) => ({ kind: "worktree-disk", agentId }))
        : [{ kind: "worktree-disk", agentId: "" }];
    }
    default:
      return [];
  }
}

observeResourceEvents((event) => {
  if (event.reason === "initial") return;
  const resources = event.resources;
  for (const resource of resources)
    for (const key of entries.keys())
      if (keyMatchesResource(key, resource)) {
        const entry = entries.get(key);
        if (entry && !entry.settled) entry.invalidated = true;
        else {
          entries.delete(key);
          entryResources.delete(key);
          entryPaths.delete(key);
        }
      }
  if (resources.some((resource) => resource.kind === "accounts")) {
    for (const key of entries.keys()) {
      if (
        ["/api/models", "/api/costs", "/api/limits"].includes(
          entryPaths.get(key) || "",
        )
      ) {
        const entry = entries.get(key);
        if (entry && !entry.settled) entry.invalidated = true;
        else {
          entries.delete(key);
          entryResources.delete(key);
          entryPaths.delete(key);
        }
      }
    }
  }
});

/** Share one successful GET response and one in-flight request for this renderer load. */
export function getShared<Path extends GetPath>(
  path: Path,
  options: GetOptions<Path> = {} as GetOptions<Path>,
  force = false,
): Promise<GetResult<Path>> {
  const key = JSON.stringify([
    currentWorkspaceScope(),
    path,
    canonical(options.query),
  ]);
  const existing = entries.get(key);
  if (existing && (!force || !existing.settled))
    return (
      existing.settled ? Promise.resolve(existing.value) : existing.promise
    ) as Promise<GetResult<Path>>;
  if (force && existing?.settled) {
    entries.delete(key);
    entryResources.delete(key);
    entryPaths.delete(key);
  }
  const request: Entry = {
    promise: undefined!,
    settled: false,
    invalidated: false,
  };
  request.promise = requestSharedGet(path, options).then(
    (value) => {
      request.settled = true;
      request.value = value;
      if (request.invalidated && entries.get(key) === request) {
        entries.delete(key);
        entryResources.delete(key);
        entryPaths.delete(key);
      }
      return value;
    },
    (error: unknown) => {
      if (entries.get(key) === request) entries.delete(key);
      entryResources.delete(key);
      entryPaths.delete(key);
      throw error;
    },
  );
  entries.set(key, request);
  entryResources.set(key, resourceRefs(path, options.query));
  entryPaths.set(key, path);
  return request.promise as Promise<GetResult<Path>>;
}

export function invalidateSharedRead<Path extends GetPath>(
  path: Path,
  query?: GetOptions<Path>["query"],
) {
  const key = JSON.stringify([currentWorkspaceScope(), path, canonical(query)]);
  const entry = entries.get(key);
  if (entry && !entry.settled) entry.invalidated = true;
  else {
    entries.delete(key);
    entryResources.delete(key);
    entryPaths.delete(key);
  }
}
