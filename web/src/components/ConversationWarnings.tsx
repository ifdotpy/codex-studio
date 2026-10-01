import { useState } from "react";
import { ActionIcon, Modal } from "@mantine/core";
import { CircleAlert } from "lucide-react";
import { errorDetails } from "../errorPresentation";
import type { Json, Message } from "../types";
import ErrorDescription from "./ErrorDescription";
import { isNonBlockingWarning } from "./NativeNotice";
import "./conversation-warnings.css";

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
              <h3>Account notice</h3>
              <p>
                <ErrorDescription value={notice.message} role="status" />
              </p>
              {notice.details != null && (
                <pre>{errorDetails(notice.details)}</pre>
              )}
            </article>
          ))}
          {chatWarnings.map((message) => {
            return (
              <article key={`chat:${message.id}`}>
                <h3>Conversation warning</h3>
                <p>
                  <ErrorDescription value={message.text} role="status" />
                </p>
                {message.details != null && (
                  <pre>{errorDetails(message.details)}</pre>
                )}
              </article>
            );
          })}
        </div>
      </Modal>
    </>
  );
}
