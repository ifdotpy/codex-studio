import { useEffect, useState } from "react";
import { currentCapacityRetry } from "../capacityRetry";
import { limitRecovery } from "../limitRecovery";
import { useRecoveredLimit } from "./useRecoveredLimit";
import CapacityRetry from "./CapacityRetry";
import LimitRecoveryNotice from "./LimitRecoveryNotice";
import ConnectionRecovery, {
  canCheckConnection,
  matchingConnectionCheck,
} from "./ConnectionRecovery";
import type { Agent, Message, Json } from "../types";
import { agentErrorLabel } from "../types";
import { Button } from "@mantine/core";
import { CircleAlert } from "lucide-react";
import { failureMessage } from "./turnFailureReason";
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
  limits,
  openLimits,
  newChat,
  chooseChat,
  refresh,
}: {
  agent: Agent;
  planType?: string;
  limits?: Json | null;
  openLimits?: () => void;
  newChat?: () => void;
  chooseChat?: () => void;
  refresh?: () => Promise<void>;
}) {
  const [now, setNow] = useState(() => Date.now() / 1000);
  useEffect(() => {
    if (!limits) return;
    const timer = setInterval(() => setNow(Date.now() / 1000), 1000);
    return () => clearInterval(timer);
  }, [limits]);
  const recovered = useRecoveredLimit(agent, limits ?? null, now);
  const recovery = recovered ? null : limitRecovery(agent, limits ?? null, now);
  const retry = currentCapacityRetry(agent);
  const blocked = nativeThreadError(agent);
  const connectionCheck = matchingConnectionCheck(agent);
  const error = nativeErrorView(blocked || agent.error, planType);
  const detailView = connectionCheck
    ? nativeErrorView(agent.error, planType)
    : error;
  if (!blocked && !retry && recovered) return null;
  return (
    <div
      className={`native-error ${recovery ? "usage-limit" : retry ? "warning" : error.severity}`}
      role="alert"
    >
      {!recovery && (
        <span>
          {(connectionCheck && "Previous turn needs review") ||
            error.title ||
            (blocked ? error.message : agentErrorLabel(agent)) ||
            (retry ? "Model retry" : "")}
        </span>
      )}
      {connectionCheck ? (
        <small>
          {connectionCheck.nativeState === "active"
            ? "Codex reported this thread as active. Its outcome is not confirmed."
            : "Codex answered the connection check. The previous outcome is unconfirmed. Review the history before continuing."}
        </small>
      ) : !recovery && !retry ? (
        <Guidance error={error} />
      ) : null}
      {recovery && (
        <LimitRecoveryNotice
          key={JSON.stringify(recovery)}
          recovery={recovery}
          inline
        />
      )}
      {retry && (
        <CapacityRetry key={retry.id} agentId={agent.id} retry={retry} />
      )}
      {refresh && canCheckConnection(agent) && (
        <ConnectionRecovery
          key={JSON.stringify([
            agent.id,
            agent.threadId,
            agent.turnId,
            agent.epoch,
            agent.accountKey,
            agent.error,
          ])}
          agentId={agent.id}
          refresh={refresh}
        />
      )}
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
          <Button
            size="xs"
            variant="light"
            color={recovery ? "gray" : undefined}
            onClick={openLimits}
          >
            View account limits
          </Button>
        )}
      {(connectionCheck || error.details || (recovery && error.message)) && (
        <details>
          <summary>Details</summary>
          <pre>{detailView.details || detailView.message}</pre>
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
  if (
    error &&
    ["usageLimitExceeded", "rateLimitExceeded"].includes(error.kind)
  ) {
    const reportedReset = error.message.match(/\btry again at\s+([^\r\n]+)/i);
    const message = reportedReset
      ? `Try again at ${reportedReset[1]}`
      : error.hint;
    const raw = details || error.message;
    return (
      <div className="native-notice native-limit-notice" data-message={item.id}>
        <div className="native-limit-notice-heading">
          <CircleAlert size={16} aria-hidden="true" />
          <strong>{error.title}</strong>
        </div>
        <p>{message}</p>
        {raw && (
          <details>
            <summary>Details</summary>
            <pre>
              {typeof raw === "string" ? raw : JSON.stringify(raw, null, 2)}
            </pre>
          </details>
        )}
      </div>
    );
  }
  return (
    <div
      className={`native-notice ${error?.severity || item.nativeNotice}`}
      data-message={item.id}
    >
      <details>
        <summary>
          {item.nativeNotice === "error"
            ? failureMessage(item.nativeError || item.text)
            : error?.title || error?.message || item.text}
        </summary>
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
