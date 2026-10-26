import { MantineProvider } from "@mantine/core";
import { createRoot } from "react-dom/client";
import { useRef, useState } from "react";
import PromptInput from "./PromptInput";

type InputStatePatch = {
  text?: string;
  canSend?: boolean;
  tooLong?: boolean;
};
declare global {
  interface Window {
    inputEvents?: { sends: number; queues: number; changes: number };
    setInputState?: (patch: InputStatePatch) => void;
  }
}

function IsolatedInputHarness() {
  const [value, setValue] = useState("");
  const [canSend, setCanSend] = useState(true);
  const [tooLong, setTooLong] = useState(false);
  const input = useRef<HTMLTextAreaElement>(null);
  const testWindow = window;
  const events = (testWindow.inputEvents ||= {
    sends: 0,
    queues: 0,
    changes: 0,
  });
  testWindow.setInputState = (patch: InputStatePatch) => {
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
  const root = document.getElementById("root");
  if (!root) throw new Error("Missing prompt input harness root");
  createRoot(root).render(<IsolatedInputHarness />);
}
