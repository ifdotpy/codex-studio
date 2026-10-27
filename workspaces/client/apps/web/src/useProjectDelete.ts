import { randomUUID } from "./uuid";
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
    readers: Set<string>;
  } | null>(null);
  const confirmedSuppression = useRef({
    workspaceId: "",
    ids: new Set<string>(),
  });
  const restored = useRef("");
  const publishSuppression = () => {
    const current = latest.current;
    const scope = new Set(
      confirmedSuppression.current.workspaceId === current.workspaceId
        ? confirmedSuppression.current.ids
        : [],
    );
    const intention = pendingRef.current?.intent;
    if (
      intention &&
      intention.request.expected_workspace_id === current.workspaceId &&
      (intention.phase === "submitting" || intention.phase === "unknown")
    )
      for (const id of intention.scope) scope.add(id);
    if (scope.size) current.onSuspend(scope);
    else current.onResume(new Set());
  };

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
    const readers = sameWorkspace ? previous.readers : new Set<string>();
    prior.current = { data, workspaceId, readers };
    if (
      sameWorkspace &&
      previous.data.runtime.projects === data.runtime.projects &&
      previous.data.threads === data.threads &&
      previous.data.runtime.rooms === data.runtime.rooms
    )
      return;
    // Registry removal is not evidence of chat deletion. Only absent entity IDs
    // from the complete authoritative projection retire their local readers/drafts.
    const present = new Set(
      [...data.threads, ...data.runtime.rooms, ...(data.chats || [])].map(
        (row) => row.id,
      ),
    );
    const ids = new Set<string>();
    for (const id of readers)
      if (!present.has(id)) {
        ids.add(id);
        readers.delete(id);
      }
    for (const project of data.runtime.projects)
      for (const id of projectReaderScope(
        project,
        data.threads,
        data.runtime.rooms,
      ))
        readers.add(id);
    const suppression = confirmedSuppression.current;
    if (suppression.workspaceId && suppression.workspaceId !== workspaceId) {
      suppression.ids.clear();
      suppression.workspaceId = workspaceId;
      publishSuppression();
    }
    if (suppression.workspaceId === workspaceId) {
      let changed = false;
      for (const id of suppression.ids)
        if (!present.has(id)) {
          suppression.ids.delete(id);
          changed = true;
        }
      if (changed) publishSuppression();
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
      publishSuppression();
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
        const ids = [
          ...intent.result.deletedAgentIds,
          ...intent.result.deletedRoomIds,
        ];
        const current = latest.current;
        const suppression = confirmedSuppression.current;
        if (suppression.workspaceId !== current.workspaceId) {
          suppression.workspaceId = current.workspaceId;
          suppression.ids.clear();
        }
        const present = new Set(
          [
            ...(current.data?.threads || []),
            ...(current.data?.runtime.rooms || []),
            ...(current.data?.chats || []),
          ].map((row) => row.id),
        );
        const complete =
          current.data?.entitiesComplete === true &&
          current.data.entityWorkspaceId === current.workspaceId;
        for (const id of ids)
          if (!complete || present.has(id)) suppression.ids.add(id);
        publishSuppression();
        current.onFinish(ids);
        pendingRef.current = null;
        setPending(null);
        setTarget(null);
      } else if (intent.phase === "rejected") {
        if (confirmedSuppression.current.ids.size) publishSuppression();
        else latest.current.onResume(new Set(intent.scope));
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
    const { data, workspaceId, services } = latest.current;
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
          request_id: randomUUID(),
          expected_workspace_id: workspaceId,
          expected_project_seq: target.seq,
        },
        target.project.name || target.project.path,
        [...scope],
        backend.storage,
      );
      const next = { intent, backend, attempt: 0 };
      pendingRef.current = next;
      publishSuppression();
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
