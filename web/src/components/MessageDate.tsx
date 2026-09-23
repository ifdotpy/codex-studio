import { localDateTime } from "../local-time";
export default function MessageDate({ at }: { at?: number | string | null }) {
  const value =
    at == null ||
    at === "" ||
    (typeof at === "number" && (!Number.isFinite(at) || at <= 0))
      ? null
      : new Date(typeof at === "number" ? at * 1000 : at);
  const valid = value && Number.isFinite(value.getTime());
  return (
    <time
      className="message-date"
      dateTime={valid ? value.toISOString() : undefined}
    >
      {valid
        ? localDateTime(value, {
            year: "numeric",
            month: "short",
            day: "numeric",
            hour: "2-digit",
            minute: "2-digit",
          })
        : "Date unavailable"}
    </time>
  );
}
