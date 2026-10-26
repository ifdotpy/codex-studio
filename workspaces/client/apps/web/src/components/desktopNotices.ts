import type { components } from "../generated/api";

type DiskSpaceStatus = components["schemas"]["DiskSpaceStatus"];

export type DesktopNotice = {
  id: "supervisor" | "disk";
  text: string;
  level: NonNullable<DiskSpaceStatus["level"]>;
};

type DesktopNoticeSource = Partial<
  Pick<
    components["schemas"]["DesktopResponse"],
    "supervisorNotice" | "diskSpace"
  >
>;

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
  const rank = (notice: DesktopNotice) =>
    notice.level === "full" ? 2 : notice.level === "critical" ? 1 : 0;
  return notices.sort((a, b) => rank(b) - rank(a));
}
