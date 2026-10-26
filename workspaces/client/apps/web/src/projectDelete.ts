import { ApiError, errorText, type post } from "./api";
import { definiteDeleteRejection } from "./chatDeleteScope";
import type { components } from "./generated/api";
import type { Agent, Project } from "./types";

type DeleteRequest = components["schemas"]["ProjectDeleteRequest"];
type DeleteReply = components["schemas"]["ProjectDeleteResponse"];
type DeleteFailureCode = components["schemas"]["ProjectDeleteFailureCode"];
type DeleteError = components["schemas"]["ProjectDeleteErrorResponse"];
// Exhaustiveness keeps this classification paired with the generated contract.
const definitiveCodes = {
  project_not_found: true,
  project_workspace_conflict: true,
  project_sequence_conflict: true,
  project_request_conflict: true,
  project_remote_scope: true,
  project_membership_conflict: true,
  project_lifecycle_unavailable: true,
} satisfies Record<DeleteFailureCode, true>;

function definiteProjectRejection(failure: unknown): boolean {
  if (!(failure instanceof ApiError)) return false;
  if (failure.response && typeof failure.response === "object") {
    const response = failure.response as Partial<DeleteError>;
    if (
      response.outcome === "not_applied" &&
      typeof response.error === "string" &&
      typeof response.code === "string" &&
      Object.hasOwn(definitiveCodes, response.code)
    )
      return true;
  }
  return definiteDeleteRejection(failure);
}
type IntentStorage = Pick<Storage, "getItem" | "setItem" | "removeItem">;
const storageKey = (workspaceId: string) =>
  `studio-project-delete-intention-v1:${workspaceId}`;
const unknownMessage =
  "Delete result unconfirmed. Recover the saved request to check its receipt. Studio will keep the same request identity.";

/** Reader protection only. The server independently selects the full deletion membership. */
export function projectReaderScope(
  project: Project,
  agents: readonly Agent[],
  rooms: readonly {
    id: string;
    projectPath?: string | null | undefined;
    rootId?: string | null | undefined;
  }[],
): Set<string> {
  const paths = new Set([
    project.path,
    ...(project.locations || []).map((location) => location.path),
  ]);
  const scope = new Set<string>();
  for (const agent of agents) {
    const bound = !!(agent.projectId || agent.projectServerId);
    if (
      bound
        ? agent.projectId === project.id &&
          agent.projectServerId === project.homeServerId
        : paths.has(agent.hostProjectPath || agent.cwd)
    ) {
      scope.add(agent.id);
    }
  }
  let changed = true;
  while (changed) {
    changed = false;
    for (const agent of agents) {
      if (agent.parentId && scope.has(agent.parentId) && !scope.has(agent.id)) {
        scope.add(agent.id);
        changed = true;
      }
    }
  }
  for (const room of rooms)
    if (
      (room.projectPath && paths.has(room.projectPath)) ||
      (room.rootId && scope.has(room.rootId))
    )
      scope.add(room.id);
  return scope;
}

function confirmedReply(
  value: unknown,
  request: DeleteRequest,
): value is DeleteReply {
  if (!value || typeof value !== "object") return false;
  const reply = value as DeleteReply;
  return (
    reply.id === request.id &&
    reply.request_id === request.request_id &&
    reply.outcome === "deleted" &&
    Array.isArray(reply.deletedAgentIds) &&
    reply.deletedAgentIds.every((id) => typeof id === "string") &&
    Array.isArray(reply.deletedRoomIds) &&
    reply.deletedRoomIds.every((id) => typeof id === "string")
  );
}

/** A saved confirmed intention survives remounts; only explicit recovery may replay its exact body. */
export class ProjectDeleteIntent {
  phase: "submitting" | "unknown" | "rejected" | "deleted" = "submitting";
  error = "";
  result: DeleteReply | null = null;
  private started = false;
  readonly request: DeleteRequest;
  readonly name: string;
  readonly scope: string[];
  private readonly storage: IntentStorage;
  constructor(
    request: DeleteRequest,
    name: string,
    scope: string[],
    storage: IntentStorage,
  ) {
    this.request = request;
    this.name = name;
    this.scope = scope;
    this.storage = storage;

    this.request = Object.freeze({ ...request });
    const existing = storage.getItem(storageKey(request.expected_workspace_id));
    if (existing) {
      const saved = JSON.parse(existing) as { request?: DeleteRequest };
      if (JSON.stringify(saved.request) !== JSON.stringify(request))
        throw new Error(
          "Recover the saved project deletion before confirming another intention.",
        );
      return;
    }
    // Save before changing readers or issuing HTTP. Storage failure must send nothing.
    storage.setItem(
      storageKey(request.expected_workspace_id),
      JSON.stringify({ request, name, scope }),
    );
  }

  static restore(
    storage: IntentStorage,
    workspaceId: string,
  ): ProjectDeleteIntent | null {
    const raw = storage.getItem(storageKey(workspaceId));
    if (!raw) return null;
    const value = JSON.parse(raw) as {
      request: DeleteRequest;
      name: string;
      scope: string[];
    };
    if (
      !value ||
      !value.request ||
      value.request.expected_workspace_id !== workspaceId
    )
      throw new Error(
        "The saved project deletion request has an invalid workspace. It was not resent.",
      );
    if (
      typeof value.request.id !== "string" ||
      !value.request.id ||
      typeof value.request.request_id !== "string" ||
      !value.request.request_id ||
      !Number.isSafeInteger(value.request.expected_project_seq) ||
      typeof value.name !== "string" ||
      !Array.isArray(value.scope) ||
      !value.scope.every((id) => typeof id === "string")
    )
      throw new Error(
        "The saved project deletion request is invalid. It was not resent.",
      );
    const intent = new ProjectDeleteIntent(
      value.request,
      value.name,
      value.scope,
      storage,
    );
    intent.phase = "unknown";
    intent.error = unknownMessage;
    return intent;
  }

  retry() {
    if (this.phase !== "unknown") return;
    this.phase = "submitting";
    this.started = false;
    this.error = "";
  }

  async submit(send: typeof post): Promise<boolean> {
    if (this.phase !== "submitting" || this.started) return false;
    this.started = true;
    try {
      const result = await send("/api/projects/delete", this.request);
      if (!confirmedReply(result, this.request))
        throw new Error("Invalid project deletion receipt");
      this.result = result;
      this.phase = "deleted";
    } catch (failure) {
      if (definiteProjectRejection(failure)) {
        this.phase = "rejected";
        this.error = errorText(failure);
      } else {
        this.phase = "unknown";
        this.error = `${unknownMessage} ${errorText(failure)}`;
      }
    }
    if (this.phase === "deleted" || this.phase === "rejected") {
      try {
        this.storage.removeItem(storageKey(this.request.expected_workspace_id));
      } catch (failure) {
        this.error += ` The saved request could not be cleared: ${errorText(failure)}`;
      }
    }
    return true;
  }
}
