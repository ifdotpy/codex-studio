import {
  ApiError,
  type ApiPostPath,
  type PostBody,
  type PostResult,
  type PostOptions,
} from "../api";
import {
  createSidebarServices,
  type SidebarBackend,
  type SidebarTarget,
} from "../components/sidebar/services";
import type { Project } from "../types";
import { mergeSidebar, type MergedSidebar } from "./mergedSidebar";
import {
  mergedSidebarPreferences,
  sidebarOrderWrite,
} from "./mergedSidebarPreferences";

const acknowledgedOrderRevisions = new WeakMap<SidebarBackend, number>();

export function sidebarOwner(
  model: MergedSidebar,
  target: SidebarTarget,
): string {
  if (target.kind === "chat")
    return model.requireReference(model.chatReferences, target.id).owner;
  if (target.kind === "order") return model.orderOwner(target.group);
  if (target.chat)
    return model.requireReference(model.chatReferences, target.chat).owner;
  if (target.team)
    return model.requireReference(model.teamReferences, target.team).owner;
  if (target.folder)
    return model.requireReference(model.folderReferences, target.folder).owner;
  return model.requireReference(model.projectReferences, target.path).owner;
}

export function sidebarProject(
  model: MergedSidebar,
  owner: string,
  path: string,
): Project {
  const source = model.groups
    .get(path)
    ?.members.find((member) => member.source.id === owner);
  const projected = model.data.runtime.projects.find(
    (project) => project.path === path,
  );
  if (!projected) throw new Error("The project is unavailable on this server.");
  if (!source) {
    const reference = model.projectReferences.get(path);
    if (reference?.owner !== owner)
      throw new Error("The project belongs to another server.");
    return { ...projected, organizationRevision: 0, peerTeamsRevision: 0 };
  }
  return {
    ...projected,
    organizationRevision: source.project.organizationRevision,
    peerTeamsRevision: source.project.peerTeamsRevision,
    folders: (projected.folders || []).filter((folder) =>
      source.project.folders?.some(
        (native) =>
          model.folderKey(owner, source.project.path || "", native.id) ===
          folder.id,
      ),
    ),
  };
}

const record = (value: unknown): Record<string, unknown> => {
  if (!value || typeof value !== "object" || Array.isArray(value))
    throw new Error("Invalid sidebar request.");
  return value as Record<string, unknown>;
};

/** Translate view identities once on the connection captured by a form. */
export function sidebarWireRequest(
  model: MergedSidebar,
  owner: string,
  endpoint: ApiPostPath,
  body: unknown,
): Record<string, unknown> {
  const value = { ...record(body) };
  const chat = (id: unknown) => {
    if (typeof id !== "string")
      throw new Error("Invalid sidebar chat identity.");
    const reference = model.requireReference(model.chatReferences, id);
    if (reference.owner !== owner)
      throw new Error("These chats belong to different servers.");
    return reference.id;
  };
  const team = (id: unknown, allowNew = false) => {
    if (id === null || id === undefined) return id;
    if (typeof id !== "string")
      throw new Error("Invalid sidebar team identity.");
    const reference = model.teamReferences.get(id);
    if (!reference && allowNew) return id;
    if (!reference || reference.owner !== owner)
      throw new Error("The team belongs to another server.");
    return reference.id;
  };
  const folder = (path: string, id: unknown, allowNew = false) => {
    if (id === null || id === undefined || id === "") return id;
    if (typeof id !== "string")
      throw new Error("Invalid sidebar folder identity.");
    const member = model.groups
      .get(path)
      ?.members.find((item) => item.source.id === owner);
    const original = member?.project.folders?.find(
      (item) =>
        model.folderKey(owner, member.project.path || "", item.id) === id,
    );
    if (original) return original.id;
    if (allowNew && !model.folderReferences.has(id)) return id;
    throw new Error("The folder is unavailable on this chat's server.");
  };
  if (endpoint === "/api/rename") {
    value.id = chat(value.id);
    return value;
  }
  if (endpoint === "/api/organization") {
    value.id = chat(value.id);
    if (typeof value.project_path === "string") {
      const path = value.project_path;
      value.project_path = model.wireProject(owner, path);
      if ("project_folder" in value)
        value.project_folder = folder(path, value.project_folder);
      if ("expected_folder" in value)
        value.expected_folder = folder(path, value.expected_folder);
    }
    return value;
  }
  if (endpoint === "/api/projects" && value.action === "reorder") {
    return {
      ...value,
      ...sidebarOrderWrite(
        model,
        owner,
        value.groups as Record<string, string[]>,
      ),
    };
  }
  if (endpoint !== "/api/projects" && endpoint !== "/api/peer-teams")
    throw new Error("The sidebar API path is unavailable.");
  if (typeof value.path !== "string")
    throw new Error("The sidebar project path is unavailable.");
  const path = value.path;
  const member = model.groups
    .get(path)
    ?.members.find((item) => item.source.id === owner);
  value.path = model.wireProject(owner, path);
  if (endpoint === "/api/projects") {
    if ("expected_revision" in value)
      value.expected_revision = member?.project.organizationRevision || 0;
    if ("folder_id" in value)
      value.folder_id = folder(
        path,
        value.folder_id,
        value.action === "add_folder",
      );
    if ("parent_id" in value) value.parent_id = folder(path, value.parent_id);
  } else {
    if ("expected_revision" in value)
      value.expected_revision = member?.project.peerTeamsRevision || 0;
    if ("team_id" in value)
      value.team_id = team(value.team_id, value.action === "save");
    if (Array.isArray(value.members)) value.members = value.members.map(chat);
    if ("member" in value) value.member = chat(value.member);
    if ("target" in value) value.target = chat(value.target);
  }
  return value;
}

