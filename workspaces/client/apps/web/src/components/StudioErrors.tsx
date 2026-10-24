import { useServerApi } from "../servers/ServerServicesContext";
import { useCallback, useEffect, useState } from "react";
import { Badge, Button, Group, Modal, Stack, Text } from "@mantine/core";
import { TriangleAlert } from "lucide-react";
import { errorText, type GetResult } from "../api";

export type StudioErrorItem = GetResult<"/api/studio-errors">["items"][number];

/** How often the sidebar count is read again while the app is open. Unit: ms. */
const REFRESH_MS = 120_000;

/** The label of a row's safe action. */
export function actionLabel(item: StudioErrorItem) {
  return item.action === "resume" ? "Resume" : "Open";
}

function when(at: number) {
  return at > 0 ? new Date(at * 1000).toLocaleString() : "";
}

/** The rows with their reason and one safe action each. */
export function StudioErrorList({
  items,
  busy,
  onAction,
  onDismiss,
}: {
  items: StudioErrorItem[];
  busy: string | null;
  onAction: (item: StudioErrorItem) => void;
  onDismiss: (item: StudioErrorItem) => void;
}) {
  if (items.length === 0) {
    return (
      <Text size="sm">No agent work ended because of a Studio error.</Text>
    );
  }
  return (
    <Stack gap="sm" aria-label="Studio errors">
      {items.map((item) => (
        <div className="studio-error-row" key={item.id} data-kind={item.kind}>
          <Text fw={600} size="sm">
            {item.agentName}
          </Text>
          <Text size="sm">{item.reason}</Text>
          {item.subject && (
            <Text size="xs" c="dimmed" lineClamp={2}>
              {item.subject}
            </Text>
          )}
          {item.detail && (
            <Text size="xs" c="dimmed" lineClamp={2}>
              {item.detail}
            </Text>
          )}
          <Group gap="xs" mt={4}>
            {when(item.at) && (
              <Text size="xs" c="dimmed">
                {when(item.at)}
              </Text>
            )}
            <Button
              size="xs"
              variant="light"
              loading={busy === item.id}
              disabled={busy !== null && busy !== item.id}
              onClick={() => onAction(item)}
            >
              {actionLabel(item)}
            </Button>
            <Button
              size="xs"
              variant="subtle"
              disabled={busy !== null}
              onClick={() => onDismiss(item)}
            >
              Dismiss
            </Button>
          </Group>
        </div>
      ))}
    </Stack>
  );
}

/**
 * The sidebar entry for agent work that ended because of a Studio error. It
 * shows the count, and its list offers only safe actions: Resume runs
 * connection recovery, which never sends uncertain input again, and Open shows
 * the agent. Nothing here runs a command or sends input.
 */
export function StudioErrorsButton({ open }: { open: (id: string) => void }) {
  const { get, post } = useServerApi();
  const [items, setItems] = useState<StudioErrorItem[]>([]);
  const [opened, setOpened] = useState(false);
  const [busy, setBusy] = useState<string | null>(null);
  const [error, setError] = useState("");
  const refresh = useCallback(() => {
    void get("/api/studio-errors").then(
      (value) => setItems(value.items),
      () => undefined,
    );
  }, [get]);
  useEffect(() => {
    refresh();
    const timer = window.setInterval(refresh, REFRESH_MS);
    return () => window.clearInterval(timer);
  }, [refresh]);
  if (items.length === 0 && !opened) return null;
  const act = (item: StudioErrorItem) => {
    if (item.action !== "resume") {
      setOpened(false);
      open(item.agentId);
      return;
    }
    setBusy(item.id);
    setError("");
    void post("/api/connection-recovery", { id: item.agentId })
      .then(
        () => refresh(),
        (failure) => setError(errorText(failure)),
      )
      .finally(() => setBusy(null));
  };
  const dismiss = (item: StudioErrorItem) => {
    setBusy(item.id);
    setError("");
    void post("/api/studio-errors/dismiss", { id: item.id })
      .then(
        (value) => setItems(value.items),
        (failure) => setError(errorText(failure)),
      )
      .finally(() => setBusy(null));
  };
  return (
    <>
      <Button
        className="sidebar-nav-button sidebar-studio-errors"
        title={`${items.length} agent work items ended because of a Studio error`}
        leftSection={<TriangleAlert size={15} />}
        onClick={() => {
          setOpened(true);
          refresh();
        }}
      >
        Studio errors{" "}
        {items.length > 0 && <Badge size="sm">{items.length}</Badge>}
      </Button>
      <Modal
        opened={opened}
        onClose={() => setOpened(false)}
        title="Agent work that Studio interrupted"
        size="lg"
      >
        {error && (
          <Text c="red" size="sm" mb="sm">
            {error}
          </Text>
        )}
        <StudioErrorList
          items={items}
          busy={busy}
          onAction={act}
          onDismiss={dismiss}
        />
      </Modal>
    </>
  );
}
