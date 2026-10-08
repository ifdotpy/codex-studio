import { parseInvitation } from "./pairing";
import { Button, Modal, TextInput, Textarea } from "@mantine/core";
import { useRef, useState } from "react";
import { serveOrigin, type StudioServer } from "./registry";
import { serverCredentialAdapter } from "./transport";
export default function ServerManager({
  opened,
  close,
  servers,
  add,
  remove,
}: {
  opened: boolean;
  close: () => void;
  servers: StudioServer[];
  add: (server: StudioServer) => void;
  remove: (server: StudioServer) => void;
}) {
  const [origin, setOrigin] = useState("");
  const [code, setCode] = useState("");
  const [label, setLabel] = useState("");
  const [error, setError] = useState("");
  const [busy, setBusy] = useState(false);
  const request = useRef<{ origin: string; code: string; id: string } | null>(
    null,
  );
  return (
    <Modal
      opened={opened}
      onClose={close}
      title="Studio servers"
      aria-label="Studio servers"
    >
      <p>
        Open the server settings on the other computer. Create a pairing
        invitation there.
      </p>
      <form
        onSubmit={(event) => {
          event.preventDefault();
          if (busy) return;
          setError("");
          let address: string;
          try {
            address = serveOrigin(origin);
          } catch (failure) {
            setError(String((failure as Error).message));
            return;
          }
          if (!code.trim()) {
            setError("Paste the pairing invitation.");
            return;
          }
          try {
            const invitation = parseInvitation(code.trim(), address);
            const saved = JSON.parse(
              localStorage.getItem("studio-pairing-attempt-v1") || "null",
            );
            if (
              !request.current ||
              request.current.origin !== address ||
              request.current.code !== code.trim()
            ) {
              const id =
                saved?.origin === address &&
                saved?.inviteId === invitation.inviteId
                  ? saved.id
                  : crypto.randomUUID();
              request.current = { origin: address, code: code.trim(), id };
              localStorage.setItem(
                "studio-pairing-attempt-v1",
                JSON.stringify({
                  origin: address,
                  inviteId: invitation.inviteId,
                  id,
                }),
              );
            }
          } catch (failure) {
            setError((failure as Error).message);
            return;
          }
          const attempt = request.current;
          setBusy(true);
          void (async () => {
            const server = await serverCredentialAdapter().pair(
              address,
              attempt.code,
              attempt.id,
            );
            add({ ...server, label: label.trim() || server.label });
            setCode("");
            setOrigin("");
            setLabel("");
            request.current = null;
            localStorage.removeItem("studio-pairing-attempt-v1");
          })()
            .catch((failure: unknown) =>
              setError(
                failure instanceof Error ? failure.message : String(failure),
              ),
            )
            .finally(() => setBusy(false));
        }}
      >
        <TextInput
          label="Server address"
          value={origin}
          onChange={(event) => setOrigin(event.currentTarget.value)}
          required
          disabled={busy}
        />
        <Textarea
          label="Pairing invitation"
          value={code}
          onChange={(event) => setCode(event.currentTarget.value)}
          required
          disabled={busy}
          autoComplete="off"
        />
        <TextInput
          label="Server name"
          value={label}
          onChange={(event) => setLabel(event.currentTarget.value)}
          maxLength={128}
          disabled={busy}
        />
        <Button type="submit" loading={busy}>
          Pair server
        </Button>
      </form>
      {error && <p role="alert">{error}</p>}
      <ul>
        {servers.map((server) => (
          <li key={server.id}>
            <strong>{server.label}</strong>
            <small>{server.origin}</small>
            {server.id !== "local" && (
              <Button
                variant="subtle"
                disabled={busy}
                onClick={() => {
                  setBusy(true);
                  setError("");
                  void serverCredentialAdapter()
                    .forget(server)
                    .then(() => remove(server))
                    .catch((failure: unknown) =>
                      setError(
                        failure instanceof Error
                          ? failure.message
                          : String(failure),
                      ),
                    )
                    .finally(() => setBusy(false));
                }}
              >
                Remove from this UI
              </Button>
            )}
          </li>
        ))}
      </ul>
      <p className="notice">
        Remove stops this UI connection. Revoke access in the server settings to
        remove its permission.
      </p>
    </Modal>
  );
}