export function createMergedSidebarServices(
  model: MergedSidebar,
  raw: ReadonlyMap<string, SidebarBackend>,
  online: (owner: string) => boolean,
  aliases: Record<string, string>,
) {
  const owners = new Set([
    ...raw.keys(),
    ...[...model.projectReferences.values()].map(
      (reference) => reference.owner,
    ),
  ]);
  const wrapped = new Map<string, SidebarBackend>();
  const latestOrderRevision = new Map(
    model.sources.map((source) => [
      source.id,
      source.sidebar.sidebarOrder?.revision || 0,
    ]),
  );
  for (const owner of owners) {
    const backend = raw.get(owner);
    const source = model.sourceById.get(owner);
    if (backend && source)
      latestOrderRevision.set(
        owner,
        Math.max(
          latestOrderRevision.get(owner) || 0,
          acknowledgedOrderRevisions.get(backend) || 0,
        ),
      );
    const missing: SidebarBackend = {
      ownerId: owner,
      post: async () => {
        throw new Error("The sidebar server is unavailable.");
      },
      storage: {
        getItem: () => null,
        setItem: () => {
          throw new Error("The sidebar server is unavailable.");
        },
        removeItem: () => {},
      },
      saved: (_key, fallback) => fallback,
      save: () => {},
      editPreference: (_key, previous) => previous,
      subscribePreference: () => () => {},
    };
    if (!backend) {
      wrapped.set(owner, missing);
      continue;
    }
    wrapped.set(owner, {
      ...backend,
      async post<Path extends ApiPostPath>(
        path: Path,
        body: PostBody<Path>,
        options?: PostOptions,
      ): Promise<PostResult<Path>> {
        if (!online(owner)) throw new Error("The sidebar server is offline.");
        const requestId = record(body).request_id || options?.requestId;
        const key =
          typeof requestId === "string"
            ? `studio-sidebar-wire:${requestId}`
            : undefined;
        const stored = key
          ? backend.saved<{
              path: string;
              visible: unknown;
              wire: Record<string, unknown>;
            } | null>(key, null)
          : null;
        if (
          stored &&
          (stored.path !== path ||
            JSON.stringify(stored.visible) !== JSON.stringify(body))
        )
          throw new Error(
            "The saved sidebar request belongs to another change.",
          );
        const wire =
          stored?.wire || sidebarWireRequest(model, owner, path, body);
        if (!stored && path === "/api/projects" && wire.action === "reorder")
          wire.expected_revision = latestOrderRevision.get(owner) ?? 0;
        if (key && !stored) backend.save(key, { path, visible: body, wire });
        let response: PostResult<Path>;
        try {
          response = await backend.post(path, wire as PostBody<Path>, options);
          if (key) backend.storage.removeItem(key);
        } catch (failure) {
          if (
            key &&
            failure instanceof ApiError &&
            failure.status >= 400 &&
            failure.status < 500 &&
            failure.status !== 408
          )
            backend.storage.removeItem(key);
          throw failure;
        }
        const result = record(response);
        if (
          path === "/api/projects" &&
          wire.action === "reorder" &&
          typeof result.revision === "number" &&
          Number.isSafeInteger(result.revision)
        ) {
          latestOrderRevision.set(owner, result.revision);
          if (backend) acknowledgedOrderRevisions.set(backend, result.revision);
        }
        if (path === "/api/organization" && "projectFolder" in result) {
          const reference = model.chatReferences.get(
            record(body).id as string,
          )!;
          return {
            ...result,
            projectFolder:
              typeof result.projectFolder === "string"
                ? model.folderKey(
                    owner,
                    reference.path || "",
                    result.projectFolder,
                  )
                : result.projectFolder,
          } as PostResult<Path>;
        }
        if (path === "/api/projects" && wire.action === "reorder") {
          const updated = mergeSidebar(
            model.sources.map((source) =>
              source.id === owner
                ? {
                    ...source,
                    sidebar: {
                      ...source.sidebar,
                      sidebarOrder: result as unknown as NonNullable<
                        typeof source.sidebar.sidebarOrder
                      >,
                    },
                  }
                : source,
            ),
            (model.data.runtime.sidebarOrder?.revision || 0) + 1,
            true,
          );
          return {
            ...result,
            revision: updated.data.runtime.sidebarOrder!.revision,
            groups: updated.order,
          } as PostResult<Path>;
        }
        return response;
      },
    });
  }
  const cache = mergedSidebarPreferences(model, raw);
  const services = createSidebarServices(
    cache,
    [...wrapped.values()],
    (target) => sidebarOwner(model, target),
  );
  return {
    ...services,
    available: (target: SidebarTarget) => {
      try {
        return online(sidebarOwner(model, target));
      } catch {
        return false;
      }
    },
    project: (path: string, target: SidebarTarget) =>
      sidebarProject(model, sidebarOwner(model, target), path),
    displayPath: (path: string, target?: SidebarTarget) =>
      target
        ? model.wireProject(sidebarOwner(model, target), path)
        : model.projectReferences.get(path)?.path || path,
    chatPath: (id: string) => model.chatReferences.get(id)?.path || "",
    alias: (target: SidebarTarget) => aliases[sidebarOwner(model, target)],
  };
}
