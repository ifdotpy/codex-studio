import { useEffect, useState } from "react";
import { api } from "../api";
import "./browser-access-notice.css";

type BrowserStatus = { enabled: boolean; reason?: string | null };

export default function BrowserAccessNotice({
  accountKey,
  active,
}: {
  accountKey: string;
  active: boolean;
}) {
  const [status, setStatus] = useState<BrowserStatus | null>(null);

  useEffect(() => {
    setStatus(null);
    if (!active) return;
    const controller = new AbortController();
    void api<{ browser?: BrowserStatus }>(
      `/api/desktop?account_key=${encodeURIComponent(accountKey)}`,
      undefined,
      { signal: controller.signal },
    )
      .then((data) => setStatus(data.browser || null))
      .catch(() => {});
    return () => controller.abort();
  }, [accountKey, active]);

  if (!active || !status || status.enabled) return null;
  return (
    <p className="browser-access-notice" role="status">
      Browser access is off: {status.reason || "not available for this account."}
    </p>
  );
}
