import { Button, NumberInput, Select, Text } from "@mantine/core";
import { useEffect, useRef, useState } from "react";
import { api, errorText } from "../api";
import type { Agent } from "../types";
import { shortModel } from "./ExecutionSettings";
import "./review-schedules.css";

export interface ReviewSchedule {
  reviewerId: string;
  intervalMinutes: number;
  enabled: boolean;
  removed?: boolean;
  revision: number;
  nextAt?: number | null;
  lastRunAt?: number | null;
  status?: string;
  reason?: string;
  roomId: string;
}

export default function ReviewSchedules({
  agent,
  agents,
  workspaceId,
  stateDir,
  refresh,
  openRoom,
}: {
  agent: Agent;
  agents: Agent[];
  workspaceId: string;
  stateDir: string;
  refresh: () => Promise<void>;
  openRoom: (id: string) => void;
}) {
  const [target, setTarget] = useState<string | null>(null);
  const [minutes, setMinutes] = useState<string | number>(30);
  const [known, setKnown] = useState<Record<string, ReviewSchedule>>({});
  const [pending, setPending] = useState(false);
  const [error, setError] = useState("");
  const active = useRef(true);
  const lock = useRef(false);
  useEffect(() => {
    active.current = true;
    return () => {
      active.current = false;
    };
  }, []);
  const assignments = (candidates: Agent[]) =>
    candidates.flatMap((candidate) =>
      (candidate.reviewSchedules || [])
        .filter((item: ReviewSchedule) => item.reviewerId === agent.id)
        .map((item: ReviewSchedule) => [candidate.id, item] as const),
    );
  const schedules = new Map<string, ReviewSchedule>(assignments(agents));
  for (const [targetId, item] of Object.entries(known))
    if ((schedules.get(targetId)?.revision || 0) < item.revision)
      schedules.set(targetId, item);
  const teamId = agent.rootId || (agent.isLead ? agent.id : undefined);
  const sameTeam = (candidate?: Agent) =>
    !!teamId &&
    candidate?.source === "managed" &&
    (candidate.rootId || (candidate.isLead ? candidate.id : undefined)) ===
      teamId;
  const choices = agents.filter(
    (candidate) =>
      sameTeam(candidate) &&
      candidate.id !== agent.id &&
      !candidate.deletedAt &&
      (!schedules.has(candidate.id) || schedules.get(candidate.id)?.removed),
  );
  const save = async (
    targetId: string,
    intervalMinutes: number,
    enabled: boolean,
    removed = false,
  ) => {
    if (lock.current) return;
    if (
      enabled &&
      !removed &&
      !sameTeam(agents.find((item) => item.id === targetId))
    ) {
      setError("The reviewed chat must belong to this team.");
      return;
    }
    lock.current = true;
    setPending(true);
    setError("");
    try {
      const previous = schedules.get(targetId);
      const result = await api<Agent>(
        "/api/organization",
        {
          id: targetId,
          review_schedule: {
            reviewer_id: agent.id,
            interval_minutes: intervalMinutes,
            enabled,
            removed,
            expected_revision: previous?.revision || 0,
          },
        },
        { workspaceId, timeoutMs: 15000 },
      );
      if (!active.current) return;
      const saved = (result.reviewSchedules || []).find(
        (item: ReviewSchedule) => item.reviewerId === agent.id,
      );
      if (
        result.id !== targetId ||
        !saved ||
        saved.intervalMinutes !== intervalMinutes ||
        saved.enabled !== enabled ||
        !!saved.removed !== removed ||
        saved.revision < (previous?.revision || 0)
      )
        throw new Error("The server did not confirm the review schedule.");
      setKnown((old) => ({ ...old, [targetId]: saved }));
      setTarget(null);
      await refresh();
    } catch (cause) {
      if (active.current) {
        setError(errorText(cause));
        // Reload authoritative settings after a lost response or another device's edit.
        try {
          const snapshot = await api<{ threads: Agent[]; stateDir: string }>(
            "/api/state?view=chat",
          );
          const canonical = snapshot.threads.find(
            (item) => item.id === agent.id,
          );
          if (
            active.current &&
            snapshot.stateDir === stateDir &&
            canonical &&
            canonical.threadId === agent.threadId
          )
            setKnown(Object.fromEntries(assignments(snapshot.threads)));
        } catch {
          /* Keep the form and error so the user can retry. */
        }
      }
    } finally {
      lock.current = false;
      if (active.current) setPending(false);
    }
  };
  const validMinutes =
    typeof minutes === "number" &&
    Number.isInteger(minutes) &&
    minutes >= 1 &&
    minutes <= 10080;
  return (
    <section className="review-schedules" aria-label="Chat reviews">
      <Text fw={600}>Review other chats</Text>
      <Text size="sm" c="dimmed">
        This agent reviews the selected chats on a timer. Review events arrive
        in this chat. Both agents can discuss findings in Messages.
      </Text>
      {[...schedules.entries()]
        .filter(([, item]) => !item.removed)
        .map(([targetId, item]) => (
          <ReviewRow
            key={targetId}
            targetId={targetId}
            item={item}
            target={agents.find((candidate) => candidate.id === targetId)}
            sameTeam={sameTeam(
              agents.find((candidate) => candidate.id === targetId),
            )}
            pending={pending}
            save={save}
            openRoom={openRoom}
          />
        ))}
      <Select
        label="Chat to review"
        placeholder="Select a chat in this team"
        searchable
        clearable
        data={choices.map((candidate) => ({
          value: candidate.id,
          label: `${candidate.name} (${shortModel(candidate.model)})`,
        }))}
        value={target}
        onChange={setTarget}
        disabled={pending}
        nothingFoundMessage="No matching agents in this team"
      />
      <NumberInput
        label="Review interval (minutes)"
        min={1}
        max={10080}
        allowDecimal={false}
        value={minutes}
        onChange={setMinutes}
        disabled={pending}
      />
      <Button
        disabled={
          !choices.some((candidate) => candidate.id === target) ||
          !validMinutes ||
          pending
        }
        onClick={() =>
          target && validMinutes && void save(target, Number(minutes), true)
        }
      >
        Add chat
      </Button>
      <Text size="xs" c="dimmed">
        Default: every 30 minutes. No new work means no repeat review. A busy
        reviewer receives one queued check.
      </Text>
      {error && (
        <Text role="alert" c="red" size="sm">
          {error}
        </Text>
      )}
    </section>
  );
}

