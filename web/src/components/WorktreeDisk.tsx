import type { Agent } from "../types";
import type { WorktreeDiskSnapshot } from "../hooks/useWorktreeDisk";

export function formatDiskBytes(bytes: number) {
  if (bytes < 1024) return `${bytes} B`;
  const units = ["KiB", "MiB", "GiB", "TiB"];
  const power = Math.min(
    Math.floor(Math.log(bytes) / Math.log(1024)),
    units.length,
  );
  const value = bytes / 1024 ** power;
  return `${value >= 10 ? value.toFixed(0) : value.toFixed(1)} ${units[power - 1]}`;
}

export function WorkerDiskLabel({
  agent,
  disk,
}: {
  agent: Agent;
  disk?: WorktreeDiskSnapshot["workers"][string];
}) {
  if (!agent.worktree) return null;
  const label =
    disk?.state === "ready" && disk.bytes !== undefined
      ? formatDiskBytes(disk.bytes)
      : disk?.state === "missing"
        ? "No folder"
        : disk?.state === "unavailable"
          ? "Unavailable"
          : "Measuring";
  return (
    <small
      className="worker-disk"
      title="Allocated disk blocks. APFS clones can share blocks."
    >
      Disk: {label}
    </small>
  );
}

export function TeamDiskTotal({
  workers,
  disk,
}: {
  workers: Agent[];
  disk?: WorktreeDiskSnapshot;
}) {
  if (!disk) return null;
  const worktrees = workers.filter((agent) => agent.worktree);
  if (!worktrees.length) return null;
  const measured = worktrees.map((agent) => disk.workers[agent.id]);
  const bytes = measured.reduce(
    (sum, row) => sum + (row?.state === "ready" ? row.bytes || 0 : 0),
    0,
  );
  const unknown = measured.filter((row) => row?.state !== "ready").length;
  return (
    <div
      className="team-disk-total"
      title="Allocated disk blocks. APFS clones can share blocks."
    >
      <span>
        Worktrees: {formatDiskBytes(bytes)}
        {unknown ? `, ${unknown} not measured` : ""}
      </span>
      {disk.error && <span>Disk measure unavailable.</span>}
      {disk.warning && (
        <strong>
          Disk limit reached: {formatDiskBytes(disk.totalBytes)} /{" "}
          {formatDiskBytes(disk.limitBytes)} across all workers.
        </strong>
      )}
    </div>
  );
}
