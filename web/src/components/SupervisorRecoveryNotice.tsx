import { useEffect, useState } from "react";
import { get } from "../api";
import { watchResourceReads } from "./watchResourceReads";
import "./supervisor-recovery-notice.css";

export default function SupervisorRecoveryNotice() {
  const [notice, setNotice] = useState<string | null>(null);

  useEffect(() => {
    let live = true;
    const controller = new AbortController();
    const stop = watchResourceReads(
      { kind: "desktop" },
      async () => {
        const data = await get("/api/desktop", {
          signal: controller.signal,
        });
        if (live) setNotice(data.supervisorNotice || null);
      },
      () => {
        // A transient status failure does not replace the current notice.
      },
    );
    return () => {
      live = false;
      controller.abort();
      stop();
    };
  }, []);

  if (!notice) return null;
  return (
    <div className="supervisor-recovery-notice" role="alert">
      {notice}
    </div>
  );
}