function ReviewRow({
  item,
  targetId,
  target,
  pending,
  sameTeam,
  save,
  openRoom,
}: {
  item: ReviewSchedule;
  targetId: string;
  target?: Agent;
  pending: boolean;
  sameTeam: boolean;
  save: (
    id: string,
    minutes: number,
    enabled: boolean,
    removed?: boolean,
  ) => Promise<void>;
  openRoom: (id: string) => void;
}) {
  const [minutes, setMinutes] = useState<string | number>(item.intervalMinutes);
  useEffect(
    () => setMinutes(item.intervalMinutes),
    [item.intervalMinutes, item.revision],
  );
  const valid =
    typeof minutes === "number" &&
    Number.isInteger(minutes) &&
    minutes >= 1 &&
    minutes <= 10080;
  return (
    <div className="review-schedule" data-review-target={targetId}>
      <Text fw={500}>{target?.name || "Unavailable chat"}</Text>
      <Text size="sm" c="dimmed">
        {!sameTeam
          ? "Unavailable: outside this team. History remains available."
          : !item.enabled
            ? "Paused"
            : item.reason || item.status || "Scheduled"}
      </Text>
      {!!item.nextAt && item.enabled && sameTeam && (
        <Text size="xs" c="dimmed">
          Next check: {new Date(item.nextAt * 1000).toLocaleString()}
        </Text>
      )}
      <NumberInput
        label={`Interval for ${target?.name || "chat"} (minutes)`}
        min={1}
        max={10080}
        allowDecimal={false}
        value={minutes}
        onChange={setMinutes}
        disabled={pending || !sameTeam}
      />
      <div className="review-schedule-actions">
        <Button
          size="xs"
          variant="light"
          disabled={
            pending || !sameTeam || !valid || minutes === item.intervalMinutes
          }
          onClick={() => void save(targetId, Number(minutes), item.enabled)}
        >
          Save interval
        </Button>
        <Button
          size="xs"
          variant="light"
          disabled={pending || !sameTeam}
          onClick={() =>
            void save(targetId, item.intervalMinutes, !item.enabled)
          }
        >
          {item.enabled ? "Pause" : "Resume"}
        </Button>
        <Button
          size="xs"
          variant="subtle"
          onClick={() => openRoom(item.roomId)}
        >
          Discussion
        </Button>
        <Button
          size="xs"
          variant="subtle"
          disabled={pending}
          onClick={() => void save(targetId, item.intervalMinutes, false, true)}
        >
          Remove
        </Button>
      </div>
    </div>
  );
}
