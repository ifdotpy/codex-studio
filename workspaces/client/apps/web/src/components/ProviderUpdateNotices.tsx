import type { components } from "../generated/api";

type ProviderUpdateWarning = components["schemas"]["ProviderVersionWarningDto"];

export default function ProviderUpdateNotices({
  notices,
}: {
  notices: ProviderUpdateWarning[];
}) {
  return (
    <>
      {notices
        .filter(
          (notice) =>
            notice.level === "info" && notice.id.startsWith("provider-update:"),
        )
        .map((notice) => (
          <p role="status" className="native-runtime-warning" key={notice.id}>
            {notice.message}
          </p>
        ))}
    </>
  );
}
