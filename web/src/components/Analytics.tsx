import { useEffect, useMemo, useState } from "react";
import {
  ActionIcon,
  Button,
  Loader,
  Modal,
  NativeSelect,
  Tabs,
  Tooltip,
} from "@mantine/core";
import {
  ArrowDownToLine,
  ChevronLeft,
  ChevronRight,
  RefreshCw,
} from "lucide-react";
import { api, errorText } from "../api";
import type { Agent, Json } from "../types";
import "./Analytics.css";

const finite = (n: unknown): n is number =>
  typeof n === "number" && Number.isFinite(n);
const count = (n: unknown) => (finite(n) ? n.toLocaleString() : "Unavailable");
const bytes = (n: unknown) => {
  if (!finite(n)) return "Unavailable";
  if (n < 1024) return `${count(n)} B`;
  if (n < 1024 ** 2) return `${(n / 1024).toFixed(1)} KiB`;
  return `${(n / 1024 ** 2).toFixed(2)} MiB`;
};
const duration = (n: unknown) =>
  !finite(n)
    ? "Unavailable"
    : n < 1000
      ? `${Math.round(n)} ms`
      : n < 60000
        ? `${(n / 1000).toFixed(1)} s`
        : `${(n / 60000).toFixed(1)} min`;
const time = (n: unknown) =>
  finite(n) ? new Date(n * 1000).toLocaleString() : "Unavailable";
const shortTime = (n: number) =>
  new Date(n * 1000).toLocaleTimeString([], {
    hour: "2-digit",
    minute: "2-digit",
  });
