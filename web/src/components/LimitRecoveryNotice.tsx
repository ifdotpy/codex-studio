import { useState } from "react";
import { Button } from "@mantine/core";
import type { LimitRecovery } from "../limitRecovery";

export default function LimitRecoveryNotice({
  recovery,
}: {
  recovery: LimitRecovery;
}) {
  const [copyStatus, setCopyStatus] = useState("");
  return (
    <div className="account-limit-recovery" role="status">
      <strong>{recovery.title}</strong>
      <p>{recovery.message}</p>
      {recovery.resetAt && (
        <p>
          Reported reset:{" "}
          <time dateTime={new Date(recovery.resetAt * 1000).toISOString()}>
            {new Date(recovery.resetAt * 1000).toLocaleString()}
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
                await navigator.clipboard.writeText(recovery.ownerRequest!);
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
