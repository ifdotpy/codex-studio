import { Button, Group, Modal, Stack, Text } from "@mantine/core";
import { useState } from "react";
import type { components } from "../../generated/api";
import type { SidebarBackend } from "./services";
import type { LayrReadinessSummary } from "./LayrReadiness";

export type LayrSync = components["schemas"]["ProjectLayrSyncDto"];
/** One server's Mac folder sync; `server` is its alias when the sidebar combines servers. */
export type LayrSyncEntry = {
  server?: string;
  ownerId?: string;
  sync: LayrSync;
};
export type LayrSyncSummary = {
  state: LayrSync["state"];
  waiting: number;
  deletes: number;
  title: string;
};
export type MacSyncChoice = "mac" | "vm" | "agent";

// The row shows the state that needs attention first.
const ATTENTION: LayrSync["state"][] = [
  "error",
  "conflict",
  "paused",
  "waiting",
  "syncing",
  "synced",
];

/** Paths that wait for the user: conflicts, then first-round differences. */
export function macSyncOpen(sync: LayrSync): string[] {
  return [...new Set([...sync.conflicts, ...sync.choices])];
}

const files = (count: number) => (count === 1 ? "1 file" : `${count} files`);

export function layrSyncText(sync: LayrSync): string {
  const open = macSyncOpen(sync).length;
  switch (sync.state) {
    case "synced":
      return "Mac folder synced with the VM";
    case "syncing":
      return "Mac folder sync is running";
    case "waiting":
      return sync.withAgent.length
        ? `Mac folder sync: the agent combines ${files(sync.withAgent.length)}`
        : "Mac folder sync waits for the VM";
    case "conflict":
      return `Mac folder sync: ${files(open)} changed on both sides`;
    case "paused":
      if (sync.deletes)
        return `Mac folder sync paused: confirm ${files(sync.deletes)} deleted on the Mac`;
      return "Mac folder sync paused";
    case "error":
      return sync.reason
        ? `Mac folder sync failed: ${sync.reason}`
        : "Mac folder sync failed";
  }
}

export function layrSyncSummary(
  entries: readonly LayrSyncEntry[],
): LayrSyncSummary | null {
  if (!entries.length) return null;
  const state = ATTENTION.find((item) =>
    entries.some((entry) => entry.sync.state === item),
  )!;
  const waiting = entries.reduce(
    (total, entry) => total + macSyncOpen(entry.sync).length,
    0,
  );
  const deletes = entries.reduce(
    (total, entry) => total + entry.sync.deletes,
    0,
  );
  const title = entries
    .map(
      (entry) =>
        (entry.server ? `${entry.server}: ` : "") + layrSyncText(entry.sync),
    )
    .join("\n");
  return { state, waiting, deletes, title };
}

export type ProjectChip = {
  summary: LayrReadinessSummary;
  attention: boolean;
  /** The project's vault: a cloud replaces the dot; null without a vault. */
  vault?: "ok" | "error" | null;
};

const ATTENTION_SYNC: LayrSync["state"][] = ["error", "conflict"];

/** The row has one chip, LAYR, on a second line. A Mac folder sync that needs the user (a
 * conflict, a failure, files that wait for a choice or deletions to confirm) marks it and
 * adds its report to the tooltip; a quiet or paused sync does not show. */
export function projectChip(
  layr: LayrReadinessSummary | null,
  sync: LayrSyncSummary | null,
): ProjectChip | null {
  if (!layr) return null;
  const attention =
    !!sync &&
    (ATTENTION_SYNC.includes(sync.state) ||
      sync.waiting > 0 ||
      sync.deletes > 0);
  return {
    summary:
      attention && sync
        ? { ...layr, title: `${layr.title}\n${sync.title}` }
        : layr,
    attention,
  };
}

const CHOICES: { choice: MacSyncChoice; label: string; hint: string }[] = [
  {
    choice: "mac",
    label: "Keep Mac",
    hint: "The VM takes the file from your Mac folder.",
  },
  {
    choice: "vm",
    label: "Keep VM",
    hint: "Your Mac folder takes the file from the VM.",
  },
  {
    choice: "agent",
    label: "Ask agent",
    hint: "The lead chat combines both versions in the VM.",
  },
];

/** Per path: keep the Mac version, keep the VM version, or ask the lead to combine them. */
export function MacSyncConflicts({
  entry,
  backend,
  projectName,
  onClose,
}: {
  entry: LayrSyncEntry | null;
  backend: SidebarBackend | null;
  projectName: string;
  onClose: () => void;
}) {
  const [busy, setBusy] = useState<string | null>(null);
  const [done, setDone] = useState<Record<string, MacSyncChoice>>({});
  const [error, setError] = useState<string | null>(null);
  const sync = entry?.sync;
  const open = sync ? macSyncOpen(sync).filter((path) => !done[path]) : [];
  const withAgent = sync
    ? sync.withAgent.filter(
        (path) => done[path] !== "mac" && done[path] !== "vm",
      )
    : [];

  async function choose(path: string, choice: MacSyncChoice) {
    if (!sync || !backend) return;
    setBusy(path);
    setError(null);
    try {
      await backend.post(
        "/api/mac-sync/resolve",
        { projectId: sync.projectId, path, choice },
        { timeoutMs: 30000 },
      );
      setDone((current) => ({ ...current, [path]: choice }));
    } catch (failure) {
      setError(failure instanceof Error ? failure.message : String(failure));
    } finally {
      setBusy(null);
    }
  }

  function close() {
    setDone({});
    setError(null);
    onClose();
  }

  const row = (path: string, choices: MacSyncChoice[]) => (
    <Group
      key={path}
      className="mac-sync-conflict"
      justify="space-between"
      wrap="wrap"
      gap="xs"
    >
      <Text size="sm" className="mac-sync-conflict-path" title={path}>
        {path}
      </Text>
      <Group gap={6} wrap="nowrap">
        {CHOICES.filter((item) => choices.includes(item.choice)).map((item) => (
          <Button
            key={item.choice}
            size="compact-xs"
            variant="default"
            title={item.hint}
            loading={busy === path}
            disabled={!!busy && busy !== path}
            onClick={() => void choose(path, item.choice)}
          >
            {item.label}
          </Button>
        ))}
      </Group>
    </Group>
  );

  return (
    <Modal
      opened={!!entry}
      onClose={close}
      title={`Mac folder sync: ${projectName}${entry?.server ? ` (${entry.server})` : ""}`}
      size="lg"
    >
      <Stack gap="sm">
        <Text size="sm" c="dimmed">
          These files changed on your Mac and in the VM. Choose which version to
          keep, or ask the agent to combine them.
        </Text>
        {open.map((path) => row(path, ["mac", "vm", "agent"]))}
        {withAgent.length > 0 && (
          <>
            <Text size="sm" fw={600}>
              The agent combines these files
            </Text>
            {withAgent.map((path) => row(path, ["mac", "vm"]))}
          </>
        )}
        {!open.length && !withAgent.length && (
          <Text size="sm">No files wait for a choice.</Text>
        )}
        {error && (
          <Text size="sm" c="red" role="alert">
            {error}
          </Text>
        )}
      </Stack>
    </Modal>
  );
}
