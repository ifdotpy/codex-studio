import { Button, Modal, Stack, TextInput } from "@mantine/core";
import { useEffect, useState } from "react";
import type { useAccounts } from "./Accounts";
import AccountSignIn from "./AccountSignIn";
import ClaudeAddSignIn from "./ClaudeAddSignIn";

export type AddAccountIntent = {
  provider?: "codex" | "claude";
  email?: string | null;
  label?: string;
  serverLabel?: string;
};

export default function AccountAddDialog({
  opened,
  intent,
  state,
  onClose,
}: {
  opened: boolean;
  intent: AddAccountIntent | null;
  state: ReturnType<typeof useAccounts>;
  onClose: () => void;
}) {
  const [provider, setProvider] = useState<"codex" | "claude" | "">("");
  const [label, setLabel] = useState("");
  const [email, setEmail] = useState("");
  const [flow, setFlow] = useState(false);
  useEffect(() => {
    if (!opened) return;
    setProvider(intent?.provider || "");
    setLabel(intent?.label || "");
    setEmail(intent?.email || "");
    setFlow(!!intent?.provider);
  }, [intent, opened]);
  const serverLabel = intent?.serverLabel || "This computer";
  return (
    <>
      <Modal
        opened={opened && !flow}
        onClose={onClose}
        title="Add account"
        centered
      >
        <Stack gap="md">
          <div
            className="account-provider-choices"
            role="group"
            aria-label="Account type"
          >
            <button
              type="button"
              className={`account-provider-choice${provider === "codex" ? " selected" : ""}`}
              aria-pressed={provider === "codex"}
              onClick={() => setProvider("codex")}
            >
              <strong>Codex</strong>
              <span>ChatGPT subscription, one-time code</span>
            </button>
            <button
              type="button"
              className={`account-provider-choice${provider === "claude" ? " selected" : ""}`}
              aria-pressed={provider === "claude"}
              onClick={() => setProvider("claude")}
            >
              <strong>Claude</strong>
              <span>Claude subscription, link + paste code</span>
            </button>
          </div>
          <TextInput label="Server" value={serverLabel} readOnly />
          <TextInput
            label="Name"
            value={label}
            maxLength={32}
            onChange={(event) => setLabel(event.currentTarget.value)}
          />
          <TextInput
            label="Email (optional)"
            type="email"
            value={email}
            onChange={(event) => setEmail(event.currentTarget.value)}
            description="Studio checks that you sign in with this account."
          />
          <div className="account-add-dialog-actions">
            <Button variant="default" onClick={onClose}>
              Cancel
            </Button>
            <Button
              color="indigo"
              disabled={!provider}
              onClick={() => setFlow(true)}
            >
              Continue
            </Button>
          </div>
        </Stack>
      </Modal>
      {flow && provider === "codex" && (
        <Modal
          opened
          onClose={onClose}
          title={`Sign in to Codex · ${label || "Codex"}`}
          centered
        >
          <Stack gap="sm">
            <TextInput label="Server" value={serverLabel} readOnly />
            <p>
              Open auth.openai.com/codex/device. Use a private window if your
              browser has another ChatGPT account signed in.
            </p>
            <AccountSignIn state={state} emailHint={email} label={label} />
          </Stack>
        </Modal>
      )}
      {flow && provider === "claude" && (
        <ClaudeAddSignIn
          label={label || "Claude Code"}
          email={email}
          serverLabel={serverLabel}
          onClose={onClose}
          onReady={state.refresh}
        />
      )}
    </>
  );
}
