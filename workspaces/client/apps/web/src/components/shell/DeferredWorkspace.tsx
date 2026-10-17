import { useEffect, useState, type ComponentProps } from "react";

type WorkspaceComponent = typeof import("./Workspace").default;

// Mount only when requested, then retain the module and screen state. Explicit
// readiness avoids delaying first-open content behind a new Suspense fallback.
export default function DeferredWorkspace(
  props: ComponentProps<WorkspaceComponent>,
) {
  const [Workspace, setWorkspace] = useState<WorkspaceComponent | null>(null);
  const [error, setError] = useState<Error | null>(null);
  useEffect(() => {
    let current = true;
    void import("./Workspace").then(
      (module) => {
        if (current) setWorkspace(() => module.default);
      },
      (reason: unknown) => {
        if (current)
          setError(
            reason instanceof Error ? reason : new Error(String(reason)),
          );
      },
    );
    return () => {
      current = false;
    };
  }, []);
  if (error) throw error;
  return Workspace ? <Workspace {...props} /> : null;
}
