import { describe, expect, it } from "vitest";
import { desktopNotices } from "./desktopNotices";

describe("desktopNotices", () => {
  it("shows nothing when the desktop reports no notice", () => {
    expect(desktopNotices({})).toEqual([]);
    expect(
      desktopNotices({
        supervisorNotice: null,
        diskSpace: { level: null, message: null },
      }),
    ).toEqual([]);
  });

  it("puts a critical disk warning before the supervisor notice", () => {
    expect(
      desktopNotices({
        supervisorNotice: "Supervisor stopped.",
        diskSpace: { level: "critical", message: "Only 3.0 GB free." },
      }).map((notice) => [notice.id, notice.level]),
    ).toEqual([
      ["disk", "critical"],
      ["supervisor", "warning"],
    ]);
  });

  it("shows the one disk-full notice first", () => {
    const text =
      "Disk is full: 130 MiB free. Studio starts no new turns and resumes held agents when 2 GiB is free. Free disk space now.";
    expect(
      desktopNotices({
        supervisorNotice: "Supervisor stopped.",
        diskSpace: { level: "full", message: text },
      }),
    ).toEqual([
      { id: "disk", text, level: "full" },
      { id: "supervisor", text: "Supervisor stopped.", level: "warning" },
    ]);
  });

  it("keeps a low disk warning", () => {
    expect(
      desktopNotices({
        diskSpace: { level: "warning", message: "Only 12.0 GB free." },
      }),
    ).toEqual([{ id: "disk", text: "Only 12.0 GB free.", level: "warning" }]);
  });
});
