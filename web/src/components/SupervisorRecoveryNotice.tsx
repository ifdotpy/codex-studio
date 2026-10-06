import { useEffect, useState } from "react";
import { getShared } from "../sharedRead";
import { watchResourceReads } from "./watchResourceReads";
import "./supervisor-recovery-notice.css";

export default function SupervisorRecoveryNotice() {
  const [notice, setNotice] = useState<string | null>(null);

  useEffect(() => {
    let live = true;
    const stop = watchResourceReads(
      { kind: "desktop" },
      async () => {
        const data = await getShared("/api/desktop");
        if (live) setNotice(data.supervisorNotice || null);
      },
      () => {
        // A transient status failure does not replace the current notice.
      },
    );
    return () => {
      live = false;
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
