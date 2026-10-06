import { useEffect, useState } from "react";
import { type GetResult } from "../api";
import { getShared } from "../sharedRead";
import "./browser-access-notice.css";

type BrowserStatus = NonNullable<GetResult<"/api/desktop">["browser"]>;

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
    let live = true;
    void getShared("/api/desktop", {
      query: { account_key: accountKey },
    })
      .then((data) => {
        if (live) setStatus(data.browser || null);
      })
      .catch(() => {});
    return () => {
      live = false;
    };
  }, [accountKey, active]);

  if (!active || !status || status.enabled) return null;
  return (
    <p className="browser-access-notice" role="status">
      Browser access is off:{" "}
      {status.reason || "not available for this account."}
    </p>
  );
}
