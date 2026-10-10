import { useState } from "react";
import { ActionIcon, Modal } from "@mantine/core";
import { CircleAlert } from "lucide-react";
import { displayError, errorDetails } from "../errorPresentation";
import type { Json, Message } from "../types";
import ErrorDescription from "./ErrorDescription";
import { isNonBlockingWarning } from "./NativeNotice";
import "./conversation-warnings.css";

function warningTitle(value: unknown, fallback: string) {
  const text = displayError(value);
  if (/\b(version|baseline)\b/i.test(text)) {
    const provider = /\bClaude\b/i.test(text)
      ? "Claude"
      : /\bCodex\b/i.test(text)
        ? "Codex"
        : "Provider";
    return `${provider} version needs attention`;
  }
  const first = text.split(/\n|[.!?](?:\s|$)/)[0].trim();
  if (!first) return fallback;
  const words = first.split(/\s+/);
  return words.length > 8 ? `${words.slice(0, 8).join(" ")}…` : first;
}

export default function ConversationWarnings({
  scope,
  notices,
  accountKey,
  messages,
}: {
  scope: string;
  notices?: Json[];
  accountKey: string;
  messages: Message[];
}) {
  const [openScope, setOpenScope] = useState<string | null>(null);
  const accountNotices =
    notices?.filter((notice) => notice.accountKey === accountKey) || [];
  const chatWarnings = messages.filter(isNonBlockingWarning);
  const count = accountNotices.length + chatWarnings.length;
  const opened = openScope === scope && count > 0;

  return (
    <>
      {count > 0 && (
        <ActionIcon
          className="conversation-warning-trigger"
          variant="subtle"
          color="orange"
          aria-label="Warnings"
          title="Warnings"
          onClick={() => setOpenScope(scope)}
        >
          <CircleAlert size={18} aria-hidden="true" />
        </ActionIcon>
      )}
      <Modal
        key={scope}
        opened={opened}
        onClose={() => setOpenScope(null)}
        title="Warnings"
        size="lg"
        centered
        className="conversation-warnings-modal"
      >
        <div className="conversation-warning-list">
          {accountNotices.map((notice) => (
            <article key={`account:${notice.id}`}>
              <h3>{warningTitle(notice.message, "Account notice")}</h3>
              <p>
                <ErrorDescription value={notice.message} role="status" />
              </p>
              {notice.details != null && (
                <details>
                  <summary>Details</summary>
                  <pre>{errorDetails(notice.details)}</pre>
                </details>
              )}
            </article>
          ))}
          {chatWarnings.map((message) => {
            return (
              <article key={`chat:${message.id}`}>
                <h3>{warningTitle(message.text, "Conversation warning")}</h3>
                <p>
                  <ErrorDescription value={message.text} role="status" />
                </p>
                {"details" in message && message.details != null && (
                  <details>
                    <summary>Details</summary>
                    <pre>{errorDetails(message.details)}</pre>
                  </details>
                )}
              </article>
            );
          })}
        </div>
      </Modal>
    </>
  );
}
