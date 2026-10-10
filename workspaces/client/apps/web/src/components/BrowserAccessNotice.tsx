import { Tooltip } from "@mantine/core";
import { useEffect, useState } from "react";
import { SettingsRow } from "./ui/primitives";
import { get, type GetResult } from "../api";
import "./browser-access-notice.css";

type BrowserStatus = NonNullable<GetResult<"/api/desktop">["browser"]>;

export default function BrowserAccessNotice({
  accountKey,
  active,
  compact = false,
}: {
  accountKey: string;
  active: boolean;
  compact?: boolean;
}) {
  const [status, setStatus] = useState<BrowserStatus | null>(null);

  useEffect(() => {
    setStatus(null);
    if (!active) return;
    const controller = new AbortController();
    void get("/api/desktop", {
      query: { account_key: accountKey },
      signal: controller.signal,
    })
      .then((data) => setStatus(data.browser || null))
      .catch(() => {});
    return () => controller.abort();
  }, [accountKey, active]);

  if (compact)
    return (
      <SettingsRow label="Browser">
        <Tooltip
          label={
            status?.reason ||
            (status?.enabled
              ? "Browser access is available."
              : "Not available for this account.")
          }
        >
          <span role="status">{status?.enabled ? "On" : "Off"}</span>
        </Tooltip>
      </SettingsRow>
    );
  if (!active || !status || status.enabled) return null;
  return (
    <div className="browser-access-notice" role="status">
      <SettingsRow label="Browser access">
        <span>Off</span>
      </SettingsRow>
      <details>
        <summary>Details</summary>
        {status.reason || "Not available for this account."}
      </details>
    </div>
  );
}
