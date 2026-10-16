import type { components } from "../generated/api";

type DiskSpaceStatus = components["schemas"]["DiskSpaceStatus"];

export type DesktopNotice = {
  id: "supervisor" | "disk";
  text: string;
  level: NonNullable<DiskSpaceStatus["level"]>;
};

type DesktopNoticeSource = {
  supervisorNotice?: string | null;
  diskSpace?: Pick<DiskSpaceStatus, "level" | "message"> | null;
};

// The notices that /api/desktop asks the window to show, most severe first.
export function desktopNotices(data: DesktopNoticeSource): DesktopNotice[] {
  const notices: DesktopNotice[] = [];
  const disk = data.diskSpace;
  if (disk?.level && disk.message)
    notices.push({ id: "disk", text: disk.message, level: disk.level });
  if (data.supervisorNotice)
    notices.push({
      id: "supervisor",
      text: data.supervisorNotice,
      level: "warning",
    });
  return notices.sort(
    (a, b) => Number(b.level === "critical") - Number(a.level === "critical"),
  );
}
