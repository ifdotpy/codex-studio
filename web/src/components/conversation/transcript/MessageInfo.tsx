import { useEffect, useRef, useState } from "react";
import { ActionIcon, Popover } from "@mantine/core";
import { Copy, Info, X } from "lucide-react";
import { api } from "../../../api";
import { localDateTime } from "../../../local-time";
import { peekSessionCost } from "../../../sessionCostCache";
import type { Message } from "../../../types";
import type { UsageAccount } from "../../Usage";
import "./message-info.css";

type Metadata = {
  at?: number | string;
  model?: string;
  effort?: string;
  accountKey?: string;
  accountLabel?: string;
  provider?: string;
  turnDurationMs?: number;
  responseRate?: number;
  tokens?: { outputTokens?: number; reasoningOutputTokens?: number };
};

export type MessageInfoProps = {
  message: Message;
  agentId?: string;
  stateDir: string;
  rootId?: string;
  accounts?: UsageAccount[];
  onCopy: (value: string) => void;
};

const knownNumber = (value: unknown): value is number =>
  typeof value === "number" && Number.isFinite(value) && value >= 0;
const duration = (value: number) =>
  value === 0
    ? "0 s"
    : value < 1000
      ? `${Math.max(1, Math.round(value))} ms`
      : `${(value / 1000).toLocaleString(undefined, { maximumFractionDigits: 1 })} s`;

export default function MessageInfo({
  message,
  agentId,
  stateDir,
  rootId,
  accounts,
  onCopy,
}: MessageInfoProps) {
  const [open, setOpen] = useState(false);
  const [metadata, setMetadata] = useState<Metadata | null>(null);
  const [loading, setLoading] = useState(false);
  const [failed, setFailed] = useState(false);
  const target = useRef<HTMLButtonElement>(null);
  const itemId =
    agentId && message.id.startsWith(`${agentId}:`)
      ? message.id.slice(agentId.length + 1)
      : message.id;
  useEffect(() => {
    if (!open || !agentId) return;
    let active = true;
    setLoading(true);
    setFailed(false);
    setMetadata(null);
    const query = new URLSearchParams({
      agent: agentId,
      view: "message-info",
      item: itemId,
    });
    if (message.turnId) query.set("turn", message.turnId);
    if (message.threadId) query.set("thread", message.threadId);
    api<Metadata>(`/api/analytics?${query}`, undefined, { timeoutMs: 5000 })
      .then((value) => {
        if (active) setMetadata(value);
      })
      .catch(() => {
        if (active) setFailed(true);
      })
      .finally(() => {
        if (active) setLoading(false);
      });
    return () => {
      active = false;
    };
  }, [
    open,
    agentId,
    itemId,
    message.turnId,
    message.threadId,
    message.streaming,
    message.turnStatus,
  ]);
  const close = () => {
    setOpen(false);
    target.current?.focus({ preventScroll: true });
  };
  const at = message.at ?? message.created ?? message.timestamp ?? metadata?.at;
  const date =
    at == null || at === ""
      ? null
      : new Date(typeof at === "number" ? at * 1000 : at);
  const account = accounts?.find(
    (entry) => entry.key === (metadata?.accountKey || message.accountKey),
  );
  const cost = open && rootId ? peekSessionCost(stateDir, rootId) : null;
  const rows: [string, string][] = [];
  if (date && Number.isFinite(date.getTime()) && date.getTime() > 0)
    rows.push([
      "Local date and time",
      localDateTime(date, {
        year: "numeric",
        month: "short",
        day: "numeric",
        hour: "2-digit",
        minute: "2-digit",
        second: "2-digit",
      }),
    ]);
  if (metadata?.model || message.model)
    rows.push(["Model", metadata?.model || message.model]);
  if (metadata?.effort || message.effort)
    rows.push(["Reasoning effort", metadata?.effort || message.effort]);
  if (metadata?.provider || account?.provider)
    rows.push(["Provider", metadata?.provider || account!.provider!]);
  if (metadata?.accountLabel || account?.label)
    rows.push(["Account", metadata?.accountLabel || account!.label]);
  if (knownNumber(metadata?.turnDurationMs))
    rows.push(["Turn duration", duration(metadata.turnDurationMs)]);
  if (knownNumber(metadata?.tokens?.outputTokens))
    rows.push([
      "Response output tokens",
      metadata.tokens.outputTokens.toLocaleString(),
    ]);
  if (knownNumber(metadata?.tokens?.reasoningOutputTokens))
    rows.push([
      "Response reasoning tokens",
      metadata.tokens.reasoningOutputTokens.toLocaleString(),
    ]);
  if (knownNumber(metadata?.responseRate))
    rows.push([
      "Response output rate",
      `${metadata.responseRate.toLocaleString(undefined, { maximumFractionDigits: 1 })} tok/s`,
    ]);
  if (cost?.pricingState === "ready" && knownNumber(cost.totalUSD))
    rows.push([
      "Session estimate (USD)",
      cost.totalUSD > 0 && cost.totalUSD < 0.0001
        ? "< $0.0001"
        : `$${cost.totalUSD.toLocaleString(undefined, { maximumFractionDigits: 4 })}`,
    ]);
  return (
    <Popover
      opened={open}
      onChange={setOpen}
      onDismiss={close}
      position="bottom-end"
      width={340}
      middlewares={{ size: true, shift: { padding: 8 } }}
      withArrow
      trapFocus
      returnFocus
      shadow="md"
      transitionProps={{ duration: 0 }}
    >
      <Popover.Target>
        <ActionIcon
          ref={target}
          size="sm"
          className="message-info-control"
          aria-label="Message information"
          aria-haspopup="dialog"
          aria-expanded={open}
          onClick={() => setOpen(!open)}
          onKeyDown={(event) => {
            if (open && event.key === "Escape") {
              event.preventDefault();
              event.stopPropagation();
              close();
            }
          }}
        >
          <Info size={14} />
        </ActionIcon>
      </Popover.Target>
      <Popover.Dropdown
        className="message-info-popover"
        role="dialog"
        aria-label="Message information"
        onKeyDown={(event) => {
          if (event.key === "Escape") {
            event.preventDefault();
            event.stopPropagation();
            close();
          }
        }}
      >
        <div className="message-info-heading">
          <strong>Message information</strong>
          <ActionIcon
            size="sm"
            aria-label="Close message information"
            data-autofocus
            onClick={close}
          >
            <X size={14} />
          </ActionIcon>
        </div>
        <dl>
          {rows.map(([label, value]) => (
            <div className="message-info-row" key={label}>
              <dt>{label}</dt>
              <dd>{value}</dd>
            </div>
          ))}
          {[
            ["Turn ID", message.turnId],
            ["Item ID", itemId],
          ].map(([label, value]) =>
            value ? (
              <div className="message-info-row" key={label}>
                <dt>{label}</dt>
                <dd className="message-info-id">
                  <code>{value}</code>
                  <ActionIcon
                    size="sm"
                    aria-label={`Copy ${label.toLowerCase()}`}
                    onClick={() => onCopy(value)}
                  >
                    <Copy size={14} />
                  </ActionIcon>
                </dd>
              </div>
            ) : null,
          )}
        </dl>
        {loading && <p role="status">Read metadata…</p>}
        {failed && <p role="status">Could not read metadata.</p>}
      </Popover.Dropdown>
    </Popover>
  );
}
