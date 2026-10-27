import { Button, Modal, Stack, TextInput } from "@mantine/core";
import { useEffect, useState } from "react";
import { layerZIndex } from "../theme";
import type { useAccounts } from "./Accounts";
import AccountSignIn from "./AccountSignIn";
import ClaudeAddSignIn from "./ClaudeAddSignIn";
import {
  PROVIDER_KINDS,
  PROVIDER_LABELS,
  ProviderKind,
} from "../generated/providerKind.ts";

export const ACCOUNT_SIGN_IN_HINTS: Record<ProviderKind, string> = {
  [ProviderKind.Codex]: "ChatGPT subscription, one-time code",
  [ProviderKind.Claude]: "Claude subscription, link + paste code",
};

export type AddAccountIntent = {
  provider?: ProviderKind;
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
  const [provider, setProvider] = useState<ProviderKind | "">("");
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
  const close = () => {
    setFlow(false);
    onClose();
  };
  return (
    <>
      <Modal
        opened={opened && !flow}
        zIndex={layerZIndex.modalAbovePopover}
        onClose={close}
        title="Add account"
        centered
      >
        <Stack gap="md">
          <div
            className="account-provider-choices"
            role="group"
            aria-label="Account type"
          >
            {PROVIDER_KINDS.map((kind) => (
              <button
                key={kind}
                type="button"
                className={`account-provider-choice${provider === kind ? " selected" : ""}`}
                aria-pressed={provider === kind}
                onClick={() => setProvider(kind)}
              >
                <strong>{PROVIDER_LABELS[kind]}</strong>
                <span>{ACCOUNT_SIGN_IN_HINTS[kind]}</span>
              </button>
            ))}
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
            <Button variant="default" onClick={close}>
              Cancel
            </Button>
            <Button disabled={!provider} onClick={() => setFlow(true)}>
              Continue
            </Button>
          </div>
        </Stack>
      </Modal>
      {opened && flow && provider === ProviderKind.Codex && (
        <Modal
          opened={opened}
          zIndex={layerZIndex.modalAbovePopover}
          onClose={close}
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
      {opened && flow && provider === ProviderKind.Claude && (
        <ClaudeAddSignIn
          label={label || "Claude Code"}
          email={email}
          serverLabel={serverLabel}
          scope={state.scope || "local"}
          opened={opened}
          onClose={close}
          onReady={state.refresh}
        />
      )}
    </>
  );
}
