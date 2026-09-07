import type { Agent, Message, Json } from "../types";
import { agentErrorLabel } from "../types";
import { nativeErrorView } from "../nativeErrors";
import "./native-notice.css";

export function NativeError({ agent }: { agent: Agent }) {
  const error = nativeErrorView(agent.error);
  return (
    <div className="native-error" role="alert">
      <span>{agentErrorLabel(agent)}</span>
      {error.hint && <small>{error.hint}</small>}
      {error.details && (
        <details>
          <summary>Details</summary>
          <pre>{error.details}</pre>
        </details>
      )}
    </div>
  );
}

export function NativeNotice({ item }: { item: Message }) {
  const details = item.nativeError
    ? nativeErrorView(item.nativeError).details
    : item.details;
  return (
    <details
      className={`native-notice ${item.nativeNotice}`}
      data-message={item.id}
    >
      <summary>{item.text}</summary>
      {details && (
        <pre>
          {typeof details === "string"
            ? details
            : JSON.stringify(details, null, 2)}
        </pre>
      )}
    </details>
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
