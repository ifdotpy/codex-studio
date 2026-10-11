import { Button, Group, Modal, Stack, Text, Textarea } from "@mantine/core";
import { useState } from "react";
import { errorText } from "../../api";
import type { SidebarBackend } from "./services";
import type { VaultEntry } from "./Vault";

/**
 * Keep a vault key on this Mac from the paper words, so that the Desk disk and Pull work
 * without a terminal. The words go to this Mac's server and from there to layr-desk on stdin;
 * the dialog forgets them when it closes.
 */
export default function VaultUnlock({
  entry,
  backend,
  onClose,
  onDone,
}: {
  entry: VaultEntry;
  backend: Pick<SidebarBackend, "post">;
  onClose: () => void;
  onDone: (text: string) => void;
}) {
  const [words, setWords] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const name = entry.vault.prefix ?? "the vault";
  const submit = async () => {
    setBusy(true);
    setError(null);
    try {
      await backend.post(
        "/api/vault/unlock",
        { projectId: entry.vault.projectId, words },
        { timeoutMs: 30000 },
      );
      setWords("");
      onDone(`${name}: this Mac keeps the vault key in its Keychain`);
      onClose();
    } catch (failure) {
      setError(errorText(failure));
    } finally {
      setBusy(false);
    }
  };
  return (
    <Modal opened onClose={onClose} title={`Unlock ${name} on this Mac`}>
      <Stack>
        <Text size="sm">
          Enter the 24 words of the vault&apos;s paper key. This Mac keeps the
          vault key in its Keychain, so the Desk disk and Pull work here.
        </Text>
        <Textarea
          aria-label="Paper key"
          autosize
          minRows={3}
          autoComplete="off"
          spellCheck={false}
          value={words}
          onChange={(event) => setWords(event.currentTarget.value)}
        />
        {error && (
          <Text size="sm" c="red" role="alert">
            {error}
          </Text>
        )}
        <Group justify="flex-end">
          <Button variant="default" onClick={onClose}>
            Cancel
          </Button>
          <Button
            loading={busy}
            disabled={!words.trim()}
            onClick={() => void submit()}
          >
            Unlock
          </Button>
        </Group>
      </Stack>
    </Modal>
  );
}
