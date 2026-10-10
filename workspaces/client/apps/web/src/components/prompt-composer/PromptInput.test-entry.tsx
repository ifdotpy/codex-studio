import { MantineProvider } from "@mantine/core";
import { createRoot } from "react-dom/client";
import { useRef, useState } from "react";
import PromptInput from "./PromptInput";

function IsolatedInputHarness() {
  const [value, setValue] = useState("");
  const [canSend, setCanSend] = useState(true);
  const [tooLong, setTooLong] = useState(false);
  const input = useRef<HTMLTextAreaElement>(null);
  const testWindow = window as any;
  const events = (testWindow.inputEvents ||= {
    sends: 0,
    queues: 0,
    changes: 0,
  });
  testWindow.setInputState = (patch: {
    text?: string;
    canSend?: boolean;
    tooLong?: boolean;
  }) => {
    if (patch.text !== undefined) setValue(patch.text);
    if (patch.canSend !== undefined) setCanSend(patch.canSend);
    if (patch.tooLong !== undefined) setTooLong(patch.tooLong);
  };
  return (
    <MantineProvider>
      <PromptInput
        value={value}
        session="isolated-test"
        getDraft={() => value}
        setDraft={setValue}
        input={input}
        mobile={false}
        managed
        canSend={canSend}
        blocked={false}
        sending={false}
        uploading={false}
        draftTooLong={tooLong}
        modelCommand={false}
        hasAttachments={false}
        onChange={(next) => {
          events.changes++;
          setValue(next);
        }}
        onPasteFiles={() => {}}
        onSend={() => events.sends++}
        onQueue={() => events.queues++}
        onRecallKeyDown={() => false}
        skillCatalog={{
          enabled: true,
          agentId: "isolated-agent",
          workspace: "isolated-workspace",
          account: "default",
          cwd: "/workspace",
          provider: "codex",
        }}
      />
    </MantineProvider>
  );
}

export function mount() {
  createRoot(document.getElementById("root")!).render(<IsolatedInputHarness />);
}
