import { createElement, createRef } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { MantineProvider } from "@mantine/core";
import { describe, expect, it } from "vitest";
import PromptInput, { enterSends } from "./PromptInput";

const key = (fields: Partial<Parameters<typeof enterSends>[0]> = {}) => ({
  key: "Enter",
  shiftKey: false,
  metaKey: false,
  ctrlKey: false,
  isComposing: false,
  ...fields,
});

const textarea = (mobile: boolean) =>
  renderToStaticMarkup(
    createElement(
      MantineProvider,
      null,
      createElement(PromptInput, {
        value: "",
        session: "enter-test",
        getDraft: () => "",
        setDraft: () => {},
        input: createRef<HTMLTextAreaElement>(),
        mobile,
        managed: true,
        canSend: true,
        blocked: false,
        sending: false,
        uploading: false,
        draftTooLong: false,
        modelCommand: false,
        hasAttachments: false,
        onChange: () => {},
        onPasteFiles: () => {},
        onSend: () => {},
        onQueue: () => {},
        onRecallKeyDown: () => false,
        skillCatalog: {
          enabled: false,
          agentId: "agent",
          workspace: "workspace",
          account: "default",
          cwd: "/workspace",
          provider: "codex",
        },
      }),
    ),
  ).match(/<textarea[^>]*>/)?.[0] ?? "";

describe("composer Enter", () => {
  it("sends on a fine pointer and adds a line with Shift", () => {
    expect(enterSends(key(), false)).toBe(true);
    expect(enterSends(key({ shiftKey: true }), false)).toBe(false);
  });
  it("adds a line on a touch composer", () => {
    expect(enterSends(key(), true)).toBe(false);
    expect(enterSends(key({ shiftKey: true }), true)).toBe(false);
  });
  it("sends with Cmd or Ctrl on both pointer types", () => {
    for (const touch of [false, true]) {
      expect(enterSends(key({ metaKey: true }), touch)).toBe(true);
      expect(enterSends(key({ ctrlKey: true }), touch)).toBe(true);
    }
  });
  it("never sends with Shift, during composition or for other keys", () => {
    for (const touch of [false, true]) {
      expect(enterSends(key({ shiftKey: true, metaKey: true }), touch)).toBe(
        false,
      );
      expect(enterSends(key({ shiftKey: true, ctrlKey: true }), touch)).toBe(
        false,
      );
      expect(enterSends(key({ isComposing: true, metaKey: true }), touch)).toBe(
        false,
      );
      expect(enterSends(key({ key: "a", metaKey: true }), touch)).toBe(false);
    }
  });
  it("shows the return key and the send keys only on a touch composer", () => {
    const touch = textarea(true);
    expect(touch).toContain('enterKeyHint="enter"');
    expect(touch).toContain("⌘/Ctrl + Enter to send");
    expect(textarea(false)).not.toContain("enterKeyHint");
  });
});
