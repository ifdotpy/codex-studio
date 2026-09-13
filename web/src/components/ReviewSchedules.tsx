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
  const [reviewer, setReviewer] = useState<string | null>(null);
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
  const schedules = new Map<string, ReviewSchedule>(
    (agent.reviewSchedules || []).map((item: ReviewSchedule) => [
      item.reviewerId,
      item,
    ]),
  );
  for (const item of Object.values(known))
    if ((schedules.get(item.reviewerId)?.revision || 0) < item.revision)
      schedules.set(item.reviewerId, item);
  const choices = agents.filter(
    (candidate) =>
      candidate.source === "managed" &&
      candidate.id !== agent.id &&
      !candidate.deletedAt &&
      (!schedules.has(candidate.id) || schedules.get(candidate.id)?.removed),
  );
  const save = async (
    reviewerId: string,
    intervalMinutes: number,
    enabled: boolean,
    removed = false,
  ) => {
    if (lock.current) return;
    lock.current = true;
    setPending(true);
    setError("");
    try {
      const previous = schedules.get(reviewerId);
      const result = await api<Agent>(
        "/api/organization",
        {
          id: agent.id,
          review_schedule: {
            reviewer_id: reviewerId,
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
        (item: ReviewSchedule) => item.reviewerId === reviewerId,
      );
      if (
        result.id !== agent.id ||
        !saved ||
        saved.intervalMinutes !== intervalMinutes ||
        saved.enabled !== enabled ||
        !!saved.removed !== removed ||
        saved.revision < (previous?.revision || 0)
      )
        throw new Error("The server did not confirm the review schedule.");
      setKnown((old) => ({ ...old, [reviewerId]: saved }));
      setReviewer(null);
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
            setKnown(
              Object.fromEntries(
                (canonical.reviewSchedules || []).map(
                  (item: ReviewSchedule) => [item.reviewerId, item],
                ),
              ),
            );
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
    <section className="review-schedules" aria-label="Chat reviewers">
      <Text fw={600}>Reviewers</Text>
      <Text size="sm" c="dimmed">
        Another chat checks this chat on a timer. Both agents can discuss
        findings in Messages.
      </Text>
      {[...schedules.values()]
        .filter((item) => !item.removed)
        .map((item) => (
          <ReviewRow
            key={item.reviewerId}
            item={item}
            reviewer={agents.find(
              (candidate) => candidate.id === item.reviewerId,
            )}
            pending={pending}
            save={save}
            openRoom={openRoom}
          />
        ))}
      <Select
        label="Reviewer chat"
        placeholder="Select a chat"
        searchable
        clearable
        data={choices.map((candidate) => ({
          value: candidate.id,
          label: `${candidate.name} (${shortModel(candidate.model)})`,
        }))}
        value={reviewer}
        onChange={setReviewer}
        disabled={pending}
        nothingFoundMessage="No matching chats"
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
        disabled={!reviewer || !validMinutes || pending}
        onClick={() =>
          reviewer && validMinutes && void save(reviewer, Number(minutes), true)
        }
      >
        Add reviewer
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
  reviewer,
  pending,
  save,
  openRoom,
}: {
  item: ReviewSchedule;
  reviewer?: Agent;
  pending: boolean;
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
    <div className="review-schedule" data-reviewer={item.reviewerId}>
      <Text fw={500}>{reviewer?.name || "Unavailable chat"}</Text>
      <Text size="sm" c="dimmed">
        {!item.enabled ? "Paused" : item.reason || item.status || "Scheduled"}
      </Text>
      {!!item.nextAt && item.enabled && (
        <Text size="xs" c="dimmed">
          Next check: {new Date(item.nextAt * 1000).toLocaleString()}
        </Text>
      )}
      <NumberInput
        label={`Interval for ${reviewer?.name || "reviewer"} (minutes)`}
        min={1}
        max={10080}
        allowDecimal={false}
        value={minutes}
        onChange={setMinutes}
        disabled={pending}
      />
      <div className="review-schedule-actions">
        <Button
          size="xs"
          variant="light"
          disabled={pending || !valid || minutes === item.intervalMinutes}
          onClick={() =>
            void save(item.reviewerId, Number(minutes), item.enabled)
          }
        >
          Save interval
        </Button>
        <Button
          size="xs"
          variant="light"
          disabled={pending}
          onClick={() =>
            void save(item.reviewerId, item.intervalMinutes, !item.enabled)
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
          onClick={() =>
            void save(item.reviewerId, item.intervalMinutes, false, true)
          }
        >
          Remove
        </Button>
      </div>
    </div>
  );
}
