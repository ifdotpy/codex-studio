import type { Snapshot } from "../types";

type Runtime = NonNullable<Snapshot["runtime"]>;
type Task = Runtime["tasks"][number];
type Monitor = Runtime["monitors"][number] & { kind: "monitor" };

export const activeTask = (task: Task | Monitor) =>
  typeof task.status === "string" &&
  ["running", "starting", "approval", "pending", "stopping"].includes(
    task.status,
  );

export function backgroundTasks(data: Snapshot | null): (Task | Monitor)[] {
  return [
    ...(data?.runtime?.monitors ?? []).map((monitor) => ({
      ...monitor,
      kind: "monitor" as const,
    })),
    ...(data?.runtime?.tasks ?? []),
  ];
}
