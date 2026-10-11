import { Button, Group, Modal, Stack, Text, TextInput } from "@mantine/core";
import { useEffect, useRef, useState, type FormEvent } from "react";
import {
  answered,
  changeRequest,
  listSecrets,
  nameProblem,
  removeSecret,
  setSecret,
  valueProblem,
  type PendingChange,
} from "./ProjectSecrets";
import type { SidebarBackend } from "./services";

function message(failure: unknown) {
  return failure instanceof Error ? failure.message : String(failure);
}

/**
 * The secrets of a layr project (docs/project-secrets.md): the names with Remove, and a form
 * that adds a name and its value. A stored value never comes back from the server; the typed
 * value stays only in this dialog's state and goes when the dialog closes or the add succeeds.
 */
export default function ProjectSecretsDialog({
  projectId,
  backend,
  title,
  onClose,
}: {
  projectId: string;
  backend: Pick<SidebarBackend, "post">;
  title: string;
  onClose: () => void;
}) {
  const [names, setNames] = useState<string[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState<string | null>(null);
  const [name, setName] = useState("");
  const [value, setValue] = useState("");
  const edits = useRef(0);
  const pending = useRef<PendingChange | null>(null);

  useEffect(() => {
    let live = true;
    listSecrets(backend, projectId).then(
      (list) => live && setNames(list),
      (failure) => live && setError(message(failure)),
    );
    return () => {
      live = false;
    };
  }, [backend, projectId]);

  async function change(
    key: string,
    run: (requestId: string) => Promise<string[]>,
  ) {
    const request = changeRequest(pending.current, key);
    pending.current = request;
    setBusy(key);
    setError(null);
    try {
      setNames(await run(request.requestId));
      pending.current = null;
      return true;
    } catch (failure) {
      if (answered(failure)) pending.current = null;
      setError(message(failure));
      return false;
    } finally {
      setBusy(null);
    }
  }

  async function add(event: FormEvent) {
    event.preventDefault();
    if (nameProblem(name) || valueProblem(value)) return;
    const added = await change(`set:${name}:${edits.current}`, (requestId) =>
      setSecret(backend, projectId, name, value, requestId),
    );
    if (added) {
      edits.current += 1;
      setName("");
      setValue("");
    }
  }

  const nameError = name ? nameProblem(name) : null;
  const valueError = value ? valueProblem(value) : null;
  return (
    <Modal opened onClose={onClose} title={title} size="lg">
      <Stack gap="sm">
        <Text size="sm" c="dimmed">
          Each agent of this project gets these variables when it starts. A
          running agent gets a change when it starts again. Studio never shows a
          stored value.
        </Text>
        {names === null && !error && <Text size="sm">Loading…</Text>}
        {names?.length === 0 && <Text size="sm">No secrets yet.</Text>}
        {!!names?.length && (
          <Stack gap={4} role="list" aria-label="Secret names">
            {names.map((item) => (
              <Group
                key={item}
                role="listitem"
                justify="space-between"
                wrap="nowrap"
                gap="xs"
              >
                <Text size="sm" ff="monospace" truncate title={item}>
                  {item}
                </Text>
                <Button
                  size="compact-xs"
                  variant="default"
                  aria-label={`Remove ${item}`}
                  loading={busy === `remove:${item}`}
                  disabled={!!busy && busy !== `remove:${item}`}
                  onClick={() =>
                    void change(`remove:${item}`, (requestId) =>
                      removeSecret(backend, projectId, item, requestId),
                    )
                  }
                >
                  Remove
                </Button>
              </Group>
            ))}
          </Stack>
        )}
        <form onSubmit={(event) => void add(event)}>
          <Stack gap="xs">
            <TextInput
              label="Name"
              placeholder="API_KEY"
              value={name}
              error={nameError}
              autoComplete="off"
              spellCheck={false}
              onChange={(event) => {
                edits.current += 1;
                setName(event.currentTarget.value);
              }}
            />
            <TextInput
              label="Value"
              type="password"
              value={value}
              error={valueError}
              autoComplete="new-password"
              spellCheck={false}
              onChange={(event) => {
                edits.current += 1;
                setValue(event.currentTarget.value);
              }}
            />
            {!!name && names?.includes(name) && (
              <Text size="xs" c="dimmed">
                Add replaces the value of {name}.
              </Text>
            )}
            <Group justify="flex-end">
              <Button
                type="submit"
                size="xs"
                loading={!!busy?.startsWith("set:")}
                disabled={
                  !!busy || !!nameProblem(name) || !!valueProblem(value)
                }
              >
                Add
              </Button>
            </Group>
          </Stack>
        </form>
        {error && (
          <Text size="sm" c="red" role="alert">
            {error}
          </Text>
        )}
      </Stack>
    </Modal>
  );
}
