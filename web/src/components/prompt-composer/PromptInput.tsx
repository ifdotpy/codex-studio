import { Textarea } from "@mantine/core";
import {
  useCallback,
  useLayoutEffect,
  useRef,
  useState,
  type KeyboardEvent,
  type RefObject,
} from "react";
import ComposerAutocomplete from "../ComposerAutocomplete";
import { useSkillAutocomplete } from "../useSkillAutocomplete";
import type { DraftReader, DraftWriter } from "./PromptComposer";

const INPUT_ROWS = 2;
const MAX_COMPOSER_VISIBLE_RATIO = 0.6;
const MIN_TRANSCRIPT_HEIGHT = 32;

export default function PromptInput(p: {
  value: string;
  session: string;
  getDraft: DraftReader;
  setDraft: DraftWriter;
  input: RefObject<HTMLTextAreaElement | null>;
  mobile: boolean;
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
  const lastAppliedLayout = useRef<{
    inputHeight: string;
    composerHeight: number;
  } | null>(null);
  const resizeInput = useCallback(() => {
    const element = p.input.current;
    const composer = element?.closest<HTMLElement>("#composer");
    if (!element || !composer) return;

    const transcript = composer
      .closest<HTMLElement>("#conversation")
      ?.querySelector<HTMLElement>("#messages");
    const transcriptScrollTop = transcript?.scrollTop;
    const restoreTranscriptScroll = () => {
      if (
        transcript &&
        transcriptScrollTop !== undefined &&
        transcript.scrollTop !== transcriptScrollTop
      )
        transcript.scrollTop = transcriptScrollTop;
    };
    const previousHeight = element.style.height;
    if (element.scrollHeight <= element.clientHeight) {
      if (previousHeight) {
        element.style.removeProperty("height");
        restoreTranscriptScroll();
        lastAppliedLayout.current = {
          inputHeight: "",
          composerHeight: composer.getBoundingClientRect().height,
        };
      }
      return;
    }

    const style = getComputedStyle(element);
    const pixels = (value: string) => Number.parseFloat(value) || 0;
    const lineHeight = pixels(style.lineHeight) || pixels(style.fontSize) * 1.2;
    const minimumHeight = Math.max(
      pixels(style.minHeight),
      lineHeight * element.rows +
        pixels(style.paddingTop) +
        pixels(style.paddingBottom) +
        pixels(style.borderTopWidth) +
        pixels(style.borderBottomWidth),
    );
    const composerBounds = composer.getBoundingClientRect();
    const inputBounds = element.getBoundingClientRect();
    const composerChrome = composerBounds.height - inputBounds.height;
    const visibleHeight = window.visualViewport?.height || window.innerHeight;
    const conversation = composer.closest<HTMLElement>("#conversation");
    const footer = conversation?.querySelector<HTMLElement>(".usage-footer");
    let spaceAvailable = minimumHeight;
    let fixedVerticalSpace = 0;
    if (conversation && footer && transcript) {
      const conversationStyle = getComputedStyle(conversation);
      const composerStyle = getComputedStyle(composer);
      const footerStyle = getComputedStyle(footer);
      // The requests panel lives inside the transcript scroller; the transcript
      // minimum reserves room for it without treating its content height as fixed.
      fixedVerticalSpace =
        footer.getBoundingClientRect().height +
        composerChrome +
        pixels(composerStyle.marginTop) +
        pixels(composerStyle.marginBottom) +
        pixels(footerStyle.marginTop) +
        pixels(footerStyle.marginBottom) +
        pixels(conversationStyle.paddingTop) +
        pixels(conversationStyle.paddingBottom) +
        MIN_TRANSCRIPT_HEIGHT;
      const conversationBounds = conversation.getBoundingClientRect();
      // The conversation is positioned below the fixed workspace header, so
      // its measured height already reserves the header within the viewport.
      spaceAvailable = Math.max(
        minimumHeight,
        conversationBounds.height - fixedVerticalSpace,
      );
    }
    const borderHeight =
      (Number.parseFloat(style.borderTopWidth) || 0) +
      (Number.parseFloat(style.borderBottomWidth) || 0);
    const contentHeight =
      element.scrollHeight +
      (style.boxSizing === "border-box" ? borderHeight : 0);
    const maximumHeight = Math.max(
      minimumHeight,
      Math.min(
        visibleHeight * MAX_COMPOSER_VISIBLE_RATIO - composerChrome,
        spaceAvailable,
      ),
    );
    const nextHeight = Math.min(
      Math.max(contentHeight, minimumHeight),
      maximumHeight,
    );
    const nextInlineHeight =
      nextHeight <= minimumHeight + 0.5 ? "" : `${nextHeight}px`;
    if (previousHeight !== nextInlineHeight) {
      if (nextInlineHeight) element.style.height = nextInlineHeight;
      else element.style.removeProperty("height");
    }
    lastAppliedLayout.current = {
      inputHeight: element.style.height,
      composerHeight: composer.getBoundingClientRect().height,
    };
    restoreTranscriptScroll();
  }, [p.input]);

  useLayoutEffect(() => {
    resizeInput();
  }, [p.value, resizeInput]);

  useLayoutEffect(() => {
    const element = p.input.current;
    const composer = element?.closest<HTMLElement>("#composer");
    if (!element || !composer) return;
    const viewport = window.visualViewport;
    let resizeFrame = 0;
    const scheduleResize = () => {
      cancelAnimationFrame(resizeFrame);
      resizeFrame = requestAnimationFrame(resizeInput);
    };
    const observer = new ResizeObserver(() => {
      const expected = lastAppliedLayout.current;
      if (
        expected &&
        element.style.height === expected.inputHeight &&
        Math.abs(
          composer.getBoundingClientRect().height - expected.composerHeight,
        ) <= 0.5
      )
        return;
      resizeInput();
    });
    observer.observe(composer);
    viewport?.addEventListener("resize", scheduleResize);
    viewport?.addEventListener("scroll", scheduleResize);
    window.addEventListener("resize", scheduleResize);
    return () => {
      observer.disconnect();
      cancelAnimationFrame(resizeFrame);
      viewport?.removeEventListener("resize", scheduleResize);
      viewport?.removeEventListener("scroll", scheduleResize);
      window.removeEventListener("resize", scheduleResize);
    };
  }, [p.input, resizeInput]);

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
        !p.modelCommand &&
        ((commandQuery !== null &&
          commandQuery !== "rename" &&
          dismissedCommand !== `${p.session}:${p.value}`) ||
          !!skills.range)
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
        onChange={(event) => {
          p.onChange(event.currentTarget.value);
          resizeInput();
        }}
        error={p.draftTooLong}
        aria-describedby={p.draftTooLong ? "draft-length-error" : undefined}
        rows={INPUT_ROWS}
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
