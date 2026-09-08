import type { Agent, Message, Json } from "../types";
import { agentErrorLabel } from "../types";
import { Button } from "@mantine/core";
import { nativeErrorView, nativeThreadError } from "../nativeErrors";
import "./native-notice.css";

function Guidance({ error }: { error: ReturnType<typeof nativeErrorView> }) {
  return (
    <>
      {error.hint && <small>{error.hint}</small>}
      {!!error.links.length && (
        <div className="native-error-links">
          {error.links.map((link) => (
            <a
              key={link.href}
              href={link.href}
              target="_blank"
              rel="noreferrer"
            >
              {link.label}
            </a>
          ))}
        </div>
      )}
    </>
  );
}

export function NativeError({
  agent,
  planType,
  openLimits,
  newChat,
  chooseChat,
}: {
  agent: Agent;
  planType?: string;
  openLimits?: () => void;
  newChat?: () => void;
  chooseChat?: () => void;
}) {
  const blocked = nativeThreadError(agent);
  const error = nativeErrorView(blocked || agent.error, planType);
  return (
    <div className={`native-error ${error.severity}`} role="alert">
      <span>
        {error.title || (blocked ? error.message : agentErrorLabel(agent))}
      </span>
      <Guidance error={error} />
      {!!blocked && (
        <div className="native-error-actions">
          {newChat && (
            <Button size="compact-xs" onClick={newChat}>
              New chat
            </Button>
          )}
          {chooseChat && (
            <Button size="compact-xs" onClick={chooseChat}>
              Choose another chat
            </Button>
          )}
        </div>
      )}
      {openLimits &&
        ["usageLimitExceeded", "rateLimitExceeded"].includes(error.kind) && (
          <Button size="compact-xs" onClick={openLimits}>
            View account limits
          </Button>
        )}
      {error.details && (
        <details>
          <summary>Details</summary>
          <pre>{error.details}</pre>
        </details>
      )}
    </div>
  );
}

export function NativeNotice({
  item,
  planType,
}: {
  item: Message;
  planType?: string;
}) {
  if (item.nativeHookQuiet) return null;
  const error = item.nativeError
    ? nativeErrorView(item.nativeError, planType)
    : null;
  const details = error?.details || item.details;
  return (
    <div
      className={`native-notice ${error?.severity || item.nativeNotice}`}
      data-message={item.id}
    >
      <details>
        <summary>{error?.title || error?.message || item.text}</summary>
        {details && (
          <pre>
            {typeof details === "string"
              ? details
              : JSON.stringify(details, null, 2)}
          </pre>
        )}
      </details>
      {error && <Guidance error={error} />}
    </div>
  );
}

export function NativeAccountNotices({
  notices,
  accountKey,
}: {
  notices?: Json[];
  accountKey: string;
}) {
  const current = notices?.filter((n) => n.accountKey === accountKey) || [];
  if (!current.length) return null;
  return (
    <details className="native-account-notices native-notice">
      <summary>Account notices ({current.length})</summary>
      {current.map((n) => (
        <div key={n.id}>
          <p>{n.message}</p>
          {n.details && <pre>{n.details}</pre>}
        </div>
      ))}
    </details>
  );
}
