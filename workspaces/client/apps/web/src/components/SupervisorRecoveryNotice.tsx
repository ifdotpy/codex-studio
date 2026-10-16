import { useEffect, useState } from "react";
import { get } from "../api";
import { desktopNotices, type DesktopNotice } from "./desktopNotices";
import { watchResourceReads } from "./watchResourceReads";
import "./supervisor-recovery-notice.css";

// Host notices from /api/desktop: supervisor recovery and low disk space.
export default function SupervisorRecoveryNotice() {
  const [notices, setNotices] = useState<DesktopNotice[]>([]);

  useEffect(() => {
    let live = true;
    const controller = new AbortController();
    const stop = watchResourceReads(
      { kind: "desktop" },
      async () => {
        const data = await get("/api/desktop", {
          signal: controller.signal,
        });
        if (live) setNotices(desktopNotices(data));
      },
      () => {
        // A transient status failure does not replace the current notices.
      },
    );
    return () => {
      live = false;
      controller.abort();
      stop();
    };
  }, []);

  return notices.map((notice) => (
    <div
      key={notice.id}
      className="supervisor-recovery-notice"
      data-level={notice.level}
      role="alert"
    >
      {notice.text}
    </div>
  ));
}