const colors = [
  "light-dark(#7044bc, #a59bff)",
  "light-dark(#187c70, #70c9bd)",
  "light-dark(#8a5700, #e2b479)",
  "light-dark(#ab3266, #ec91b2)",
  "light-dark(#286fbf, #82b4ef)",
  "light-dark(#65751f, #c1ca81)",
];
const tokenFields = [
  ["inputTokens", "Input", "Includes cached input"],
  ["cachedInputTokens", "Cached input", "Subset of input"],
  ["cacheWriteInputTokens", "Cache write", "Only when reported"],
  ["outputTokens", "Output", "Includes reasoning output"],
  ["reasoningOutputTokens", "Reasoning output", "Subset of output"],
  ["totalTokens", "Total", "Provider-reported total"],
];
function Metric({
  label,
  value,
  hint,
}: {
  label: string;
  value: string;
  hint?: string;
}) {
  return (
    <div className="analytics-metric">
      <span>{label}</span>
      <strong>{value}</strong>
      {hint && <small>{hint}</small>}
    </div>
  );
}
function Raw({
  value,
  label = "Recorded metadata",
}: {
  value: unknown;
  label?: string;
}) {
  return (
    <details className="analytics-raw">
      <summary>{label}</summary>
      <pre>{JSON.stringify(value, null, 2)}</pre>
    </details>
  );
}
function ContextChart({
  timeline,
  compactions,
}: {
  timeline: Json[];
  compactions: Json[];
}) {
  const points = timeline
    .filter(
      (p) =>
        finite(p.at) &&
        finite(p.last?.totalTokens) &&
        finite(p.modelContextWindow) &&
        p.modelContextWindow > 0,
    )
    .sort((a, b) => a.at - b.at);
  if (!points.length)
    return (
      <div className="analytics-empty">
        No context-window measurements in this period.
      </div>
    );
  const start = points[0].at,
    end = points.at(-1)!.at;
  const groups = new Map<string, Json[]>();
  points.forEach((p) =>
    groups.set(p.agentId, [...(groups.get(p.agentId) || []), p]),
  );
  const peak = Math.max(
    100,
    ...points.map((p) => (p.last.totalTokens / p.modelContextWindow) * 100),
  );
  const x = (p: Json) => 42 + ((p.at - start) / (end - start || 1)) * 906;
  const y = (p: Json) =>
    152 - (((p.last.totalTokens / p.modelContextWindow) * 100) / peak) * 136;
  return (
    <>
      <svg
        className="analytics-chart"
        viewBox="0 0 970 182"
        role="img"
        aria-label={`Context usage over time, ${points.length} observations across ${groups.size} agents`}
      >
        {[0, 50, 100].map((n) => (
          <g key={n}>
            <line
              x1="42"
              x2="948"
              y1={152 - (n / peak) * 136}
              y2={152 - (n / peak) * 136}
            />
            <text x="3" y={156 - (n / peak) * 136}>
              {n}%
            </text>
          </g>
        ))}
        {compactions
          .filter((c) => finite(c.at) && c.at >= start && c.at <= end)
          .map((c) => (
            <line
              key={c.id}
              className="analytics-compaction-marker"
              x1={x(c)}
              x2={x(c)}
              y1="12"
              y2="152"
            >
              <title>{`Compaction · ${c.agentName || c.agentId} · ${time(c.at)}`}</title>
            </line>
          ))}
        {[...groups].map(([id, group], i) => (
          <g key={id} style={{ color: colors[i % colors.length] }}>
            <polyline
              points={group.map((p) => `${x(p)},${y(p)}`).join(" ")}
              fill="none"
              stroke="currentColor"
              strokeWidth="1.8"
            />
            {group.map((p) => (
              <circle key={p.id} cx={x(p)} cy={y(p)} r="3" fill="currentColor">
                <title>{`${p.agentName || id}: ${count(p.last.totalTokens)} / ${count(p.modelContextWindow)} tokens (${((p.last.totalTokens / p.modelContextWindow) * 100).toFixed(1)}%)\n${time(p.at)}`}</title>
              </circle>
            ))}
          </g>
        ))}
        <text x="42" y="176">
          {shortTime(start)}
        </text>
        <text x="948" y="176" textAnchor="end">
          {shortTime(end)}
        </text>
      </svg>
      <div className="analytics-legend">
        {[...groups].map(([id, group], i) => (
          <span key={id}>
            <i style={{ background: colors[i % colors.length] }} />
            {group[0].agentName || id}
          </span>
        ))}
      </div>
    </>
  );
}
function TokenGroups({
  title,
  rows,
  identity,
  detail = false,
}: {
  title: string;
  rows: Json[];
  identity: string;
  detail?: boolean;
}) {
  return (
    <section className="analytics-section">
      <header>
        <h3>{title}</h3>
        <span>Provider token use</span>
      </header>
      {!rows.length ? (
        <p className="analytics-note">No usage records.</p>
      ) : (
        <div className="analytics-table-scroll">
          <table className="analytics-table">
            <thead>
              <tr>
                <th>
                  {title === "Agents"
                    ? "Agent"
                    : title === "Models"
                      ? "Model"
                      : "Account"}
                </th>
                <th>Input</th>
                <th>Cached input</th>
                <th>Output</th>
                <th>Total</th>
                {detail && (
                  <>
                    <th>Calls</th>
                    <th>Failed</th>
                    <th>Compactions</th>
                  </>
                )}
              </tr>
            </thead>
            <tbody>
              {rows.map((r, i) => (
                <tr key={`${r[identity]}:${i}`}>
                  <td>
                    {r[identity] || "Unavailable"}
                    {detail && <Raw value={r} label="Details" />}
                  </td>
                  <td>{count(r.tokens?.inputTokens)}</td>
                  <td>{count(r.tokens?.cachedInputTokens)}</td>
                  <td>{count(r.tokens?.outputTokens)}</td>
                  <td>{count(r.tokens?.totalTokens)}</td>
                  {detail && (
                    <>
                      <td>{count(r.toolCalls)}</td>
                      <td>{count(r.failedToolCalls)}</td>
                      <td>{count(r.compactions)}</td>
                    </>
                  )}
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}
    </section>
  );
}
function CountGroup({ title, values }: { title: string; values: Json }) {
  return (
    <div>
      <h4>{title}</h4>
      <dl>
        {Object.entries(values).map(([key, value]) => (
          <div key={key}>
            <dt>{key}</dt>
            <dd>{count(value)}</dd>
          </div>
        ))}
      </dl>
      {!Object.keys(values).length && (
        <p className="analytics-note">No records.</p>
      )}
    </div>
  );
}
function ToolRanking({
  title,
  tools,
  model = false,
  onSelect,
}: {
  title: string;
  tools: Json[];
  model?: boolean;
  onSelect: (name: string) => void;
}) {
  const output = (t: Json) => (model ? t.modelOutputBytes : t.outputBytes);
  const input = (t: Json) => (model ? t.modelInputBytes : t.inputBytes);
  const ranked = [...tools].sort((a, b) => (output(b) || 0) - (output(a) || 0));
  const maxBytes = Math.max(1, ...ranked.map((t) => output(t) || 0));
  return (
    <section className="analytics-section">
      <header>
        <h3>{title}</h3>
        <span>Ranked by output bytes</span>
      </header>
      {!ranked.length ? (
        <p className="analytics-note">No measurements recorded.</p>
      ) : (
        <div className="analytics-table-scroll">
          <table className="analytics-table">
            <thead>
              <tr>
                <th>Tool</th>
                <th>Calls</th>
                <th>Input</th>
                <th>Output</th>
                <th>Duration</th>
                <th>Failed</th>
              </tr>
            </thead>
            <tbody>
              {ranked.map((t) => (
                <tr key={`${t.type}:${t.name}`}>
                  <td>
                    <button
                      className="analytics-tool-name"
                      onClick={() => onSelect(t.name)}
                    >
                      {t.name}
                    </button>
                    <div className="analytics-bar">
                      <i
                        style={{
                          width: `${((output(t) || 0) / maxBytes) * 100}%`,
                        }}
                      />
                    </div>
                  </td>
                  <td>{count(t.calls)}</td>
                  <td>{bytes(input(t))}</td>
                  <td>{bytes(output(t))}</td>
                  <td>{duration(t.durationMs)}</td>
                  <td>{count(t.failed)}</td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}
    </section>
  );
}
function Payload({ title, data }: { title: string; data?: Json | null }) {
  return (
    <div className="analytics-payload">
      <h4>{title}</h4>
      <dl>
        <dt>UTF-8 bytes</dt>
        <dd>{count(data?.bytes)}</dd>
        <dt>Characters</dt>
        <dd>{count(data?.chars)}</dd>
        <dt>Lines</dt>
        <dd>{count(data?.lines)}</dd>
        <dt>Images</dt>
        <dd>{count(data?.imageCount)}</dd>
        <dt>Image bytes</dt>
        <dd>{bytes(data?.imageBytes)}</dd>
      </dl>
    </div>
  );
}
export default function Analytics({
  agent,
  onClose,
}: {
  agent: Agent;
  onClose: () => void;
}) {
  const [scope, setScope] = useState("agent");
  const [period, setPeriod] = useState("all");
  const [tool, setTool] = useState("");
  const [offset, setOffset] = useState(0);
  const [refresh, setRefresh] = useState(0);
  const [dataRecord, setData] = useState<Json | null>(null);
  const [loadedQuery, setLoadedQuery] = useState("");
  const [error, setError] = useState("");
  const [busy, setBusy] = useState(true);
  const [exporting, setExporting] = useState(false);
  const [selected, setSelected] = useState<string | null>(null);
  const [tab, setTab] = useState<string | null>("overview");
  const [knownTools, setKnownTools] = useState<string[]>([]);
  const snapshotAt = useMemo(
    () => Math.floor(Date.now() / 1000),
    [agent.id, scope, period, refresh],
  );
  const query = useMemo(() => {
    const q = new URLSearchParams({
      agent: agent.id,
      scope,
      limit: "30",
      offset: String(offset),
      to: String(snapshotAt),
    });
    if (period !== "all") q.set("from", String(snapshotAt - Number(period)));
    if (tool) q.set("tool", tool);
    return q.toString();
  }, [agent.id, scope, period, tool, offset, snapshotAt]);
  const viewKey = JSON.stringify([agent.id, scope, period, tool, offset]);
  const data = loadedQuery === viewKey ? dataRecord : null;
  useEffect(() => {
    let active = true;
    setBusy(true);
    setError("");
    if (loadedQuery !== viewKey) setSelected(null);
    api<Json>(`/api/analytics?${query}`)
      .then((result) => {
        if (!active) return;
        setData(result);
        setLoadedQuery(viewKey);
        setKnownTools(
          (previous) =>
            [
              ...new Set([
                ...previous,
                ...(result.tools || []).map((t: Json) => t.name),
              ]),
            ].sort() as string[],
        );
      })
      .catch((e) => {
        if (active) {
          setError(errorText(e));
        }
      })
      .finally(() => {
        if (active) setBusy(false);
      });
    return () => {
      active = false;
    };
  }, [query, refresh]);
  const filter = (change: () => void) => {
    change();
    setOffset(0);
  };
  const download = async () => {
    setExporting(true);
    setError("");
    try {
      const result = await api<Json>(`/api/analytics?${query}&export=1`);
      const url = URL.createObjectURL(
        new Blob([JSON.stringify(result, null, 2)], {
          type: "application/json",
        }),
      );
      const a = document.createElement("a");
      a.href = url;
      a.download = `codex-studio-analytics-${scope}-${new Date().toISOString().replaceAll(":", "-")}.json`;
      a.click();
      window.setTimeout(() => URL.revokeObjectURL(url), 1000);
    } catch (e) {
      setError(errorText(e));
    } finally {
      setExporting(false);
    }
  };
  const s = data?.summary || {};
  const timeline: Json[] = data?.timeline || [];
  const tools: Json[] = [...(data?.tools || [])].sort(
    (a, b) => (b.outputBytes || 0) - (a.outputBytes || 0),
  );
  const calls: Json[] = data?.calls || [];
  const selectedCall = calls.find((c) => c.id === selected);
  const total = data?.pagination?.total || 0;
  return (
    <Modal
      opened
      onClose={onClose}
      title="Context analytics"
      size="min(1160px, calc(100vw - 24px))"
      className="analytics-modal"
      styles={{ body: { padding: 0 }, content: { overflow: "hidden" } }}
    >
      <div className="analytics-root" aria-busy={busy}>
        <div className="analytics-toolbar">
          <NativeSelect
            label="Scope"
            aria-label="Analytics scope"
            value={scope}
            onChange={(e) =>
              filter(() => {
                setScope(e.currentTarget.value);
                setTool("");
                setKnownTools([]);
              })
            }
            data={[
              { value: "agent", label: "This agent" },
              { value: "team", label: "Whole team" },
              { value: "all", label: "All agents" },
            ]}
          />
          <NativeSelect
            label="Period"
            aria-label="Analytics period"
            value={period}
            onChange={(e) => filter(() => setPeriod(e.currentTarget.value))}
            data={[
              { value: "all", label: "All recorded history" },
              { value: "3600", label: "Last hour" },
              { value: "86400", label: "Last 24 hours" },
              { value: "604800", label: "Last 7 days" },
              { value: "2592000", label: "Last 30 days" },
            ]}
          />
          <NativeSelect
            label="Tool calls"
            aria-label="Analytics tool"
            value={tool}
            onChange={(e) =>
              filter(() => {
                setTool(e.currentTarget.value);
                setTab("tools");
              })
            }
            data={[
              { value: "", label: "All tools" },
              ...knownTools.map((name) => ({ value: name, label: name })),
            ]}
          />
          <div className="analytics-toolbar-actions">
            <Tooltip label="Refresh analytics">
              <ActionIcon
                aria-label="Refresh analytics"
                variant="subtle"
                size="lg"
                disabled={busy}
                onClick={() => setRefresh((n) => n + 1)}
              >
                <RefreshCw size={16} />
              </ActionIcon>
            </Tooltip>
            <Button
              variant="light"
              size="xs"
              leftSection={<ArrowDownToLine size={14} />}
              loading={exporting}
              disabled={busy || !data}
              onClick={() => void download()}
            >
              Export JSON
            </Button>
          </div>
        </div>
        <div className="analytics-scope">
          <span>
            {scope === "agent"
              ? agent.name
              : scope === "team"
                ? "Selected team"
                : "All recorded agents"}
          </span>
          <span>
            {busy
              ? "Updating…"
              : data
                ? `Updated ${time(data.generatedAt)}`
                : ""}
          </span>
        </div>
        {error && (
          <p className="analytics-error" role="alert">
            {error}
          </p>
        )}
        {busy && !data && (
          <div className="analytics-empty">
            <Loader size="sm" aria-label="Load analytics" />
          </div>
        )}
        {data && (
          <Tabs value={tab} onChange={setTab} keepMounted={false}>
            <Tabs.List>
              <Tabs.Tab value="overview">Overview</Tabs.Tab>
              <Tabs.Tab value="tools">
                Tool calls{" "}
                <span
                  className="analytics-tab-count"
                  title="Model-visible tool calls"
                >
                  {count(s.modelToolCalls)}
                </span>
              </Tabs.Tab>
              <Tabs.Tab value="activity">Activity</Tabs.Tab>
            </Tabs.List>
            <div className="analytics-scroll">
              <p className="analytics-definition">
                Token use comes from provider reports. Tool payloads use bytes,
                not tokens. Exact token use per tool is unavailable.
              </p>
              {(data.coverage?.captureErrors?.count > 0 ||
                data.coverage?.historyErrors?.length > 0) && (
                <p className="analytics-filter-note" role="status">
                  Collection reported errors. Some measurements may be missing.
                  See Data coverage below.
                </p>
              )}
              {s.provisionalUsageSamples > 0 && (
                <p className="analytics-filter-note">
                  {count(s.provisionalUsageSamples)} provisional notices
                  excluded; current requests may appear after history sync.
                  These notices can duplicate known requests.
                </p>
              )}
              {tool && (
                <p className="analytics-filter-note">
                  Tool filter applies to tool calls. Provider token use covers
                  the selected agents and period.
                </p>
              )}
              <Tabs.Panel value="overview">
                {(finite(s.exactResponseSamples) ||
                  finite(s.legacyUsageSamples)) && (
                  <p className="analytics-note">
                    Token evidence: {count(s.exactResponseSamples)} identified
                    response records; {count(s.legacyUsageSamples)} legacy usage
                    records.
                  </p>
                )}
                <div className="analytics-metrics">
                  {tokenFields.map(([key, label, hint]) => (
                    <Metric
                      key={key}
                      label={label}
                      value={count(s.tokens?.[key])}
                      hint={
                        finite(s.tokenObservations?.[key]) &&
                        finite(s.usageSamples) &&
                        s.usageSamples > 0
                          ? `${hint}. ${s.tokenObservations[key] < s.usageSamples ? `Partial: ${count(s.tokenObservations[key])} of ${count(s.usageSamples)} reports` : `All ${count(s.usageSamples)} reports`}`
                          : hint
                      }
                    />
                  ))}
                </div>
                <div className="analytics-secondary">
                  <Metric
                    label="Cache hit rate"
                    value={
                      finite(s.cacheHitRate)
                        ? `${(s.cacheHitRate * 100).toFixed(1)}%`
                        : "Unavailable"
                    }
                    hint="Cached input / input"
                  />
                  <Metric
                    label="Peak context"
                    value={
                      finite(s.peakContextPercent)
                        ? `${s.peakContextPercent.toFixed(1)}%`
                        : "Unavailable"
                    }
                    hint={`${count(s.peakContextTokens)} tokens`}
                  />
                  <Metric
                    label="Compactions"
                    value={count(s.compactions)}
                    hint="Recorded events"
                  />
                  <Metric
                    label="Agents / turns"
                    value={`${count(s.agents)} / ${count(s.turns)}`}
                    hint={`${count(s.usageSamples)} usage reports`}
                  />
                </div>
                <section className="analytics-section">
                  <header>
                    <h3>Context over time</h3>
                    <span>Last reported total / model window</span>
                  </header>
                  <ContextChart
                    timeline={timeline}
                    compactions={data.compactions || []}
                  />
                  <p className="analytics-note">
                    Each line follows one agent. Points show provider reports,
                    not continuous measurements. Dotted vertical lines mark
                    compactions. Up to 500 recent reports appear here.
                  </p>
                </section>
                <section className="analytics-section">
                  <header>
                    <h3>Tool payloads</h3>
                    <Button
                      variant="subtle"
                      size="compact-xs"
                      onClick={() => setTab("tools")}
                    >
                      Inspect calls
                    </Button>
                  </header>
                  <div className="analytics-secondary">
                    <Metric
                      label="Model input payload"
                      value={bytes(s.modelInputBytes)}
                      hint="Recorded tool arguments"
                    />
                    <Metric
                      label="Model result payload"
                      value={bytes(s.modelOutputBytes)}
                      hint="Tool results returned to the model"
                    />
                    <Metric
                      label="Model tool calls"
                      value={count(s.modelToolCalls)}
                      hint={`${count(s.modelFailedToolCalls)} reported failures`}
                    />
                    <Metric
                      label="Native tool calls"
                      value={count(s.protocolToolCalls)}
                      hint={`${count(s.protocolFailedToolCalls)} reported failures`}
                    />
                  </div>
                  <p className="analytics-note">
                    Model tool time: {duration(s.modelDurationMs)}. Native tool
                    time: {duration(s.protocolDurationMs)}. Each value sums
                    measured calls within its group; parallel calls can overlap.
                  </p>
                </section>
              </Tabs.Panel>
              <Tabs.Panel value="tools">
                <ToolRanking
                  title="Model-visible tool results"
                  tools={tools.filter(
                    (t) =>
                      t.payloadBoundary === "model" ||
                      t.type === "modelToolCall",
                  )}
                  model
                  onSelect={(name) => filter(() => setTool(name))}
                />
                <ToolRanking
                  title="Native protocol output"
                  tools={tools.filter(
                    (t) =>
                      t.payloadBoundary !== "model" &&
                      t.type !== "modelToolCall",
                  )}
                  onSelect={(name) => filter(() => setTool(name))}
                />
                <Raw
                  value={tools}
                  label="Tool measurement coverage and duration distributions"
                />
                <p className="analytics-note">
                  Native output may be truncated or wrapped before the model
                  receives it. These two groups overlap and must not be added
                  together.
                </p>
                <section className="analytics-section">
                  <header>
                    <h3>Call records</h3>
                    <span>{count(total)} model and native records</span>
                  </header>
                  <div className="analytics-calls">
                    {calls.map((c) => (
                      <div
                        key={c.id}
                        className={`analytics-call ${selected === c.id ? "is-selected" : ""}`}
                      >
                        <button
                          className="analytics-call-row"
                          aria-expanded={selected === c.id}
                          onClick={() =>
                            setSelected(selected === c.id ? null : c.id)
                          }
                        >
                          <span className="analytics-call-main">
                            <strong>{c.name}</strong>
                            <small>
                              {c.agentName || c.agentId} · {time(c.startedAt)} ·{" "}
                              {c.payloadBoundary === "model"
                                ? "Model-visible"
                                : "Native protocol"}
                            </small>
                          </span>
                          <span
                            className={`analytics-status ${["failed", "error"].includes(c.status) ? "is-failed" : ""}`}
                          >
                            {c.status}
                          </span>
                          <span>
                            {bytes(
                              c.payloadBoundary === "model"
                                ? c.modelOutput?.bytes
                                : c.output?.bytes,
                            )}
                          </span>
                          <span>{duration(c.durationMs)}</span>
                        </button>
                        {selected === c.id && selectedCall && (
                          <div className="analytics-call-detail">
                            <dl className="analytics-identities">
                              {[
                                ["Call", c.id],
                                ["Agent", c.agentId],
                                ["Thread", c.threadId],
                                ["Turn", c.turnId],
                                ["Model", c.model],
                                ["Account", c.accountKey],
                                ["Source", c.source],
                                ["Started", time(c.startedAt)],
                                ["Finished", time(c.finishedAt)],
                              ].map(([label, value]) => (
                                <div key={label}>
                                  <dt>{label}</dt>
                                  <dd>{value || "Unavailable"}</dd>
                                </div>
                              ))}
                            </dl>
                            {c.command && (
                              <pre className="analytics-command">
                                {c.command}
                              </pre>
                            )}
                            {c.error && (
                              <p className="analytics-error">
                                {typeof c.error === "string"
                                  ? c.error
                                  : JSON.stringify(c.error)}
                              </p>
                            )}
                            {c.payloadBoundary === "model" ||
                            c.modelInput ||
                            c.modelOutput ? (
                              <div className="analytics-payloads">
                                <Payload
                                  title="Model tool arguments"
                                  data={c.modelInput}
                                />
                                <Payload
                                  title="Model-visible result"
                                  data={c.modelOutput}
                                />
                              </div>
                            ) : null}
                            {c.payloadBoundary !== "model" && (
                              <div className="analytics-payloads">
                                <Payload
                                  title="Protocol input"
                                  data={c.input}
                                />
                                <Payload
                                  title="Protocol output"
                                  data={c.output}
                                />
                              </div>
                            )}
                            <Raw value={c} />
                          </div>
                        )}
                      </div>
                    ))}
                  </div>
                  {!calls.length && (
                    <p className="analytics-empty">No call records.</p>
                  )}
                  <div className="analytics-pagination">
                    <span>
                      {total
                        ? `${offset + 1} to ${Math.min(offset + calls.length, total)} of ${count(total)}`
                        : "0 calls"}
                    </span>
                    <Button
                      size="compact-xs"
                      variant="subtle"
                      leftSection={<ChevronLeft size={14} />}
                      disabled={busy || offset === 0}
                      onClick={() => setOffset((n) => Math.max(0, n - 30))}
                    >
                      Previous
                    </Button>
                    <Button
                      size="compact-xs"
                      variant="subtle"
                      rightSection={<ChevronRight size={14} />}
                      disabled={busy || !data.pagination?.hasMore}
                      onClick={() => setOffset((n) => n + 30)}
                    >
                      Next
                    </Button>
                  </div>
                </section>
              </Tabs.Panel>
              <Tabs.Panel value="activity">
                <TokenGroups
                  title="Agents"
                  rows={data.agentTotals || []}
                  identity="name"
                  detail
                />
                <TokenGroups
                  title="Models"
                  rows={data.modelTotals || []}
                  identity="model"
                />
                <TokenGroups
                  title="Accounts"
                  rows={data.accountTotals || []}
                  identity="accountKey"
                />
                <section className="analytics-section">
                  <header>
                    <h3>Turns</h3>
                    <span>
                      Latest 100 turns; elapsed time includes tools and waits
                    </span>
                  </header>
                  <div className="analytics-observations">
                    {[...(data.turns || [])]
                      .sort((a, b) => (b.startedAt || 0) - (a.startedAt || 0))
                      .slice(0, 100)
                      .map((turn: Json) => (
                        <details key={`${turn.agentId}:${turn.turnId}`}>
                          <summary>
                            <span>
                              {turn.agentName || turn.agentId}
                              <small>
                                {time(turn.startedAt)} · {turn.status}
                              </small>
                            </span>
                            <span>
                              {duration(turn.durationMs)}
                              <small>
                                First output {duration(turn.firstOutputDelayMs)}
                              </small>
                            </span>
                          </summary>
                          <Raw value={turn} />
                        </details>
                      ))}
                  </div>
                  {!data.turns?.length && (
                    <p className="analytics-note">
                      No turn measurements recorded.
                    </p>
                  )}
                </section>
                <section className="analytics-section">
                  <header>
                    <h3>Background activity</h3>
                    <span>Latest 100 monitors and event delivery</span>
                  </header>
                  <div className="analytics-operation-counts">
                    <CountGroup
                      title="Events"
                      values={data.operations?.eventCounts || {}}
                    />
                    <CountGroup
                      title="Approval states"
                      values={data.operations?.approvalCounts || {}}
                    />
                  </div>
                  <div className="analytics-observations">
                    {[...(data.operations?.monitors || [])]
                      .sort((a, b) => (b.created || 0) - (a.created || 0))
                      .slice(0, 100)
                      .map((m: Json) => (
                        <details key={m.id}>
                          <summary>
                            <span>
                              {m.id}
                              <small>
                                {m.agent} · {time(m.created)}
                              </small>
                            </span>
                            <span>
                              {m.status}
                              <small>
                                {bytes(m.bytes)} · Exit {count(m.exitCode)}
                              </small>
                            </span>
                          </summary>
                          <Raw value={m} />
                        </details>
                      ))}
                  </div>
                </section>
                <section className="analytics-section">
                  <header>
                    <h3>Protocol events</h3>
                    <span>
                      Full UTC-hour buckets may overlap the selected period
                    </span>
                  </header>
                  <div className="analytics-table-scroll">
                    <table className="analytics-table">
                      <thead>
                        <tr>
                          <th>Method</th>
                          <th>Events</th>
                          <th>Bytes</th>
                        </tr>
                      </thead>
                      <tbody>
                        {Object.entries(
                          (data.notifications || []).reduce(
                            (groups: Record<string, Json>, n: Json) => {
                              const group = (groups[n.method] ||= {
                                count: 0,
                                bytes: 0,
                              });
                              group.count += n.count;
                              group.bytes += n.bytes;
                              return groups;
                            },
                            {},
                          ),
                        ).map(([method, value]) => {
                          const n = value as Json;
                          return (
                            <tr key={method}>
                              <td>{method}</td>
                              <td>{count(n.count)}</td>
                              <td>{bytes(n.bytes)}</td>
                            </tr>
                          );
                        })}
                      </tbody>
                    </table>
                  </div>
                  <Raw
                    value={data.notifications || []}
                    label="Event buckets by agent and hour"
                  />
                </section>
                <section className="analytics-section">
                  <header>
                    <h3>Account limit history</h3>
                    <span>Latest 100 allowance snapshots, not cost</span>
                  </header>
                  <div className="analytics-observations">
                    {[...(data.rateLimits || [])]
                      .sort((a, b) => (b.at || 0) - (a.at || 0))
                      .slice(0, 100)
                      .map((r: Json, i: number) => (
                        <details key={`${r.accountKey}:${r.at}:${i}`}>
                          <summary>
                            <span>{r.accountKey}</span>
                            <span>{time(r.at)}</span>
                          </summary>
                          <Raw value={r.data} label="Recorded allowance" />
                        </details>
                      ))}
                  </div>
                  {!data.rateLimits?.length && (
                    <p className="analytics-note">
                      No allowance snapshots recorded.
                    </p>
                  )}
                </section>

                <section className="analytics-section">
                  <header>
                    <h3>Compactions</h3>
                    <span>
                      {count(s.compactions)} recorded; latest 100 shown
                    </span>
                  </header>
                  {!(data.compactions || []).length ? (
                    <p className="analytics-note">
                      No compaction records available in this period.
                    </p>
                  ) : (
                    <div className="analytics-observations">
                      {data.compactions.slice(0, 100).map((c: Json) => (
                        <details key={c.id}>
                          <summary>
                            <span>
                              {c.agentName || c.agentId}
                              <small>{time(c.at)}</small>
                            </span>
                            <span>Compaction</span>
                          </summary>
                          <Raw value={c} />
                        </details>
                      ))}
                    </div>
                  )}
                </section>
                <section className="analytics-section">
                  <header>
                    <h3>Recorded item types</h3>
                    <span>
                      Messages, reasoning, tools and other observed items
                    </span>
                  </header>
                  <div className="analytics-table-scroll">
                    <table className="analytics-table">
                      <thead>
                        <tr>
                          <th>Type</th>
                          <th>Items</th>
                          <th>Bytes</th>
                          <th>Characters</th>
                        </tr>
                      </thead>
                      <tbody>
                        {(data.items || []).map((item: Json) => (
                          <tr key={item.type}>
                            <td>{item.type}</td>
                            <td>{count(item.count)}</td>
                            <td>{bytes(item.bytes)}</td>
                            <td>{count(item.chars)}</td>
                          </tr>
                        ))}
                      </tbody>
                    </table>
                  </div>
                  {!data.items?.length && (
                    <p className="analytics-empty">
                      No item measurements recorded.
                    </p>
                  )}
                </section>
                <section className="analytics-section">
                  <header>
                    <h3>Usage reports</h3>
                    <span>{timeline.length} recent observations</span>
                  </header>
                  <div className="analytics-observations">
                    {[...timeline].reverse().map((p) => (
                      <details key={p.id}>
                        <summary>
                          <span>
                            {p.agentName || p.agentId}
                            <small>
                              {time(p.at)} · {p.source}
                            </small>
                          </span>
                          <span>
                            {count(p.last?.totalTokens)} tokens
                            {p.reset && <small>Cumulative counter reset</small>}
                          </span>
                        </summary>
                        <div>
                          <dl className="analytics-identities">
                            <div>
                              <dt>Turn</dt>
                              <dd>{p.turnId || "Unavailable"}</dd>
                            </div>
                            <div>
                              <dt>Model</dt>
                              <dd>{p.model || "Unavailable"}</dd>
                            </div>
                          </dl>
                          <Raw
                            value={p}
                            label="Provider report and recorded metadata"
                          />
                        </div>
                      </details>
                    ))}
                  </div>
                  {!timeline.length && (
                    <p className="analytics-empty">
                      No provider usage reports recorded.
                    </p>
                  )}
                </section>
              </Tabs.Panel>
              <details className="analytics-coverage">
                <summary>Data coverage and measurement limits</summary>
                <p>
                  Collection started {time(data.coverage?.trackingSince)}. Older
                  stored history may have fewer measurements. Missing values
                  stay unavailable.
                </p>
                {(data.coverage?.notes || []).map((note: string, i: number) => (
                  <p key={i}>{note}</p>
                ))}
                <p>
                  Cached input is part of input. Reasoning output is part of
                  output. Parallel tool durations can overlap. Payload sizes do
                  not measure billable tokens.
                </p>
                <p>
                  Cost estimates remain in Account limits. They are shared
                  across chats and cannot identify exact spend for these calls.
                </p>
                {data.coverage?.captureErrors?.count > 0 && (
                  <p className="analytics-error">
                    {count(data.coverage.captureErrors.count)} collection errors
                    recorded. Some data may be missing.
                  </p>
                )}
                <Raw value={data.coverage} label="Coverage metadata" />
                <Raw
                  value={data.provisionalUsage || []}
                  label="Excluded provisional notices (latest 100)"
                />
                <Raw
                  value={data.history || []}
                  label="History import checkpoints"
                />
                <Raw
                  value={data.summary}
                  label="Summary and measurement counts"
                />
              </details>
            </div>
          </Tabs>
        )}
      </div>
    </Modal>
  );
}
