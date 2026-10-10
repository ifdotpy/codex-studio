type LocalTimeOptions = Omit<
  Intl.DateTimeFormatOptions,
  "timeZone" | "hour12" | "hourCycle"
>;

// Resolve the device time zone on every call, including after a system change.
export function localDateTime(date: Date, options: LocalTimeOptions = {}) {
  return date.toLocaleString(undefined, { ...options, hourCycle: "h23" });
}

export function localTime(date: Date, options: LocalTimeOptions = {}) {
  return date.toLocaleTimeString(undefined, { ...options, hourCycle: "h23" });
}

// Provider notices contain text clocks, rather than timestamp fields.
export function serviceTimeText(text: string, reference = new Date()): string {
  if (!/^(?:You've hit|You’ve hit|Claude usage limit reached)/i.test(text))
    return text;
  let result = text.replace(
    /\b(\d{1,2})(?::([0-5]\d))?\s*([ap])m\s*\(([A-Za-z_]+(?:\/[A-Za-z_+-]+)+)\)/gi,
    (original, hour, minute, period, zone) => {
      if (+hour < 1 || +hour > 12 || !Number.isFinite(reference.getTime()))
        return original;
      try {
        const source = new Intl.DateTimeFormat("en-US", {
          timeZone: zone,
          year: "numeric",
          month: "2-digit",
          day: "2-digit",
          hour: "2-digit",
          minute: "2-digit",
          second: "2-digit",
          hourCycle: "h23",
        });
        const fields = (date: Date) =>
          Object.fromEntries(
            source.formatToParts(date).map((part) => [part.type, part.value]),
          );
        const day = fields(reference);
        const clock = Date.UTC(
          +day.year,
          +day.month - 1,
          +day.day,
          (+hour % 12) + (period.toLowerCase() === "p" ? 12 : 0),
          +(minute || 0),
        );
        let instant = clock;
        for (let n = 0; n < 3; n++) {
          const part = fields(new Date(instant));
          const shown = Date.UTC(
            +part.year,
            +part.month - 1,
            +part.day,
            +part.hour,
            +part.minute,
            +part.second,
          );
          instant += clock - shown;
        }
        return localTime(new Date(instant), {
          hour: "2-digit",
          minute: "2-digit",
        });
      } catch {
        return original;
      }
    },
  );
  result = result.replace(
    /\b\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d(?:\.\d+)?Z\b/g,
    (iso) => localDateTime(new Date(iso)),
  );
  return result.replace(
    /\b(\d{1,2})(?::([0-5]\d))?\s*([ap])m\b/gi,
    (original, hour, minute, period) =>
      +hour >= 1 && +hour <= 12
        ? `${String((+hour % 12) + (period.toLowerCase() === "p" ? 12 : 0)).padStart(2, "0")}:${minute || "00"}`
        : original,
  );
}
