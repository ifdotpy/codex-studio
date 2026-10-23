import type { ReactNode } from "react";

// The surface remains in the same portal container across presentation changes.
export function WorkspaceSurface({ children }: { children: ReactNode }) {
  return <div className="workspace-surface">{children}</div>;
}
