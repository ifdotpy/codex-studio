import { useEffect, useLayoutEffect, useRef, useState } from "react";
import { errorText } from "./api";
import type {
  SidebarBackend,
  SidebarServices,
} from "./components/sidebar/services";
import { ProjectDeleteIntent, projectReaderScope } from "./projectDelete";
import type { Project, Snapshot } from "./types";

export function useProjectDelete(options: {
  data: Snapshot | null;
  workspaceId: string;
  services: SidebarServices;
  onSuspend: (scope: ReadonlySet<string>) => void;
  onResume: (scope: ReadonlySet<string>) => void;
  onFinish: (ids: string[]) => void;
  notify: (message: string) => void;
}) {
  const latest = useRef(options);
  latest.current = options;
  const [target, setTarget] = useState<{
    project: Project;
    seq: number;
    workspaceId: string;
  } | null>(null);
  const [pending, setPending] = useState<{
    intent: ProjectDeleteIntent;
    backend: SidebarBackend;
    attempt: number;
  } | null>(null);
  const pendingRef = useRef(pending);
  const [error, setError] = useState("");
  const [, render] = useState(0);
  const prior = useRef<{
    data: Snapshot;
    workspaceId: string;
    readers: Map<string, Set<string>>;
  } | null>(null);
  const restored = useRef("");

  useLayoutEffect(() => {
    const { data, workspaceId, onFinish } = options;
    if (
      !data ||
      data.entitiesComplete !== true ||
      data.entityWorkspaceId !== workspaceId ||
      !workspaceId
    )
      return;
    const previous = prior.current;
    const sameWorkspace =
      previous?.workspaceId === workspaceId &&
      previous.data.stateDir === data.stateDir;
    const readers = sameWorkspace
      ? previous.readers
      : new Map<string, Set<string>>();
    prior.current = { data, workspaceId, readers };
    if (
      sameWorkspace &&
      previous.data.runtime.projects === data.runtime.projects &&
      previous.data.threads === data.threads &&
      previous.data.runtime.rooms === data.runtime.rooms
    )
      return;
    // Retain observed reader IDs until their project retires: agent and project
    // tombstones can reach React in separate stream publications.
    const registered = new Set(
      data.runtime.projects.map((project) => project.id),
    );
    const ids = new Set<string>();
    for (const [projectId, scope] of readers) {
      if (registered.has(projectId)) continue;
      for (const id of scope) ids.add(id);
      readers.delete(projectId);
    }
    for (const project of data.runtime.projects) {
      const scope = readers.get(project.id) || new Set<string>();
      for (const id of projectReaderScope(
        project,
        data.threads,
        data.runtime.rooms,
      ))
        scope.add(id);
      readers.set(project.id, scope);
    }
    if (ids.size) onFinish([...ids]);
  }, [options.data, options.workspaceId]);

  useEffect(() => {
    if (!options.workspaceId || restored.current === options.workspaceId)
      return;
    restored.current = options.workspaceId;
    const previous = pendingRef.current;
    if (
      previous &&
      previous.intent.request.expected_workspace_id !== options.workspaceId
    ) {
      // Its durable record belongs to the old workspace. A late response must
      // never clear an overlapping chat ID or draft in the newly selected one.
      pendingRef.current = null;
      setPending(null);
      setTarget(null);
      options.onResume(new Set(previous.intent.scope));
    }
    try {
      const backend = options.services.cache;
      const intent = ProjectDeleteIntent.restore(
        backend.storage,
        options.workspaceId,
      );
      if (!intent) return;
      const next = { intent, backend, attempt: 0 };
      pendingRef.current = next;
      options.onSuspend(new Set(intent.scope));
      setPending(next);
    } catch (failure) {
      setError(errorText(failure));
      options.notify(errorText(failure));
    }
  }, [options.workspaceId, options.services]);

  useEffect(() => {
    if (!pending || pending.intent.phase !== "submitting") return;
    // This effect follows the render that unmounts affected resource readers.
    void pending.intent.submit(pending.backend.post).then((submitted) => {
      if (
        !submitted ||
        pendingRef.current !== pending ||
        latest.current.workspaceId !==
          pending.intent.request.expected_workspace_id
      )
        return;
      const { intent } = pending;
      if (intent.phase === "deleted" && intent.result) {
        if (intent.error) latest.current.notify(intent.error);
        latest.current.onFinish([
          ...intent.result.deletedAgentIds,
          ...intent.result.deletedRoomIds,
        ]);
        latest.current.onResume(new Set(intent.scope));
        pendingRef.current = null;
        setPending(null);
        setTarget(null);
      } else if (intent.phase === "rejected") {
        latest.current.onResume(new Set(intent.scope));
        latest.current.notify(intent.error);
      }
      render((value) => value + 1);
    });
  }, [pending]);

  const open = (idOrPath: string) => {
    if (pendingRef.current) return;
    const { data, workspaceId, notify } = latest.current;
    const project =
      data?.runtime.projects.find((row) => row.id === idOrPath) ||
      data?.runtime.projects.find((row) => row.path === idOrPath);
    const seq = project?.id
      ? data?.runtime.projectSequences?.[project.id]
      : undefined;
    if (
      !project ||
      !workspaceId ||
      seq === undefined ||
      !Number.isSafeInteger(seq) ||
      data?.entitiesComplete !== true ||
      data.entityWorkspaceId !== workspaceId
    ) {
      notify(
        "Wait for the project's authoritative streamed state before deleting it.",
      );
      return;
    }
    setError("");
    setTarget({ project, seq, workspaceId });
  };
  const confirm = () => {
    if (!target || pendingRef.current) return;
    const { data, workspaceId, services, onSuspend } = latest.current;
    if (!data || !workspaceId || !target.project.id || !target.project.path)
      return;
    if (
      workspaceId !== target.workspaceId ||
      data.entityWorkspaceId !== target.workspaceId
    ) {
      setError(
        "The workspace changed. Close this dialog and confirm deletion again from the current project.",
      );
      return;
    }
    try {
      const backend = services.owner({
        kind: "project",
        path: target.project.path,
      });
      const scope = projectReaderScope(
        target.project,
        data.threads,
        data.runtime.rooms,
      );
      const intent = new ProjectDeleteIntent(
        {
          id: target.project.id,
          request_id: crypto.randomUUID(),
          expected_workspace_id: workspaceId,
          expected_project_seq: target.seq,
        },
        target.project.name || target.project.path,
        [...scope],
        backend.storage,
      );
      const next = { intent, backend, attempt: 0 };
      pendingRef.current = next;
      onSuspend(scope);
      setPending(next);
    } catch (failure) {
      setError(errorText(failure));
    }
  };
  return {
    open,
    dialog: {
      name:
        pending?.intent.name ??
        target?.project.name ??
        target?.project.path ??
        null,
      phase:
        pending?.intent.phase === "deleted"
          ? ("submitting" as const)
          : (pending?.intent.phase ?? ("confirming" as const)),
      error: pending?.intent.error || error,
      onConfirm: confirm,
      onCancel: () => {
        const current = pendingRef.current;
        if (current && current.intent.phase !== "rejected") return;
        pendingRef.current = null;
        setPending(null);
        setTarget(null);
        setError("");
      },
      onRecover: () => {
        const current = pendingRef.current;
        if (
          !current ||
          current.intent.phase !== "unknown" ||
          current.intent.request.expected_workspace_id !==
            latest.current.workspaceId
        )
          return;
        current.intent.retry();
        const next = { ...current, attempt: current.attempt + 1 };
        pendingRef.current = next;
        setPending(next);
      },
    },
  };
}
