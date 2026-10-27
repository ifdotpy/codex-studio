import { useState } from "react";
import { ActionIcon, Badge, Modal, Tabs, Text } from "@mantine/core";
import { CircleAlert, Info } from "lucide-react";
import { useAppLog } from "../appLog";
import { displayError, errorDetails } from "../errorPresentation";
import type { components } from "../generated/api";
import type { Message } from "../types";
import ErrorDescription from "./ErrorDescription";
import { isNonBlockingWarning } from "./NativeNotice";
import "./conversation-warnings.css";
import {
  LEVELS,
  LEVEL_COLORS,
  StudioErrorList,
  useStudioErrors,
} from "./StudioErrors";

type NativeNotice =
  | components["schemas"]["NativeNoticeRecord"]
  | components["schemas"]["ProviderVersionWarningDto"];

function isInfoNotice(
  notice: NativeNotice,
): notice is components["schemas"]["ProviderVersionWarningDto"] {
  return "level" in notice && notice.level === "info";
}

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
  const first = text.split(/\n|[.!?](?:\s|$)/)[0]?.trim();
  if (!first) return fallback;
  const words = first.split(/\s+/);
  return words.length > 8 ? `${words.slice(0, 8).join(" ")}…` : first;
}

export default function ConversationWarnings({
  scope,
  notices,
  accountKey,
  messages,
  openAgent = () => {},
}: {
  scope: string;
  notices?: NativeNotice[];
  accountKey: string;
  messages: Message[];
  openAgent?: (id: string) => void;
}) {
  const errors = useStudioErrors();
  const logs = useAppLog();
  const [openScope, setOpenScope] = useState<string | null>(null);
  const accountNotices =
    notices?.filter((notice) => notice.accountKey === accountKey) || [];
  const chatWarnings = messages.filter(isNonBlockingWarning);
  const highest = LEVELS.find(
    (level) =>
      errors.items.some((item) => item.level === level) ||
      (level === "warning" &&
        (accountNotices.some((notice) => !isInfoNotice(notice)) ||
          chatWarnings.length > 0)) ||
      (level === "log" && logs.length > 0),
  );
  const hasWarning = highest !== undefined && highest !== "log";
  const noticeCount = accountNotices.length + chatWarnings.length;
  const title =
    hasWarning || (noticeCount === 0 && logs.length === 0)
      ? "Warnings"
      : "Notices";
  const count = noticeCount + errors.items.length + logs.length;
  const opened = openScope === scope;

  return (
    <>
      <ActionIcon
        className={`conversation-warning-trigger${hasWarning ? "" : " conversation-info-trigger"}`}
        variant="subtle"
        color={highest ? LEVEL_COLORS[highest] : "blue"}
        aria-label={title}
        title={`${title} (${count})`}
        onClick={() => {
          setOpenScope(scope);
        }}
      >
        {hasWarning ? (
          <CircleAlert size={18} aria-hidden="true" />
        ) : (
          <Info size={18} aria-hidden="true" />
        )}
        {count > 0 && (
          <Badge
            className="conversation-warning-count"
            size="xs"
            aria-label={`${count} notices and app errors`}
          >
            {count}
          </Badge>
        )}
      </ActionIcon>
      <Modal
        key={scope}
        opened={opened}
        onClose={() => setOpenScope(null)}
        title={title}
        size="lg"
        centered
        className="conversation-warnings-modal"
      >
        <Tabs
          defaultValue={noticeCount ? "notices" : "errors"}
          keepMounted={false}
        >
          <Tabs.List>
            <Tabs.Tab value="notices">
              Notices{noticeCount > 0 && ` (${noticeCount})`}
            </Tabs.Tab>
            <Tabs.Tab value="errors">
              App errors
              {errors.items.length + logs.length > 0 &&
                ` (${errors.items.length + logs.length})`}
            </Tabs.Tab>
          </Tabs.List>
          <Tabs.Panel value="notices" pt="md">
            <div className="conversation-warning-list">
              {noticeCount === 0 && <Text size="sm">No notices.</Text>}
              {accountNotices.map((notice) => (
                <article key={`account:${notice.id}`}>
                  <h3>
                    {isInfoNotice(notice)
                      ? notice.id.startsWith("provider-update:")
                        ? "Provider update available"
                        : "Information"
                      : warningTitle(notice.message, "Account notice")}
                  </h3>
                  <p>
                    <ErrorDescription value={notice.message} role="status" />
                  </p>
                  {"details" in notice && notice.details != null && (
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
                    <h3>
                      {warningTitle(message.text, "Conversation warning")}
                    </h3>
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
          </Tabs.Panel>
          <Tabs.Panel value="errors" pt="md">
            {errors.error && (
              <Text c="red" size="sm" mb="sm" role="alert">
                {errors.error}
              </Text>
            )}
            {errors.notice && (
              <Text size="sm" mb="sm" role="status">
                {errors.notice}
              </Text>
            )}
            <StudioErrorList
              items={errors.items}
              selected={errors.selected}
              busy={errors.busy}
              onSelect={errors.select}
              onSelectAll={errors.selectAll}
              onDelete={() => void errors.remove()}
              onAction={(item) => {
                if (item.action === "resume") void errors.resume(item);
                else if (item.action === "check") {
                  setOpenScope(null);
                  openAgent(item.agentId);
                }
              }}
            />
          </Tabs.Panel>
        </Tabs>
      </Modal>
    </>
  );
}
