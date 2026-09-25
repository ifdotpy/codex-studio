import { localDateTime } from "../local-time";
import { useState } from "react";
import { Button } from "@mantine/core";
import type { LimitRecovery } from "../limitRecovery";
import { copyText } from "../clipboard";
import { api, errorText } from "../api";
import type { Json } from "../types";

export default function LimitRecoveryNotice({
  recovery,
  agentId,
  usageResume,
  inline = false,
}: {
  recovery: LimitRecovery;
  agentId?: string;
  usageResume?: Json;
  inline?: boolean;
}) {
  const [copyStatus, setCopyStatus] = useState("");
  const [confirmed, setConfirmed] = useState<Json | null>(null);
  const [busy, setBusy] = useState(false);
  const [resumeError, setResumeError] = useState("");
  const resume =
    confirmed && (confirmed.updatedAt || 0) > (usageResume?.updatedAt || 0)
      ? confirmed
      : usageResume;
  const scheduled = resume?.status === "scheduled";
  const plannedAt = resume?.plannedAt;
  const nextCheckAt = resume?.dueAt;
  const canToggle =
    !!agentId &&
    !!resume?.id &&
    (scheduled || resume?.reason === "Automatic resume is off for this chat.");
  const toggleResume = async (enabled: boolean) => {
    if (!agentId || !resume?.id || busy) return;
    setBusy(true);
    setResumeError("");
    try {
      const updated = await api<Json>("/api/usage-resume", {
        id: agentId,
        resume_id: resume.id,
        enabled,
      });
      setConfirmed(updated);
    } catch (error) {
      setResumeError(
        `Automatic resume choice could not be confirmed. ${errorText(error)}`,
      );
    } finally {
      setBusy(false);
    }
  };
  return (
    <div
      className={`account-limit-recovery${inline ? " inline" : ""}`}
      role="status"
    >
      <strong>{recovery.title}</strong>
      <p>{recovery.message}</p>
      {scheduled &&
        (typeof plannedAt === "number" || typeof nextCheckAt === "number") && (
          <p>
            {typeof plannedAt === "number"
              ? "Automatic resume planned for "
              : "Next account check "}
            <time
              dateTime={new Date(
                (plannedAt ?? nextCheckAt!) * 1000,
              ).toISOString()}
            >
              {localDateTime(new Date((plannedAt ?? nextCheckAt!) * 1000))}
            </time>
          </p>
        )}
      {resume?.reason === "Automatic resume is off for this chat." && (
        <p>Automatic resume is off for this chat.</p>
      )}
      {canToggle && (
        <Button
          size="compact-xs"
          disabled={busy}
          onClick={() => void toggleResume(!scheduled)}
        >
          {scheduled ? "Turn off automatic resume" : "Turn on automatic resume"}
        </Button>
      )}
      {resumeError && <p role="alert">{resumeError}</p>}
      {recovery.resetAt && (
        <p>
          Reported reset:{" "}
          <time dateTime={new Date(recovery.resetAt * 1000).toISOString()}>
            {localDateTime(new Date(recovery.resetAt * 1000))}
          </time>
        </p>
      )}
      {recovery.action && (
        <>
          <Button
            component="a"
            size="compact-xs"
            href={recovery.action.href}
            target="_blank"
            rel="noreferrer"
          >
            {recovery.action.label}
          </Button>
          <small>Use this chat's account in ChatGPT.</small>
        </>
      )}
      {recovery.ownerRequest && (
        <>
          <Button
            size="compact-xs"
            onClick={async () => {
              try {
                await copyText(recovery.ownerRequest!);
                setCopyStatus(
                  "Request copied. Send it to your workspace owner.",
                );
              } catch {
                setCopyStatus(recovery.ownerRequest!);
              }
            }}
          >
            Copy request for owner
          </Button>
          {copyStatus && <p>{copyStatus}</p>}
        </>
      )}
    </div>
  );
}
