import { useEffect, useState } from "react";
import { get } from "../api";
import "./supervisor-recovery-notice.css";

export default function SupervisorRecoveryNotice() {
  const [notice, setNotice] = useState<string | null>(null);

  useEffect(() => {
    let live = true;
    let timer: number | undefined;
    const controller = new AbortController();
    const refresh = async () => {
      try {
        const data = await get("/api/desktop", {
          signal: controller.signal,
        });
        if (live) setNotice(data.supervisorNotice || null);
      } catch {
        // A transient status failure does not replace the current notice.
      } finally {
        if (live) timer = window.setTimeout(refresh, 10000);
      }
    };
    void refresh();
    return () => {
      live = false;
      controller.abort();
      window.clearTimeout(timer);
    };
  }, []);

  if (!notice) return null;
  return (
    <div className="supervisor-recovery-notice" role="alert">
      {notice}
    </div>
  );
}
