import { Button, Loader } from "@mantine/core";
import type { GetResult } from "../api";
import type { Agent } from "../types";
import { useWorkspaceResource } from "./useWorkspaceResource";

export type FinderView = GetResult<"/api/agents/finder-view">;

export const VM_FOLDER_NOTE =
  "This chat works in the Studio VM. Finder shows a read-only view of the project's main line and states.";

type FolderProps = {
  agent: Agent;
  /** Only the desktop app on its own server can reveal a Mac path. */
  canReveal: boolean;
  onReveal: (path: string) => void;
};

/** The Mac folder Finder can show, or the reason it has none. */
export function finderLocation(
  view: FinderView,
): { path: string } | { reason: string } {
  if ((view.kind === "native" || view.state === "mounted") && view.path)
    return { path: view.path };
  return { reason: (view.kind === "vm" && view.error) || "Not mounted yet" };
}

export function FinderLocation({
  view,
  canReveal,
  onReveal,
}: Omit<FolderProps, "agent"> & { view: FinderView }) {
  const location = finderLocation(view);
  if ("reason" in location) return <p>{location.reason}</p>;
  return (
    <>
      <p>{location.path}</p>
      {canReveal && (
        <Button onClick={() => onReveal(location.path)}>Show in Finder</Button>
      )}
    </>
  );
}

function VmProjectFolder({ agent, canReveal, onReveal }: FolderProps) {
  const state = useWorkspaceResource("/api/agents/finder-view", 0, {
    query: { agent: agent.id },
  });
  return (
    <>
      <p>{VM_FOLDER_NOTE}</p>
      {state.error ? (
        <p role="alert">{state.error}</p>
      ) : state.data ? (
        <FinderLocation
          view={state.data}
          canReveal={canReveal}
          onReveal={onReveal}
        />
      ) : (
        <Loader size="sm" aria-label="Loading the Finder view" />
      )}
      <p>To use another folder, start a new chat.</p>
    </>
  );
}

/** A VM chat works in a guest path; Finder shows its read-only ~/Studio view. */
export default function ProjectFolderDialog(props: FolderProps) {
  const { agent, canReveal, onReveal } = props;
  if (agent.executionMode === "vm") return <VmProjectFolder {...props} />;
  return (
    <>
      <p>{agent.cwd}</p>
      <p>To use another folder, start a new chat.</p>
      {canReveal && agent.cwd && (
        <Button onClick={() => onReveal(agent.cwd!)}>Show in Finder</Button>
      )}
    </>
  );
}
