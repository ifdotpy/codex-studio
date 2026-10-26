import { useServerApi } from "../servers/ServerServicesContext";
import { localDateTime } from "../local-time";
import { useEffect, useState } from "react";
import { Button } from "@mantine/core";
import type { LimitRecovery } from "../usage/limitRecovery";
import { copyText } from "../clipboard/clipboard";
import { errorText, type PostResult } from "../api";
import type { Agent } from "../types";

/**
 * The one line of a usage limit: when it resets, or that Studio resumes the chat by itself,
 * or, after the reset time, that Studio checks the account. The account limits page keeps
 * the model eligibility details.
 */
export function limitLine(
  resetAt: number | undefined,
  resumes: boolean,
  now: number,
) {
  if (resetAt != null && now >= resetAt)
    return resumes
      ? "The limit has reset. Studio is checking the account."
      : "The limit has reset. Refresh to check the account.";
  if (resumes)
    return "Limit reached. Studio resumes this chat after the reset.";
  return resetAt != null
    ? `Limit reached. Resets at ${localDateTime(new Date(resetAt * 1000))}.`
    : "Limit reached.";
}

export default function LimitRecoveryNotice({
  recovery,
  agentId,
  usageResume,
  inline = false,
  tabIndex,
  onRefresh,
}: {
  recovery?: LimitRecovery | undefined;
  agentId?: string;
  usageResume?: Agent["usageResume"] | undefined;
  inline?: boolean;
  tabIndex?: number | undefined;
  /** Read the account limits again. */
  onRefresh?: (() => Promise<void> | void) | undefined;
}) {
  const { post } = useServerApi();
  const [now, setNow] = useState(() => Date.now() / 1000);
  const [refreshing, setRefreshing] = useState(false);

  const [copyStatus, setCopyStatus] = useState("");
  const [confirmed, setConfirmed] =
    useState<PostResult<"/api/usage-resume"> | null>(null);
  const [busy, setBusy] = useState(false);
  const [resumeError, setResumeError] = useState("");
  const resume =
    confirmed && (confirmed.updatedAt || 0) > (usageResume?.updatedAt || 0)
      ? confirmed
      : usageResume;
  const scheduled = resume?.status === "scheduled";
  const plannedAt = resume?.plannedAt;
  const nextCheckAt = resume?.dueAt;
  const auth = resume?.cause === "auth";
  const resetAt =
    recovery?.resetAt ?? recovery?.eligibility?.resetAt ?? undefined;
  // The line changes when the reset time passes.
  useEffect(() => {
    if (resetAt == null || resetAt <= Date.now() / 1000) return;
    const timer = setTimeout(
      () => setNow(Date.now() / 1000),
      resetAt * 1000 - Date.now() + 50,
    );
    return () => clearTimeout(timer);
  }, [resetAt]);
  const canToggle =
    !!agentId &&
    !!resume?.id &&
    (scheduled || resume?.reason === "Automatic resume is off for this chat.");
  const toggleResume = async (enabled: boolean) => {
    if (!agentId || !resume?.id || busy) return;
    setBusy(true);
    setResumeError("");
    try {
      const updated = await post("/api/usage-resume", {
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
      {auth ? (
        <>
          <strong>
            {scheduled
              ? "Waiting for the account sign-in"
              : "Automatic resume is off"}
          </strong>
          <p>
            {scheduled
              ? "Studio will continue this chat when the account works again."
              : "Turn on automatic resume to continue this chat after sign-in works."}
          </p>
          {scheduled &&
            (typeof plannedAt === "number" ||
              typeof nextCheckAt === "number") && (
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
        </>
      ) : (
        <p>
          {limitLine(resetAt, scheduled, Math.max(now, Date.now() / 1000))}
          {!scheduled && onRefresh && (
            <>
              {" "}
              <Button
                size="compact-xs"
                variant="light"
                loading={refreshing}
                tabIndex={tabIndex}
                onClick={async () => {
                  setRefreshing(true);
                  try {
                    await onRefresh();
                  } finally {
                    setRefreshing(false);
                  }
                }}
              >
                Refresh
              </Button>
            </>
          )}
        </p>
      )}
      {resume?.reason === "Automatic resume is off for this chat." && (
        <p>Automatic resume is off for this chat.</p>
      )}
      {canToggle && (
        <Button
          tabIndex={tabIndex}
          size="compact-xs"
          disabled={busy}
          onClick={() => void toggleResume(!scheduled)}
        >
          {scheduled ? "Turn off automatic resume" : "Turn on automatic resume"}
        </Button>
      )}
      {resumeError && <p role="alert">{resumeError}</p>}
      {recovery?.action && (
        <>
          <Button
            tabIndex={tabIndex}
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
      {recovery?.ownerRequest && (
        <>
          <Button
            tabIndex={tabIndex}
            size="compact-xs"
            onClick={async () => {
              try {
                await copyText(recovery!.ownerRequest!);
                setCopyStatus(
                  "Request copied. Send it to your workspace owner.",
                );
              } catch {
                setCopyStatus(recovery!.ownerRequest!);
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
