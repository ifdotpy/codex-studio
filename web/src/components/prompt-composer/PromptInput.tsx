import { Textarea } from "@mantine/core";
import {
  useCallback,
  useState,
  type KeyboardEvent,
  type RefObject,
} from "react";
import ComposerAutocomplete from "../ComposerAutocomplete";
import { useSkillAutocomplete } from "../useSkillAutocomplete";
import type { DraftReader, DraftWriter } from "./PromptComposer";

export default function PromptInput(p: {
  value: string;
  session: string;
  getDraft: DraftReader;
  setDraft: DraftWriter;
  input: RefObject<HTMLTextAreaElement | null>;
  mobile: boolean;
  shortViewport: boolean;
  managed: boolean;
  canSend: boolean;
  blocked: boolean;
  sending: boolean;
  uploading: boolean;
  draftTooLong: boolean;
  modelCommand: boolean;
  hasAttachments: boolean;
  onChange: (value: string) => void;
  onPasteFiles: (files: File[]) => void;
  onSend: () => void;
  onQueue: () => void;
  onRecallKeyDown: (event: KeyboardEvent<HTMLTextAreaElement>) => boolean;
  skillCatalog: {
    enabled: boolean;
    agentId: string;
    workspace: string;
    account: string;
    cwd: string;
    provider: string;
  };
}) {
  const insertSkill = useCallback(
    (text: string, start: number, end: number) => {
      const current = p.input.current?.value ?? p.getDraft(p.session);
      p.setDraft(
        current.slice(0, start) + text + current.slice(end),
        p.session,
      );
      requestAnimationFrame(() => {
        const element = p.input.current;
        if (!element) return;
        const caret = start + text.length;
        element.focus();
        element.setSelectionRange(caret, caret);
      });
    },
    [p.getDraft, p.input, p.session, p.setDraft],
  );
  const skills = useSkillAutocomplete({
    ...p.skillCatalog,
    draft: p.value,
    input: p.input,
    insert: insertSkill,
  });
  const commandQuery =
    p.managed && /^\/[a-z-]*$/i.test(p.value)
      ? p.value.slice(1).toLowerCase()
      : null;
  const [dismissedCommand, setDismissedCommand] = useState<string | null>(null);
  const commandMatches =
    commandQuery !== null && "rename".includes(commandQuery);
  return (
    <ComposerAutocomplete
      id={commandQuery !== null ? "command-suggestions" : "skill-suggestions"}
      loadingMessage="Loading skills…"
      emptyMessage="No matching skills"
      label={commandQuery !== null ? "Commands" : "Skills"}
      opened={
        (commandQuery !== null &&
          commandQuery !== "rename" &&
          dismissedCommand !== `${p.session}:${p.value}`) ||
        !!skills.range
      }
      resetKey={commandQuery ?? skills.range?.signature}
      options={
        commandQuery !== null
          ? commandMatches
            ? [
                {
                  value: "rename",
                  label: "/rename",
                  description:
                    "Name this chat from its conversation, or add a name.",
                },
              ]
            : []
          : skills.matches.map((skill) => ({
              value: skill.name,
              label: skill.name,
              description: skill.description,
            }))
      }
      loading={commandQuery === null && skills.loading}
      error={
        commandQuery === null && skills.loadError
          ? "Could not load skills"
          : undefined
      }
      warning={
        commandQuery === null && skills.hasErrors
          ? "Some skills could not be loaded"
          : undefined
      }
      onDismiss={() => {
        if (commandQuery !== null)
          setDismissedCommand(`${p.session}:${p.value}`);
        else skills.dismiss();
      }}
      onSelect={(name) => {
        if (commandQuery !== null) {
          if (name === "rename") insertSkill("/rename ", 0, p.value.length);
          return;
        }
        const skill = skills.matches.find((item) => item.name === name);
        if (skill) skills.choose(skill);
      }}
    >
      <Textarea
        onPaste={(event) => {
          const files = Array.from(event.clipboardData.files);
          if (p.managed && files.length) {
            event.preventDefault();
            p.onPasteFiles(files);
          }
        }}
        variant="unstyled"
        autosize
        minRows={1}
        maxRows={p.shortViewport ? 3 : 8}
        id="message"
        ref={p.input}
        aria-label="Message"
        aria-description={
          p.mobile
            ? "Use the send button to send."
            : p.managed
              ? "Enter sends after tool calls. Tab queues the message for after the turn. Shift + Enter adds a new line."
              : "Enter to send. Shift + Enter for a new line."
        }
        placeholder={
          p.blocked
            ? "Start a new chat or open another chat."
            : p.canSend
              ? "What should we work on?"
              : "This session has no live mailbox"
        }
        disabled={!p.canSend}
        value={p.value}
        onChange={(event) => p.onChange(event.currentTarget.value)}
        error={p.draftTooLong}
        aria-describedby={p.draftTooLong ? "draft-length-error" : undefined}
        rows={1}
        onClick={skills.updateRange}
        onBlur={skills.blur}
        onKeyUp={skills.updateRange}
        onSelect={skills.updateRange}
        onKeyDown={(event) => {
          if (
            event.key === "Tab" &&
            p.managed &&
            p.canSend &&
            !p.modelCommand &&
            !event.shiftKey &&
            !event.altKey &&
            !event.ctrlKey &&
            !event.metaKey &&
            !event.repeat &&
            !event.nativeEvent.isComposing &&
            !p.sending &&
            !p.uploading &&
            !p.draftTooLong &&
            (p.value.trim() || p.hasAttachments)
          ) {
            event.preventDefault();
            p.onQueue();
            return;
          }
          if (p.onRecallKeyDown(event)) return;
          if (
            !p.mobile &&
            event.key === "Enter" &&
            !event.shiftKey &&
            !event.nativeEvent.isComposing
          ) {
            event.preventDefault();
            p.onSend();
          }
        }}
      />
    </ComposerAutocomplete>
  );
}
