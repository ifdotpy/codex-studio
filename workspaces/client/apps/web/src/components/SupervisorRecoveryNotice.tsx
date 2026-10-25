import { useEffect, useState } from "react";
import { useServerSync } from "../servers/ServerServicesContext";
import { desktopNotices, type DesktopNotice } from "./desktopNotices";
import "./supervisor-recovery-notice.css";

// Host notices from /api/desktop: supervisor recovery and low disk space.
export default function SupervisorRecoveryNotice() {
  const { subscribeStateProjection } = useServerSync();
  const [notices, setNotices] = useState<DesktopNotice[]>([]);

  useEffect(
    () =>
      subscribeStateProjection(
        (snapshot) => {
          if (snapshot?.runtime.desktop)
            setNotices(desktopNotices(snapshot.runtime.desktop));
        },
        () => {},
      ),
    [subscribeStateProjection],
  );

  return notices.map((notice) => (
    <div
      key={notice.id}
      className="supervisor-recovery-notice"
      data-level={notice.level}
      title={notice.text}
      aria-label={notice.text}
      role="alert"
    >
      {notice.text}
    </div>
  ));
}
