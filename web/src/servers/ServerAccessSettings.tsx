import { Button, Textarea, TextInput } from "@mantine/core";
import { useEffect, useRef, useState } from "react";
import { serverAccess, errorText } from "../api";
import { serverLocalStorage as localStorage } from "./storage";
import type { ServerAccessState, ServerAccessRequest } from "./accessContract";
export default function ServerAccessSettings({ active }: { active: boolean }) {
  const [state, setState] = useState<ServerAccessState | null>(null);
  const [invitation, setInvitation] = useState("");
  const [label, setLabel] = useState("Studio server");
  const [error, setError] = useState("");
  const [busy, setBusy] = useState(false);
  const pending = useRef<ServerAccessRequest | null>(null);
  const load = async () => {
    const result = await serverAccess("GET");
    if (!("identity" in result))
      throw new Error("The server access response is invalid.");
    setState(result);
  };
  useEffect(() => {
    if (!active) return;
    void load().catch((failure: unknown) => setError(errorText(failure)));
    try {
      pending.current = JSON.parse(
        localStorage.getItem("studio-server-access-pending") || "null",
      );
    } catch {}
  }, [active]);
  if (!active) return null;
  const run = (next: ServerAccessRequest) => {
    if (busy) return;
    if (
      pending.current &&
      JSON.stringify({ ...pending.current, requestId: "" }) !==
        JSON.stringify({ ...next, requestId: "" })
    ) {
      setError(
        "Retry the saved server access request before you start another request.",
      );
      return;
    }
    const body = pending.current || next;
    try {
      localStorage.setItem(
        "studio-server-access-pending",
        JSON.stringify(body),
      );
      pending.current = body;
    } catch {
      setError("The request could not be saved. No server access changed.");
      return;
    }
    setBusy(true);
    setError("");
    void serverAccess("POST", body)
      .then(async (result) => {
        if ("invitation" in result)
          setInvitation(JSON.stringify(result.invitation, null, 2));
        pending.current = null;
        localStorage.removeItem("studio-server-access-pending");
        await load();
      })
      .catch((failure: unknown) => setError(errorText(failure)))
      .finally(() => setBusy(false));
  };
  return (
    <section aria-label="Server access">
      <p>Pair your other Studio UI with this server through Tailscale Serve.</p>
      {state && (
        <p>
          {state.identity.label} ({state.identity.origin})
        </p>
      )}
      <TextInput
        label="Server name"
        value={label}
        maxLength={80}
        onChange={(event) => setLabel(event.currentTarget.value)}
      />
      <Button
        disabled={busy}
        onClick={() =>
          run({
            action: "create_invite",
            label,
            requestId: crypto.randomUUID(),
          })
        }
      >
        Create pairing invitation
      </Button>
      {pending.current && (
        <Button
          disabled={busy}
          variant="subtle"
          onClick={() => run(pending.current!)}
        >
          Retry saved request
        </Button>
      )}
      {invitation && (
        <Textarea
          label="Pairing invitation"
          value={invitation}
          readOnly
          autosize
          minRows={6}
          description="Copy the whole invitation into the other UI. The invitation grants full server access."
        />
      )}
      {error && <p role="alert">{error}</p>}
      {state?.clients.map((client) => (
        <div key={client.clientId}>
          <strong>{client.label}</strong>
          <Button
            disabled={busy || client.status === "revoked"}
            variant="subtle"
            onClick={() =>
              run({
                action: "revoke",
                clientId: client.clientId,
                requestId: crypto.randomUUID(),
              })
            }
          >
            Revoke access
          </Button>
        </div>
      ))}
    </section>
  );
}
