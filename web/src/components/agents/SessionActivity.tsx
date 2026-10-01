import { ChevronRight, Clock3, Terminal, Users } from "lucide-react";
import { useEffect, useState } from "react";
import "./session-activity.css";

export type SessionActivityItem = {
  id: string;
  kind: "task" | "monitor" | "agent";
  agentId: string;
  agentName: string;
  label: string;
  command?: string;
  created?: number;
  status: string;
};

function elapsed(created: number | undefined, now: number) {
  if (created === undefined || !Number.isFinite(created)) return "";
  const minutes = Math.floor(Math.max(0, now - created) / 60);
  if (minutes < 1) return "<1m";
  if (minutes < 60) return `${minutes}m`;
  return `${Math.floor(minutes / 60)}h ${minutes % 60}m`;
}

export default function SessionActivity({
  activities,
  onOpen,
}: {
  activities: SessionActivityItem[];
  onOpen: (activity: SessionActivityItem) => void;
}) {
  const [expanded, setExpanded] = useState(false);
  const [now, setNow] = useState(() => Date.now() / 1000);
  const timed = activities.some(
    (activity) =>
      activity.created !== undefined && Number.isFinite(activity.created),
  );
  useEffect(() => {
    if (!timed) return;
    let timer: ReturnType<typeof setInterval> | undefined;
    const resume = () => {
      clearInterval(timer);
      if (document.hidden) return;
      setNow(Date.now() / 1000);
      timer = setInterval(() => setNow(Date.now() / 1000), 30000);
    };
    resume();
    document.addEventListener("visibilitychange", resume);
    return () => {
      clearInterval(timer);
      document.removeEventListener("visibilitychange", resume);
    };
  }, [timed]);
  useEffect(() => {
    if (activities.length <= 1) setExpanded(false);
  }, [activities.length]);
  if (!activities.length) return null;
  const shown = expanded ? activities : activities.slice(0, 1);
  return (
    <section
      className="session-activity"
      aria-label="Session activity"
      data-expanded={expanded}
    >
      <ul aria-label="Active work">
        {shown.map((activity) => {
          const Icon =
            activity.kind === "agent"
              ? Users
              : activity.kind === "monitor"
                ? Clock3
                : Terminal;
          const age = elapsed(activity.created, now);
          return (
            <li key={`${activity.kind}:${activity.id}`}>
              <button
                type="button"
                className="session-activity-row"
                data-activity-id={activity.id}
                data-activity-kind={activity.kind}
                onClick={() => onOpen(activity)}
              >
                <Icon
                  className="session-activity-icon"
                  size={14}
                  aria-hidden="true"
                />
                <span className="session-activity-copy">
                  <span className="session-activity-owner">
                    {activity.agentName}
                  </span>
                  {expanded && (
                    <span className="session-activity-label">
                      {activity.label}
                    </span>
                  )}
                  {expanded && activity.command && (
                    <code className="session-activity-command">
                      {activity.command}
                    </code>
                  )}
                </span>
                {age && (
                  <span
                    className="session-activity-age"
                    aria-label={`Elapsed ${age}`}
                  >
                    {age}
                  </span>
                )}
                <ChevronRight size={14} aria-hidden="true" />
              </button>
            </li>
          );
        })}
      </ul>
      {activities.length > 1 && (
        <button
          type="button"
          className="session-activity-more"
          aria-expanded={expanded}
          aria-label={
            expanded ? "Show less" : `Show ${activities.length - 1} more`
          }
          onClick={() => setExpanded((value) => !value)}
        >
          {expanded ? "Show less" : `+${activities.length - 1}`}
        </button>
      )}
    </section>
  );
}
